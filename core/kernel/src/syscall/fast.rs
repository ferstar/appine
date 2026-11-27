//! Syscalls without the BKL (the kernel/process map).
//!
//! Parallel lstat/readlink/access calls (`git status` preloading the index, build tools, language
//! servers' thread pools) used to queue for the BKL, getting slower with more threads. Only the
//! common cases (`newfstatat`, `statx`, `readlinkat`, `faccessat`, `faccessat2`) that change no
//! state and whose inputs can all be had without locks are handled here: credentials need no
//! search-permission checks, the path starts at the root or the current directory, and the whole
//! path is in the store's metadata cache ([`Cached`]). When that is not possible nothing is
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
//! Read-only thread and credential ID queries (`getpid`, `getppid`, `gettid`, `getuid`, `geteuid`,
//! `getgid`, `getegid`) are also served from the namespace-translated values cached in [`Fast`]
//! while [`Shared::fast_epoch`] matches; reparenting live children on parent exit bumps the epoch.
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
    AT_EACCESS, AT_FDCWD, AT_SYMLINK_NOFOLLOW, ROOT_DEV, encode_stat, encode_statx,
    faccessat_flags, newfstatat_flags, statx_flags,
};
use super::{PATH_MAX, SysResult};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::itimer::Expiry;
use crate::kernel::{Shared, Tid};
use crate::task::{Task, TaskState};
use crate::vfs::store::{Ino, Stat};
use crate::vfs::{Cached, CachedCred, Node, ROOT_MOUNT};

/// Outcome of attempting a syscall without the BKL ([`Fast::syscall`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FastOutcome {
    /// Completed without leaving guest mode (`TaskSync::in_guest` remained `true` throughout).
    InGuest,
    /// Completed after leaving guest mode (`sleep` or `sched_yield` cleared `TaskSync::in_guest`).
    LeftGuest,
    /// Not handled; caller falls back to the BKL path.
    Fallback,
}

