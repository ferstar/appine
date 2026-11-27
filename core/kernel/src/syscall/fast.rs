//! Syscalls without the BKL (the kernel/process map).
//!
//! Parallel lstat calls (`git status` preloading the index, build tools, language servers' thread
//! pools) used to queue for the BKL, getting slower with more threads. Only the common cases that
//! change no state and whose inputs can all be had without locks are handled here: credentials
//! need no search-permission checks, the path starts at the root or the current directory, and the
//! whole path is in the store's metadata cache ([`Cached`]). When that is not possible nothing is
//! changed and the call goes to [`dispatch`](super::dispatch) with the BKL held, which also fills
//! the cache with what is needed.
//!
//! Clock reads (`clock_gettime`, `gettimeofday`) are here too. The host's vDSO reads most clocks
//! without entering the kernel layer (the kernel/vdso map), but they come here from its svc
//! stubs (the built vDSO, a time namespace with offsets), from what the vDSO leaves to the kernel
//! (CPU-time clocks), and from programs making the syscall themselves; with stubs only, one go
//! build made 2.18 million of them, and the few threads queuing for the BKL waited 2.5 s in total.
//! The host clocks and the thread's own CPU clock need no kernel state; process CPU clocks and
//! other threads' go to the BKL path.
//!
//! Futex wait and wake (the kernel/futex map) and nanosleep are here too: the futex wait queue has
//! its own lock ([`Shared::futexes`]), and a waiting thread sleeps on its own event, woken by futex
//! wakes, signals and thread-group exit just as when waiting with the BKL held. Operations with
//! `FUTEX_PRIVATE_FLAG` are always keyed by address space and address (even in shared mappings) and
//! skip checking [`Mm::shared_maps`](crate::mm::Mm::shared_maps); non-private operations handle
//! private memory only (writable in the thread's page table and not in a writable shared mapping,
//! [`Mm::futex_key`](crate::mm::Mm::futex_key)). When the thread wakes neither by a futex wake nor
//! by expiry (most likely a signal arrived), the call goes to the BKL path to be redone, which
//! handles the signal and the restart.
//!
//! `sched_yield` is here too: it only yields the host thread, which needs no kernel state.
//!
//! Read-only thread and credential ID queries (`getpid`, `gettid`, `getuid`, `geteuid`, `getgid`,
//! `getegid`) are also served from the namespace-translated values cached in [`Fast`] while
//! [`Shared::fast_epoch`] matches; `getppid` stays on the BKL path because reparenting on parent
//! exit does not bump the epoch.
//!
//! For a thread with seccomp only two kinds of syscalls come here: numbers for which the filter
//! always returns ALLOW (cached by number, [`crate::seccomp::always_allowed`]), and the vDSO stubs'
//! reads of clocks Linux's vDSO reads in user space (which are not checked, as they do not enter
//! the kernel on Linux). Others go to the BKL path to be checked.
//!
//! While holding the BKL a thread records its root directory, current directory and credentials
//! ([`Fast`]). Other threads may change them (a chdir shared through `CLONE_FS`, credentials shared
//! by the thread group); the changing side increments the count of [`Shared::fast_changed`], which
//! invalidates what was recorded.
//!
//! While accessing guest memory the thread is still marked as running guest code
//! (`TaskSync::in_guest`): a thread that unmaps memory waits for it to leave before releasing the
//! host memory ([`Mm::sync`](crate::mm::Mm::sync)), just as for guest code's own memory accesses.
//! It is not marked while sleeping, and after waking it no longer accesses guest memory.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use appine_engine::{Cpu, PAGE_SIZE, prot};

use super::fs::{
    AT_FDCWD, AT_SYMLINK_NOFOLLOW, ROOT_DEV, encode_stat, encode_statx, newfstatat_flags,
    statx_flags,
};
use super::{PATH_MAX, SysResult};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::itimer::Expiry;
use crate::kernel::{Shared, Tid};
use crate::task::{Task, TaskState};
use crate::vfs::store::{Ino, Stat};
use crate::vfs::{Cached, Node, ROOT_MOUNT};

/// The state a thread records for syscalls without the BKL.
pub(crate) struct Fast {
    /// [`Shared::fast_epoch`] when recorded.
    epoch: u64,
    /// Syscalls are being traced (`APPINE_STRACE`): all go to the BKL path so they can be printed.
    traced: bool,
    /// None when paths cannot be resolved without the BKL (the credentials need search-permission
    /// checks, or the root is not in the store).
    paths: Option<Paths>,
    /// The thread's own tid (execve may take over the main thread's; [`Shared::fast_epoch`] changes
    /// then too).
    tid: Tid,
    /// The thread group's pid as the thread's own pid namespace numbers it (`getpid`).
    ns_tgid: Tid,
    /// The same tid as the thread's own pid namespace numbers it (`gettid`, and what
    /// `pthread_getcpuclockid` puts into its clock id).
    ns_tid: Tid,
    /// The thread's real and effective user and group IDs translated through its user namespace
    /// (`getuid`, `geteuid`, `getgid`, `getegid`); invalidated by credential, user-namespace and
    /// `uid_map`/`gid_map` changes via [`Shared::fast_epoch`].
    uid: u32,
    euid: u32,
    gid: u32,
    egid: u32,
    /// The host thread's CPU clock and its reading when the thread started ([`Task::cpu_ns`]).
    cpu_clock: Option<(i32, u64)>,
    /// The address space's [`crate::mm::Mm::id`], for futex keys.
    mm: u64,
    /// The ranges of the writable shared mappings in the address space
    /// ([`crate::mm::Mm::shared_maps`]): non-private futex keys there are not the address space's.
    shared_maps: Arc<RwLock<Vec<(u64, u64)>>>,
    /// The address of the vDSO mapping and whether its clock functions are svc stubs
    /// ([`crate::mm::Mm::vdso_svc`]; an execve that replaces the address space changes
    /// [`Shared::fast_epoch`], and so does entering a time namespace).
    vdso: (u64, bool),
    /// Offsets (nanoseconds) of the time namespace the thread is in: the monotonic clock's and
    /// boottime's. setns switching namespaces calls [`Shared::fast_changed`], and the offsets no
    /// longer change once a process has entered.
    time: [i64; 2],
}

