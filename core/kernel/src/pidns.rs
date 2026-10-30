//! pid 命名空间（pid_namespaces(7)）。内核里的 pid、tid 总是全局的（开机时的命名空间里的
//! 编号），每个新建的命名空间记一张全局编号到本层编号的表：进程在它所在的命名空间和所有
//! 祖先命名空间里各有一个编号，系统调用和 procfs 的边界上按调用者（procfs 按挂上它的进程）
//! 的命名空间换算，看不到的进程是 ESRCH 或者 0。命名空间里的第一个进程编号是 1，是它的
//! reaper：命名空间里的孤儿过继给它，它退出时命名空间里的进程都被结束，之后不能再建进程。

use std::collections::HashMap;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::cred::UserNs;
use crate::errno::Errno;
use crate::kernel::{PID_MAX, Tid};

/// 开机时的 pid 命名空间的编号（`/proc/<pid>/ns/pid`）。
pub(crate) const INIT_PID_NS: u64 = 4_026_531_836;
/// 层数超过它的 pid 命名空间里不能再建（Linux 的 `MAX_PID_NS_LEVEL`）。
pub(crate) const MAX_PID_NS_LEVEL: u32 = 32;
/// 编号用到 `PID_MAX` 之后从这里重新找（Linux 的 `RESERVED_PIDS`）。
const RESERVED_PIDS: Tid = 300;

/// 一个 pid 命名空间。开机时的那个不记表：全局编号就是它的编号。
#[derive(Debug)]
pub(crate) struct PidNs {
    pub id: u64,
    pub parent: Option<Rc<PidNs>>,
    /// 嵌套的层数，开机时的是 0。
    pub level: u32,
    /// 属主：建立它时所在的用户命名空间（setns 要在这里有 `CAP_SYS_ADMIN`）。
    pub owner: Rc<UserNs>,
    /// 全局编号到本层编号，和反过来的。
    local: RefCell<HashMap<Tid, Tid>>,
    global: RefCell<HashMap<Tid, Tid>>,
    /// 下一个要试的本层编号。
    next: std::cell::Cell<Tid>,
    /// 编号为 1 的进程（全局 pid）。
    pub reaper: std::cell::Cell<Option<Tid>>,
    /// reaper 退出了：不能再在里面建进程（ENOMEM）。
    pub dead: std::cell::Cell<bool>,
}

impl PidNs {
    /// 开机时的 pid 命名空间（每次新建一份，按编号比较；它不记表）。
    pub fn init() -> Rc<PidNs> {
        Rc::new(PidNs {
            id: INIT_PID_NS,
            parent: None,
            level: 0,
            owner: UserNs::init(),
            local: Default::default(),
            global: Default::default(),
            next: std::cell::Cell::new(1),
            reaper: std::cell::Cell::new(None),
            dead: std::cell::Cell::new(false),
        })
    }

