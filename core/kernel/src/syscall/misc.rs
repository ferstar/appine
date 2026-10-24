//! 进程信息、时间、信号等杂项 syscall。

use std::time::Duration;

use super::{IO_CHUNK, SysResult, get, put};
use crate::errno::Errno;
use crate::process::{Process, SigAction, Signal};
use crate::uaccess::UserSlice;

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
/// 去掉 `FUTEX_PRIVATE_FLAG`、`FUTEX_CLOCK_REALTIME` 之后的命令位。
const FUTEX_CMD_MASK: u32 = 0x7f;

const SIG_BLOCK: u32 = 0;
const SIG_UNBLOCK: u32 = 1;
const SIG_SETMASK: u32 = 2;
/// guest 的 sigset_t 在内核 ABI 里的大小（64 个信号）。
const SIGSET_SIZE: usize = 8;

const TIMER_ABSTIME: u32 = 1;
const CLOCK_REALTIME: i32 = 0;

const PR_SET_NAME: i32 = 15;
const PR_GET_NAME: i32 = 16;

const GRND_NONBLOCK: u32 = 1;
const GRND_RANDOM: u32 = 2;
const GRND_INSECURE: u32 = 4;

/// `uname -r` 报告的内核版本，即 ABI 契约（设计文档 §7.5）。
const RELEASE: &[u8] = b"6.1.0-appine";

pub(super) fn set_tid_address(p: &mut Process, addr: u64) -> SysResult {
    p.clear_child_tid = addr;
    Ok(p.pid as u64)
}

pub(super) fn futex(p: &mut Process, uaddr: u64, op: u32, val: u32) -> SysResult {
    match op & FUTEX_CMD_MASK {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            if !uaddr.is_multiple_of(4) {
                return Err(Errno::EINVAL);
            }
            if u32::from_le_bytes(get(p, uaddr)?) != val {
                return Err(Errno::EAGAIN);
            }
            // 单线程进程里没有人能唤醒它：按虚假唤醒返回，由调用方重新检查条件
            crate::debug::log(format_args!(
                "futex wait at {uaddr:#x} in a single-threaded process"
            ));
            Ok(0)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => Ok(0),
        _ => Err(Errno::ENOSYS),
    }
}

/// 读取 `struct timespec`。
fn read_timespec(p: &Process, addr: u64) -> Result<(i64, i64), Errno> {
    let b: [u8; 16] = get(p, addr)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(Errno::EINVAL);
    }
    Ok((sec, nsec))
}

fn write_pair(p: &Process, addr: u64, a: i64, b: i64) -> Result<(), Errno> {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    put(p, addr, buf)
}

pub(super) fn nanosleep(p: &mut Process, req: u64, _rem: u64) -> SysResult {
    let (sec, nsec) = read_timespec(p, req)?;
    std::thread::sleep(Duration::new(sec as u64, nsec as u32));
    Ok(0)
}

pub(super) fn clock_nanosleep(
    p: &mut Process,
    clock: i32,
    flags: u32,
    req: u64,
    _rem: u64,
) -> SysResult {
    let (sec, nsec) = read_timespec(p, req)?;
    let mut target = Duration::new(sec as u64, nsec as u32);
    if flags & TIMER_ABSTIME != 0 {
        let (now_sec, now_nsec) = appine_hal::clock_gettime(clock).map_err(Errno)?;
        let now = Duration::new(now_sec.max(0) as u64, now_nsec as u32);
        target = target.saturating_sub(now);
    } else {
        appine_hal::clock_gettime(clock).map_err(Errno)?;
    }
    std::thread::sleep(target);
    Ok(0)
}

pub(super) fn clock_gettime(p: &mut Process, clock: i32, tp: u64) -> SysResult {
    let (sec, nsec) = appine_hal::clock_gettime(clock).map_err(Errno)?;
    write_pair(p, tp, sec, nsec).map(|()| 0)
}

pub(super) fn clock_getres(p: &mut Process, clock: i32, res: u64) -> SysResult {
    let (sec, nsec) = appine_hal::clock_getres(clock).map_err(Errno)?;
    if res != 0 {
        write_pair(p, res, sec, nsec)?;
    }
    Ok(0)
}

pub(super) fn gettimeofday(p: &mut Process, tv: u64, tz: u64) -> SysResult {
    if tv != 0 {
        let (sec, nsec) = appine_hal::clock_gettime(CLOCK_REALTIME).map_err(Errno)?;
        write_pair(p, tv, sec, nsec / 1000)?;
    }
    if tz != 0 {
        put(p, tz, [0u8; 8])?;
    }
    Ok(0)
}

/// 把信号挂到本进程上；是否终止进程由 [`Process::next_signal`] 在 syscall 返回前决定。
fn send_to_self(p: &mut Process, sig: i32) -> SysResult {
    if sig == 0 {
        return Ok(0);
    }
    let sig = Signal::new(sig).ok_or(Errno::EINVAL)?;
    p.pending |= sig.bit();
    Ok(0)
}

pub(super) fn kill(p: &mut Process, pid: i32, sig: i32) -> SysResult {
    // 0 是本进程组，-1 是所有进程：单进程里都只有自己
    if pid != p.pid && pid != 0 && pid != -1 && pid != -p.pid {
        return Err(Errno::ESRCH);
    }
    send_to_self(p, sig)
}