struct Paths {
    vfs: Cached,
    root: Ino,
    /// None when the current directory is not in the boot root filesystem (it is in `/proc` or in a
    /// bind mount).
    cwd: Option<Ino>,
}

impl Fast {
    /// Records the thread's current state. Called with the BKL held.
    pub(crate) fn new(shared: &Shared, task: &Task, t: &TaskState) -> Fast {
        let epoch = shared.fast_epoch();
        let cred = task.cred.borrow();
        let fs = t.fs.borrow();
        // Resolve only in the boot root filesystem: in other mounts `..` must go through the
        // mount point
        let paths = match (&t.vfs, &fs.root) {
            (Some(vfs), &Node::Store(ROOT_MOUNT, root)) if cred.searches_all() => {
                let vfs = vfs.cached();
                vfs.usable.then(|| Paths {
                    vfs,
                    root,
                    cwd: match fs.cwd {
                        Node::Store(ROOT_MOUNT, ino) => Some(ino),
                        _ => None,
                    },
                })
            }
            _ => None,
        };
        let (ns_tgid, ns_tid) = {
            let group = t.group.borrow();
            (
                group.pid_ns.vnr(task.tgid).unwrap_or(0),
                group.pid_ns.vnr(task.tid.get()).unwrap_or(0),
            )
        };
        Fast {
            epoch,
            traced: crate::debug::tracing(),
            paths,
            tid: task.tid.get(),
            ns_tgid,
            ns_tid,
            uid: cred.uid_out(cred.ruid),
            euid: cred.uid_out(cred.euid),
            gid: cred.gid_out(cred.rgid),
            egid: cred.gid_out(cred.egid),
            cpu_clock: task.clock.get().map(|c| (c, task.cpu_base.get().0)),
            mm: t.mm.borrow().id,
            shared_maps: t.mm.borrow().shared_maps(),
            vdso: (t.mm.borrow().vdso, t.mm.borrow().vdso_svc),
            time: task.ns.borrow().time.offsets_ns(),
        }
    }

    /// Records the state again if what was recorded is stale. Called with the BKL held.
    pub(crate) fn refresh(&mut self, shared: &Shared, task: &Task, t: &TaskState) {
        if shared.fast_epoch() != self.epoch {
            *self = Fast::new(shared, task, t);
        }
    }

    /// Runs the syscall `cpu` stopped at without the BKL. Returns true when done (the result
    /// written to x0); on false the caller handles it normally with the BKL held, and nothing but
    /// [`TaskState::fast_sleep`] has been changed here.
    pub(crate) fn syscall(&self, shared: &Shared, task: &Task, cpu: &mut Cpu) -> bool {
        // With an ic ivau not yet invalidated, the thread must first enter the kernel to invalidate
        // it (task::handle); under PTRACE_SYSCALL it must stop at entry and exit, and when
        // single-stepping the finished syscall must be reported (crate::ptrace); with seccomp it
        // must be checked first (crate::seccomp)
        if self.traced
            || cpu.state().ivau_page != 0
            || task.syscall_traced.load(Ordering::Relaxed)
            || task.stepping.load(Ordering::Relaxed)
            || !self.seccomp_allows(task, cpu)
        {
            return false;
        }
        crate::bug::trap(crate::bug::Trap::Fast, self.tid);
        let Some(r) = self.dispatch(shared, task, cpu) else {
            return false;
        };
        cpu.state_mut().x[0] = match r {
            Ok(v) => v,
            Err(e) => e.to_syscall_return(),
        };
        true
    }

    /// What may run without checking under seccomp: the vDSO stubs' clock reads
    /// ([`crate::vdso::clock_stub_at`]; on Linux they do not enter the kernel), and numbers for
    /// which the filter always returns ALLOW ([`crate::seccomp::always_allowed`]).
    fn seccomp_allows(&self, task: &Task, cpu: &Cpu) -> bool {
        if !task.seccomp_on.load(Ordering::Relaxed) {
            return true;
        }
        let s = cpu.state();
        crate::vdso::clock_stub_at(self.vdso, s.pc, s.x[8], s.x[0])
            || crate::seccomp::always_allowed(task, s.x[8])
    }

