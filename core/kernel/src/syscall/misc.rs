//! futex、时间、资源限制等杂项 syscall。

use std::time::{Duration, Instant};

use super::{IO_CHUNK, SysResult, get, put, read_timespec, write_pair};
use crate::errno::Errno;
use crate::futex::Key;
use crate::itimer::ITimer;
use crate::kernel::{Cx, request_wake};
use crate::process::{RLIM_NLIMITS, RLIMIT_NOFILE, Signal};
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
const PR_SET_NAME: i32 = 15;
const PR_GET_NAME: i32 = 16;
const PR_GET_SECCOMP: i32 = 21;
const PR_CAPBSET_READ: i32 = 23;
const PR_GET_TIMERSLACK: i32 = 30;
const PR_SET_NO_NEW_PRIVS: i32 = 38;
const PR_GET_NO_NEW_PRIVS: i32 = 39;
/// 最大的 capability 编号（`CAP_CHECKPOINT_RESTORE`）。
const CAP_LAST_CAP: u64 = 40;

const GRND_NONBLOCK: u32 = 1;
const GRND_RANDOM: u32 = 2;
const GRND_INSECURE: u32 = 4;

/// `uname -r` 报告的内核版本，即 ABI 契约（设计文档 §7.5）。
const RELEASE: &[u8] = b"6.1.0-appine";