enum Done {
    InGuest(SysResult),
    LeftGuest(SysResult),
}

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
    /// The parent process's pid as the thread's own pid namespace numbers it (`getppid`);
    /// invalidated via [`Shared::fast_epoch`] whenever live children are reparented.
    ns_ppid: Tid,
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
    /// Effective credentials (`cx.cred()`) used by `faccessat2(..., AT_EACCESS)`.
    eaccess_cred: CachedCred,
    /// Real-ID credentials (`cx.cred().for_access()`) used by `faccessat` / `faccessat2` without
    /// `AT_EACCESS`.
    access_cred: CachedCred,
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
                vfs.usable.then(|| {
                    let ac = cred.for_access();
                    Paths {
                        vfs,
                        root,
                        cwd: match fs.cwd {
                            Node::Store(ROOT_MOUNT, ino) => Some(ino),
                            _ => None,
                        },
                        eaccess_cred: CachedCred {
                            fsuid: cred.fsuid,
                            cap_effective: cred.cap_effective,
                        },
                        access_cred: CachedCred {
                            fsuid: ac.fsuid,
                            cap_effective: ac.cap_effective,
                        },
                    }
                })
            }
            _ => None,
        };
        let (ns_tgid, ns_ppid, ns_tid) = {
            let group = t.group.borrow();
            (
                group.pid_ns.vnr(task.tgid).unwrap_or(0),
                group.pid_ns.vnr(group.ppid).unwrap_or(0),
                group.pid_ns.vnr(task.tid.get()).unwrap_or(0),
            )
        };
        Fast {
            epoch,
            traced: crate::debug::tracing(),
            paths,
            tid: task.tid.get(),
            ns_tgid,
            ns_ppid,
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

    /// Runs the syscall `cpu` stopped at without the BKL. Returns [`FastOutcome::InGuest`] or
    /// [`FastOutcome::LeftGuest`] when done (the result written to x0); on
    /// [`FastOutcome::Fallback`] the caller handles it normally with the BKL held, and nothing but
    /// [`TaskState::fast_sleep`] (and `TaskSync::in_guest`/`entry` if it slept) has been changed.
    pub(crate) fn syscall(&self, shared: &Shared, task: &Task, cpu: &mut Cpu) -> FastOutcome {
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
            return FastOutcome::Fallback;
        }
        crate::bug::trap(crate::bug::Trap::Fast, self.tid);
        let Some(done) = self.dispatch(shared, task, cpu) else {
            return FastOutcome::Fallback;
        };
        let (r, outcome) = match done {
            Done::InGuest(r) => (r, FastOutcome::InGuest),
            Done::LeftGuest(r) => (r, FastOutcome::LeftGuest),
        };
        cpu.state_mut().x[0] = match r {
            Ok(v) => v,
            Err(e) => e.to_syscall_return(),
        };
        outcome
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

    fn dispatch(&self, shared: &Shared, task: &Task, cpu: &Cpu) -> Option<Done> {
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
            124 => Some(Done::LeftGuest(sched_yield(task))),
            113 => self
                .clock_gettime(cpu, a[0] as i32, a[1])
                .map(Done::InGuest),
            114 => clock_getres(cpu, a[0] as i32, a[1]).map(Done::InGuest),
            169 => gettimeofday(cpu, a[0], a[1]).map(Done::InGuest),
            // getpid, getppid, getuid, geteuid, getgid, getegid, gettid
            172..=178 => {
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                Some(Done::InGuest(Ok(match s.x[8] {
                    172 => self.ns_tgid as u64,
                    173 => self.ns_ppid as u64,
                    174 => u64::from(self.uid),
                    175 => u64::from(self.euid),
                    176 => u64::from(self.gid),
                    177 => u64::from(self.egid),
                    _ => self.ns_tid as u64,
                })))
            }
            // faccessat, readlinkat, newfstatat, statx, faccessat2
            48 | 78 | 79 | 291 | 439 => {
                let p = self.paths.as_ref()?;
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                match s.x[8] {
                    48 => p.faccessat(cpu, a[0] as i32, a[1], a[2] as u32, 0),
                    78 => p.readlinkat(cpu, a[0] as i32, a[1], a[2], a[3] as usize),
                    79 => p.newfstatat(cpu, a[0] as i32, a[1], a[2], a[3] as u32),
                    291 => p.statx(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
                    _ => p.faccessat(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
                }
                .map(Done::InGuest)
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
) -> Option<Done> {
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
        return Some(Done::InGuest(Ok(woken as u64)));
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
    let Some(waiter) = shared
        .futexes()
        .enqueue_if::<()>(key, bitset, task.event.clone(), || {
            // SAFETY: host points at 4-byte-aligned guest memory in this page; the thread is marked
            // as running guest code, so the page is not released meanwhile
            Ok(unsafe { AtomicU32::from_ptr(host) }.load(Ordering::SeqCst) == val)
        })
        .unwrap()
    else {
        return Some(Done::InGuest(Err(Errno::EAGAIN)));
    };
    let waiting = Enqueued::new(shared, [waiter]);
    crate::bug::trap(crate::bug::Trap::FutexWait, task.tgid);
    let woke = sleep(task, cpu, deadline);
    match waiting.leave() {
        Some(_) => Some(Done::LeftGuest(Ok(0))),
        // Most likely a signal arrived: the BKL path re-checks the value, enqueues again and
        // handles the signal, waiting until the deadline computed here without reading the
        // timeout again ([`TaskState::fast_sleep`])
        None if woke => {
            if deadline.is_some() {
                task.state.borrow_mut().fast_sleep = deadline;
            }
            None
        }
        None => Some(Done::LeftGuest(Err(Errno::ETIMEDOUT))),
    }
}

fn nanosleep(task: &Task, cpu: &Cpu, req: u64) -> Option<Done> {
    let deadline = Expiry::after(read_timespec(cpu, req)?);
    sleep_until(task, cpu, deadline)
}

/// The wall clock, the monotonic clock and the since-boot clock of `misc::clock_nanosleep`.
/// `offset` is this clock's offset in the time namespace (absolute times are converted with it).
fn clock_nanosleep(
    task: &Task,
    cpu: &Cpu,
    (clock, offset): (i32, i64),
    flags: u32,
    req: u64,
) -> Option<Done> {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME)
        || flags & !TIMER_ABSTIME != 0
    {
        return None;
    }
    let t = read_timespec(cpu, req)?;
    if flags & TIMER_ABSTIME == 0 {
        return sleep_until(task, cpu, Expiry::new(clock, t, false).ok()?);
    }
    let t = super::misc::to_host_clock(offset, t);
    let deadline = Expiry::new(clock, t, true).ok()?;
    sleep_until(task, cpu, deadline)
}

/// Sleeps until `deadline`. When woken (most likely by a signal) the BKL path goes on sleeping,
/// with the deadline recorded in [`TaskState::fast_sleep`]: it takes that instead of reading the
/// time again, so a relative sleep does not start over from now, the time remaining when a
/// signal interrupts comes from it, and memory unmapped meanwhile is never read (Linux's restart
/// keeps the deadline too).
fn sleep_until(task: &Task, cpu: &Cpu, deadline: Expiry) -> Option<Done> {
    if !sleep(task, cpu, Some(deadline)) {
        return Some(Done::LeftGuest(Ok(0)));
    }
    task.state.borrow_mut().fast_sleep = Some(deadline);
    None
}

/// Sleeps without the BKL until `deadline` (None sleeps forever), or until the thread's event is
/// signalled (futex wake, signal, being ended). While sleeping it does not count as running guest
/// code: a thread that unmaps memory need not wait for it (Mm::sync), and it is marked again on
/// returning to the run loop. Records [`crate::task::KernelEntry`] before marking `sleeping` so
/// `/proc/<pid>/syscall` always sees the syscall while the thread is in state `S`. Returns true
/// when the event was signalled (or a host signal interrupted the wait); a jump of the deadline's
/// clock signals it too ([`crate::itimer::clock_jumped`]), and the locked path then looks at the
/// deadline again.
fn sleep(task: &Task, cpu: &Cpu, deadline: Option<Expiry>) -> bool {
    task.sync.entry.enter(cpu.state(), true);
    task.sync.in_guest.store(false, Ordering::Release);
    task.sleeping.store(true, Ordering::SeqCst);
    let woke = loop {
        let timeout = match deadline {
            Some(d) if d.passed() => break false,
            Some(d) => Some(d.remaining()),
            None => None,
        };
        match task.event.wait(timeout) {
            Ok(false) => {}
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
    task.sync.in_guest.store(false, Ordering::Release);
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
    fn readlinkat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        buf: u64,
        size: usize,
    ) -> Option<SysResult> {
        if size as isize <= 0 {
            return None;
        }
        let mut path_buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut path_buf)?;
        let cwd = self.resolve_cwd(dirfd, path)?;
        let mut out = [0u8; PATH_MAX];
        let cap = size.min(out.len());
        let n = match self.vfs.readlink(self.root, cwd, path, &mut out[..cap])? {
            Ok(n) => n,
            Err(e) => return Some(Err(e)),
        };
        // If the target filled the stack buffer and the caller asked for more, fall back so a
        // target longer than PATH_MAX is never truncated.
        if n == out.len() && size > out.len() {
            return None;
        }
        write(cpu, buf, &out[..n])?;
        Some(Ok(n as u64))
    }

    fn faccessat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        amode: u32,
        flags: u32,
    ) -> Option<SysResult> {
        faccessat_flags(amode, flags).ok()?;
        let mut path_buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut path_buf)?;
        let cwd = self.resolve_cwd(dirfd, path)?;
        let cred = if flags & AT_EACCESS != 0 {
            self.eaccess_cred
        } else {
            self.access_cred
        };
        self.vfs
            .access(
                self.root,
                cwd,
                path,
                amode,
                flags & AT_SYMLINK_NOFOLLOW == 0,
                cred,
            )
            .map(|r| r.map(|()| 0))
    }

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
        let cwd = self.resolve_cwd(dirfd, path)?;
        self.vfs
            .stat(self.root, cwd, path, flags & AT_SYMLINK_NOFOLLOW == 0)
    }

    fn resolve_cwd(&self, dirfd: i32, path: &[u8]) -> Option<Ino> {
        match path.first()? {
            b'/' => Some(self.root),
            _ if dirfd == AT_FDCWD => self.cwd,
            _ => None,
        }
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

    fn run_fast_outcome(
        fast: &Fast,
        shared: &Shared,
        task: &Task,
        cpu: &mut Cpu,
        nr: u64,
        args: [u64; 6],
    ) -> FastOutcome {
        let s = cpu.state_mut();
        s.x[..6].copy_from_slice(&args);
        s.x[8] = nr;
        fast.syscall(shared, task, cpu)
    }

    fn run_fast(
        fast: &Fast,
        shared: &Shared,
        task: &Task,
        cpu: &mut Cpu,
        nr: u64,
        args: [u64; 6],
    ) -> Option<u64> {
        (run_fast_outcome(fast, shared, task, cpu, nr, args) != FastOutcome::Fallback)
            .then(|| cpu.state().x[0])
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
                let w = shared.futexes().enqueue(slow_private_key, u32::MAX, ev);
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
                assert!(w.is_woken());
                assert!(!shared.futexes().is_queued(&w));
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
            let w_shared = shared.futexes().enqueue(priv_shared_key, u32::MAX, ev1);
            let w_private = shared.futexes().enqueue(priv_private_key, u32::MAX, ev2);
            let non_private_priv = run_fast(
                &fast,
                &shared,
                cx.task,
                &mut cpu,
                98,
                [private_addr, u64::from(FUTEX_WAKE), 2, 0, 0, 0],
            );
            assert_eq!(non_private_priv, Some(1));
            assert!(w_shared.is_woken());
            assert!(!shared.futexes().is_queued(&w_shared));
            assert!(!w_private.is_woken());
            assert!(shared.futexes().is_queued(&w_private));
            assert!(!shared.futexes().remove(&w_private));
        }
        drop(cpu);
        drop(shared);
    }

    #[test]
    fn id_syscalls_match_slow_path_across_threads_pid_ns_and_cred_changes() {
        const ID_NRS: [u64; 7] = [172, 173, 174, 175, 176, 177, 178];

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
                mnt_ns: MntNs::standalone(INIT_MNT_NS, kernel.init_ns.ipc.owner.clone()),
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
            // Parent (INIT = 1 in root pid_ns) is outside child_pid_ns, so getppid (173) reports 0.
            assert_eq!(
                run_fast_noargs(&leader_fast, &shared, cx.task, &mut leader_cpu, 173),
                Some(0)
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

    fn with_rw_fast_task(f: impl FnOnce(&Arc<Shared>, &Cx<'_>, &Fast, &mut Cpu, u64)) {
        let mut kernel = Kernel::new(Tier::Interpreter);
        let (proc, _, mut cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
        let shared = Arc::new(Shared::new(kernel));
        {
            let k = shared.lock();
            let task = k.tasks[&proc.pid].clone();
            let mut state = task.state.borrow_mut();
            let cx = Cx::new(&shared, k, &task, &mut state);
            let addr = cx
                .mm_mut()
                .mmap(
                    0,
                    PAGE_SIZE,
                    prot::READ | prot::WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    None,
                )
                .unwrap();
            let fast = Fast::new(&shared, cx.task, cx.t);
            f(&shared, &cx, &fast, &mut cpu, addr);
        }
        drop(cpu);
        drop(shared);
    }

    #[test]
    fn getppid_fast_path_invalidates_and_updates_on_reparent() {
        let mut kernel = Kernel::new(Tier::Interpreter);
        // Set up INIT (1) -> subreaper (10) -> parent (20) -> child (30).
        let init_group = Rc::new(RefCell::new(crate::process::ThreadGroup::new(
            crate::kernel::INIT,
            0,
            Rc::new(RefCell::new(crate::signal::SigHandlers::default())),
        )));
        kernel.groups.insert(crate::kernel::INIT, init_group);
        let (_, _, sub_cpu) = crate::pseudo::tests::add_task(&mut kernel, 10, 0, true, b"");
        let (_, _, par_cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
        let (_, _, mut child_cpu) = crate::pseudo::tests::add_task(&mut kernel, 30, 0, true, b"");
        kernel.groups[&10].borrow_mut().ppid = crate::kernel::INIT;
        kernel.groups[&10].borrow_mut().child_subreaper = true;
        kernel.groups[&10].borrow_mut().children.push(20);
        kernel.groups[&20].borrow_mut().ppid = 10;
        kernel.groups[&20].borrow_mut().children.push(30);
        kernel.groups[&30].borrow_mut().ppid = 20;

        let shared = Arc::new(Shared::new(kernel));
        {
            let mut k = shared.lock();
            let child = k.tasks[&30].clone();
            let mut child_fast = Fast::new(&shared, &child, &child.state.borrow());

            // Initially getppid returns 20 in guest mode.
            child.sync.in_guest.store(true, Ordering::SeqCst);
            assert_eq!(
                run_fast_outcome(&child_fast, &shared, &child, &mut child_cpu, 173, [0; 6]),
                FastOutcome::InGuest
            );
            assert_eq!(child_cpu.state().x[0], 20);

            // Parent (20) exits: child (30) is reparented to subreaper (10), bumping fast_epoch.
            let parent_group = k.groups[&20].clone();
            k.tasks.remove(&20);
            parent_group.borrow_mut().threads.clear();
            crate::process::finish_group(&mut k, &parent_group, None);

            // Stale child_fast must fall back rather than returning old ppid 20.
            assert_eq!(
                run_fast_noargs(&child_fast, &shared, &child, &mut child_cpu, 173),
                None
            );

            // After refresh, child_fast serves the new ppid 10 without the BKL.
            child_fast.refresh(&shared, &child, &child.state.borrow());
            assert_eq!(
                run_fast_noargs(&child_fast, &shared, &child, &mut child_cpu, 173),
                Some(10)
            );

            // Subreaper (10) exits: child (30) is reparented to INIT (1).
            let sub_group = k.groups[&10].clone();
            k.tasks.remove(&10);
            sub_group.borrow_mut().threads.clear();
            crate::process::finish_group(&mut k, &sub_group, None);
            assert_eq!(
                run_fast_noargs(&child_fast, &shared, &child, &mut child_cpu, 173),
                None
            );
            child_fast.refresh(&shared, &child, &child.state.borrow());
            assert_eq!(
                run_fast_noargs(&child_fast, &shared, &child, &mut child_cpu, 173),
                Some(crate::kernel::INIT as u64)
            );
        }
        drop((child_cpu, par_cpu, sub_cpu));
        drop(shared);
    }

    #[test]
    fn non_sleeping_fast_syscalls_stay_in_guest_without_kernel_entry() {
        with_rw_fast_task(|shared, cx, fast, cpu, addr| {
            cx.mm()
                .write(crate::uaccess::UserSlice::new(addr, 4), &1u32.to_le_bytes())
                .unwrap();

            for (nr, args) in [
                (172u64, [0; 6]),                  // getpid
                (178, [0; 6]),                     // gettid
                (113, [1, addr + 16, 0, 0, 0, 0]), // clock_gettime(CLOCK_MONOTONIC)
                (169, [addr + 32, 0, 0, 0, 0, 0]), // gettimeofday
                (
                    98,
                    [addr, u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG), 1, 0, 0, 0],
                ),
                (
                    98,
                    [addr, u64::from(FUTEX_WAIT | FUTEX_PRIVATE_FLAG), 0, 0, 0, 0],
                ), // EAGAIN
            ] {
                cx.task.sync.in_guest.store(true, Ordering::SeqCst);
                let outcome = run_fast_outcome(fast, shared, cx.task, cpu, nr, args);
                assert_eq!(
                    outcome,
                    FastOutcome::InGuest,
                    "syscall {nr} must stay in guest"
                );
                assert!(
                    cx.task.sync.in_guest.load(Ordering::Relaxed),
                    "syscall {nr} must keep in_guest true"
                );
                assert_eq!(
                    cx.task.sync.entry.get(),
                    None,
                    "non-sleeping fast syscall {nr} must skip KernelEntry"
                );
            }
        });
    }

    #[test]
    fn sleeping_and_yielding_fast_syscalls_leave_guest_and_record_entry() {
        with_rw_fast_task(|shared, cx, fast, cpu, addr| {
            // 1 ns timespec for nanosleep, plus futex word = 0 at addr + 32.
            let mut ts = [0u8; 16];
            ts[8..16].copy_from_slice(&1i64.to_le_bytes());
            cx.mm()
                .write(crate::uaccess::UserSlice::new(addr, 16), &ts)
                .unwrap();

            // 1. sched_yield leaves guest mode without populating KernelEntry.
            cx.task.sync.in_guest.store(true, Ordering::SeqCst);
            assert_eq!(
                run_fast_outcome(fast, shared, cx.task, cpu, 124, [0; 6]),
                FastOutcome::LeftGuest
            );
            assert!(!cx.task.sync.in_guest.load(Ordering::Relaxed));
            assert_eq!(cx.task.sync.entry.get(), None);

            // 2. nanosleep leaves guest mode and records KernelEntry for /proc/<pid>/syscall.
            cx.task.sync.in_guest.store(true, Ordering::SeqCst);
            cpu.state_mut().sp = 0x7000;
            cpu.state_mut().pc = 0x4004;
            assert_eq!(
                run_fast_outcome(fast, shared, cx.task, cpu, 101, [addr, 0, 0, 0, 0, 0]),
                FastOutcome::LeftGuest
            );
            assert!(!cx.task.sync.in_guest.load(Ordering::Relaxed));
            assert_eq!(
                cx.task.sync.entry.get(),
                Some([101, addr, 0, 0, 0, 0, 0, 0x7000, 0x4004])
            );

            // 3. Blocking futex(WAIT) records KernelEntry before entering state S and wakes via
            // FUTEX_WAKE returning LeftGuest with x0 == 0.
            let futex_addr = addr + 32;
            let key = Key::Private {
                mm: cx.mm().id,
                addr: futex_addr,
                shared: false,
            };
            let shared_waker = shared.clone();
            let sync_waker = cx.task.sync.clone();
            let waker = std::thread::spawn(move || {
                // Wait until waiter has enqueued and cleared in_guest.
                while sync_waker.in_guest.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                let entry = sync_waker.entry.get();
                let woken = loop {
                    let n = shared_waker.futexes().wake(key, 1, u32::MAX);
                    crate::kernel::flush_signals();
                    if n > 0 {
                        break n;
                    }
                    std::thread::yield_now();
                };
                (entry, woken)
            });

            cx.task.sync.in_guest.store(true, Ordering::SeqCst);
            cpu.state_mut().sp = 0x8000;
            cpu.state_mut().pc = 0x5004;
            let futex_args = [
                futex_addr,
                u64::from(FUTEX_WAIT | FUTEX_PRIVATE_FLAG),
                0,
                0,
                0,
                0,
            ];
            let outcome = run_fast_outcome(fast, shared, cx.task, cpu, 98, futex_args);
            let (seen_entry, woken) = waker.join().unwrap();
            assert_eq!(outcome, FastOutcome::LeftGuest);
            assert_eq!(cpu.state().x[0], 0);
            assert_eq!(woken, 1);
            assert!(!cx.task.sync.in_guest.load(Ordering::Relaxed));
            assert_eq!(
                seen_entry,
                Some([98, futex_args[0], futex_args[1], 0, 0, 0, 0, 0x8000, 0x5004])
            );
        });
    }

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct VfsFastFixture {
        // Drop order: `cpu` before `shared` (Mmu lifetime), then `_dir` removes the store dir.
        cpu: Cpu,
        shared: Arc<Shared>,
        _dir: TempDir,
        pid: Tid,
        page: u64,
        ro_page: u64,
    }

    impl VfsFastFixture {
        fn new(name: &str) -> Self {
            use crate::vfs::acl;
            use crate::vfs::store::{ROOT, Store, inode_flags, mode};
            use crate::vfs::{Vfs, dev};

            let dir = std::env::temp_dir()
                .join(format!("appine-fast-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            {
                let store = Store::create(&dir).unwrap();
                dev::ensure_dev_nodes(&store).unwrap();
                let bin = store.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
                let usr = store.mkdir(ROOT, b"usr", 0o755, 0, 0).unwrap();
                let usr_bin = store.mkdir(usr, b"bin", 0o755, 0, 0).unwrap();
                store.create_file(bin, b"busybox", 0o755, 0, 0).unwrap();
                store.create_file(bin, b"plain", 0o644, 0, 0).unwrap();
                store.create_file(bin, b"secret", 0o600, 0, 0).unwrap();
                let (imm_ino, _) = store.create_file(bin, b"imm", 0o644, 0, 0).unwrap();
                store.set_flags(imm_ino, inode_flags::IMMUTABLE).unwrap();
                store
                    .create_file(bin, b"owned_ro", 0o400, 1000, 1000)
                    .unwrap();
                store
                    .mknod(bin, b"pipe", mode::S_IFIFO | 0o644, 0, 0, 0)
                    .unwrap();
                let (acl_ino, _) = store.create_file(bin, b"acl", 0o640, 0, 0).unwrap();
                let acl_entries: [acl::AclEntry; 5] = [
                    (0x01, 6, u32::MAX),
                    (acl::ACL_USER, 4, 1000),
                    (0x04, 4, u32::MAX),
                    (0x10, 4, u32::MAX),
                    (0x20, 0, u32::MAX),
                ];
                store
                    .set_xattr(
                        acl_ino,
                        acl::ACL_ACCESS,
                        &acl::acl_encode(&acl_entries),
                        false,
                        false,
                    )
                    .unwrap();
                store
                    .symlink(usr_bin, b"env", b"/bin/busybox", 0, 0)
                    .unwrap();
                store.symlink(ROOT, b"bindir", b"/bin", 0, 0).unwrap();
                store
                    .symlink(ROOT, b"dangling", b"/no/such/file", 0, 0)
                    .unwrap();
            }

            let mut kernel = Kernel::new(Tier::Interpreter);
            kernel.vfs = Some(Rc::new(Vfs::open(&dir).unwrap()));
            let (proc, _, cpu) = crate::pseudo::tests::add_task(&mut kernel, 20, 0, true, b"");
            let shared = Arc::new(Shared::new(kernel));
            let (page, ro_page) = {
                let k = shared.lock();
                let task = k.tasks[&proc.pid].clone();
                let mut state = task.state.borrow_mut();
                let cx = Cx::new(&shared, k, &task, &mut state);
                let page = cx
                    .mm_mut()
                    .mmap(
                        0,
                        PAGE_SIZE,
                        prot::READ | prot::WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS,
                        None,
                    )
                    .unwrap();
                let ro_page = cx
                    .mm_mut()
                    .mmap(0, PAGE_SIZE, prot::READ, MAP_PRIVATE | MAP_ANONYMOUS, None)
                    .unwrap();
                // Warm store metadata + negative caches through the normal VFS path.
                let vfs = cx.vfs().unwrap();
                let store = vfs.store();
                let view = cx.view(&vfs);
                let cred = cx.cred();
                for (p, follow) in [
                    (&b"/bin/busybox"[..], true),
                    (b"/bin/plain", true),
                    (b"/bin/secret", true),
                    (b"/bin/imm", true),
                    (b"/bin/owned_ro", true),
                    (b"/bin/pipe", true),
                    (b"/bin/acl", true),
                    (b"/usr/bin/env", false),
                    (b"/bindir", false),
                    (b"/bindir/", false),
                    (b"/dangling", false),
                    (b"/dangling", true),
                    (b"/missing", true),
                ] {
                    if let Ok(Node::Store(_, ino)) =
                        vfs.resolve(&view, &cred, &Node::ROOT, &Node::ROOT, p, follow)
                    {
                        let _ = store.attrs(ino);
                        let _ = store.readlink(ino);
                    }
                }
                let _ = store.lookup(ROOT, b"no");
                (page, ro_page)
            };

            Self {
                cpu,
                shared,
                _dir: TempDir(dir),
                pid: proc.pid,
                page,
                ro_page,
            }
        }
    }

    fn write_cstr(cx: &Cx<'_>, addr: u64, s: &[u8]) {
        let mut bytes = Vec::with_capacity(s.len() + 1);
        bytes.extend_from_slice(s);
        bytes.push(0);
        cx.mm()
            .write(crate::uaccess::UserSlice::new(addr, bytes.len()), &bytes)
            .unwrap();
    }

    #[test]
    fn readlinkat_fast_path_matches_slow_path_and_fallbacks() {
        let mut fx = VfsFastFixture::new("readlinkat");
        let shared = fx.shared.clone();
        let path_addr = fx.page;
        let out_addr = fx.page + 256;
        let ro_addr = fx.ro_page;
        {
            let k = shared.lock();
            let task = k.tasks[&fx.pid].clone();
            let mut state = task.state.borrow_mut();
            let mut cx = Cx::new(&shared, k, &task, &mut state);
            let mut fast = Fast::new(&shared, cx.task, cx.t);

            // Absolute path (even with invalid dirfd), relative AT_FDCWD path, dangling symlink,
            // non-symlinks (EINVAL), trailing slash on symlink-to-dir (EINVAL), missing (ENOENT).
            for (dirfd, path, size) in [
                (AT_FDCWD, &b"/usr/bin/env"[..], 64usize),
                (999, b"/usr/bin/env", 64),
                (AT_FDCWD, b"usr/bin/env", 64),
                (AT_FDCWD, b"/usr/bin/env", 4),    // truncated buffer
                (AT_FDCWD, b"/usr/bin/env", 8192), // size > PATH_MAX
                (AT_FDCWD, b"/dangling", 64),
                (AT_FDCWD, b"/bin/busybox", 64),
                (AT_FDCWD, b"/bin", 64),
                (AT_FDCWD, b"/bindir/", 64),
                (AT_FDCWD, b"/missing", 64),
            ] {
                write_cstr(&cx, path_addr, path);
                let args = [dirfd as i64 as u64, path_addr, out_addr, size as u64, 0, 0];

                // Fill output with canary before fast call.
                let canary = [0xAAu8; 64];
                cx.mm()
                    .write(
                        crate::uaccess::UserSlice::new(out_addr, canary.len()),
                        &canary,
                    )
                    .unwrap();
                let entry_before = cx.task.sync.entry.get();
                cx.task.sync.in_guest.store(true, Ordering::SeqCst);
                let outcome = run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 78, args);
                assert_eq!(
                    outcome,
                    FastOutcome::InGuest,
                    "readlinkat({dirfd}, {:?}, {size}) must stay in guest",
                    String::from_utf8_lossy(path)
                );
                assert!(cx.task.sync.in_guest.load(Ordering::Relaxed));
                assert_eq!(cx.task.sync.entry.get(), entry_before);
                let fast_ret = fx.cpu.state().x[0];
                let mut fast_buf = [0u8; 64];
                cx.mm()
                    .read(
                        crate::uaccess::UserSlice::new(out_addr, fast_buf.len()),
                        &mut fast_buf,
                    )
                    .unwrap();

                // Reset canary and run slow path for exact comparison.
                cx.mm()
                    .write(
                        crate::uaccess::UserSlice::new(out_addr, canary.len()),
                        &canary,
                    )
                    .unwrap();
                let slow_ret = run_slow(&mut cx, &mut fx.cpu, 78, args);
                let mut slow_buf = [0u8; 64];
                cx.mm()
                    .read(
                        crate::uaccess::UserSlice::new(out_addr, slow_buf.len()),
                        &mut slow_buf,
                    )
                    .unwrap();
                assert_eq!(
                    fast_ret,
                    slow_ret,
                    "readlinkat({dirfd}, {:?}, {size}) return mismatch",
                    String::from_utf8_lossy(path)
                );
                assert_eq!(
                    fast_buf,
                    slow_buf,
                    "readlinkat({dirfd}, {:?}, {size}) buffer mismatch",
                    String::from_utf8_lossy(path)
                );
            }

            // Fallback cases: size <= 0, empty path, non-AT_FDCWD relative path, uncached path,
            // mount point (/proc), read-only output buffer.
            for (label, dirfd, path, buf, size) in [
                ("size=0", AT_FDCWD, &b"/usr/bin/env"[..], out_addr, 0usize),
                ("size<0", AT_FDCWD, b"/usr/bin/env", out_addr, usize::MAX),
                ("empty path", AT_FDCWD, b"", out_addr, 64),
                ("dirfd relative", 5, b"usr/bin/env", out_addr, 64),
                ("uncached", AT_FDCWD, b"/never-looked-up", out_addr, 64),
                ("mount point", AT_FDCWD, b"/proc", out_addr, 64),
                ("ro output", AT_FDCWD, b"/usr/bin/env", ro_addr, 64),
            ] {
                write_cstr(&cx, path_addr, path);
                let args = [dirfd as i64 as u64, path_addr, buf, size as u64, 0, 0];
                assert_eq!(
                    run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 78, args),
                    FastOutcome::Fallback,
                    "readlinkat fallback expected for {label}"
                );
            }
            // Confirm slow path returns EINVAL for size <= 0.
            write_cstr(&cx, path_addr, b"/usr/bin/env");
            assert_eq!(
                run_slow(
                    &mut cx,
                    &mut fx.cpu,
                    78,
                    [AT_FDCWD as i64 as u64, path_addr, out_addr, 0, 0, 0]
                ),
                Errno::EINVAL.to_syscall_return()
            );
            assert_eq!(
                run_slow(
                    &mut cx,
                    &mut fx.cpu,
                    78,
                    [
                        AT_FDCWD as i64 as u64,
                        path_addr,
                        out_addr,
                        usize::MAX as u64,
                        0,
                        0
                    ]
                ),
                Errno::EINVAL.to_syscall_return()
            );

            // Stale fast_epoch forces fallback; refresh re-enables fast path.
            shared.fast_changed();
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    78,
                    [AT_FDCWD as i64 as u64, path_addr, out_addr, 64, 0, 0]
                ),
                FastOutcome::Fallback
            );
            fast.refresh(&shared, cx.task, cx.t);
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    78,
                    [AT_FDCWD as i64 as u64, path_addr, out_addr, 64, 0, 0]
                ),
                FastOutcome::InGuest
            );

            // When cwd is outside ROOT_MOUNT (e.g. /proc), relative paths fall back while absolute
            // paths remain on the fast path.
            let proc_node = {
                let vfs = cx.vfs().unwrap();
                let view = cx.view(&vfs);
                vfs.resolve(&view, &cx.cred(), &Node::ROOT, &Node::ROOT, b"/proc", true)
                    .unwrap()
            };
            cx.t.fs.borrow_mut().set_cwd(proc_node);
            shared.fast_changed();
            fast.refresh(&shared, cx.task, cx.t);
            write_cstr(&cx, path_addr, b"usr/bin/env");
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    78,
                    [AT_FDCWD as i64 as u64, path_addr, out_addr, 64, 0, 0]
                ),
                FastOutcome::Fallback
            );
            write_cstr(&cx, path_addr, b"/usr/bin/env");
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    78,
                    [AT_FDCWD as i64 as u64, path_addr, out_addr, 64, 0, 0]
                ),
                FastOutcome::InGuest
            );
        }
    }

    #[test]
    fn faccessat_and_faccessat2_fast_path_match_slow_path_and_semantics() {
        use crate::cred::{cap, secbit};
        use crate::vfs::ms;

        const AT_EMPTY_PATH: u32 = 0x1000;
        const F_OK: u32 = 0;
        const X_OK: u32 = 1;
        const W_OK: u32 = 2;
        const R_OK: u32 = 4;

        let mut fx = VfsFastFixture::new("faccessat");
        let shared = fx.shared.clone();
        let path_addr = fx.page;
        {
            let k = shared.lock();
            let task = k.tasks[&fx.pid].clone();
            let mut state = task.state.borrow_mut();
            let mut cx = Cx::new(&shared, k, &task, &mut state);
            let mut fast = Fast::new(&shared, cx.task, cx.t);

            // Root fast path across faccessat (48) and faccessat2 (439).
            for (nr, dirfd, path, amode, flags) in [
                (48u64, AT_FDCWD, &b"/bin/busybox"[..], F_OK, 0u32),
                (48, AT_FDCWD, b"/bin/busybox", R_OK | W_OK | X_OK, 0),
                (48, 999, b"/bin/busybox", R_OK | X_OK, 0),
                (48, AT_FDCWD, b"bin/busybox", R_OK | X_OK, 0),
                (48, AT_FDCWD, b"/bin/plain", R_OK | W_OK, 0),
                (48, AT_FDCWD, b"/bin/plain", X_OK, 0),
                (48, AT_FDCWD, b"/bin", R_OK | W_OK | X_OK, 0),
                (48, AT_FDCWD, b"/bin/imm", F_OK, 0),
                (48, AT_FDCWD, b"/bin/imm", R_OK, 0),
                (48, AT_FDCWD, b"/bin/imm", W_OK, 0),
                (48, AT_FDCWD, b"/dangling", F_OK, 0),
                (48, AT_FDCWD, b"/missing", F_OK, 0),
                (439, AT_FDCWD, b"/dangling", F_OK, AT_SYMLINK_NOFOLLOW),
                (
                    439,
                    AT_FDCWD,
                    b"/dangling",
                    R_OK | W_OK | X_OK,
                    AT_SYMLINK_NOFOLLOW,
                ),
                (439, AT_FDCWD, b"/bin/busybox", X_OK, AT_EACCESS),
                (439, AT_FDCWD, b"/bin/busybox", R_OK, AT_EMPTY_PATH),
            ] {
                write_cstr(&cx, path_addr, path);
                let args = [
                    dirfd as i64 as u64,
                    path_addr,
                    u64::from(amode),
                    u64::from(flags),
                    0,
                    0,
                ];
                let entry_before = cx.task.sync.entry.get();
                cx.task.sync.in_guest.store(true, Ordering::SeqCst);
                let outcome = run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, nr, args);
                assert_eq!(
                    outcome,
                    FastOutcome::InGuest,
                    "syscall {nr}({dirfd}, {:?}, {amode}, {flags:#x}) must stay in guest",
                    String::from_utf8_lossy(path)
                );
                assert!(cx.task.sync.in_guest.load(Ordering::Relaxed));
                assert_eq!(cx.task.sync.entry.get(), entry_before);
                let fast_ret = fx.cpu.state().x[0];
                let slow_ret = run_slow(&mut cx, &mut fx.cpu, nr, args);
                assert_eq!(
                    fast_ret,
                    slow_ret,
                    "syscall {nr}({dirfd}, {:?}, {amode}, {flags:#x}) mismatch",
                    String::from_utf8_lossy(path)
                );
            }

            // Fallback cases: invalid amode, invalid flags, empty path (even with AT_EMPTY_PATH),
            // relative path with non-AT_FDCWD dirfd, extended ACL on non-exec X_OK, mount point,
            // uncached path.
            for (label, nr, dirfd, path, amode, flags) in [
                (
                    "bad amode 48",
                    48u64,
                    AT_FDCWD,
                    &b"/bin/busybox"[..],
                    8u32,
                    0u32,
                ),
                ("bad amode 439", 439, AT_FDCWD, b"/bin/busybox", 8, 0),
                (
                    "bad flags 439",
                    439,
                    AT_FDCWD,
                    b"/bin/busybox",
                    F_OK,
                    0x4000,
                ),
                ("empty path", 439, AT_FDCWD, b"", F_OK, AT_EMPTY_PATH),
                ("dirfd relative", 48, 3, b"bin/busybox", F_OK, 0),
                ("acl X_OK", 48, AT_FDCWD, b"/bin/acl", X_OK, 0),
                ("mount point", 48, AT_FDCWD, b"/proc", F_OK, 0),
                ("uncached", 48, AT_FDCWD, b"/never-looked-up", F_OK, 0),
            ] {
                write_cstr(&cx, path_addr, path);
                let args = [
                    dirfd as i64 as u64,
                    path_addr,
                    u64::from(amode),
                    u64::from(flags),
                    0,
                    0,
                ];
                assert_eq!(
                    run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, nr, args),
                    FastOutcome::Fallback,
                    "faccessat fallback expected for {label}"
                );
            }
            // Confirm slow path returns EINVAL for bad amode and bad flags.
            write_cstr(&cx, path_addr, b"/bin/busybox");
            assert_eq!(
                run_slow(
                    &mut cx,
                    &mut fx.cpu,
                    48,
                    [AT_FDCWD as i64 as u64, path_addr, 8, 0, 0, 0]
                ),
                Errno::EINVAL.to_syscall_return()
            );
            assert_eq!(
                run_slow(
                    &mut cx,
                    &mut fx.cpu,
                    439,
                    [AT_FDCWD as i64 as u64, path_addr, 0, 0x4000, 0, 0]
                ),
                Errno::EINVAL.to_syscall_return()
            );

            // Setuid-root credentials (ruid=1000, euid=0, full caps, securebits=0):
            // - AT_EACCESS uses effective caps (fast path returns Ok(0) on /bin/secret).
            // - Without AT_EACCESS, for_access() drops caps for non-root ruid=1000, so fast path
            //   MUST fall back and slow path returns EACCES on 0600 root-owned /bin/secret.
            let mut setuid_cred = cx.cred();
            setuid_cred.ruid = 1000;
            setuid_cred.rgid = 1000;
            cx.set_cred(setuid_cred);
            // Stale fast epoch falls back before refresh.
            write_cstr(&cx, path_addr, b"/bin/secret");
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    439,
                    [
                        AT_FDCWD as i64 as u64,
                        path_addr,
                        u64::from(R_OK | W_OK),
                        u64::from(AT_EACCESS),
                        0,
                        0
                    ]
                ),
                FastOutcome::Fallback
            );
            fast.refresh(&shared, cx.task, cx.t);
            let eaccess_args = [
                AT_FDCWD as i64 as u64,
                path_addr,
                u64::from(R_OK | W_OK),
                u64::from(AT_EACCESS),
                0,
                0,
            ];
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 439, eaccess_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], 0);
            assert_eq!(run_slow(&mut cx, &mut fx.cpu, 439, eaccess_args), 0);

            for (nr, flags) in [(48u64, 0u32), (439, 0)] {
                let args = [
                    AT_FDCWD as i64 as u64,
                    path_addr,
                    u64::from(R_OK),
                    u64::from(flags),
                    0,
                    0,
                ];
                assert_eq!(
                    run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, nr, args),
                    FastOutcome::Fallback,
                    "setuid-root without AT_EACCESS must fall back for syscall {nr}"
                );
                assert_eq!(
                    run_slow(&mut cx, &mut fx.cpu, nr, args),
                    Errno::EACCES.to_syscall_return()
                );
            }

            // SECBIT_NO_SETUID_FIXUP with ruid=1000, euid=0, CAP_DAC_READ_SEARCH only:
            // access_cred keeps fsuid=1000 and CAP_DAC_READ_SEARCH, so on uid-1000 0400
            // /bin/owned_ro, R_OK -> fast Ok(0) and W_OK -> fast EACCES; on uid-0 /bin/plain,
            // W_OK falls back to BKL.
            let mut fixup_cred = cx.cred();
            fixup_cred.securebits = secbit::NO_SETUID_FIXUP;
            fixup_cred.cap_effective = 1 << cap::DAC_READ_SEARCH;
            fixup_cred.cap_permitted = 1 << cap::DAC_READ_SEARCH;
            cx.set_cred(fixup_cred);
            fast.refresh(&shared, cx.task, cx.t);

            write_cstr(&cx, path_addr, b"/bin/owned_ro");
            let ro_r_args = [AT_FDCWD as i64 as u64, path_addr, u64::from(R_OK), 0, 0, 0];
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, ro_r_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], 0);
            assert_eq!(run_slow(&mut cx, &mut fx.cpu, 48, ro_r_args), 0);

            let ro_w_args = [AT_FDCWD as i64 as u64, path_addr, u64::from(W_OK), 0, 0, 0];
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, ro_w_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], Errno::EACCES.to_syscall_return());
            assert_eq!(
                run_slow(&mut cx, &mut fx.cpu, 48, ro_w_args),
                Errno::EACCES.to_syscall_return()
            );

            write_cstr(&cx, path_addr, b"/bin/plain");
            assert_eq!(
                run_fast_outcome(
                    &fast,
                    &shared,
                    cx.task,
                    &mut fx.cpu,
                    48,
                    [AT_FDCWD as i64 as u64, path_addr, u64::from(W_OK), 0, 0, 0]
                ),
                FastOutcome::Fallback
            );

            // Restore root credentials and test ROOT_MOUNT MS_NOEXEC and ROOT_DEV MS_RDONLY.
            cx.set_cred(crate::cred::Cred::root());
            {
                let vfs = cx.vfs().unwrap();
                vfs.update_mount(ROOT_MOUNT, |m| m.flags |= ms::NOEXEC);
            }
            shared.fast_changed();
            fast.refresh(&shared, cx.task, cx.t);
            write_cstr(&cx, path_addr, b"/bin/busybox");
            let x_args = [AT_FDCWD as i64 as u64, path_addr, u64::from(X_OK), 0, 0, 0];
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, x_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], Errno::EACCES.to_syscall_return());
            assert_eq!(
                run_slow(&mut cx, &mut fx.cpu, 48, x_args),
                Errno::EACCES.to_syscall_return()
            );
            {
                let vfs = cx.vfs().unwrap();
                vfs.update_mount(ROOT_MOUNT, |m| m.flags &= !ms::NOEXEC);
                vfs.set_sb_flags(ROOT_DEV, ms::RDONLY);
            }
            shared.fast_changed();
            // Stale epoch before refresh must fall back.
            write_cstr(&cx, path_addr, b"/bin/plain");
            let w_args = [AT_FDCWD as i64 as u64, path_addr, u64::from(W_OK), 0, 0, 0];
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, w_args),
                FastOutcome::Fallback
            );
            fast.refresh(&shared, cx.task, cx.t);
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, w_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], Errno::EROFS.to_syscall_return());
            assert_eq!(
                run_slow(&mut cx, &mut fx.cpu, 48, w_args),
                Errno::EROFS.to_syscall_return()
            );
            // Special file (FIFO) is exempt from EROFS.
            write_cstr(&cx, path_addr, b"/bin/pipe");
            assert_eq!(
                run_fast_outcome(&fast, &shared, cx.task, &mut fx.cpu, 48, w_args),
                FastOutcome::InGuest
            );
            assert_eq!(fx.cpu.state().x[0], 0);
            assert_eq!(run_slow(&mut cx, &mut fx.cpu, 48, w_args), 0);
        }
    }
}
