//! futex、时间、资源限制等杂项 syscall。

use std::time::{Duration, Instant};

use super::{IO_CHUNK, SysResult, get, put, read_timespec, write_pair};
use crate::cred::{cap, secbit};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::itimer::{CpuTimer, ITimer};
use crate::kernel::Cx;
use crate::process::{RLIM_NLIMITS, RLIMIT_NOFILE, Rusage, Signal};
use crate::uaccess::UserSlice;

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_CLOCK_REALTIME: u32 = 256;
/// 去掉 `FUTEX_PRIVATE_FLAG`、`FUTEX_CLOCK_REALTIME` 之后的命令位。
const FUTEX_CMD_MASK: u32 = 0x7f;

const TIMER_ABSTIME: u32 = 1;
const ITIMER_REAL: i32 = 0;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;

const PR_SET_PDEATHSIG: i32 = 1;
const PR_GET_PDEATHSIG: i32 = 2;
const PR_GET_DUMPABLE: i32 = 3;
const PR_SET_DUMPABLE: i32 = 4;
const PR_GET_KEEPCAPS: i32 = 7;
const PR_SET_KEEPCAPS: i32 = 8;
const PR_SET_NAME: i32 = 15;
const PR_GET_NAME: i32 = 16;
const PR_GET_SECCOMP: i32 = 21;
const PR_CAPBSET_READ: i32 = 23;
const PR_CAPBSET_DROP: i32 = 24;
const PR_GET_SECUREBITS: i32 = 27;
const PR_SET_SECUREBITS: i32 = 28;
const PR_GET_TIMING: i32 = 13;
const PR_SET_TIMING: i32 = 14;
const PR_TIMING_STATISTICAL: u64 = 0;
const PR_SET_SECCOMP: i32 = 22;
const PR_SET_THP_DISABLE: i32 = 41;
const PR_GET_THP_DISABLE: i32 = 42;
const PR_CAP_AMBIENT: i32 = 47;
const PR_CAP_AMBIENT_IS_SET: u64 = 1;
const PR_CAP_AMBIENT_RAISE: u64 = 2;
const PR_CAP_AMBIENT_LOWER: u64 = 3;
const PR_CAP_AMBIENT_CLEAR_ALL: u64 = 4;
const PR_GET_SPECULATION_CTRL: i32 = 52;
const PR_SPEC_NOT_AFFECTED: u64 = 0;
const PR_SET_TIMERSLACK: i32 = 29;
const PR_GET_TIMERSLACK: i32 = 30;
const PR_SET_CHILD_SUBREAPER: i32 = 36;
const PR_GET_CHILD_SUBREAPER: i32 = 37;
const PR_SET_NO_NEW_PRIVS: i32 = 38;
const PR_GET_NO_NEW_PRIVS: i32 = 39;
/// 最大的 capability 编号（`CAP_CHECKPOINT_RESTORE`）。
const CAP_LAST_CAP: u64 = 40;

const GRND_NONBLOCK: u32 = 1;
const GRND_RANDOM: u32 = 2;
const GRND_INSECURE: u32 = 4;

/// `uname -r` 报告的内核版本，即 ABI 契约（设计文档 §7.5）。
const RELEASE: &[u8] = b"6.1.0-appine";

/// 把 `clock` 上的绝对时间换算成等待的截止时刻。已经过去的时间换算成过去的时刻：
/// 周期性的定时器要按它算错过了几个周期。早到单调时钟表示不了时就是现在。
pub(super) fn deadline_from_clock(clock: i32, abs: Duration) -> Result<Instant, Errno> {
    let (sec, nsec) = appine_hal::clock_gettime(clock).map_err(Errno)?;
    let now = Duration::new(sec.max(0) as u64, nsec as u32);
    let base = Instant::now();
    Ok(match abs.checked_sub(now) {
        Some(ahead) => base + ahead,
        None => base.checked_sub(now - abs).unwrap_or(base),
    })
}

/// futex 的键：翻译后的宿主地址。
fn futex_key(cx: &Cx<'_>, uaddr: u64) -> Result<Key, Errno> {
    if !uaddr.is_multiple_of(4) {
        return Err(Errno::EINVAL);
    }
    cx.mm().futex_key(uaddr).ok_or(Errno::EFAULT)
}