pub(super) fn tkill(p: &mut Process, tid: i32, sig: i32) -> SysResult {
    if tid <= 0 {
        return Err(Errno::EINVAL);
    }
    if tid != p.pid {
        return Err(Errno::ESRCH);
    }
    send_to_self(p, sig)
}

pub(super) fn tgkill(p: &mut Process, tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tgid <= 0 || tid <= 0 {
        return Err(Errno::EINVAL);
    }
    if tgid != p.pid {
        return Err(Errno::ESRCH);
    }
    tkill(p, tid, sig)
}

pub(super) fn rt_sigaction(
    p: &mut Process,
    sig: i32,
    act: u64,
    oact: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let sig = Signal::new(sig).ok_or(Errno::EINVAL)?;
    if act != 0 && (sig == Signal::KILL || sig == Signal::STOP) {
        return Err(Errno::EINVAL);
    }
    let slot = sig.number() as usize - 1;
    let new = if act != 0 {
        let b: [u8; 32] = get(p, act)?;
        let word = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        Some(SigAction {
            handler: word(0),
            flags: word(1),
            restorer: word(2),
            mask: word(3),
        })
    } else {
        None
    };
    if oact != 0 {
        let old = p.sigactions[slot];
        let mut b = [0u8; 32];
        for (i, w) in [old.handler, old.flags, old.restorer, old.mask]
            .into_iter()
            .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        put(p, oact, b)?;
    }
    if let Some(new) = new {
        p.sigactions[slot] = new;
        // 改成忽略之后，已经挂起的同一信号被丢弃
        if p.signal_ignored(sig) {
            p.pending &= !sig.bit();
        }
    }
    Ok(0)
}

pub(super) fn rt_sigprocmask(
    p: &mut Process,
    how: u32,
    set: u64,
    oset: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let new = if set != 0 {
        let s = u64::from_le_bytes(get(p, set)?);
        Some(match how {
            SIG_BLOCK => p.sigmask | s,
            SIG_UNBLOCK => p.sigmask & !s,
            SIG_SETMASK => s,
            _ => return Err(Errno::EINVAL),
        })
    } else {
        None
    };
    if oset != 0 {
        put(p, oset, p.sigmask.to_le_bytes())?;
    }
    if let Some(mask) = new {
        p.sigmask = mask & !(Signal::KILL.bit() | Signal::STOP.bit());
    }
    Ok(0)
}

pub(super) fn uname(p: &mut Process, buf: u64) -> SysResult {
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
    p.mm.write(UserSlice::new(buf, out.len()), &out).map(|()| 0)
}

pub(super) fn prlimit(p: &mut Process, pid: i32, resource: u32, new: u64, old: u64) -> SysResult {
    if pid != 0 && pid != p.pid {
        return Err(Errno::ESRCH);
    }
    let slot = p
        .rlimits
        .get(resource as usize)
        .copied()
        .ok_or(Errno::EINVAL)?;
    let new = if new != 0 {
        let b: [u8; 16] = get(p, new)?;
        let cur = u64::from_le_bytes(b[..8].try_into().unwrap());
        let max = u64::from_le_bytes(b[8..].try_into().unwrap());
        if cur > max {
            return Err(Errno::EINVAL);
        }
        // root 可以提高硬限制，但 fd 表的容量是固定的
        if resource == crate::process::RLIMIT_NOFILE && max > crate::fd::MAX_FDS as u64 {
            return Err(Errno::EPERM);
        }
        Some((cur, max))
    } else {
        None
    };
    if old != 0 {
        write_pair(p, old, slot.0 as i64, slot.1 as i64)?;
    }
    if let Some(v) = new {
        p.rlimits[resource as usize] = v;
    }
    Ok(0)
}

pub(super) fn umask(p: &mut Process, mask: u32) -> SysResult {
    let old = p.umask;
    p.umask = mask & 0o777;
    Ok(old as u64)
}

pub(super) fn prctl(p: &mut Process, option: i32, arg: u64) -> SysResult {
    match option {
        PR_SET_NAME => {
            // 名字可以不以 NUL 结尾，最多取 15 字节
            let raw = match p.mm.read_cstr(arg, 15) {
                Ok(name) => name,
                Err(e) if e == Errno::ENAMETOOLONG => get::<15>(p, arg)?.to_vec(),
                Err(e) => return Err(e),
            };
            p.comm = [0; 16];
            p.comm[..raw.len()].copy_from_slice(&raw);
            Ok(0)
        }
        PR_GET_NAME => put(p, arg, p.comm).map(|()| 0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sysinfo(p: &mut Process, buf: u64) -> SysResult {
    let (uptime, _) = appine_hal::clock_gettime(appine_hal::CLOCK_BOOTTIME).map_err(Errno)?;
    let (total, free) = appine_hal::memory_info();
    // struct sysinfo（64 位）：uptime、loads[3]、totalram…freeswap、procs、totalhigh、freehigh、mem_unit
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&uptime.to_le_bytes());
    b[32..40].copy_from_slice(&total.to_le_bytes());
    b[40..48].copy_from_slice(&free.to_le_bytes());
    b[80..82].copy_from_slice(&1u16.to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    put(p, buf, b).map(|()| 0)
}

pub(super) fn getrandom(p: &mut Process, buf: u64, len: usize, flags: u32) -> SysResult {
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
        if let Err(e) = p.mm.write(
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
