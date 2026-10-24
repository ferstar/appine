//! timerfd：timerfd_create、timerfd_settime、timerfd_gettime，以及对它的 read。
//!
//! 到期时刻按宿主的单调时钟记录，读取和 poll 时把已经到期的次数算出来；等待的线程把
//! 最早的到期时刻当作超时（[`super::poll`]）。设置绝对时间时按定时器的时钟换算。
//! `TFD_TIMER_CANCEL_ON_SET` 接受但不起作用：guest 改不了宿主的时间。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::misc::deadline_from_clock;
use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, TimerFd, oflags};
use crate::kernel::Cx;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;

const TFD_TIMER_ABSTIME: u32 = 1;
const TFD_TIMER_CANCEL_ON_SET: u32 = 2;

pub(super) fn timerfd_create(cx: &mut Cx<'_>, clock: i32, flags: u32) -> SysResult {
    if !matches!(
        clock,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_BOOTTIME
            | CLOCK_REALTIME_ALARM
            | CLOCK_BOOTTIME_ALARM
    ) {
        return Err(Errno::EINVAL);
    }
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let t = TimerFd {
        clock,
        deadline: None,
        interval: Duration::ZERO,
        expirations: 0,
    };
    let file = OpenFile::new(
        FileKind::TimerFd(Rc::new(RefCell::new(t))),
        oflags::O_RDONLY | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

fn timer_of(cx: &Cx<'_>, fd: i32) -> Result<Rc<RefCell<TimerFd>>, Errno> {
    match &cx.fds().get(fd)?.borrow().kind {
        FileKind::TimerFd(t) => Ok(t.clone()),
        _ => Err(Errno::EINVAL),
    }
}

/// 读 `struct timespec`：秒为负或者纳秒超出范围时返回 EINVAL。
fn timespec(b: &[u8]) -> Result<Duration, Errno> {
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(Duration::new(sec as u64, nsec as u32))
}

/// `struct itimerspec`：间隔和离下一次到期还有多久（没有启动时都是 0）。
fn current(t: &mut TimerFd) -> [u8; 32] {
    let now = Instant::now();
    t.update(now);
    let left = t
        .deadline
        .map_or(Duration::ZERO, |d| d.saturating_duration_since(now));
    let mut b = [0u8; 32];
    for (o, d) in [(0, t.interval), (16, left)] {
        b[o..o + 8].copy_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&(d.subsec_nanos() as i64).to_le_bytes());
    }
    b
}

pub(super) fn timerfd_settime(
    cx: &mut Cx<'_>,
    fd: i32,
    flags: u32,
    new: u64,
    old: u64,
) -> SysResult {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(Errno::EINVAL);
    }
    let timer = timer_of(cx, fd)?;
    let spec: [u8; 32] = get(cx, new)?;
    let interval = timespec(&spec[..16])?;
    let value = timespec(&spec[16..])?;
    let mut t = timer.borrow_mut();
    if old != 0 {
        put(cx, old, current(&mut t))?;
    }
    t.interval = interval;
    t.expirations = 0;
    t.deadline = if value.is_zero() {
        None
    } else if flags & TFD_TIMER_ABSTIME != 0 {
        // 闹钟类时钟和对应的普通时钟读数相同
        let clock = match t.clock {
            CLOCK_REALTIME_ALARM => CLOCK_REALTIME,
            CLOCK_BOOTTIME_ALARM => CLOCK_BOOTTIME,
            c => c,
        };
        Some(deadline_from_clock(clock, value)?)
    } else {
        Some(Instant::now() + value)
    };
    Ok(0)
}

pub(super) fn timerfd_gettime(cx: &mut Cx<'_>, fd: i32, cur: u64) -> SysResult {
    let timer = timer_of(cx, fd)?;
    let b = current(&mut timer.borrow_mut());
    put(cx, cur, b).map(|()| 0)
}

/// 读出到期的次数（8 字节），还没到期时等待。
pub(super) fn read(
    cx: &mut Cx<'_>,
    timer: &Rc<RefCell<TimerFd>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if buf.len() < 8 {
        return Err(Errno::EINVAL);
    }
    loop {
        let deadline = {
            let mut t = timer.borrow_mut();
            t.update(Instant::now());
            if t.expirations > 0 {
                buf[..8].copy_from_slice(&std::mem::take(&mut t.expirations).to_le_bytes());
                return Ok(8);
            }
            t.deadline
        };
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(deadline, &mut [])?;
    }
}