    /// 在 `parent` 下面新建的命名空间，属于用户命名空间 `owner`。
    pub fn child(parent: &Rc<PidNs>, owner: Rc<UserNs>) -> Rc<PidNs> {
        Rc::new(PidNs {
            id: crate::vfs::new_ns_id(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner,
            local: Default::default(),
            global: Default::default(),
            next: std::cell::Cell::new(1),
            reaper: std::cell::Cell::new(None),
            dead: std::cell::Cell::new(false),
        })
    }

    /// 全局编号 `g` 在这个命名空间里的编号；在这里看不到时是 None（`pid_nr_ns`）。
    pub fn vnr(&self, g: Tid) -> Option<Tid> {
        if self.level == 0 {
            return Some(g);
        }
        self.local.borrow().get(&g).copied()
    }

    /// 这个命名空间里的编号 `l` 对应的全局编号（`find_vpid`，不检查有没有这个进程）。
    pub fn find(&self, l: Tid) -> Option<Tid> {
        if self.level == 0 {
            return Some(l);
        }
        self.global.borrow().get(&l).copied()
    }

    /// 全局编号为 `g` 的进程在这里看得到（它在这个命名空间或者后代命名空间里）。
    pub fn sees(&self, g: Tid) -> bool {
        self.vnr(g).is_some()
    }

    /// 这个命名空间里所有看得到的全局编号。
    pub fn members(&self) -> Vec<Tid> {
        self.local.borrow().keys().copied().collect()
    }

    /// `self` 是 `other` 或者它的祖先。
    pub fn is_ancestor_of(&self, other: &PidNs) -> bool {
        let mut cur = Some(other);
        while let Some(ns) = cur {
            if ns.id == self.id {
                return true;
            }
            cur = ns.parent.as_deref();
        }
        false
    }

    /// 从第 `from` 层到这一层的命名空间，按层数从小到大（`/proc/<pid>/status` 的 `NSpid` 等
    /// 每层一列）。
    pub fn levels_from(self: &Rc<PidNs>, from: u32) -> Vec<Rc<PidNs>> {
        let mut out = Vec::new();
        let mut cur = Some(self.clone());
        while let Some(ns) = cur.filter(|ns| ns.level >= from) {
            cur = ns.parent.clone();
            out.push(ns);
        }
        out.reverse();
        out
    }

    /// 设置最后分配的本层编号（写 `ns_last_pid`）。
    pub fn set_last_pid(&self, pid: Tid) {
        self.next.set(pid + 1);
    }

    /// 最后分配的本层编号（`/proc/sys/kernel/ns_last_pid`）。
    pub fn last_pid(&self) -> Tid {
        self.next.get() - 1
    }

    /// 给全局编号 `g` 分配本层编号：指定了 `want` 时用它（已经有人用了是 EEXIST），否则从
    /// 上次的后面找没用过的，到 `PID_MAX` 绕回 300（Linux 的 `RESERVED_PIDS`，`alloc_pid`）。
    fn alloc(&self, g: Tid, want: Option<Tid>) -> Result<Tid, Errno> {
        let mut global = self.global.borrow_mut();
        if let Some(l) = want {
            if global.contains_key(&l) {
                return Err(Errno::EEXIST);
            }
            global.insert(l, g);
            self.local.borrow_mut().insert(g, l);
            return Ok(l);
        }
        let mut l = self.next.get();
        for _ in 0..PID_MAX {
            if l >= PID_MAX {
                l = RESERVED_PIDS;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = global.entry(l) {
                e.insert(g);
                self.local.borrow_mut().insert(g, l);
                self.next.set(l + 1);
                return Ok(l);
            }
            l += 1;
        }
        Err(Errno::EAGAIN)
    }

    /// 新的线程、进程 `g` 在命名空间 `ns` 里：在它和所有祖先（开机时的除外）里各分配一个
    /// 编号，`set_tid` 是 clone3 指定的，从 `ns` 那层起往外（检查过了，见
    /// [`crate::kernel::Kernel::choose_pid`]）。编号是 1 的进程是命名空间的 reaper；reaper
    /// 退出之后是 ENOMEM。
    pub fn register(ns: &Rc<PidNs>, g: Tid, set_tid: &[Tid]) -> Result<(), Errno> {
        if ns.dead.get() {
            return Err(Errno::ENOMEM);
        }
        let mut done: Vec<&PidNs> = Vec::new();
        let mut cur = Some(&**ns);
        let mut want = set_tid.iter().copied();
        while let Some(n) = cur.filter(|n| n.level > 0) {
            match n.alloc(g, want.next()) {
                Ok(1) if n.reaper.get().is_none() => {
                    n.reaper.set(Some(g));
                    done.push(n);
                }
                Ok(_) => done.push(n),
                Err(e) => {
                    for d in done {
                        d.release(g);
                    }
                    return Err(e);
                }
            }
            cur = n.parent.as_deref();
        }
        Ok(())
    }

    /// 全局编号 `g` 不再使用（线程、僵尸进程都没了）：从 `ns` 和它的祖先里去掉。
    pub fn unregister(ns: &PidNs, g: Tid) {
        let mut cur = Some(ns);
        while let Some(n) = cur.filter(|n| n.level > 0) {
            n.release(g);
            cur = n.parent.as_deref();
        }
    }

    fn release(&self, g: Tid) {
        if let Some(l) = self.local.borrow_mut().remove(&g) {
            self.global.borrow_mut().remove(&l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_namespaces() {
        let init = PidNs::init();
        let a = PidNs::child(&init, UserNs::init());
        // 第一个进程是 1 号，成为 reaper；祖先命名空间（开机时的）里用全局编号
        PidNs::register(&a, 100, &[]).unwrap();
        PidNs::register(&a, 101, &[]).unwrap();
        assert_eq!(a.reaper.get(), Some(100));
        assert_eq!((a.vnr(100), a.vnr(101), a.vnr(7)), (Some(1), Some(2), None));
        assert_eq!((a.find(2), a.find(3)), (Some(101), None));
        assert_eq!(init.vnr(101), Some(101));
        assert_eq!(a.last_pid(), 2);
        // 嵌套的命名空间里的进程在每一层各有一个编号
        let b = PidNs::child(&a, UserNs::init());
        PidNs::register(&b, 102, &[]).unwrap();
        assert_eq!((b.vnr(102), a.vnr(102)), (Some(1), Some(3)));
        assert!(!b.sees(101));
        let levels: Vec<u32> = b.levels_from(1).iter().map(|n| n.level).collect();
        assert_eq!(levels, [1, 2]);
        assert_eq!(b.levels_from(0).len(), 3);
        assert!(b.levels_from(3).is_empty());
        assert!(a.is_ancestor_of(&b) && init.is_ancestor_of(&b) && !b.is_ancestor_of(&a));
        // 编号不再用了：各层都去掉，再分配时不重复用刚用过的
        PidNs::unregister(&b, 102);
        assert_eq!((b.vnr(102), a.vnr(102)), (None, None));
        PidNs::register(&a, 103, &[]).unwrap();
        assert_eq!(a.vnr(103), Some(4));
        // 写 ns_last_pid 之后从它的下一个开始；用到 pid_max 之后从 300 起
        a.set_last_pid(41);
        PidNs::register(&a, 104, &[]).unwrap();
        assert_eq!(a.vnr(104), Some(42));
        a.set_last_pid(PID_MAX);
        PidNs::register(&a, 105, &[]).unwrap();
        assert_eq!(a.vnr(105), Some(RESERVED_PIDS));
        // clone3 指定的编号：从里层起往外，用掉的是 EEXIST
        let c = PidNs::child(&a, UserNs::init());
        PidNs::register(&c, 107, &[1, 77]).unwrap();
        assert_eq!(
            (c.vnr(107), a.vnr(107), c.reaper.get()),
            (Some(1), Some(77), Some(107))
        );
        assert_eq!(PidNs::register(&c, 108, &[5, 77]), Err(Errno::EEXIST));
        assert_eq!((c.vnr(108), a.vnr(108)), (None, None));
        // reaper 退出之后不能再在里面建进程
        a.dead.set(true);
        assert_eq!(PidNs::register(&a, 106, &[]), Err(Errno::ENOMEM));
        assert_eq!(a.vnr(106), None);
    }
}