/// 把 `clock` 上的绝对时间换算成等待的截止时刻。
fn deadline_from_clock(clock: i32, abs: Duration) -> Result<Instant, Errno> {
    let (sec, nsec) = appine_hal::clock_gettime(clock).map_err(Errno)?;
    let now = Duration::new(sec.max(0) as u64, nsec as u32);
    Ok(Instant::now() + abs.saturating_sub(now))
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
            if u32::from_le_bytes(get(cx, uaddr)?) != val {
                return Err(Errno::EAGAIN);
            }
            let id = cx.k().futexes.enqueue(key, bitset);
            loop {
                if !cx.kr().futexes.is_queued(id) {
                    return Ok(0);
                }
                let err = if deadline.is_some_and(|d| Instant::now() >= d) {
                    Some(Errno::ETIMEDOUT)
                } else if cx.interrupted() {
                    Some(Errno::ERESTARTSYS)
                } else {
                    cx.wait(deadline, &mut []).err()
                };
                if let Some(e) = err {
                    cx.k().futexes.remove(id);
                    return Err(e);
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE { u32::MAX } else { val3 };
            if bitset == 0 {
                return Err(Errno::EINVAL);
            }
            let key = futex_key(cx, uaddr)?;
            Ok(cx.k().futexes.wake(key, val, bitset) as u64)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let key = futex_key(cx, uaddr)?;
            let key2 = futex_key(cx, uaddr2)?;
            if cmd == FUTEX_CMP_REQUEUE && u32::from_le_bytes(get(cx, uaddr)?) != val3 {
                return Err(Errno::EAGAIN);
            }
            // 第四个参数在 REQUEUE 里是要移动的个数，不是超时
            let n = cx.k().futexes.requeue(key, val, key2, timeout as u32);
            Ok(n as u64)
        }
        _ => Err(Errno::ENOSYS),
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
    let t = read_timespec(cx, req)?;
    match sleep_until(cx, Instant::now() + t) {
        Ok(()) => Ok(0),
        Err(left) => {
            if rem != 0 {
                write_pair(cx, rem, left.as_secs() as i64, left.subsec_nanos() as i64)?;
            }
            // 没有处理函数的信号打断时按原来的时长重新执行（还没有 restart_syscall）
            Err(Errno::ERESTARTNOHAND)
        }
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
fn write_itimerval(cx: &Cx<'_>, addr: u64, t: Option<ITimer>) -> Result<(), Errno> {
    let (interval, left) = match t {
        // 与 Linux 相同，还没到期的定时器至少报告 1 微秒
        Some(t) => (
            t.interval,
            t.deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_micros(1)),
        ),
        None => (Duration::ZERO, Duration::ZERO),
    };
    let mut b = [0u8; 32];
    for (o, d) in [(0, interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_micros() as i64).to_le_bytes());
    }
    put(cx, addr, b)
}

/// 只有 `ITIMER_REAL`；按 CPU 时间计时的两种返回 EINVAL。
fn check_itimer(which: i32) -> Result<(), Errno> {
    if which != ITIMER_REAL {
        crate::debug::log(format_args!("setitimer: timer {which} is not supported"));
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub(super) fn getitimer(cx: &mut Cx<'_>, which: i32, curr: u64) -> SysResult {
    check_itimer(which)?;
    let t = cx.t.group.borrow().itimer;
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
    let prev = cx.t.group.borrow().itimer;
    if old != 0 {
        write_itimerval(cx, old, prev)?;
    }
    cx.t.group.borrow_mut().itimer = (!value.is_zero()).then(|| ITimer {
        deadline: Instant::now() + value,
        interval,
    });
    // 让负责定时器的宿主主线程重新计算等待时间
    request_wake();
    Ok(0)
}

pub(super) fn clock_nanosleep(
    cx: &mut Cx<'_>,
    clock: i32,
    flags: u32,
    req: u64,
    rem: u64,
) -> SysResult {
    let t = read_timespec(cx, req)?;
    let deadline = if flags & TIMER_ABSTIME != 0 {
        deadline_from_clock(clock, t)?
    } else {
        appine_hal::clock_gettime(clock).map_err(Errno)?;
        Instant::now() + t
    };
    match sleep_until(cx, deadline) {
        Ok(()) => Ok(0),
        Err(left) => {
            if rem != 0 && flags & TIMER_ABSTIME == 0 {
                write_pair(cx, rem, left.as_secs() as i64, left.subsec_nanos() as i64)?;
            }
            Err(Errno::ERESTARTNOHAND)
        }
    }
}

pub(super) fn clock_gettime(cx: &mut Cx<'_>, clock: i32, tp: u64) -> SysResult {
    let (sec, nsec) = appine_hal::clock_gettime(clock).map_err(Errno)?;
    write_pair(cx, tp, sec, nsec).map(|()| 0)
}

pub(super) fn clock_getres(cx: &mut Cx<'_>, clock: i32, res: u64) -> SysResult {
    let (sec, nsec) = appine_hal::clock_getres(clock).map_err(Errno)?;
    if res != 0 {
        write_pair(cx, res, sec, nsec)?;
    }
    Ok(0)
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

pub(super) fn sched_getaffinity(cx: &mut Cx<'_>, pid: i32, len: usize, mask: u64) -> SysResult {
    if pid != 0 && !cx.kr().tasks.contains_key(&pid) {
        return Err(Errno::ESRCH);
    }
    // 内核的 cpumask 按 unsigned long 对齐；guest 给的缓冲区至少要能放下它
    if len < 8 || !len.is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    let cpus = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(64);
    let bits = if cpus == 64 {
        u64::MAX
    } else {
        (1u64 << cpus) - 1
    };
    put(cx, mask, bits.to_le_bytes())?;
    Ok(8)
}

pub(super) fn times(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let ticks = crate::loader::CLOCK_TICKS;
    // struct tms：CPU 时间分不出用户态和内核态，全部算作用户态
    if buf != 0 {
        let to_ticks = |ns: u64| ns / (1_000_000_000 / ticks);
        let g = cx.t.group.borrow();
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&to_ticks(g.cpu_ns(cx.kr())).to_le_bytes());
        b[16..24].copy_from_slice(&to_ticks(g.child_cpu_ns).to_le_bytes());
        drop(g);
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
    let cpu = match who {
        RUSAGE_SELF => cx.t.group.borrow().cpu_ns(cx.kr()),
        RUSAGE_CHILDREN => cx.t.group.borrow().child_cpu_ns,
        RUSAGE_THREAD => cx.task.cpu_ns(),
        _ => return Err(Errno::EINVAL),
    };
    put(cx, usage, crate::process::encode_rusage(cpu)).map(|()| 0)
}

pub(super) fn uname(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let fields: [&[u8]; 6] = [
        b"Linux",
        b"localhost",
        RELEASE,
        b"#1 SMP PREEMPT appine",
        b"aarch64",
        b"(none)",
    ];
    let mut out = [0u8; 6 * 65];
    for (i, f) in fields.iter().enumerate() {
        out[i * 65..i * 65 + f.len()].copy_from_slice(f);
    }
    cx.mm()
        .write(UserSlice::new(buf, out.len()), &out)
        .map(|()| 0)
}

pub(super) fn prlimit(cx: &mut Cx<'_>, pid: i32, resource: u32, new: u64, old: u64) -> SysResult {
    let group = if pid == 0 || pid == cx.tgid() {
        cx.t.group.clone()
    } else {
        cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)?
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
        // root 可以提高硬限制，但 fd 表的容量是固定的
        if resource == RLIMIT_NOFILE && max > crate::fd::MAX_FDS as u64 {
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
    }
    Ok(0)
}

pub(super) fn umask(cx: &mut Cx<'_>, mask: u32) -> SysResult {
    let mut fs = cx.t.fs.borrow_mut();
    let old = fs.umask;
    fs.umask = mask & 0o777;
    Ok(old as u64)
}

pub(super) fn prctl(cx: &mut Cx<'_>, option: i32, arg: u64) -> SysResult {
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
            cx.t.group.borrow_mut().pdeathsig = arg as i32;
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let sig = cx.t.group.borrow().pdeathsig;
            put(cx, arg, sig.to_le_bytes()).map(|()| 0)
        }
        PR_GET_DUMPABLE => Ok(cx.t.group.borrow().dumpable as u64),
        PR_SET_DUMPABLE if arg <= 1 => {
            cx.t.group.borrow_mut().dumpable = arg == 1;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS if arg == 1 => {
            cx.t.group.borrow_mut().no_new_privs = true;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => Ok(cx.t.group.borrow().no_new_privs as u64),
        // 没有 seccomp 过滤
        PR_GET_SECCOMP => Ok(0),
        // root 拥有全部 capability
        PR_CAPBSET_READ if arg <= CAP_LAST_CAP => Ok(1),
        // Linux 的默认值 50 微秒
        PR_GET_TIMERSLACK => Ok(50_000),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sysinfo(cx: &mut Cx<'_>, buf: u64) -> SysResult {
    let (uptime, _) = appine_hal::clock_gettime(appine_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    let (total, free) = appine_hal::memory_info();
    let procs = cx.kr().tasks.len().min(u16::MAX as usize) as u16;
    // struct sysinfo（64 位）：uptime、loads[3]、totalram…freeswap、procs、totalhigh、freehigh、mem_unit
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&uptime.to_le_bytes());
    b[32..40].copy_from_slice(&total.to_le_bytes());
    b[40..48].copy_from_slice(&free.to_le_bytes());
    b[80..82].copy_from_slice(&procs.to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    put(cx, buf, b).map(|()| 0)
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