    fn dispatch(&self, shared: &Shared, task: &Task, cpu: &Cpu) -> Option<SysResult> {
        let s = cpu.state();
        let a = [s.x[0], s.x[1], s.x[2], s.x[3], s.x[4], s.x[5]];
        match s.x[8] {
            98 if a[1] as u32 & FUTEX_PRIVATE_FLAG != 0 || !self.in_shared_map(a[0]) => {
                futex(shared, (task, cpu, self.mm), &a, self.time[0])
            }
            101 => nanosleep(task, cpu, a[0]),
            115 => {
                let clock = a[0] as i32;
                clock_nanosleep(task, cpu, (clock, self.offset(clock)), a[1] as u32, a[2])
            }
            124 => Some(sched_yield(task)),
            113 => self.clock_gettime(cpu, a[0] as i32, a[1]),
            114 => clock_getres(cpu, a[0] as i32, a[1]),
            169 => gettimeofday(cpu, a[0], a[1]),
            // getpid, getuid, geteuid, getgid, getegid, gettid (getppid = 173 stays on the BKL
            // path because reparenting on parent exit does not bump fast_epoch)
            172 | 174..=178 => {
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                Some(Ok(match s.x[8] {
                    172 => self.ns_tgid as u64,
                    174 => u64::from(self.uid),
                    175 => u64::from(self.euid),
                    176 => u64::from(self.gid),
                    177 => u64::from(self.egid),
                    _ => self.ns_tid as u64,
                }))
            }
            79 | 291 => {
                let p = self.paths.as_ref()?;
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                match s.x[8] {
                    79 => p.newfstatat(cpu, a[0] as i32, a[1], a[2], a[3] as u32),
                    _ => p.statx(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
                }
            }
            _ => None,
        }
    }

    /// The host clocks and the thread's own CPU clock of `misc::clock_gettime`.
    fn clock_gettime(&self, cpu: &Cpu, clock: i32, tp: u64) -> Option<SysResult> {
        let (sec, nsec) = if self.own_cpu_clock(clock) {
            let (host, base) = self.cpu_clock?;
            let (sec, nsec) = appine_hal::clock_gettime(host).ok()?;
            let ns = (sec as u64 * 1_000_000_000 + nsec as u64).saturating_sub(base);
            ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
        } else if host_clock(clock) {
            super::misc::add_offset(appine_hal::clock_gettime(clock).ok()?, self.offset(clock))
        } else {
            return None;
        };
        write(cpu, tp, &pair(sec, nsec))?;
        Some(Ok(0))
    }

    /// The offset of clock `clock` in the time namespace ([`crate::nsproxy::TimeNs::offset`]).
    fn offset(&self, clock: i32) -> i64 {
        match clock {
            1 | 4 | 6 => self.time[0],
            7 | 9 => self.time[1],
            _ => 0,
        }
    }

    /// `addr` is in a writable shared file mapping.
    fn in_shared_map(&self, addr: u64) -> bool {
        let ranges = self
            .shared_maps
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        ranges.iter().any(|&(s, e)| (s..e).contains(&addr))
    }

    /// `clock` is the thread's own CPU clock: `CLOCK_THREAD_CPUTIME_ID`, or its own as encoded by
    /// `pthread_getcpuclockid` (`misc::cpu_clock`).
    fn own_cpu_clock(&self, clock: i32) -> bool {
        clock == CLOCK_THREAD_CPUTIME_ID
            || clock < 0 && clock & 3 != 3 && clock & 4 != 0 && {
                let tid = !(clock >> 3);
                tid == 0 || tid == self.ns_tid
            }
    }
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// Use the host's clock directly (the clocks of `misc::clock_gettime` that are not CPU clocks).
/// Clocks the host does not know go to the BKL path, which reports the error.
fn host_clock(clock: i32) -> bool {
    clock >= 0 && clock != CLOCK_PROCESS_CPUTIME_ID && clock != CLOCK_THREAD_CPUTIME_ID
}

fn clock_getres(cpu: &Cpu, clock: i32, res: u64) -> Option<SysResult> {
    if !host_clock(clock) {
        return None;
    }
    let (sec, nsec) = appine_hal::clock_getres(clock).ok()?;
    if res != 0 {
        write(cpu, res, &pair(sec, nsec))?;
    }
    Some(Ok(0))
}

fn gettimeofday(cpu: &Cpu, tv: u64, tz: u64) -> Option<SysResult> {
    if tv != 0 {
        let (sec, nsec) = appine_hal::clock_gettime(CLOCK_REALTIME).ok()?;
        write(cpu, tv, &pair(sec, nsec / 1000))?;
    }
    if tz != 0 {
        write(cpu, tz, &super::misc::timezone())?;
    }
    Some(Ok(0))
}

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const TIMER_ABSTIME: u32 = 1;

/// Futex wait and wake of `misc::futex` for private keys (`FUTEX_PRIVATE_FLAG` on any writable
/// page, or non-private operations on private memory), with `a` the syscall arguments and `mm`
/// the address space's [`crate::mm::Mm::id`]. The timeout is relative in FUTEX_WAIT and absolute
/// in WAIT_BITSET.
/// `monotonic` is the monotonic clock's offset in the time namespace (WAIT_BITSET's absolute
/// timeout is converted with it).
fn futex(
    shared: &Shared,
    (task, cpu, mm): (&Task, &Cpu, u64),
    a: &[u64; 6],
    monotonic: i64,
) -> Option<SysResult> {
    let (uaddr, op, val, timeout, val3) = (a[0], a[1] as u32, a[2] as u32, a[3], a[5] as u32);
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    // FUTEX_CLOCK_REALTIME with another command is ENOSYS, which the BKL path answers
    if op & FUTEX_CLOCK_REALTIME != 0 && cmd != FUTEX_WAIT_BITSET {
        return None;
    }
    let bitset = match cmd {
        FUTEX_WAIT | FUTEX_WAKE => u32::MAX,
        FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET if val3 != 0 => val3,
        _ => return None,
    };
    // The key of a `FUTEX_PRIVATE_FLAG` operation or private memory is the address space and the
    // address (Mm::futex_key; the caller left non-private operations on shared mappings to the BKL
    // path). Borrowed copy-on-write pages and read-only mappings are rare places for a futex: they
    // go to the BKL path too, which reads them
    if !uaddr.is_multiple_of(4) {
        return None;
    }
    let host = cpu.translate(uaddr, prot::WRITE)?.as_ptr().cast::<u32>();
    let key = Key::Private {
        mm,
        addr: uaddr,
        shared: op & FUTEX_PRIVATE_FLAG == 0,
    };
    if cmd == FUTEX_WAKE || cmd == FUTEX_WAKE_BITSET {
        let woken = shared
            .futexes()
            .wake(key, super::misc::wake_count(val), bitset);
        crate::kernel::flush_signals();
        return Some(Ok(woken as u64));
    }
    let deadline = match timeout {
        0 => None,
        addr => {
            let t = read_timespec(cpu, addr)?;
            Some(if cmd == FUTEX_WAIT {
                Expiry::after(t)
            } else {
                let (clock, offset) = if op & FUTEX_CLOCK_REALTIME != 0 {
                    (CLOCK_REALTIME, 0)
                } else {
                    (CLOCK_MONOTONIC, monotonic)
                };
                let t = super::misc::to_host_clock(offset, t);
                Expiry::new(clock, t, true).ok()?
            })
        }
    };
    let id = {
        let mut futexes = shared.futexes();
        // SAFETY: host points at 4-byte-aligned guest memory in this page; the thread is marked
        // as running guest code, so the page is not released meanwhile
        let now = unsafe { AtomicU32::from_ptr(host) }.load(Ordering::SeqCst);
        if now != val {
            return Some(Err(Errno::EAGAIN));
        }
        futexes.enqueue(key, bitset, task.event.clone())
    };
    let waiting = Enqueued::new(shared, [id]);
    crate::bug::trap(crate::bug::Trap::FutexWait, task.tgid);
    let woke = sleep(task, deadline);
    match waiting.leave() {
        Some(_) => Some(Ok(0)),
        // Most likely a signal arrived: the BKL path re-checks the value, enqueues again and
        // handles the signal, waiting until the deadline computed here without reading the
        // timeout again ([`TaskState::fast_sleep`])
        None if woke => {
            if deadline.is_some() {
                task.state.borrow_mut().fast_sleep = deadline;
            }
            None
        }
        None => Some(Err(Errno::ETIMEDOUT)),
    }
}

fn nanosleep(task: &Task, cpu: &Cpu, req: u64) -> Option<SysResult> {
    let deadline = Expiry::after(read_timespec(cpu, req)?);
    sleep_until(task, deadline)
}

/// The wall clock, the monotonic clock and the since-boot clock of `misc::clock_nanosleep`.
/// `offset` is this clock's offset in the time namespace (absolute times are converted with it).
fn clock_nanosleep(
    task: &Task,
    cpu: &Cpu,
    (clock, offset): (i32, i64),
    flags: u32,
    req: u64,
) -> Option<SysResult> {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME)
        || flags & !TIMER_ABSTIME != 0
    {
        return None;
    }
    let t = read_timespec(cpu, req)?;
    if flags & TIMER_ABSTIME == 0 {
        return sleep_until(task, Expiry::new(clock, t, false).ok()?);
    }
    let t = super::misc::to_host_clock(offset, t);
    let deadline = Expiry::new(clock, t, true).ok()?;
    sleep_until(task, deadline)
}

/// Sleeps until `deadline`. When woken (most likely by a signal) the BKL path goes on sleeping,
/// with the deadline recorded in [`TaskState::fast_sleep`]: it takes that instead of reading the
/// time again, so a relative sleep does not start over from now, the time remaining when a
/// signal interrupts comes from it, and memory unmapped meanwhile is never read (Linux's restart
/// keeps the deadline too).
fn sleep_until(task: &Task, deadline: Expiry) -> Option<SysResult> {
    if !sleep(task, Some(deadline)) {
        return Some(Ok(0));
    }
    task.state.borrow_mut().fast_sleep = Some(deadline);
    None
}

/// Sleeps without the BKL until `deadline` (None sleeps forever), or until the thread's event is
/// signalled (futex wake, signal, being ended). While sleeping it does not count as running guest
/// code: a thread that unmaps memory need not wait for it (Mm::sync), and it is marked again on
/// returning to the run loop. Returns true when the event was signalled (or a host signal
/// interrupted the wait); a jump of the deadline's clock signals it too ([`crate::itimer::clock_jumped`]),
/// and the locked path then looks at the deadline again.
fn sleep(task: &Task, deadline: Option<Expiry>) -> bool {
    const POLLIN: i16 = 1;
    task.sync.in_guest.store(false, Ordering::SeqCst);
    task.sleeping.store(true, Ordering::SeqCst);
    let woke = loop {
        let timeout = match deadline {
            Some(d) if d.passed() => break false,
            Some(d) => Some(d.remaining()),
            None => None,
        };
        let mut fds = [(task.event.fd(), POLLIN, 0)];
        match appine_hal::poll(&mut fds, timeout) {
            Ok(0) => {}
            _ => break true,
        }
    };
    task.sleeping.store(false, Ordering::SeqCst);
    if woke {
        task.event.clear();
    }
    woke
}

/// sched_yield: the host thread yields its CPU, as Linux's `do_sched_yield` yields the guest
/// thread's. Spinning threads wait with it (parking_lot's and crossbeam's backoff, Go's osyield,
/// HotSpot's GC termination); when it took the BKL and only returned, eight threads passing a
/// turn around with it went 230 times slower than natively, and spinners slowed every other
/// thread's syscalls. It touches no guest memory, so it does not count as running guest code.
fn sched_yield(task: &Task) -> SysResult {
    task.sync.in_guest.store(false, Ordering::SeqCst);
    std::thread::yield_now();
    Ok(0)
}

/// Reads a `struct timespec` from guest memory (`super::read_timespec`). Returns None when it
/// cannot be read or is invalid, so the BKL path reports the error.
fn read_timespec(cpu: &Cpu, addr: u64) -> Option<Duration> {
    let mut b = [0u8; 16];
    read(cpu, addr, &mut b)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    super::timespec(sec, nsec)
}

/// `struct timespec`, `struct timeval`.
fn pair(a: i64, b: i64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    buf
}

impl Paths {
    fn newfstatat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        buf: u64,
        flags: u32,
    ) -> Option<SysResult> {
        newfstatat_flags(flags).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_stat(&st, ROOT_DEV))?;
        Some(Ok(0))
    }

    fn statx(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        flags: u32,
        mask: u32,
        buf: u64,
    ) -> Option<SysResult> {
        let flags = statx_flags(flags, mask).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(
            cpu,
            buf,
            &encode_statx(&encode_stat(&st, ROOT_DEV), ROOT_MOUNT),
        )?;
        Some(Ok(0))
    }

    /// The case of `fs::stat_at` where the path is not empty and starts at the root or the current
    /// directory.
    fn stat(&self, cpu: &Cpu, dirfd: i32, path: u64, flags: u32) -> Option<Result<Stat, Errno>> {
        let mut buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut buf)?;
        let cwd = match path.first()? {
            b'/' => self.root,
            _ if dirfd == AT_FDCWD => self.cwd?,
            _ => return None,
        };
        self.vfs
            .stat(self.root, cwd, path, flags & AT_SYMLINK_NOFOLLOW == 0)
    }
}

