//! POSIX 定时器：timer_create、timer_settime、timer_gettime、timer_getoverrun、
//! timer_delete（计时和到期见 [`crate::itimer`]）。
//!
//! 定时器属于进程，id 从 0 开始取最小的空闲值；fork 不继承，execve 时删除。musl 的
//! `SIGEV_THREAD` 在用户态实现，内核看到的是 `SIGEV_THREAD_ID`。

use std::time::Duration;

use super::misc::deadline_from_clock;
use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::itimer::{Clock, PosixTimer};
use crate::kernel::Cx;
use crate::process::Signal;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const CLOCK_TAI: i32 = 11;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;

const TIMER_ABSTIME: u32 = 1;
/// 一个进程最多的定时器个数。
const MAX_TIMERS: usize = 4096;
/// 唤醒闹钟的时钟要求的能力（`CAP_WAKE_ALARM`）。
const CAP_WAKE_ALARM: u32 = 35;

/// 时钟 id 对应的计时方式。
fn clock_of(cx: &Cx<'_>, clockid: i32) -> Result<Clock, Errno> {
    Ok(match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_TAI => Clock::Wall(clockid),
        // 唤醒闹钟：宿主的 App 不能唤醒设备，按普通的时钟计时
        CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM => {
            if !cx.cred().capable(CAP_WAKE_ALARM) {
                return Err(Errno::EPERM);
            }
            Clock::Wall(if clockid == CLOCK_REALTIME_ALARM {
                CLOCK_REALTIME
            } else {
                CLOCK_BOOTTIME
            })
        }
        CLOCK_PROCESS_CPUTIME_ID => Clock::Process(cx.tgid()),
        CLOCK_THREAD_CPUTIME_ID => Clock::Thread(cx.tid()),
        // clock_getcpuclockid 编码的时钟：(~pid << 3) | 类型，第 2 位表示线程
        c if c < 0 && c & 3 != 3 => {
            let thread = c & 4 != 0;
            let pid = match !(c >> 3) {
                0 if thread => cx.tid(),
                0 => cx.tgid(),
                p => p,
            };
            let k = cx.kr();
            if thread {
                k.tasks.get(&pid).ok_or(Errno::EINVAL)?;
                Clock::Thread(pid)
            } else {
                k.groups.get(&pid).ok_or(Errno::EINVAL)?;
                Clock::Process(pid)
            }
        }
        // CLOCK_MONOTONIC_RAW 和粗粒度的时钟不能用来做定时器
        4..=6 => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::EINVAL),
    })
}

pub(super) fn timer_create(cx: &mut Cx<'_>, clockid: i32, sevp: u64, idp: u64) -> SysResult {
    let clock = clock_of(cx, clockid)?;
    // 没有 sigevent 时按 SIGEV_SIGNAL、SIGALRM，sigev_value 是定时器 id
    let (value, signo, notify, tid) = if sevp == 0 {
        (None, Signal::ALRM.number(), SIGEV_SIGNAL, 0)
    } else {
        let b: [u8; 64] = get(cx, sevp)?;
        let word = |off: usize| i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        (
            Some(u64::from_le_bytes(b[..8].try_into().unwrap())),
            word(8),
            word(12),
            word(16),
        )
    };
    let signal = match notify {
        SIGEV_NONE => None,
        SIGEV_SIGNAL | SIGEV_THREAD | SIGEV_THREAD_ID => {
            let sig = Signal::new(signo).ok_or(Errno::EINVAL)?;
            let target = if notify == SIGEV_THREAD_ID {
                // 目标线程必须在同一个进程里
                let tid = cx.pid_in(tid);
                match cx.kr().tasks.get(&tid) {
                    Some(t) if t.tgid == cx.tgid() => Some(tid),
                    _ => return Err(Errno::EINVAL),
                }
            } else {
                None
            };
            Some((sig, target))
        }
        _ => return Err(Errno::EINVAL),
    };
    let mut g = cx.t.group.borrow_mut();
    if g.posix_timers.len() >= MAX_TIMERS {
        return Err(Errno::EAGAIN);
    }
    let id = (0..).find(|i| !g.posix_timers.contains_key(i)).unwrap();
    g.posix_timers.insert(
        id,
        PosixTimer {
            clock,
            signal,
            value: value.unwrap_or(id as u64),
            deadline: None,
            interval: Duration::ZERO,
            overrun: 0,
        },
    );
    drop(g);
    if let Err(e) = put(cx, idp, id.to_le_bytes()) {
        cx.t.group.borrow_mut().posix_timers.remove(&id);
        return Err(e);
    }
    Ok(0)
}

/// `struct itimerspec`：间隔和离下一次到期还有多久（没有启动时都是 0）。
fn encode_itimerspec(t: &PosixTimer, left: Option<Duration>) -> [u8; 32] {
    let left = left.unwrap_or(Duration::ZERO);
    // 还没到期的定时器至少报告 1 纳秒，与 Linux 相同
    let left = if t.deadline.is_some() {
        left.max(Duration::from_nanos(1))
    } else {
        left
    };
    let mut b = [0u8; 32];
    for (o, d) in [(0, t.interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_nanos() as i64).to_le_bytes());
    }
    b
}

fn timer(cx: &Cx<'_>, id: i32) -> Result<PosixTimer, Errno> {
    cx.t.group
        .borrow()
        .posix_timers
        .get(&id)
        .cloned()
        .ok_or(Errno::EINVAL)
}

pub(super) fn timer_settime(cx: &mut Cx<'_>, id: i32, flags: u32, new: u64, old: u64) -> SysResult {
    if new == 0 {
        return Err(Errno::EINVAL);
    }
    let mut t = timer(cx, id)?;
    let b: [u8; 32] = get(cx, new)?;
    let ts = |off: usize| -> Result<Duration, Errno> {
        let sec = i64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        let nsec = i64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return Err(Errno::EINVAL);
        }
        Ok(Duration::new(sec as u64, nsec as u32))
    };
    let (interval, value) = (ts(0)?, ts(16)?);
    if old != 0 {
        let left = t.remaining(cx.kr());
        put(cx, old, encode_itimerspec(&t, left))?;
    }
    let abs = flags & TIMER_ABSTIME != 0;
    if value.is_zero() {
        t.deadline = None;
    } else {
        let at = match (t.clock, abs) {
            (Clock::Wall(clock), true) => Some(deadline_from_clock(clock, value)?),
            _ => None,
        };
        t.arm(cx.kr(), value, at, abs);
    }
    t.interval = interval;
    t.overrun = 0;
    cx.t.group.borrow_mut().posix_timers.insert(id, t);
    // 让定时器线程重新计算等待时间（只叫醒它）
    crate::kernel::signal_later(&cx.shared.timers);
    Ok(0)
}

pub(super) fn timer_gettime(cx: &mut Cx<'_>, id: i32, cur: u64) -> SysResult {
    let t = timer(cx, id)?;
    let left = t.remaining(cx.kr());
    put(cx, cur, encode_itimerspec(&t, left)).map(|()| 0)
}

pub(super) fn timer_getoverrun(cx: &mut Cx<'_>, id: i32) -> SysResult {
    Ok(timer(cx, id)?.overrun.min(i32::MAX as u32) as u64)
}

pub(super) fn timer_delete(cx: &mut Cx<'_>, id: i32) -> SysResult {
    cx.t.group
        .borrow_mut()
        .posix_timers
        .remove(&id)
        .map(|_| 0)
        .ok_or(Errno::EINVAL)
}
