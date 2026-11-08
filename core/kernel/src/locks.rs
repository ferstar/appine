//! fcntl(2) record locks: traditional POSIX locks and OFD locks (open file description locks).
//!
//! Locks are recorded by inode, and each lock has one owner: a POSIX lock belongs to an fd table
//! (shared by all threads of one process), and closing any fd in that table that refers to the
//! file releases all of the table's locks on the file; an OFD lock belongs to an open file and is
//! released when its last reference is closed. Owners are recorded as weak references, so locks
//! lapse by themselves once their owner is gone (process exit, open file dropped). Locks of one
//! owner never overlap, and adjacent locks of the same type are merged, as on Linux.

use std::rc::{Rc, Weak};

use crate::cell::RefCell;
use crate::fd::{FdTable, FileRef, OpenFile};
use crate::kernel::{Kernel, Tid, request_wake_locks};
use crate::vfs::store::Ino;

/// The owner of a lock.
#[derive(Clone)]
pub(crate) enum Owner {
    /// Traditional POSIX lock: belongs to an fd table.
    Posix(Weak<RefCell<FdTable>>),
    /// OFD lock: belongs to an open file.
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

/// A lock covering `[start, end)`; an `end` of `u64::MAX` means to the end of the file and beyond.
#[derive(Clone)]
pub(crate) struct Lock {
    pub owner: Owner,
    /// The process that took the lock (the `l_pid` F_GETLK reports); -1 for OFD locks.
    pub pid: Tid,
    pub start: u64,
    pub end: u64,
    /// Write lock (exclusive); otherwise a read lock (shared).
    pub write: bool,
}

impl Lock {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    /// Conflicts with a request from a different owner with an incompatible type.
    fn conflicts(&self, req: &Lock) -> bool {
        !self.owner.same(&req.owner)
            && (self.write || req.write)
            && self.overlaps(req.start, req.end)
    }
}

/// What a lock request is waiting for (for deadlock detection).
pub(crate) struct Waiter {
    pub ino: Ino,
    pub req: Lock,
}

/// Record locks on all inodes.
#[derive(Default)]
pub(crate) struct Locks {
    by_ino: std::collections::BTreeMap<Ino, Vec<Lock>>,
    /// POSIX lock requests waiting in F_SETLKW.
    waiters: Vec<Waiter>,
}

impl Locks {
    /// The first lock (in order of start) that conflicts with the request.
    pub fn conflict(&mut self, ino: Ino, req: &Lock) -> Option<Lock> {
        let locks = self.by_ino.get_mut(&ino)?;
        locks.retain(|l| l.owner.alive());
        locks.iter().find(|l| l.conflicts(req)).cloned()
    }

    /// Sets (or, with `unlock`, removes) the locks in `req`'s range. The caller has already made
    /// sure there is no conflict.
    pub fn set(&mut self, ino: Ino, req: Lock, unlock: bool) {
        let locks = self.by_ino.entry(ino).or_default();
        let (mut start, mut end) = (req.start, req.end);
        let mut kept = Vec::with_capacity(locks.len() + 2);
        for l in locks.drain(..).filter(|l| l.owner.alive()) {
            if !l.owner.same(&req.owner) {
                kept.push(l);
                continue;
            }
            // Locks of the same type that overlap or are adjacent merge into the new lock
            if !unlock && l.write == req.write && l.start <= end && start <= l.end {
                start = start.min(l.start);
                end = end.max(l.end);
                continue;
            }
            if !l.overlaps(req.start, req.end) {
                kept.push(l);
                continue;
            }
            // Different type, or unlocking: keep only the parts outside the requested range
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
        request_wake_locks();
    }

    /// Whether waiting for `req` would deadlock: follow "whose lock blocks the waiting request"
    /// along the chain; coming back to ourselves is a deadlock (`posix_locks_deadlock`). Checks
    /// POSIX locks only.
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

    /// Records (`Some`) or clears an owner's wait in F_SETLKW.
    pub fn set_waiting(&mut self, owner: &Owner, wait: Option<Waiter>) {
        self.waiters
            .retain(|w| !w.req.owner.same(owner) && w.req.owner.alive());
        if let Some(w) = wait {
            self.waiters.push(w);
        }
    }

    /// These files in fd table `fds` were closed: release this fd table's POSIX locks on them.
    pub fn closed(&mut self, fds: &Rc<RefCell<FdTable>>, files: &[FileRef]) {
        let owner = Owner::Posix(Rc::downgrade(fds));
        for file in files {
            let Some(ino) = file.borrow().lock_ino() else {
                continue;
            };
            if let Some(locks) = self.by_ino.get_mut(&ino) {
                let before = locks.len();
                locks.retain(|l| !l.owner.same(&owner) && l.owner.alive());
                if locks.len() != before {
                    request_wake_locks();
                }
                if locks.is_empty() {
                    self.by_ino.remove(&ino);
                }
            }
        }
    }
}

/// Called after fds are closed: releases the current process's POSIX locks on these files.
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
        // Turn the middle into a read lock: split into three pieces
        l.set(1, lock(&oa, 40, 60, false), false);
        assert_eq!(
            ranges(&l, 1),
            vec![(0, 40, true), (40, 60, false), (60, 100, true)]
        );
        // Another owner: a read lock does not conflict with read locks but does with write locks
        assert!(l.conflict(1, &lock(&ob, 45, 50, false)).is_none());
        assert_eq!(l.conflict(1, &lock(&ob, 30, 50, false)).unwrap().start, 0);
        // Adjacent locks of the same type merge
        l.set(1, lock(&oa, 40, 60, true), false);
        assert_eq!(ranges(&l, 1), vec![(0, 100, true)]);
        // Unlock a piece in the middle
        l.set(1, lock(&oa, 10, 20, false), true);
        assert_eq!(ranges(&l, 1), vec![(0, 10, true), (20, 100, true)]);
        // The owner goes away, and its locks lapse with it
        drop(a);
        assert!(l.conflict(1, &lock(&ob, 0, u64::MAX, true)).is_none());
    }

    #[test]
    fn closing_any_fd_releases_the_posix_locks() {
        let a = Rc::new(RefCell::new(FdTable::new()));
        let mut l = Locks::default();
        let file = OpenFile::new(
            FileKind::Path(crate::vfs::Node::Store(crate::vfs::ROOT_MOUNT, 7)),
            0,
        );
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
        // a is waiting for b's lock; b then waiting for a's lock is a deadlock
        let wait = lock(&oa, 10, 20, true);
        assert!(!l.deadlock(1, &wait));
        l.set_waiting(&oa, Some(Waiter { ino: 1, req: wait }));
        assert!(l.deadlock(1, &lock(&ob, 0, 10, true)));
    }
}
