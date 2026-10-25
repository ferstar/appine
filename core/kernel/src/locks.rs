//! fcntl(2) 的记录锁：传统的 POSIX 锁和 OFD 锁（open file description lock）。
//!
//! 锁按 inode 记录，每把锁有一个持有者：POSIX 锁属于 fd 表（同一个进程的所有线程
//! 共用），关闭这个 fd 表里任何一个指向该文件的 fd 都会释放它在该文件上的所有锁；
//! OFD 锁属于打开文件，最后一个引用关闭时释放。持有者用弱引用记录，持有者消失
//! （进程退出、打开文件析构）后锁自动失效。同一个持有者的锁互不重叠，相邻的同类
//! 锁合并，与 Linux 相同。

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::fd::{FdTable, FileRef, OpenFile};
use crate::kernel::{Kernel, Tid, request_wake_files};
use crate::vfs::store::Ino;

/// 锁的持有者。
#[derive(Clone)]
pub(crate) enum Owner {
    /// 传统的 POSIX 锁：属于 fd 表。
    Posix(Weak<RefCell<FdTable>>),
    /// OFD 锁：属于打开文件。
    Ofd(Weak<RefCell<OpenFile>>),
}

impl Owner {
    fn same(&self, other: &Owner) -> bool {
        match (self, other) {
            (Owner::Posix(a), Owner::Posix(b)) => a.ptr_eq(b),
            (Owner::Ofd(a), Owner::Ofd(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    fn alive(&self) -> bool {
        match self {
            Owner::Posix(w) => w.strong_count() > 0,
            Owner::Ofd(w) => w.strong_count() > 0,
        }
    }
}

/// 一把锁，覆盖 `[start, end)`；`end` 为 `u64::MAX` 表示到文件末尾以及之后。
#[derive(Clone)]
pub(crate) struct Lock {
    pub owner: Owner,
    /// 加锁的进程（F_GETLK 报告的 `l_pid`）；OFD 锁是 -1。
    pub pid: Tid,
    pub start: u64,
    pub end: u64,
    /// 写锁（排他）；否则是读锁（共享）。
    pub write: bool,
}

impl Lock {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    /// 和持有者不同、类型不相容的请求冲突。
    fn conflicts(&self, req: &Lock) -> bool {
        !self.owner.same(&req.owner)
            && (self.write || req.write)
            && self.overlaps(req.start, req.end)
    }
}

/// 一次加锁请求在等待的东西（死锁检测用）。
pub(crate) struct Waiter {
    pub ino: Ino,
    pub req: Lock,
}

/// 所有 inode 上的记录锁。
#[derive(Default)]
pub(crate) struct Locks {
    by_ino: std::collections::BTreeMap<Ino, Vec<Lock>>,
    /// 正在 F_SETLKW 里等待的 POSIX 锁请求。
    waiters: Vec<Waiter>,
}

impl Locks {
    /// 和请求冲突的第一把锁（按起点排序）。
    pub fn conflict(&mut self, ino: Ino, req: &Lock) -> Option<Lock> {
        let locks = self.by_ino.get_mut(&ino)?;
        locks.retain(|l| l.owner.alive());
        locks.iter().find(|l| l.conflicts(req)).cloned()
    }

    /// 设置（或者在 `write` 为 None 时解除）`req` 范围里的锁。调用方已经确认没有冲突。
    pub fn set(&mut self, ino: Ino, req: Lock, unlock: bool) {
        let locks = self.by_ino.entry(ino).or_default();
        let (mut start, mut end) = (req.start, req.end);
        let mut kept = Vec::with_capacity(locks.len() + 2);
        for l in locks.drain(..).filter(|l| l.owner.alive()) {
            if !l.owner.same(&req.owner) {
                kept.push(l);
                continue;
            }
            // 同类、重叠或相邻的锁并进新锁
            if !unlock && l.write == req.write && l.start <= end && start <= l.end {
                start = start.min(l.start);
                end = end.max(l.end);
                continue;
            }
            if !l.overlaps(req.start, req.end) {
                kept.push(l);
                continue;
            }
            // 类型不同或者解锁：只留下请求范围以外的部分
            if l.start < req.start {
                kept.push(Lock {
                    end: req.start,
                    ..l.clone()
                });
            }
            if l.end > req.end {
                kept.push(Lock {
                    start: req.end,
                    ..l
                });
            }
        }
        if !unlock {
            kept.push(Lock { start, end, ..req });
        }
        kept.sort_by_key(|l| l.start);
        if kept.is_empty() {
            self.by_ino.remove(&ino);
        } else {
            *locks = kept;
        }
        request_wake_files();
    }

    /// 等待 `req` 会不会死锁：沿着"等待的请求被谁的锁挡住"一路找下去，回到自己就是
    /// 死锁（`posix_locks_deadlock`）。只检查 POSIX 锁。
    pub fn deadlock(&mut self, ino: Ino, req: &Lock) -> bool {
        let Owner::Posix(_) = req.owner else {
            return false;
        };
        let mut blocker = match self.conflict(ino, req) {
            Some(l) => l.owner,
            None => return false,
        };
        for _ in 0..self.waiters.len() + 1 {
            if blocker.same(&req.owner) {
                return true;
            }
            let Some(w) = self.waiters.iter().find(|w| w.req.owner.same(&blocker)) else {
                return false;
            };
            let (wino, wreq) = (w.ino, w.req.clone());
            match self.conflict(wino, &wreq) {
                Some(l) => blocker = l.owner,
                None => return false,
            }
        }
        false
    }

    /// 记下（`Some`）或者清除一个持有者在 F_SETLKW 里的等待。
    pub fn set_waiting(&mut self, owner: &Owner, wait: Option<Waiter>) {
        self.waiters
            .retain(|w| !w.req.owner.same(owner) && w.req.owner.alive());
        if let Some(w) = wait {
            self.waiters.push(w);
        }
    }

    /// 关闭了 fd 表 `fds` 里的这些文件：释放这个 fd 表在这些文件上的 POSIX 锁。
    pub fn closed(&mut self, fds: &Rc<RefCell<FdTable>>, files: &[FileRef]) {
        let owner = Owner::Posix(Rc::downgrade(fds));
        for file in files {
            let Some(ino) = file.borrow().ino() else {
                continue;
            };
            if let Some(locks) = self.by_ino.get_mut(&ino) {
                let before = locks.len();
                locks.retain(|l| !l.owner.same(&owner) && l.owner.alive());
                if locks.len() != before {
                    request_wake_files();
                }
                if locks.is_empty() {
                    self.by_ino.remove(&ino);
                }
            }
        }
    }
}

/// 关闭 fd 之后调用：释放当前进程在这些文件上的 POSIX 锁。
pub(crate) fn closed(k: &mut Kernel, fds: &Rc<RefCell<FdTable>>, files: &[FileRef]) {
    if !files.is_empty() {
        k.locks.closed(fds, files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd::{FileKind, OpenFile};

    fn owner(t: &Rc<RefCell<FdTable>>) -> Owner {
        Owner::Posix(Rc::downgrade(t))
    }

    fn lock(o: &Owner, start: u64, end: u64, write: bool) -> Lock {
        Lock {
            owner: o.clone(),
            pid: 1,
            start,
            end,
            write,
        }
    }

    fn ranges(l: &Locks, ino: Ino) -> Vec<(u64, u64, bool)> {
        l.by_ino.get(&ino).map_or(Vec::new(), |v| {
            v.iter().map(|l| (l.start, l.end, l.write)).collect()
        })
    }

    #[test]
    fn locks_split_merge_and_conflict() {
        let (a, b) = (
            Rc::new(RefCell::new(FdTable::new())),
            Rc::new(RefCell::new(FdTable::new())),
        );
        let (oa, ob) = (owner(&a), owner(&b));
        let mut l = Locks::default();
        l.set(1, lock(&oa, 0, 100, true), false);
        // 中间改成读锁：拆成三段
        l.set(1, lock(&oa, 40, 60, false), false);
        assert_eq!(
            ranges(&l, 1),
            vec![(0, 40, true), (40, 60, false), (60, 100, true)]
        );
        // 别的持有者：读锁和读锁不冲突，和写锁冲突
        assert!(l.conflict(1, &lock(&ob, 45, 50, false)).is_none());
        assert_eq!(l.conflict(1, &lock(&ob, 30, 50, false)).unwrap().start, 0);
        // 相邻的同类锁合并
        l.set(1, lock(&oa, 40, 60, true), false);
        assert_eq!(ranges(&l, 1), vec![(0, 100, true)]);
        // 解锁中间一段
        l.set(1, lock(&oa, 10, 20, false), true);
        assert_eq!(ranges(&l, 1), vec![(0, 10, true), (20, 100, true)]);
        // 持有者消失，锁随之失效
        drop(a);
        assert!(l.conflict(1, &lock(&ob, 0, u64::MAX, true)).is_none());
    }

    #[test]
    fn closing_any_fd_releases_the_posix_locks() {
        let a = Rc::new(RefCell::new(FdTable::new()));
        let mut l = Locks::default();
        let file = OpenFile::new(FileKind::Path(crate::vfs::Node::Store(7)), 0);
        l.set(7, lock(&owner(&a), 0, 10, true), false);
        l.closed(&a, &[file]);
        assert!(ranges(&l, 7).is_empty());
    }

    #[test]
    fn waiting_in_a_cycle_is_a_deadlock() {
        let (a, b) = (
            Rc::new(RefCell::new(FdTable::new())),
            Rc::new(RefCell::new(FdTable::new())),
        );
        let (oa, ob) = (owner(&a), owner(&b));
        let mut l = Locks::default();
        l.set(1, lock(&oa, 0, 10, true), false);
        l.set(1, lock(&ob, 10, 20, true), false);
        // a 在等 b 的锁；b 再去等 a 的锁就是死锁
        let wait = lock(&oa, 10, 20, true);
        assert!(!l.deadlock(1, &wait));
        l.set_waiting(&oa, Some(Waiter { ino: 1, req: wait }));
        assert!(l.deadlock(1, &lock(&ob, 0, 10, true)));
    }
}