pub(super) fn futex(
    cx: &mut Cx<'_>,
    uaddr: u64,
    op: u32,
    val: u32,
    timeout: u64,
    uaddr2: u64,
    val3: u32,
) -> SysResult {
    let cmd = op & FUTEX_CMD_MASK;
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if cmd == FUTEX_WAIT { u32::MAX } else { val3 };
            if bitset == 0 {
                return Err(Errno::EINVAL);
            }
            // FUTEX_WAIT 的超时是相对的（CLOCK_MONOTONIC），WAIT_BITSET 的是绝对的
            let deadline = match timeout {
                0 => None,
                addr => {
                    let t = read_timespec(cx, addr)?;
                    Some(if cmd == FUTEX_WAIT {
                        Instant::now() + t
                    } else {
                        let clock = if op & FUTEX_CLOCK_REALTIME != 0 {
                            CLOCK_REALTIME
                        } else {
                            CLOCK_MONOTONIC
                        };
                        deadline_from_clock(clock, t)?
                    })
                }
            };
            let key = futex_key(cx, uaddr)?;
            let shared = cx.shared;
            // 不持 BKL 的唤醒（syscall::fast）也取 futex 的锁：检查值和登记之间不会漏掉
            let id = {
                let mut futexes = shared.futexes();
                if u32::from_le_bytes(get(cx, uaddr)?) != val {
                    return Err(Errno::EAGAIN);
                }
                futexes.enqueue(key, bitset, cx.task.event.clone())
            };
            let waiting = Enqueued::new(shared, [id]);
            crate::bug::trap(crate::bug::Trap::FutexWait, cx.tgid());
            loop {
                if !shared.futexes().is_queued(id) {
                    waiting.leave();
                    return Ok(0);
                }
                let err = if deadline.is_some_and(|d| Instant::now() >= d) {
                    Some(Errno::ETIMEDOUT)
                } else if cx.interrupted() {
                    Some(Errno::ERESTARTSYS)
                } else {
                    cx.wait_alone(deadline).err()
                };
                if let Some(e) = err {
                    // 原来放开 futex 的锁之后才移除：这中间不持 BKL 的唤醒（syscall::fast）
                    // 取走了它的话，唤醒丢了，它却返回错误
                    return match waiting.leave() {
                        Some(_) => Ok(0),
                        None => Err(e),
                    };
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE { u32::MAX } else { val3 };
            if bitset == 0 {
                return Err(Errno::EINVAL);
            }
            let key = futex_key(cx, uaddr)?;
            Ok(cx.shared.futexes().wake(key, val, bitset) as u64)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            // 唤醒和移动的个数是 int，负数是 EINVAL
            if (val as i32) < 0 || (timeout as u32 as i32) < 0 {
                return Err(Errno::EINVAL);
            }
            let key = futex_key(cx, uaddr)?;
            let key2 = futex_key(cx, uaddr2)?;
            let mut futexes = cx.shared.futexes();
            if cmd == FUTEX_CMP_REQUEUE && u32::from_le_bytes(get(cx, uaddr)?) != val3 {
                return Err(Errno::EAGAIN);
            }
            // 第四个参数在 REQUEUE 里是要移动的个数，不是超时
            let n = futexes.requeue(key, val, key2, timeout as u32);
            Ok(n as u64)
        }
        _ => Err(Errno::ENOSYS),
    }
}

/// `struct futex_waitv` 的 flags：32 位的 futex，以及只在本进程内有效（我们不区分）。
const FUTEX_32: u32 = 2;
const FUTEX_PRIVATE_FLAG: u32 = 128;
/// futex_waitv 一次最多等的 futex 个数。
const FUTEX_WAITV_MAX: u32 = 128;

/// futex_waitv(2)：同时等多个 futex，任何一个被唤醒就返回它的下标。超时是 `clock`
/// 上的绝对时间。
pub(super) fn futex_waitv(
    cx: &mut Cx<'_>,
    waiters: u64,
    nr: u32,
    flags: u32,
    timeout: u64,
    clock: i32,
) -> SysResult {
    if flags != 0 || waiters == 0 || nr == 0 || nr > FUTEX_WAITV_MAX {
        return Err(Errno::EINVAL);
    }
    let deadline = match timeout {
        0 => None,
        _ if clock != CLOCK_MONOTONIC && clock != CLOCK_REALTIME => return Err(Errno::EINVAL),
        addr => Some(deadline_from_clock(clock, read_timespec(cx, addr)?)?),
    };
    // struct futex_waitv { u64 val; u64 uaddr; u32 flags; u32 __reserved; }
    let mut keys = Vec::with_capacity(nr as usize);
    for i in 0..nr as u64 {
        let w: [u8; 24] = get(cx, waiters + i * 24)?;
        let val = u64::from_le_bytes(w[..8].try_into().unwrap());
        let uaddr = u64::from_le_bytes(w[8..16].try_into().unwrap());
        let wflags = u32::from_le_bytes(w[16..20].try_into().unwrap());
        let reserved = u32::from_le_bytes(w[20..].try_into().unwrap());
        if wflags & !(FUTEX_32 | FUTEX_PRIVATE_FLAG) != 0 || wflags & FUTEX_32 == 0 || reserved != 0
        {
            return Err(Errno::EINVAL);
        }
        keys.push((futex_key(cx, uaddr)?, uaddr, val));
    }
    // 持有 futex 的锁：检查值和登记之间不会有人唤醒
    let shared = cx.shared;
    let ids: Vec<u64> = {
        let mut futexes = shared.futexes();
        for &(_, uaddr, val) in &keys {
            if u64::from(u32::from_le_bytes(get(cx, uaddr)?)) != val {
                return Err(Errno::EAGAIN);
            }
        }
        let event = &cx.task.event;
        keys.iter()
            .map(|&(key, ..)| futexes.enqueue(key, u32::MAX, event.clone()))
            .collect()
    };
    let waiting = Enqueued::new(shared, ids);
    crate::bug::trap(crate::bug::Trap::FutexWait, cx.tgid());
    let r = loop {
        let woken = waiting
            .ids()
            .iter()
            .position(|&id| !shared.futexes().is_queued(id));
        if let Some(i) = woken {
            break Ok(i as u64);
        }
        let err = if deadline.is_some_and(|d| Instant::now() >= d) {
            Some(Errno::ETIMEDOUT)
        } else if cx.interrupted() {
            Some(Errno::ERESTARTSYS)
        } else {
            cx.wait_alone(deadline).err()
        };
        if let Some(e) = err {
            break Err(e);
        }
    };
    // 与 Linux 的 futex_unqueue_multiple 一样，超时、被信号打断时刚好被唤醒的算唤醒
    match (waiting.leave(), r) {
        (Some(i), Err(_)) => Ok(i as u64),
        (_, r) => r,
    }
}

