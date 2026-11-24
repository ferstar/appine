//! flock(2) locks and fcntl(2) record locks (the kernel/locks map).
//!
//! Each lock has one owner: a POSIX lock belongs to an fd table
//! (shared by all threads of one process), and closing any non-O_PATH fd in that table referring to the
//! file releases all of the table's locks on the file. OFD and flock locks belong to an open
//! file and lapse when its last reference is closed. Record locks of one owner never overlap,
//! and adjacent ranges of the same type are merged.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::{Rc, Weak};

use crate::cell::RefCell;
use crate::fd::{FdTable, FileKind, FileRef, OpenFile};
use crate::kernel::{Kernel, Tid, request_wake_locks};
use crate::pidns::PidNs;
use crate::pseudo::{self, PseudoNode};
use crate::vfs::store::Ino;
use crate::vfs::{Node, Vfs};

/// Filesystem, pipefs, sockfs and devpts inodes have independent number spaces.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum LockKey {
    Node(Ino),
    Pipe(Ino),
    Socket(Ino),
    Devpts(u64, Ino),
}

impl LockKey {
    pub fn of(file: &OpenFile) -> Option<Self> {
        // Linux skips POSIX lock removal on close of an O_PATH descriptor.
        if file.flags & crate::fd::oflags::O_PATH != 0 {
            return None;
        }
        if let Some(ino) = file.lock_ino() {
            return Some(Self::Node(ino));
        }
        match &file.kind {
            FileKind::Pipe(end) => Some(Self::Pipe(end.pipe.borrow().ino)),
            FileKind::Socket(s) => Some(Self::Socket(pseudo::socket_ino(s))),
            FileKind::Pty(end) => {
                if let Some(ino) = end.lock_ino {
                    return Some(Self::Node(ino));
                }
                let tty = end.tty.borrow();
                let node = if end.master {
                    PseudoNode::PtsPtmx(tty.fs)
                } else {
                    PseudoNode::Pts(tty.fs, tty.index)
                };
                Some(Self::Devpts(tty.fs, pseudo::ino_of(&node)))
            }
            _ => None,
        }
    }
}

struct FlockHolder {
    file: Weak<RefCell<OpenFile>>,
    exclusive: bool,
    /// The process that took the lock, as reported in /proc/locks.
    pid: Tid,
}

#[derive(Default)]
pub(crate) struct Flocks {
    by_key: BTreeMap<LockKey, Vec<FlockHolder>>,
    sweep_at: usize,
}

/// Last close can drop a weak owner without revisiting its key. Sweep as the table grows.
fn sweep_dead<T>(
    by_key: &mut BTreeMap<LockKey, Vec<T>>,
    sweep_at: &mut usize,
    alive: impl Fn(&T) -> bool,
) {
    if by_key.len() >= (*sweep_at).max(256) {
        by_key.retain(|_, locks| {
            locks.retain(&alive);
            !locks.is_empty()
        });
        *sweep_at = 2 * by_key.len();
    }
}

impl Flocks {
    /// Returns false if another open file holds a conflicting lock.
    pub fn try_lock(&mut self, key: LockKey, file: &FileRef, exclusive: bool, pid: Tid) -> bool {
        sweep_dead(&mut self.by_key, &mut self.sweep_at, |holder| {
            holder.file.strong_count() > 0
        });
        let holders = self.by_key.entry(key).or_default();
        holders.retain(|holder| holder.file.strong_count() > 0);
        let owner = Rc::downgrade(file);
        if let Some(i) = holders.iter().position(|holder| holder.file.ptr_eq(&owner)) {
            if holders[i].exclusive == exclusive {
                return true;
            }
            // A conversion releases the old lock even when the new request would block.
            holders.remove(i);
            request_wake_locks();
        }
        if holders.iter().any(|holder| exclusive || holder.exclusive) {
            return false;
        }
        holders.push(FlockHolder {
            file: owner,
            exclusive,
            pid,
        });
        file.borrow_mut().holds_locks = true;
        true
    }