/// Reads a NUL-terminated path from guest memory into `buf` (`syscall::read_path`). Returns None
/// when it cannot be read or is too long.
fn read_path<'b>(cpu: &Cpu, addr: u64, buf: &'b mut [u8; PATH_MAX]) -> Option<&'b [u8]> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: the thread is marked as running guest code, so this page's host memory is not
        // released meanwhile
        let bytes = unsafe { std::slice::from_raw_parts(host.as_ptr(), chunk) };
        if let Some(n) = bytes.iter().position(|&b| b == 0) {
            buf[done..done + n].copy_from_slice(&bytes[..n]);
            return Some(&buf[..done + n]);
        }
        buf[done..done + chunk].copy_from_slice(bytes);
        done += chunk;
    }
    None
}

/// Reads guest memory, requiring every page to be readable; returns None when it cannot be read.
fn read(cpu: &Cpu, addr: u64, buf: &mut [u8]) -> Option<()> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: as in read_path
        unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), buf[done..].as_mut_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}

/// Writes guest memory, requiring every page to be writable; returns None when it cannot be
/// written (part may already be written; the normal path writes it again).
fn write(cpu: &Cpu, addr: u64, data: &[u8]) -> Option<()> {
    let mut done = 0;
    while done < data.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(data.len() - done);
        let host = cpu.translate(cur, prot::WRITE)?;
        // SAFETY: as in read_path
        unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host.as_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use appine_engine::{Cpu, PAGE_SIZE, prot};

    use super::*;
    use crate::cell::RefCell;
    use crate::cred::OVERFLOW_ID;
    use crate::kernel::{Cx, Kernel, Shared};
    use crate::mm::flags::{MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED};
    use crate::pidns::PidNs;
    use crate::process::Tier;
    use crate::pseudo::{Entry, PseudoNode};
    use crate::task::{FsContext, Task, TaskState};
    use crate::vfs::{INIT_MNT_NS, MntNs, Node};

    fn run_fast(
        fast: &Fast,
        shared: &Shared,
        task: &Task,
        cpu: &mut Cpu,
        nr: u64,
        args: [u64; 6],
    ) -> Option<u64> {
        let s = cpu.state_mut();
        s.x[..6].copy_from_slice(&args);
        s.x[8] = nr;
        fast.syscall(shared, task, cpu).then(|| cpu.state().x[0])
    }

    fn run_fast_noargs(
        fast: &Fast,
        shared: &Shared,
        task: &Task,
        cpu: &mut Cpu,
        nr: u64,
    ) -> Option<u64> {
        run_fast(fast, shared, task, cpu, nr, [0; 6])
    }

    fn run_slow(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64, args: [u64; 6]) -> u64 {
        let s = cpu.state_mut();
        s.x[..6].copy_from_slice(&args);
        s.x[8] = nr;
        let _ = super::super::dispatch(cx, cpu);
        cpu.state().x[0]
    }

    #[test]
    fn futex_private_flag_handles_shared_mapping_without_shared_maps_lock() {
        let mut kernel = Kernel::new(Tier::Interpreter);
        let (proc, _, mut cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
        let shared = Arc::new(Shared::new(kernel));
        {
            let k = shared.lock();
            let task = k.tasks[&proc.pid].clone();
            let mut state = task.state.borrow_mut();
            let cx = Cx::new(&shared, k, &task, &mut state);

            let shared_addr = cx
                .mm_mut()
                .mmap(
                    0,
                    PAGE_SIZE,
                    prot::READ | prot::WRITE,
                    MAP_SHARED | MAP_ANONYMOUS,
                    None,
                )
                .unwrap();
            let private_addr = cx
                .mm_mut()
                .mmap(
                    0,
                    PAGE_SIZE,
                    prot::READ | prot::WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    None,
                )
                .unwrap();
            // Write value 7 at the shared futex word and verify shared_maps contains it.
            cx.mm()
                .write(
                    crate::uaccess::UserSlice::new(shared_addr, 4),
                    &7u32.to_le_bytes(),
                )
                .unwrap();
            let mm_id = cx.mm().id;
            let shared_maps = cx.mm().shared_maps();
            assert!(
                shared_maps
                    .read()
                    .unwrap()
                    .iter()
                    .any(|&(s, e)| (s..e).contains(&shared_addr))
            );
            let slow_private_key = cx.mm().futex_key(shared_addr, true).unwrap();
            assert_eq!(
                slow_private_key,
                Key::Private {
                    mm: mm_id,
                    addr: shared_addr,
                    shared: false,
                }
            );

            let fast = Fast::new(&shared, cx.task, cx.t);

            // Hold an exclusive write lock on shared_maps while running FUTEX_PRIVATE_FLAG:
            // succeeding proves the fast path skips in_shared_map() completely.
            {
                let _write_lock = shared_maps.write().unwrap();

                // FUTEX_WAIT | FUTEX_PRIVATE_FLAG with mismatched val (0 != 7) returns -EAGAIN.
                let wait_res = run_fast(
                    &fast,
                    &shared,
                    cx.task,
                    &mut cpu,
                    98,
                    [
                        shared_addr,
                        u64::from(FUTEX_WAIT | FUTEX_PRIVATE_FLAG),
                        0,
                        0,
                        0,
                        0,
                    ],
                );
                assert_eq!(wait_res, Some(Errno::EAGAIN.to_syscall_return()));

                // FUTEX_WAKE | FUTEX_PRIVATE_FLAG wakes a waiter queued with slow-path private key.
                let ev = Arc::new(appine_hal::Event::new().unwrap());
                let id = shared.futexes().enqueue(slow_private_key, u32::MAX, ev);
                let wake_res = run_fast(
                    &fast,
                    &shared,
                    cx.task,
                    &mut cpu,
                    98,
                    [
                        shared_addr,
                        u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG),
                        1,
                        0,
                        0,
                        0,
                    ],
                );
                assert_eq!(wake_res, Some(1));
                assert!(!shared.futexes().is_queued(id));
            }

            // Non-private futex on a MAP_SHARED address must fall back to the BKL slow path.
            let non_private_shared = run_fast(
                &fast,
                &shared,
                cx.task,
                &mut cpu,
                98,
                [shared_addr, u64::from(FUTEX_WAKE), 1, 0, 0, 0],
            );
            assert_eq!(non_private_shared, None);

            // Non-private futex on a MAP_PRIVATE address stays on the fast path with shared: true,
            // matching Mm::futex_key(private_addr, false) and not waking shared: false waiters.
            let priv_shared_key = cx.mm().futex_key(private_addr, false).unwrap();
            let priv_private_key = cx.mm().futex_key(private_addr, true).unwrap();
            let ev1 = Arc::new(appine_hal::Event::new().unwrap());
            let ev2 = Arc::new(appine_hal::Event::new().unwrap());
            let id_shared = shared.futexes().enqueue(priv_shared_key, u32::MAX, ev1);
            let id_private = shared.futexes().enqueue(priv_private_key, u32::MAX, ev2);
            let non_private_priv = run_fast(
                &fast,
                &shared,
                cx.task,
                &mut cpu,
                98,
                [private_addr, u64::from(FUTEX_WAKE), 2, 0, 0, 0],
            );
            assert_eq!(non_private_priv, Some(1));
            assert!(!shared.futexes().is_queued(id_shared));
            assert!(shared.futexes().is_queued(id_private));
            shared.futexes().remove(id_private);
        }
        drop(cpu);
        drop(shared);
    }

    #[test]
    fn id_syscalls_match_slow_path_across_threads_pid_ns_and_cred_changes() {
        const ID_NRS: [u64; 6] = [172, 174, 175, 176, 177, 178];

        let mut kernel = Kernel::new(Tier::Interpreter);
        let (proc, _, mut leader_cpu) =
            crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
        let group = kernel.groups[&proc.pid].clone();
        // Put the process in a child pid namespace where local pid/tid differ from global 20/21.
        let child_pid_ns = PidNs::child(&PidNs::init(), group.borrow().cred.user_ns.clone());
        kernel.register_pid(&child_pid_ns, 20, &[1]).unwrap();
        kernel.register_pid(&child_pid_ns, 21, &[2]).unwrap();
        group.borrow_mut().pid_ns = child_pid_ns;

        // Add a second thread (tid 21) in the same thread group.
        let worker_mm = kernel.tasks[&20].res.borrow().mm.clone();
        // SAFETY: worker_mm is held by worker's TaskState for the lifetime of worker_cpu.
        let mut worker_cpu = unsafe { Cpu::new(worker_mm.borrow().mmu()) };
        let worker_state = TaskState {
            mm: worker_mm,
            fds: Rc::new(RefCell::new(crate::fd::FdTable::new())),
            fs: Rc::new(RefCell::new(FsContext {
                root: Node::ROOT,
                cwd: Node::ROOT,
                umask: 0o022,
                mnt_ns: Rc::new(MntNs {
                    id: INIT_MNT_NS,
                    owner: kernel.init_ns.ipc.owner.clone(),
                    anon: false,
                }),
            })),
            group: group.clone(),
            vfs: None,
            altstack: Default::default(),
            clear_child_tid: 0,
            vfork_done: None,
            restart: None,
            fast_sleep: None,
            sem_undo: Rc::new(crate::ipc::SemUndo::new()),
        };
        let worker = Task::new(
            21,
            20,
            &worker_cpu,
            worker_state,
            kernel.init_ns.clone(),
            0,
            [0; 16],
        )
        .unwrap();
        group.borrow_mut().threads.push(21);
        kernel.tasks.insert(21, worker);

        let shared = Arc::new(Shared::new(kernel));
        {
            let k = shared.lock();
            let leader = k.tasks[&20].clone();
            let worker = k.tasks[&21].clone();
            let mut leader_state = leader.state.borrow_mut();
            let mut worker_fast = Fast::new(&shared, &worker, &worker.state.borrow());
            let mut leader_fast = Fast::new(&shared, &leader, &leader_state);
            let mut cx = Cx::new(&shared, k, &leader, &mut leader_state);

            // Leader in child pid_ns: getpid=1, gettid=1, ids=0.
            for nr in ID_NRS {
                let slow = run_slow(&mut cx, &mut leader_cpu, nr, [0; 6]);
                let fast = run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, nr);
                assert_eq!(fast, Some(slow), "leader syscall {nr}");
            }
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 172),
                Some(1)
            );
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 178),
                Some(1)
            );
            // getppid (173) intentionally stays on the BKL path.
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 173),
                None
            );

            // Worker in child pid_ns: getpid=1, gettid=2.
            assert_eq!(
                run_fast_noargs(&worker_fast, &shared, &worker, &mut worker_cpu, 172),
                Some(1)
            );
            assert_eq!(
                run_fast_noargs(&worker_fast, &shared, &worker, &mut worker_cpu, 178),
                Some(2)
            );

            // Credential mutations invalidate both leader_fast and worker_fast via fast_epoch.
            super::super::cred::setresgid(&mut cx, 100, 200, 300).unwrap();
            super::super::cred::setresuid(&mut cx, 1000, 2000, 3000).unwrap();
            for nr in ID_NRS {
                assert_eq!(
                    run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, nr),
                    None,
                    "stale leader_fast must fall back for syscall {nr}"
                );
                assert_eq!(
                    run_fast_noargs(&worker_fast, &shared, &worker, &mut worker_cpu, nr),
                    None,
                    "stale worker_fast must fall back for syscall {nr}"
                );
            }

            leader_fast.refresh(&shared, cx.task, cx.t);
            for (nr, want) in [(174, 1000), (175, 2000), (176, 100), (177, 200)] {
                let slow = run_slow(&mut cx, &mut leader_cpu, nr, [0; 6]);
                assert_eq!(slow, want);
                assert_eq!(
                    run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, nr),
                    Some(want),
                    "refreshed leader syscall {nr}"
                );
            }

            // Refresh worker_fast: worker keeps its own per-thread creds (still 0).
            worker_fast.refresh(&shared, &worker, &worker.state.borrow());
            assert_eq!(
                run_fast_noargs(&worker_fast, &shared, &worker, &mut worker_cpu, 174),
                Some(0)
            );

            // setgid / setuid also bump fast_epoch.
            super::super::cred::setgid(&mut cx, 100).unwrap();
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 177),
                None
            );
            leader_fast.refresh(&shared, cx.task, cx.t);
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 177),
                Some(100)
            );

            super::super::cred::setuid(&mut cx, 1000).unwrap();
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 175),
                None
            );
            leader_fast.refresh(&shared, cx.task, cx.t);
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 175),
                Some(1000)
            );
        }
        drop(worker_cpu);
        drop(leader_cpu);
        drop(shared);
    }

    #[test]
    fn id_syscalls_invalidate_on_user_ns_and_id_map_writes() {
        const CLONE_NEWUSER: u64 = 0x1000_0000;
        const ID_NRS: [u64; 4] = [174, 175, 176, 177];

        let mut kernel = Kernel::new(Tier::Interpreter);
        let (proc, _, mut cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 1000, true, b"");
        let shared = Arc::new(Shared::new(kernel));
        {
            let k = shared.lock();
            let task = k.tasks[&proc.pid].clone();
            let mut state = task.state.borrow_mut();
            let mut fast = Fast::new(&shared, &task, &state);
            let mut cx = Cx::new(&shared, k, &task, &mut state);

            assert_eq!(
                run_fast_noargs(&fast, &shared, cx.task, &mut cpu, 174),
                Some(1000)
            );

            // unshare(CLONE_NEWUSER) enters a new user namespace with empty uid_map/gid_map.
            super::super::proc::unshare(&mut cx, CLONE_NEWUSER).unwrap();
            assert_eq!(
                run_fast_noargs(&fast, &shared, cx.task, &mut cpu, 174),
                None,
                "unshare(CLONE_NEWUSER) must invalidate Fast"
            );
            fast.refresh(&shared, cx.task, cx.t);
            for nr in ID_NRS {
                let slow = run_slow(&mut cx, &mut cpu, nr, [0; 6]);
                assert_eq!(slow, u64::from(OVERFLOW_ID));
                assert_eq!(
                    run_fast_noargs(&fast, &shared, cx.task, &mut cpu, nr),
                    Some(slow)
                );
            }

            let tid = cx.tid();
            let child_cred = cx.cred();
            let ns = cx.ns();
            let pid_ns = cx.pid_ns();
            let write_proc = |cx: &mut Cx<'_>, entry: Entry, data: &[u8]| {
                let node = PseudoNode::Entry(proc, None, entry);
                crate::pseudo::write(
                    cx.k(),
                    (tid, &child_cred, &child_cred),
                    (&ns, &pid_ns),
                    &node,
                    data,
                    (0, data.len()),
                )
                .unwrap();
            };

            // Write /proc/<pid>/uid_map mapping 42 -> 1000; must bump fast_epoch immediately.
            write_proc(&mut cx, Entry::UidMap, b"42 1000 1\n");
            for nr in [174, 175] {
                assert_eq!(
                    run_fast_noargs(&fast, &shared, cx.task, &mut cpu, nr),
                    None,
                    "uid_map write must invalidate syscall {nr}"
                );
            }
            fast.refresh(&shared, cx.task, cx.t);
            for nr in [174, 175] {
                assert_eq!(
                    run_fast_noargs(&fast, &shared, cx.task, &mut cpu, nr),
                    Some(42)
                );
                assert_eq!(run_slow(&mut cx, &mut cpu, nr, [0; 6]), 42);
            }

            // Deny setgroups, then write /proc/<pid>/gid_map mapping 99 -> 1000.
            write_proc(&mut cx, Entry::Setgroups, b"deny\n");
            write_proc(&mut cx, Entry::GidMap, b"99 1000 1\n");
            for nr in [176, 177] {
                assert_eq!(
                    run_fast_noargs(&fast, &shared, cx.task, &mut cpu, nr),
                    None,
                    "gid_map write must invalidate syscall {nr}"
                );
            }
            fast.refresh(&shared, cx.task, cx.t);
            for nr in [176, 177] {
                assert_eq!(
                    run_fast_noargs(&fast, &shared, cx.task, &mut cpu, nr),
                    Some(99)
                );
                assert_eq!(run_slow(&mut cx, &mut cpu, nr, [0; 6]), 99);
            }
        }
        drop(cpu);
        drop(shared);
    }

    #[test]
    fn id_syscalls_respect_ptrace_stepping_and_seccomp() {
        let mut kernel = Kernel::new(Tier::Interpreter);
        let (proc, _, mut cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
        let shared = Arc::new(Shared::new(kernel));
        {
            let k = shared.lock();
            let task = k.tasks[&proc.pid].clone();
            let state = task.state.borrow();
            let fast = Fast::new(&shared, &task, &state);

            assert_eq!(
                run_fast_noargs(&fast, &shared, &task, &mut cpu, 172),
                Some(20)
            );

            // PTRACE_SYSCALL forces fallback.
            task.syscall_traced.store(true, Ordering::Relaxed);
            assert_eq!(run_fast_noargs(&fast, &shared, &task, &mut cpu, 172), None);
            task.syscall_traced.store(false, Ordering::Relaxed);

            // PTRACE_SINGLESTEP forces fallback.
            task.stepping.store(true, Ordering::Relaxed);
            assert_eq!(run_fast_noargs(&fast, &shared, &task, &mut cpu, 172), None);
            task.stepping.store(false, Ordering::Relaxed);

            // Seccomp active without getpid in always-allow bitmap forces fallback.
            task.seccomp_on.store(true, Ordering::Relaxed);
            assert_eq!(run_fast_noargs(&fast, &shared, &task, &mut cpu, 172), None);

            // When getpid (172) is in the always-allow bitmap, the fast path handles it.
            task.seccomp_allow[172 / 64].store(1u64 << (172 % 64), Ordering::Relaxed);
            assert_eq!(
                run_fast_noargs(&fast, &shared, &task, &mut cpu, 172),
                Some(20)
            );
            // Other ID syscalls not in the bitmap still fall back.
            assert_eq!(run_fast_noargs(&fast, &shared, &task, &mut cpu, 174), None);
        }
        drop(cpu);
        drop(shared);
    }
}