/// 睡到 `deadline`，被信号打断时返回剩余时间。
fn sleep_until(cx: &mut Cx<'_>, deadline: Instant) -> Result<(), Duration> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if cx.interrupted() || cx.wait(Some(deadline), &mut []).is_err() {
            return Err(deadline - now);
        }
    }
}

pub(super) fn nanosleep(cx: &mut Cx<'_>, req: u64, rem: u64) -> SysResult {
    let early = cx.t.fast_sleep.take();
    let t = read_timespec(cx, req)?;
    sleep_rest(cx, early.unwrap_or_else(|| Instant::now() + t), rem)
}

/// 睡到 `deadline`。被信号打断时把剩下的时长写进 `rem`（不为 0 时），并记下到期时刻：
/// 没有处理函数打断时（比如停止之后又继续）由 restart_syscall 接着睡，而不是按原来的
/// 时长重新睡一遍。
fn sleep_rest(cx: &mut Cx<'_>, deadline: Instant, rem: u64) -> SysResult {
    match sleep_until(cx, deadline) {
        Ok(()) => Ok(0),
        Err(left) => {
            if rem != 0 {
                write_pair(cx, rem, left.as_secs() as i64, left.subsec_nanos() as i64)?;
            }
            cx.t.restart = Some((deadline, rem));
            Err(Errno::ERESTART_RESTARTBLOCK)
        }
    }
}

/// restart_syscall：接着做被打断的 nanosleep。与 Linux 相同，没有可以接着做的时返回 EINTR。
pub(super) fn restart_syscall(cx: &mut Cx<'_>) -> SysResult {
    match cx.t.restart.take() {
        Some((deadline, rem)) => sleep_rest(cx, deadline, rem),
        None => Err(Errno::EINTR),
    }
}

/// 读 `struct itimerval`：(间隔, 第一次到期前的时长)。
fn read_itimerval(cx: &Cx<'_>, addr: u64) -> Result<(Duration, Duration), Errno> {
    let b: [u8; 32] = get(cx, addr)?;
    let tv = |o: usize| {
        let sec = i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let usec = i64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap());
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return Err(Errno::EINVAL);
        }
        Ok(Duration::new(sec as u64, usec as u32 * 1000))
    };
    Ok((tv(0)?, tv(16)?))
}

/// 写 `struct itimerval`：间隔和离下一次到期还有多久。
fn write_itimerval(cx: &Cx<'_>, addr: u64, t: Option<(Duration, Duration)>) -> Result<(), Errno> {
    let (interval, left) = match t {
        // 与 Linux 相同，还没到期的定时器至少报告 1 微秒
        Some((interval, left)) => (interval, left.max(Duration::from_micros(1))),
        None => (Duration::ZERO, Duration::ZERO),
    };
    let mut b = [0u8; 32];
    for (o, d) in [(0, interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_micros() as i64).to_le_bytes());
    }
    put(cx, addr, b)
}

const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

/// 定时器的当前值：(间隔, 离下一次到期还有多久)，没有启动时是 None。
fn itimer_value(cx: &Cx<'_>, which: i32) -> Option<(Duration, Duration)> {
    let g = cx.t.group.borrow();
    if which == ITIMER_REAL {
        let t = g.itimer?;
        return Some((
            t.interval,
            t.deadline.saturating_duration_since(Instant::now()),
        ));
    }
    let t = g.cpu_itimers[(which - 1) as usize]?;
    let left = t.deadline.saturating_sub(g.cpu_ns(cx.kr()));
    Some((Duration::from_nanos(t.interval), Duration::from_nanos(left)))
}