    pub fn unlock(&mut self, key: LockKey, file: &FileRef) {
        let Some(holders) = self.by_key.get_mut(&key) else {
            return;
        };
        let owner = Rc::downgrade(file);
        if let Some(i) = holders.iter().position(|holder| holder.file.ptr_eq(&owner)) {
            holders.remove(i);
            request_wake_locks();
        }
        if holders.is_empty() {
            self.by_key.remove(&key);
        }
    }
}

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
    pub key: LockKey,
    pub req: Lock,
}

/// Record locks on all inodes.
#[derive(Default)]
pub(crate) struct Locks {
    by_key: BTreeMap<LockKey, Vec<Lock>>,
    sweep_at: usize,
    /// POSIX lock requests waiting in F_SETLKW.
    waiters: Vec<Waiter>,
}

impl Locks {
    /// The first lock (in order of start) that conflicts with the request.
    pub fn conflict(&mut self, key: LockKey, req: &Lock) -> Option<Lock> {
        let locks = self.by_key.get_mut(&key)?;
        locks.retain(|l| l.owner.alive());
        locks.iter().find(|l| l.conflicts(req)).cloned()
    }

    /// Sets (or, with `unlock`, removes) the locks in `req`'s range. The caller has already made
    /// sure there is no conflict.
    pub fn set(&mut self, key: LockKey, req: Lock, unlock: bool) {
        sweep_dead(&mut self.by_key, &mut self.sweep_at, |lock| {
            lock.owner.alive()
        });
        let locks = self.by_key.entry(key).or_default();
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
            self.by_key.remove(&key);
        } else {
            *locks = kept;
        }
        request_wake_locks();
    }

    /// Whether waiting for `req` would deadlock: follow "whose lock blocks the waiting request"
    /// along the chain; coming back to ourselves is a deadlock (`posix_locks_deadlock`). Checks
    /// POSIX locks only.
    pub fn deadlock(&mut self, key: LockKey, req: &Lock) -> bool {
        let Owner::Posix(_) = req.owner else {
            return false;
        };
        let mut blocker = match self.conflict(key, req) {
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
            let (key, request) = (w.key, w.req.clone());
            match self.conflict(key, &request) {
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
            let Some(key) = LockKey::of(&file.borrow()) else {
                continue;
            };
            if let Some(locks) = self.by_key.get_mut(&key) {
                let before = locks.len();
                locks.retain(|l| !l.owner.same(&owner) && l.owner.alive());
                if locks.len() != before {
                    request_wake_locks();
                }
                if locks.is_empty() {
                    self.by_key.remove(&key);
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

// ---- /proc/locks ----

/// `/proc/locks` (Linux 6.1's `locks_show` and `lock_get_status`): a line for every flock lock,
/// POSIX and OFD lock and lease, numbered by position, giving the process that took it as the
/// reader's pid namespace `ns` sees it (-1 for OFD locks; the locks of processes `ns` cannot see
/// are left out, their numbers skipped), the device and inode numbers stat shows for the file and,
/// for record locks, the byte range. Linux lists them newest first and the requests waiting for
/// each below it (`->` lines); here they come by kind and inode, and waiting requests are not
/// listed.
pub(crate) fn proc_locks(k: &Kernel, vfs: &Vfs, ns: &PidNs) -> String {
    let mut out = String::new();
    for (i, s) in shown(k, None).iter().enumerate() {
        let pid = match s.pid {
            -1 => Some(-1),
            // As Linux's locks_translate_pid: the initial namespace shows the number as recorded
            p if ns.level == 0 => Some(p),
            p => ns.vnr(p).filter(|&p| p != 0),
        };
        if let Some(pid) = pid
            && let Some(line) = s.status(vfs, pid)
        {
            writeln!(out, "{}: {line}", i + 1).unwrap();
        }
    }
    out
}

/// The `lock:` lines of `/proc/<pid>/fdinfo/<n>` (Linux's `show_fd_locks`), which lslocks reads to
/// tell who holds an OFD lock: the flock lock, OFD locks and lease held through open file `file`
/// and the POSIX locks of fd table `fds` held through it, numbered from 1, with the pid as `ns`
/// sees it (0 when it cannot). A POSIX lock records its table, not the open file that took it, so
/// it is shown with the table's first open file on the inode.
pub(crate) fn fd_locks(
    k: &Kernel,
    vfs: &Vfs,
    ns: &PidNs,
    fds: &Rc<RefCell<FdTable>>,
    file: &FileRef,
) -> String {
    let Some(key) = LockKey::of(&file.borrow()) else {
        return String::new();
    };
    let mut out = String::new();
    let mut n = 0;
    for s in shown(k, Some(key)).iter().filter(|s| {
        Rc::ptr_eq(&s.file, file)
            && s.table
                .as_ref()
                .is_none_or(|t| std::ptr::eq(t.as_ptr(), Rc::as_ptr(fds)))
    }) {
        let pid = match s.pid {
            -1 => -1,
            p if ns.level == 0 => p,
            p => ns.vnr(p).unwrap_or(0),
        };
        if let Some(line) = s.status(vfs, pid) {
            n += 1;
            writeln!(out, "lock:\t{n}: {line}").unwrap();
        }
    }
    out
}

/// One lock or lease as `/proc/locks` and fdinfo show it.
struct Shown {
    /// The kind and mode columns, padded as `lock_get_status` pads them.
    kind: &'static str,
    /// A write lock or lease, a read one, or None for `F_UNLCK` (a lease being broken to none).
    write: Option<bool>,
    /// The process that took it (-1 for OFD locks).
    pid: Tid,
    /// The open file it is held through (Linux's `fl_file`).
    file: FileRef,
    /// The last column: the byte range of a record lock, `0 EOF` otherwise.
    range: String,
    /// A POSIX lock's owner (`fl_owner`); the others are owned by `file`.
    table: Option<Weak<RefCell<FdTable>>>,
}

impl Shown {
    /// The line after the number, with `pid` as the reader sees it; None when the file has no
    /// inode to show.
    fn status(&self, vfs: &Vfs, pid: Tid) -> Option<String> {
        let (dev, ino) = shown_id(vfs, &self.file.borrow())?;
        let ty = match self.write {
            Some(true) => "WRITE",
            Some(false) => "READ",
            None => "UNLCK",
        };
        // glibc's major() and minor() of the dev_t stat reports
        let major = (dev >> 8 & 0xfff) | (dev >> 32 & !0xfff);
        let minor = (dev & 0xff) | (dev >> 12 & !0xff);
        let (kind, range) = (self.kind, &self.range);
        Some(format!(
            "{kind}{ty} {pid} {major:02x}:{minor:02x}:{ino} {range}"
        ))
    }
}

/// Every lock and lease (only those on `only` when given), in the order Linux keeps an inode's:
/// flock locks, then POSIX and OFD locks, then leases.
fn shown(k: &Kernel, only: Option<LockKey>) -> Vec<Shown> {
    let wanted = |ino: &Ino| only.is_none_or(|o| o == LockKey::Node(*ino));
    let mut out = Vec::new();
    for (key, holders) in &k.flocks.by_key {
        if only.is_some_and(|o| o != *key) {
            continue;
        }
        for holder in holders {
            if let Some(file) = holder.file.upgrade() {
                out.push(Shown {
                    kind: "FLOCK  ADVISORY  ",
                    write: Some(holder.exclusive),
                    pid: holder.pid,
                    file,
                    range: "0 EOF".into(),
                    table: None,
                });
            }
        }
    }
    for (&key, locks) in &k.locks.by_key {
        if only.is_some_and(|o| o != key) {
            continue;
        }
        for l in locks.iter().filter(|l| l.owner.alive()) {
            let Some(file) = l.owner.file(key) else {
                continue;
            };
            // Linux's inclusive end, OFFSET_MAX meaning to the end
            let last = l.end.wrapping_sub(1);
            let range = if last >= i64::MAX as u64 {
                format!("{} EOF", l.start)
            } else {
                format!("{} {last}", l.start)
            };
            let (kind, table) = match &l.owner {
                Owner::Posix(t) => ("POSIX  ADVISORY  ", Some(t.clone())),
                Owner::Ofd(_) => ("OFDLCK ADVISORY  ", None),
            };
            out.push(Shown {
                kind,
                write: Some(l.write),
                pid: l.pid,
                file,
                range,
                table,
            });
        }
    }
    // Leases are keyed by the store inode, or by an overlay node's key (same as lock_ino)
    for (_, leases) in k.leases.iter().filter(|(i, _)| wanted(i)) {
        for lease in leases {
            let Some((file, write, breaking, pid)) = lease.shown() else {
                continue;
            };
            out.push(Shown {
                kind: if breaking {
                    "LEASE  BREAKING  "
                } else {
                    "LEASE  ACTIVE    "
                },
                write,
                pid,
                file,
                range: "0 EOF".into(),
                table: None,
            });
        }
    }
    out
}

impl Owner {
    /// An open file the lock on `key` is held through: an OFD lock's own, or for a POSIX lock one
    /// in its fd table that refers to the file (any close of which would release the lock).
    fn file(&self, key: LockKey) -> Option<FileRef> {
        match self {
            Owner::Ofd(f) => f.upgrade(),
            Owner::Posix(t) => {
                let t = t.upgrade()?;
                let t = t.borrow();
                t.open_fds()
                    .into_iter()
                    .filter_map(|fd| t.get(fd).ok())
                    .find(|f| LockKey::of(&f.borrow()) == Some(key))
            }
        }
    }
}

/// The device and inode numbers stat shows for `file` (`/proc/locks` prints the inode's): those of
/// its node, with an overlay node's shown inode number.
fn shown_id(vfs: &Vfs, file: &OpenFile) -> Option<(u64, u64)> {
    if let FileKind::Pipe(end) = &file.kind
        && end.pipe.borrow().fifo.is_none()
    {
        return Some((crate::pipe::PIPE_DEV, end.pipe.borrow().ino));
    }
    if matches!(file.kind, FileKind::Socket(_) | FileKind::Pty(_)) {
        return Some(crate::syscall::dev_ino(file));
    }
    let node = file.node()?;
    let ino = match &node {
        Node::Overlay(_, o) => vfs.overlay().shown_ino(*o),
        n => n.ino()?,
    };
    Some((vfs.dev_of(&node), ino))
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
        l.by_key.get(&LockKey::Node(ino)).map_or(Vec::new(), |v| {
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
        let key = LockKey::Node(1);
        l.set(key, lock(&oa, 0, 100, true), false);
        // Turn the middle into a read lock: split into three pieces
        l.set(key, lock(&oa, 40, 60, false), false);
        assert_eq!(
            ranges(&l, 1),
            vec![(0, 40, true), (40, 60, false), (60, 100, true)]
        );
        // Another owner: a read lock does not conflict with read locks but does with write locks
        assert!(l.conflict(key, &lock(&ob, 45, 50, false)).is_none());
        assert_eq!(l.conflict(key, &lock(&ob, 30, 50, false)).unwrap().start, 0);
        // Adjacent locks of the same type merge
        l.set(key, lock(&oa, 40, 60, true), false);
        assert_eq!(ranges(&l, 1), vec![(0, 100, true)]);
        // Unlock a piece in the middle
        l.set(key, lock(&oa, 10, 20, false), true);
        assert_eq!(ranges(&l, 1), vec![(0, 10, true), (20, 100, true)]);
        // The owner goes away, and its locks lapse with it
        drop(a);
        assert!(l.conflict(key, &lock(&ob, 0, u64::MAX, true)).is_none());
    }

    #[test]
    fn closing_any_fd_releases_the_posix_locks() {
        let a = Rc::new(RefCell::new(FdTable::new()));
        let mut l = Locks::default();
        let file = OpenFile::new(
            FileKind::Path(crate::vfs::Node::Store(crate::vfs::ROOT_MOUNT, 7)),
            0,
        );
        l.set(LockKey::Node(7), lock(&owner(&a), 0, 10, true), false);
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
        let key = LockKey::Node(1);
        l.set(key, lock(&oa, 0, 10, true), false);
        l.set(key, lock(&ob, 10, 20, true), false);
        // a is waiting for b's lock; b then waiting for a's lock is a deadlock
        let wait = lock(&oa, 10, 20, true);
        assert!(!l.deadlock(key, &wait));
        l.set_waiting(&oa, Some(Waiter { key, req: wait }));
        assert!(l.deadlock(key, &lock(&ob, 0, 10, true)));
    }
}