fn check_itimer(which: i32) -> Result<(), Errno> {
    match which {
        ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn getitimer(cx: &mut Cx<'_>, which: i32, curr: u64) -> SysResult {
    check_itimer(which)?;
    let t = itimer_value(cx, which);
    write_itimerval(cx, curr, t).map(|()| 0)
}

pub(super) fn setitimer(cx: &mut Cx<'_>, which: i32, new: u64, old: u64) -> SysResult {
    check_itimer(which)?;
    // 与 Linux 相同，new 为空时当作解除
    let (interval, value) = if new == 0 {
        (Duration::ZERO, Duration::ZERO)
    } else {
        read_itimerval(cx, new)?
    };
    if old != 0 {
        let prev = itimer_value(cx, which);
        write_itimerval(cx, old, prev)?;
    }
    if which == ITIMER_REAL {
        cx.t.group.borrow_mut().itimer = (!value.is_zero()).then(|| ITimer {
            deadline: Instant::now() + value,
            interval,
        });
    } else {
        let now = cx.t.group.borrow().cpu_ns(cx.kr());
        cx.t.group.borrow_mut().cpu_itimers[(which - 1) as usize] =
            (!value.is_zero()).then(|| CpuTimer {
                deadline: now + value.as_nanos() as u64,
                interval: interval.as_nanos() as u64,
            });
    }
    // 让定时器线程重新计算等待时间。只叫醒它：原来叫醒所有等待者
    crate::kernel::signal_later(&cx.shared.timers);
    Ok(0)
}

pub(super) fn clock_nanosleep(
    cx: &mut Cx<'_>,
    clock: i32,
    flags: u32,
    req: u64,
    rem: u64,
) -> SysResult {
    let early = cx.t.fast_sleep.take();
    match clock {
        // 线程 CPU 时钟、原始单调时钟和粗粒度时钟不能用来睡眠
        CLOCK_THREAD_CPUTIME_ID | 4..=6 => return Err(Errno::EOPNOTSUPP),
        c if c < 0 && c & 4 != 0 => return Err(Errno::EINVAL),
        _ => {}
    }
    let t = read_timespec(cx, req)?;
    // 进程 CPU 时钟：按 CPU 时间换算成大约的墙上时间（每 10 毫秒检查一次）
    if let Some(cpu) = cpu_clock(cx, clock) {
        let now = cpu?;
        let target = if flags & TIMER_ABSTIME != 0 {
            t.as_nanos() as u64
        } else {
            now + t.as_nanos() as u64
        };
        while cpu_clock(cx, clock).unwrap()? < target {
            if sleep_until(cx, Instant::now() + Duration::from_millis(10)).is_err() {
                return Err(Errno::ERESTARTNOHAND);
            }
        }
        return Ok(0);
    }
    if flags & TIMER_ABSTIME == 0 {
        appine_hal::clock_gettime(clock).map_err(Errno)?;
        return sleep_rest(cx, early.unwrap_or_else(|| Instant::now() + t), rem);
    }
    // 绝对时刻：重新执行同样的参数就是接着睡
    match sleep_until(cx, deadline_from_clock(clock, t)?) {
        Ok(()) => Ok(0),
        Err(_) => Err(Errno::ERESTARTNOHAND),
    }
}

const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// guest 进程或线程的 CPU 时钟用掉的纳秒数；`clock` 不是 CPU 时钟时返回 None。负数的
/// 时钟 id 是 `clock_getcpuclockid`/`pthread_getcpuclockid` 编码出来的（Linux 的
/// `MAKE_PROCESS_CPUCLOCK`）：`(~pid << 3) | 类型`，第 2 位表示线程，pid 0 表示自己。
fn cpu_clock(cx: &Cx<'_>, clock: i32) -> Option<Result<u64, Errno>> {
    let (pid, thread) = match clock {
        CLOCK_PROCESS_CPUTIME_ID => (0, false),
        CLOCK_THREAD_CPUTIME_ID => (0, true),
        // 类型 3 不是 CPU 时钟（低 3 位为 3 是 fd 编码的动态时钟）
        c if c < 0 && c & 3 == 3 => return Some(Err(Errno::EINVAL)),
        c if c < 0 => (!(c >> 3), c & 4 != 0),
        _ => return None,
    };
    let k = cx.kr();
    Some(if thread {
        let task = if pid == 0 {
            Some(cx.task.clone())
        } else {
            k.tasks.get(&pid).cloned()
        };
        task.map(|t| t.cpu_ns()).ok_or(Errno::EINVAL)
    } else {
        let group = if pid == 0 {
            Some(cx.t.group.clone())
        } else {
            k.groups.get(&pid).cloned()
        };
        group.map(|g| g.borrow().cpu_ns(k)).ok_or(Errno::EINVAL)
    })
}

pub(super) fn clock_gettime(cx: &mut Cx<'_>, clock: i32, tp: u64) -> SysResult {
    let (sec, nsec) = match cpu_clock(cx, clock) {
        Some(ns) => {
            let ns = ns?;
            ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
        }
        None => appine_hal::clock_gettime(clock).map_err(Errno)?,
    };
    write_pair(cx, tp, sec, nsec).map(|()| 0)
}

pub(super) fn clock_getres(cx: &mut Cx<'_>, clock: i32, res: u64) -> SysResult {
    let (sec, nsec) = match cpu_clock(cx, clock) {
        Some(ns) => ns.map(|_| (0, 1))?,
        None => appine_hal::clock_getres(clock).map_err(Errno)?,
    };
    if res != 0 {
        write_pair(cx, res, sec, nsec)?;
    }
    Ok(0)
}

/// clock_settime：guest 不能改宿主的时间。可以设置的时钟（`CLOCK_REALTIME`）返回
/// EPERM，与没有 `CAP_SYS_TIME` 时相同；其余的时钟本来就不能设置。
pub(super) fn clock_settime(cx: &mut Cx<'_>, clock: i32, tp: u64) -> SysResult {
    read_timespec(cx, tp)?;
    match clock {
        CLOCK_REALTIME => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn gettimeofday(cx: &mut Cx<'_>, tv: u64, tz: u64) -> SysResult {
    if tv != 0 {
        let (sec, nsec) = appine_hal::clock_gettime(CLOCK_REALTIME).map_err(Errno)?;
        write_pair(cx, tv, sec, nsec / 1000)?;
    }
    if tz != 0 {
        put(cx, tz, [0u8; 8])?;
    }
    Ok(0)
}

pub(super) fn times(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let ticks = crate::loader::CLOCK_TICKS;
    // struct tms：自己和已回收的子进程的用户态、内核态 CPU 时间
    if buf != 0 {
        let to_ticks = |ns: u64| ns / (1_000_000_000 / ticks);
        let g = cx.t.group.borrow();
        let (me, children) = (g.cpu(cx.kr()), g.child_cpu);
        drop(g);
        let mut b = [0u8; 32];
        let fields = [me.user_ns(), me.sys_ns, children.user_ns(), children.sys_ns];
        for (i, ns) in fields.into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&to_ticks(ns).to_le_bytes());
        }
        put(cx, buf, b)?;
    }
    // 返回值是开机以来的时钟滴答数
    let (sec, nsec) = appine_hal::clock_gettime(appine_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    Ok(sec as u64 * ticks + nsec as u64 / (1_000_000_000 / ticks))
}

pub(super) fn getrusage(cx: &mut Cx<'_>, who: i32, usage: u64) -> SysResult {
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_CHILDREN: i32 = -1;
    const RUSAGE_THREAD: i32 = 1;
    if !matches!(who, RUSAGE_SELF | RUSAGE_CHILDREN | RUSAGE_THREAD) {
        return Err(Errno::EINVAL);
    }
    let group = cx.t.group.clone();
    let mut g = group.borrow_mut();
    // 与 Linux 一样，RUSAGE_THREAD 的 ru_maxrss 也是整个进程的
    if who != RUSAGE_CHILDREN {
        g.sample_rss_throttled(&cx.mm());
    }
    let r = match who {
        RUSAGE_SELF => Rusage {
            cpu: g.cpu(cx.kr()),
            maxrss_kb: g.maxrss_kb,
        },
        RUSAGE_CHILDREN => Rusage {
            cpu: g.child_cpu,
            maxrss_kb: g.child_maxrss_kb,
        },
        _ => Rusage {
            cpu: cx.task.cpu_self(),
            maxrss_kb: g.maxrss_kb,
        },
    };
    drop(g);
    put(cx, usage, r.encode()).map(|()| 0)
}

pub(super) fn uname(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let uts = cx.task.ns.borrow().uts.clone();
    let fields: [&[u8]; 6] = [
        b"Linux",
        &uts.hostname.borrow(),
        RELEASE,
        b"#1 SMP PREEMPT appine",
        b"aarch64",
        &uts.domainname.borrow(),
    ];
    let mut out = [0u8; 6 * 65];
    for (i, f) in fields.iter().enumerate() {
        out[i * 65..i * 65 + f.len()].copy_from_slice(f);
    }
    cx.mm()
        .write(UserSlice::new(buf, out.len()), &out)
        .map(|()| 0)
}

/// reboot：不能重启宿主，只接受 Ctrl-Alt-Del 的开关；其余合法的命令 EPERM。
pub(super) fn reboot(cx: &mut Cx<'_>, magic1: u32, magic2: u32, cmd: u32) -> SysResult {
    const CAP_SYS_BOOT: u32 = 22;
    const LINUX_REBOOT_MAGIC1: u32 = 0xfee1_dead;
    const MAGIC2: [u32; 4] = [672_274_793, 85_072_278, 369_367_448, 537_993_216];
    const CMD_CAD_ON: u32 = 0x89ab_cdef;
    const CMD_CAD_OFF: u32 = 0;
    const OTHER_CMDS: [u32; 6] = [
        0x0123_4567, // RESTART
        0xcdef_0123, // HALT
        0x4321_fedc, // POWER_OFF
        0xa1b2_c3d4, // RESTART2
        0xd000_fce2, // SW_SUSPEND
        0x4558_4543, // KEXEC
    ];
    if !cx.cred().capable(CAP_SYS_BOOT) {
        return Err(Errno::EPERM);
    }
    if magic1 != LINUX_REBOOT_MAGIC1 || !MAGIC2.contains(&magic2) {
        return Err(Errno::EINVAL);
    }
    match cmd {
        CMD_CAD_ON | CMD_CAD_OFF => Ok(0),
        c if OTHER_CMDS.contains(&c) => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

/// sethostname、setdomainname：改的是所在的 UTS 命名空间的，要在它的属主里有
/// `CAP_SYS_ADMIN`。
pub(super) fn set_uts_name(cx: &mut Cx<'_>, name: u64, len: i64, domain: bool) -> SysResult {
    let uts = cx.task.ns.borrow().uts.clone();
    if !cx.cred().ns_capable(&uts.owner, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|&n| n <= crate::kernel::UTS_LEN)
        .ok_or(Errno::EINVAL)?;
    let mut value = vec![0u8; len];
    cx.mm().read(UserSlice::new(name, len), &mut value)?;
    *if domain {
        uts.domainname.borrow_mut()
    } else {
        uts.hostname.borrow_mut()
    } = value;
    Ok(0)
}

pub(super) fn prlimit(cx: &mut Cx<'_>, pid: i32, resource: u32, new: u64, old: u64) -> SysResult {
    let cred = cx.cred();
    let group = if pid == 0 || pid == cx.tgid() {
        cx.t.group.clone()
    } else {
        let g = cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)?;
        // 别人的进程：它的用户 ID、组 ID 都要等于调用者的真实 ID
        let t = g.borrow().cred.clone();
        let same = [t.ruid, t.euid, t.suid].iter().all(|&u| u == cred.ruid)
            && [t.rgid, t.egid, t.sgid].iter().all(|&g| g == cred.rgid);
        if !same && !cred.capable(cap::SYS_RESOURCE) {
            return Err(Errno::EPERM);
        }
        g
    };
    if resource as usize >= RLIM_NLIMITS {
        return Err(Errno::EINVAL);
    }
    let slot = group.borrow().rlimits[resource as usize];
    let new = if new != 0 {
        let b: [u8; 16] = get(cx, new)?;
        let cur = u64::from_le_bytes(b[..8].try_into().unwrap());
        let max = u64::from_le_bytes(b[8..].try_into().unwrap());
        if cur > max {
            return Err(Errno::EINVAL);
        }
        // fd 表的容量是固定的；提高硬限制要求 CAP_SYS_RESOURCE
        if resource == RLIMIT_NOFILE && max > cx.kr().nr_open as u64
            || max > slot.1 && !cred.capable(cap::SYS_RESOURCE)
        {
            return Err(Errno::EPERM);
        }
        Some((cur, max))
    } else {
        None
    };
    if old != 0 {
        write_pair(cx, old, slot.0 as i64, slot.1 as i64)?;
    }
    if let Some(v) = new {
        group.borrow_mut().rlimits[resource as usize] = v;
        // fd 表按 RLIMIT_NOFILE 的软限制分配新的 fd
        if resource == RLIMIT_NOFILE {
            let k = cx.kr();
            for tid in &group.borrow().threads {
                if let Some(t) = k.tasks.get(tid) {
                    t.res.borrow().fds.borrow_mut().limit = v.0 as usize;
                }
            }
        }
    }
    Ok(0)
}

pub(super) fn umask(cx: &mut Cx<'_>, mask: u32) -> SysResult {
    let mut fs = cx.t.fs.borrow_mut();
    let old = fs.umask;
    fs.umask = mask & 0o777;
    Ok(old as u64)
}

pub(super) fn prctl(cx: &mut Cx<'_>, option: i32, args: [u64; 4]) -> SysResult {
    let arg = args[0];
    // 与 Linux 一样，用不到的参数必须是 0
    let unused = |from: usize| {
        if args[from..].iter().all(|&a| a == 0) {
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }
    };
    match option {
        PR_SET_NAME => {
            // 名字可以不以 NUL 结尾，最多取 15 字节
            let raw = match cx.mm().read_cstr(arg, 15) {
                Ok(name) => name,
                Err(e) if e == Errno::ENAMETOOLONG => get::<15>(cx, arg)?.to_vec(),
                Err(e) => return Err(e),
            };
            let mut comm = [0u8; 16];
            comm[..raw.len()].copy_from_slice(&raw);
            cx.task.comm.set(comm);
            Ok(0)
        }
        PR_GET_NAME => put(cx, arg, cx.task.comm.get()).map(|()| 0),
        PR_SET_PDEATHSIG => {
            if arg != 0 && Signal::new(arg as i32).is_none() {
                return Err(Errno::EINVAL);
            }
            cx.task.pdeathsig.set(arg as i32);
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let sig = cx.task.pdeathsig.get();
            put(cx, arg, sig.to_le_bytes()).map(|()| 0)
        }
        // 与 Linux 一样不检查别的参数，非 0 都算设上
        PR_SET_CHILD_SUBREAPER => {
            cx.t.group.borrow_mut().child_subreaper = arg != 0;
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let on = cx.t.group.borrow().child_subreaper as i32;
            put(cx, arg, on.to_le_bytes()).map(|()| 0)
        }
        PR_GET_DUMPABLE => Ok(cx.t.group.borrow().dumpable as u64),
        PR_SET_DUMPABLE if arg <= 1 => {
            cx.t.group.borrow_mut().dumpable = arg == 1;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS if arg == 1 => {
            unused(1)?;
            cx.t.group.borrow_mut().no_new_privs = true;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => {
            unused(0)?;
            Ok(cx.t.group.borrow().no_new_privs as u64)
        }
        PR_GET_KEEPCAPS => Ok(cx.cred().keep_caps() as u64),
        PR_SET_KEEPCAPS if arg <= 1 => {
            let c = &mut cx.t.group.borrow_mut().cred;
            if c.securebits & secbit::KEEP_CAPS_LOCKED != 0 {
                return Err(Errno::EPERM);
            }
            c.securebits = (c.securebits & !secbit::KEEP_CAPS) | (arg as u32 * secbit::KEEP_CAPS);
            Ok(0)
        }
        PR_GET_SECUREBITS => Ok(cx.cred().securebits as u64),
        PR_SET_SECUREBITS => {
            let bits = u32::try_from(arg).map_err(|_| Errno::EPERM)?;
            cx.t.group.borrow_mut().cred.set_securebits(bits)?;
            Ok(0)
        }
        PR_GET_SECCOMP => Ok(u64::from(cx.task.seccomp.borrow().mode)),
        PR_SET_SECCOMP => crate::seccomp::prctl_set(cx, arg, args[1]),
        PR_CAPBSET_READ if arg <= CAP_LAST_CAP => Ok((cx.cred().cap_bounding >> arg) & 1),
        PR_CAPBSET_DROP if arg <= CAP_LAST_CAP => {
            if !cx.cred().capable_here(cap::SETPCAP) {
                return Err(Errno::EPERM);
            }
            cx.t.group.borrow_mut().cred.cap_bounding &= !(1 << arg);
            Ok(0)
        }
        // 环境能力（capabilities(7)）
        PR_CAP_AMBIENT => match arg {
            PR_CAP_AMBIENT_CLEAR_ALL => {
                unused(1)?;
                cx.t.group.borrow_mut().cred.cap_ambient = 0;
                Ok(0)
            }
            PR_CAP_AMBIENT_IS_SET | PR_CAP_AMBIENT_RAISE | PR_CAP_AMBIENT_LOWER => {
                unused(2)?;
                if args[1] > CAP_LAST_CAP {
                    return Err(Errno::EINVAL);
                }
                let c = &mut cx.t.group.borrow_mut().cred;
                match arg {
                    PR_CAP_AMBIENT_IS_SET => Ok((c.cap_ambient >> args[1]) & 1),
                    PR_CAP_AMBIENT_RAISE => c.raise_ambient(args[1] as u32).map(|()| 0),
                    _ => {
                        c.cap_ambient &= !(1 << args[1]);
                        Ok(0)
                    }
                }
            }
            _ => Err(Errno::EINVAL),
        },
        // 只记录；0 表示恢复默认值
        PR_SET_TIMERSLACK => {
            let (_, default) = cx.task.timerslack.get();
            cx.task
                .timerslack
                .set((if arg == 0 { default } else { arg }, default));
            Ok(0)
        }
        PR_GET_TIMERSLACK => Ok(cx.task.timerslack.get().0),
        // 只有统计计时
        PR_SET_TIMING if arg == PR_TIMING_STATISTICAL => Ok(0),
        PR_GET_TIMING => Ok(PR_TIMING_STATISTICAL),
        PR_SET_THP_DISABLE => {
            unused(1)?;
            cx.t.group.borrow_mut().thp_disable = arg != 0;
            Ok(0)
        }
        PR_GET_THP_DISABLE => {
            unused(0)?;
            Ok(cx.t.group.borrow().thp_disable as u64)
        }
        // guest 看不到推测执行的缓解措施
        PR_GET_SPECULATION_CTRL => {
            unused(1)?;
            match arg {
                0..=2 => Ok(PR_SPEC_NOT_AFFECTED),
                _ => Err(Errno::ENODEV),
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

/// personality(2)：0xffffffff 只查询。
pub(super) fn personality(cx: &mut Cx<'_>, persona: u32) -> SysResult {
    let mut g = cx.t.group.borrow_mut();
    let old = g.personality;
    if persona != u32::MAX {
        g.personality = persona;
    }
    Ok(old as u64)
}

/// getcpu(2)：报告亲和性里的第一个 CPU（线程由宿主调度，不知道它实际在哪个 CPU 上）。
pub(super) fn getcpu(cx: &mut Cx<'_>, cpu: u64, node: u64) -> SysResult {
    let first = cx.task.affinity.get().trailing_zeros().min(63);
    if cpu != 0 {
        put(cx, cpu, first.to_le_bytes())?;
    }
    if node != 0 {
        put(cx, node, 0u32.to_le_bytes())?;
    }
    Ok(0)
}

pub(super) fn set_robust_list(cx: &mut Cx<'_>, head: u64, len: u64) -> SysResult {
    // struct robust_list_head 的大小
    if len != 24 {
        return Err(Errno::EINVAL);
    }
    cx.task.robust_list.set((head, len));
    Ok(0)
}

pub(super) fn get_robust_list(cx: &mut Cx<'_>, pid: i32, head: u64, len: u64) -> SysResult {
    let (h, l) = if pid == 0 {
        cx.task.robust_list.get()
    } else if pid == crate::kernel::INIT {
        // 内置的 init 属于 root，没有 robust 链表
        if cx.cred().ruid != 0 && !cx.cred().capable(cap::SYS_PTRACE) {
            return Err(Errno::EPERM);
        }
        (0, 0)
    } else {
        let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
        // 和 ptrace 一样：只能看同一个用户的线程
        let target = cx
            .kr()
            .groups
            .get(&task.tgid)
            .map(|g| g.borrow().cred.clone())
            .ok_or(Errno::ESRCH)?;
        let cred = cx.cred();
        if cred.ruid != target.ruid && !cred.ns_capable(&target.user_ns, cap::SYS_PTRACE) {
            return Err(Errno::EPERM);
        }
        task.robust_list.get()
    };
    put(cx, head, h.to_le_bytes())?;
    put(cx, len, l.to_le_bytes())?;
    Ok(0)
}

pub(super) fn sysinfo(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    // 与 Linux 的 do_sysinfo 一样有零头就进一秒
    let (secs, ns) = appine_hal::clock_gettime(appine_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    let uptime = secs + i64::from(ns != 0);
    let m = appine_hal::host_meminfo();
    let procs = cx.kr().tasks.len().min(u16::MAX as usize) as u16;
    // struct sysinfo（64 位）：uptime、loads[3]、totalram、freeram、sharedram、bufferram、
    // totalswap、freeswap、procs、totalhigh、freehigh、mem_unit；与 /proc/meminfo 一样照搬宿主的
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&uptime.to_le_bytes());
    for (i, load) in crate::loadavg::sysinfo_loads(cx.kr()).iter().enumerate() {
        b[8 + i * 8..16 + i * 8].copy_from_slice(&load.to_le_bytes());
    }
    for (i, v) in [
        m.total,
        m.free,
        m.shmem,
        m.buffers,
        m.swap_total,
        m.swap_free,
    ]
    .into_iter()
    .enumerate()
    {
        b[32 + i * 8..40 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    b[80..82].copy_from_slice(&procs.to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    put(cx, buf, b).map(|()| 0)
}

/// membarrier(2)。共用地址空间的其他线程被打断、离开 guest 代码，就相当于在它们身上
/// 执行了一次完整的内存屏障和上下文同步（V8 改写代码之后用 `SYNC_CORE` 的形式）。
/// 全局的命令也只作用于本进程：其他进程和它共享的只有共享映射，appine 的所有 guest
/// 线程都在一个宿主进程里，这里再加一个宿主的内存屏障。
pub(super) fn membarrier(cx: &mut Cx<'_>, cmd: i32, flags: u32) -> SysResult {
    const QUERY: i32 = 0;
    const GLOBAL: i32 = 1;
    const GLOBAL_EXPEDITED: i32 = 2;
    const REGISTER_GLOBAL_EXPEDITED: i32 = 4;
    const PRIVATE_EXPEDITED: i32 = 8;
    const REGISTER_PRIVATE_EXPEDITED: i32 = 16;
    const PRIVATE_EXPEDITED_SYNC_CORE: i32 = 32;
    const REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 64;
    const SUPPORTED: i32 = GLOBAL
        | GLOBAL_EXPEDITED
        | REGISTER_GLOBAL_EXPEDITED
        | PRIVATE_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED
        | PRIVATE_EXPEDITED_SYNC_CORE
        | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE;
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let registered = cx.mm().membarrier;
    match cmd {
        QUERY => Ok(SUPPORTED as u64),
        REGISTER_GLOBAL_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED
        | REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            cx.mm_mut().membarrier |= cmd;
            Ok(0)
        }
        GLOBAL | GLOBAL_EXPEDITED | PRIVATE_EXPEDITED | PRIVATE_EXPEDITED_SYNC_CORE => {
            // 私有的命令要求先登记，与 Linux 一样
            if (cmd == PRIVATE_EXPEDITED && registered & REGISTER_PRIVATE_EXPEDITED == 0)
                || (cmd == PRIVATE_EXPEDITED_SYNC_CORE
                    && registered & REGISTER_PRIVATE_EXPEDITED_SYNC_CORE == 0)
            {
                return Err(Errno::EPERM);
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            let sync = cx.task.sync.clone();
            cx.mm().quiesce(&sync);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn getrandom(cx: &mut Cx<'_>, buf: u64, len: usize, flags: u32) -> SysResult {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0
        || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE
    {
        return Err(Errno::EINVAL);
    }
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        appine_hal::fill_random(&mut chunk[..n]).map_err(Errno)?;
        if let Err(e) = cx.mm().write(
            UserSlice::new(buf.wrapping_add(done as u64), n),
            &chunk[..n],
        ) {
            if done == 0 {
                return Err(e);
            }
            break;
        }
        done += n;
    }
    Ok(done as u64)
}
