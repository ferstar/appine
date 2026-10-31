//! 间隔定时器（setitimer、getitimer；musl 的 alarm 也经由 setitimer）和 POSIX 定时器
//! （timer_create 系列）。
//!
//! 到期时刻记在线程组里，由 Machine 的定时器线程负责（[`expire`]）：等到最早的到期
//! 时刻，给到期的进程发信号，有间隔的重新计时。设置定时器时请求唤醒，定时器线程随之
//! 重新计算要等多久。
//!
//! - `ITIMER_REAL` 按宿主的单调时钟计时，到期发 SIGALRM。
//! - `ITIMER_VIRTUAL`、`ITIMER_PROF` 按进程用掉的 CPU 时间计时（宿主的线程 CPU 时钟分不出
//!   用户态和内核态，两者都按总的 CPU 时间算），到期发 SIGVTALRM、SIGPROF。有这种定时器
//!   时定时器线程每 10 毫秒检查一次，所以精度是 10 毫秒。
//! - `RLIMIT_CPU`：CPU 时间超过软限制时发 SIGXCPU，并把软限制推后一秒（与 Linux 相同，
//!   之后每秒一次），超过硬限制时发 SIGKILL。和 CPU 时间定时器一样每 10 毫秒检查。
//! - POSIX 定时器按墙上时钟（宿主的单调时钟）或者进程、线程的 CPU 时间计时，到期发
//!   `SI_TIMER` 信号（`SIGEV_THREAD_ID` 时发给指定线程）。上一个信号还没被取走时
//!   不再发，只把超限次数（overrun）记在那个挂起的信号里。

use std::time::{Duration, Instant};

use crate::bug::Trap;
use crate::cell::RefCell;
use crate::kernel::{Kernel, Tid};
use crate::process::{RLIMIT_CPU, Signal, ThreadGroup};
use crate::signal::{self, InfoFields, Pending, SigInfo, code};

/// 一个进程的 `ITIMER_REAL`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ITimer {
    /// 下一次到期的时刻。
    pub deadline: Instant,
    /// 到期后重新计时的间隔，0 表示只触发一次。
    pub interval: Duration,
}

impl ITimer {
    /// 在 `now` 到期之后的下一个状态：错过的周期不补发（与 Linux 相同，只算一次）。
    fn next(self, now: Instant) -> Option<ITimer> {
        if self.interval.is_zero() {
            return None;
        }
        let late = now.saturating_duration_since(self.deadline).as_nanos();
        let periods = (late / self.interval.as_nanos()) as u32 + 1;
        Some(ITimer {
            deadline: self.deadline + self.interval * periods,
            interval: self.interval,
        })
    }
}

/// 按 CPU 时间计时的定时器：到期时刻和间隔都是进程用掉的 CPU 纳秒数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuTimer {
    pub deadline: u64,
    pub interval: u64,
}

impl CpuTimer {
    /// 在进程用掉 `now` 纳秒时到期之后的下一个状态，错过的周期不补发。
    fn next(self, now: u64) -> Option<CpuTimer> {
        if self.interval == 0 {
            return None;
        }
        let periods = (now - self.deadline) / self.interval + 1;
        Some(CpuTimer {
            deadline: self.deadline + self.interval * periods,
            interval: self.interval,
        })
    }
}

/// CPU 时间定时器的检查间隔。
const CPU_TICK: Duration = Duration::from_millis(10);

/// POSIX 定时器按什么计时。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    /// 墙上时钟（`CLOCK_REALTIME`、`MONOTONIC`、`BOOTTIME`、`TAI`），按宿主的单调时钟
    /// 计时，到期时刻是 [`Deadline::At`]。带着宿主上对应的时钟 id：设置绝对时间时按它
    /// 换算。
    Wall(i32),
    /// 进程用掉的 CPU 时间，到期时刻是 [`Deadline::Cpu`]。
    Process(Tid),
    /// 线程用掉的 CPU 时间。
    Thread(Tid),
}

/// POSIX 定时器的下一次到期。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deadline {
    At(Instant),
    /// CPU 纳秒数。
    Cpu(u64),
}

/// 一个 POSIX 定时器（timer_create）。
#[derive(Clone, Debug)]
pub(crate) struct PosixTimer {
    pub clock: Clock,
    /// 到期时发的信号，以及 `SIGEV_THREAD_ID` 的目标线程；None 是 `SIGEV_NONE`。
    pub signal: Option<(Signal, Option<Tid>)>,
    /// `sigev_value`，放进 siginfo。
    pub value: u64,
    pub deadline: Option<Deadline>,
    pub interval: Duration,
    /// 最近送达的一个信号累计的超限次数（timer_getoverrun，取出信号时更新）。
    pub overrun: u32,
}

impl PosixTimer {
    /// 时钟现在的值：墙上时钟返回 None（直接和 `Instant::now()` 比）。
    fn cpu_now(&self, k: &Kernel) -> Option<u64> {
        match self.clock {
            Clock::Wall(_) => None,
            Clock::Process(tgid) => Some(k.groups.get(&tgid).map_or(0, |g| g.borrow().cpu_ns(k))),
            Clock::Thread(tid) => Some(k.tasks.get(&tid).map_or(0, |t| t.cpu_ns())),
        }
    }

    /// 离下一次到期还有多久；没有启动时是 None。
    pub fn remaining(&self, k: &Kernel) -> Option<Duration> {
        match self.deadline? {
            Deadline::At(d) => Some(d.saturating_duration_since(Instant::now())),
            Deadline::Cpu(d) => Some(Duration::from_nanos(
                d.saturating_sub(self.cpu_now(k).unwrap_or(0)),
            )),
        }
    }

    /// 从现在起 `value` 之后到期（`abs` 时 `value` 是 CPU 时钟的绝对值；墙上时钟的
    /// 绝对时间由调用方换算成 `at`）。
    pub fn arm(&mut self, k: &Kernel, value: Duration, at: Option<Instant>, abs: bool) {
        self.deadline = match self.cpu_now(k) {
            None => Some(Deadline::At(at.unwrap_or_else(|| Instant::now() + value))),
            Some(now) => {
                let v = value.as_nanos() as u64;
                Some(Deadline::Cpu(if abs { v } else { now + v }))
            }
        };
    }

    /// 到期了就返回错过的周期数（不含这一次，与 Linux 一样封顶在 INT_MAX），并重新计时。
    fn fire(&mut self, k: &Kernel, now: Instant) -> Option<u32> {
        let (late, deadline) = match (self.deadline?, self.cpu_now(k)) {
            (Deadline::At(d), _) if d <= now => (now.duration_since(d), Deadline::At(d)),
            (Deadline::Cpu(d), Some(cpu)) if d <= cpu => {
                (Duration::from_nanos(cpu - d), Deadline::Cpu(d))
            }
            _ => return None,
        };
        if self.interval.is_zero() {
            self.deadline = None;
            return Some(0);
        }
        // 间隔很短、起点在很久以前时周期数超出 32 位：按真实的周期数推进
        let periods = late.as_nanos() / self.interval.as_nanos();
        let step_ns = (self.interval.as_nanos() * (periods + 1)).min(u64::MAX as u128) as u64;
        self.deadline = Some(match deadline {
            Deadline::At(d) => Deadline::At(d + Duration::from_nanos(step_ns)),
            Deadline::Cpu(d) => Deadline::Cpu(d.saturating_add(step_ns)),
        });
        Some(periods.min(i32::MAX as u128) as u32)
    }
}

/// 给定时器到期的进程发信号，返回下一次要检查的时刻。
///
/// 处理一个进程时撞上内核层的 bug（[`crate::bug`]）：结束这个进程、清掉它的定时器，
/// 其余的进程照常处理。
pub(crate) fn expire(k: &Kernel) -> Option<Instant> {
    let now = Instant::now();
    let mut next: Option<Instant> = None;
    for (&tgid, group) in &k.groups {
        match crate::bug::catch(|| expire_group(k, tgid, group, now)) {
            Some(Some(n)) => next = Some(next.map_or(n, |m| m.min(n))),
            Some(None) => {}
            None => {
                disarm(&mut group.borrow_mut());
                crate::bug::kill_process(k, tgid, format_args!("kernel panic in its timers"));
            }
        }
    }
    next
}

/// 停掉进程的所有定时器。
fn disarm(g: &mut ThreadGroup) {
    g.itimer = None;
    g.cpu_itimers = [None; 2];
    g.posix_timers.clear();
}

/// 处理进程 `tgid` 的定时器，返回它下一次要检查的时刻。
fn expire_group(
    k: &Kernel,
    tgid: Tid,
    group: &RefCell<ThreadGroup>,
    now: Instant,
) -> Option<Instant> {
    crate::bug::trap(Trap::Timers, tgid);
    let mut next: Option<Instant> = None;
    let mut fired = Vec::new();
    let mut timer_fired = Vec::new();
    if group.borrow().threads.is_empty() {
        disarm(&mut group.borrow_mut());
        return None;
    }
    let cpu_limit = group.borrow().rlimits[RLIMIT_CPU as usize];
    let cpu_armed =
        group.borrow().cpu_itimers.iter().any(Option::is_some) || cpu_limit.0 != u64::MAX;
    let cpu = if cpu_armed {
        group.borrow().cpu_ns(k)
    } else {
        0
    };
    let mut g = group.borrow_mut();
    if let Some(t) = g.itimer {
        if t.deadline <= now {
            fired.push(Signal::ALRM);
            g.itimer = t.next(now);
        }
        if let Some(t) = g.itimer {
            next = Some(next.map_or(t.deadline, |n| n.min(t.deadline)));
        }
    }
    for (slot, sig) in [(0, Signal::VTALRM), (1, Signal::PROF)] {
        if let Some(t) = g.cpu_itimers[slot]
            && t.deadline <= cpu
        {
            fired.push(sig);
            g.cpu_itimers[slot] = t.next(cpu);
        }
    }
    if cpu_limit.0 != u64::MAX {
        let secs = cpu / 1_000_000_000;
        if secs >= cpu_limit.1 {
            fired.push(Signal::KILL);
        } else if secs >= cpu_limit.0 {
            fired.push(Signal::XCPU);
            g.rlimits[RLIMIT_CPU as usize].0 = cpu_limit.0 + 1;
        }
    }
    if cpu_armed {
        let tick = now + CPU_TICK;
        next = Some(next.map_or(tick, |n| n.min(tick)));
    }
    let mut timers = std::mem::take(&mut g.posix_timers);
    drop(g);
    for (&id, t) in timers.iter_mut() {
        if let Some(missed) = t.fire(k, now)
            && let Some((sig, tid)) = t.signal
        {
            let info = SigInfo {
                code: code::SI_TIMER,
                fields: InfoFields::Timer {
                    id,
                    overrun: missed,
                    value: t.value,
                },
                ..SigInfo::default()
            };
            if !add_overrun(k, tgid, tid, sig, id, missed) {
                let cpu = matches!(t.clock, Clock::Process(_));
                timer_fired.push((tid, sig, info, cpu));
            }
        }
        match t.deadline {
            Some(Deadline::At(d)) => next = Some(next.map_or(d, |n| n.min(d))),
            Some(Deadline::Cpu(_)) => {
                let tick = now + CPU_TICK;
                next = Some(next.map_or(tick, |n| n.min(tick)));
            }
            None => {}
        }
    }
    group.borrow_mut().posix_timers = timers;
    for sig in fired {
        if sig == Signal::VTALRM || sig == Signal::PROF {
            signal::send_cpu_timer(k, tgid, sig, SigInfo::kernel());
        } else {
            signal::send_to_group(k, tgid, sig, SigInfo::kernel());
        }
    }
    for (tid, sig, info, cpu) in timer_fired {
        match tid.and_then(|t| k.tasks.get(&t)) {
            Some(task) => signal::send_to_task(k, task, sig, info),
            None if cpu => signal::send_cpu_timer(k, tgid, sig, info),
            None => signal::send_to_group(k, tgid, sig, info),
        }
    }
    next
}

/// 定时器 `id` 上一次发的信号还挂着：超限次数加到那个信号的 siginfo 里，返回 true。
fn add_overrun(k: &Kernel, tgid: Tid, tid: Option<Tid>, sig: Signal, id: i32, missed: u32) -> bool {
    let bump = |pending: &mut Pending| {
        let info = pending.infos_mut(sig).find(|info| {
            info.code == code::SI_TIMER
                && matches!(info.fields, InfoFields::Timer { id: i, .. } if i == id)
        });
        match info.map(|info| &mut info.fields) {
            Some(InfoFields::Timer { overrun, .. }) => {
                *overrun = overrun.saturating_add(missed + 1).min(i32::MAX as u32);
                true
            }
            _ => false,
        }
    };
    match tid.and_then(|t| k.tasks.get(&t)) {
        Some(task) => bump(&mut task.pending.borrow_mut()),
        None => k
            .groups
            .get(&tgid)
            .is_some_and(|g| bump(&mut g.borrow_mut().pending)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_timers_skip_missed_periods() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let t = ITimer {
            deadline: start,
            interval: ms(10),
        };
        assert_eq!(t.next(start).unwrap().deadline, start + ms(10));
        assert_eq!(t.next(start + ms(35)).unwrap().deadline, start + ms(40));
        let once = ITimer {
            deadline: start,
            interval: Duration::ZERO,
        };
        assert_eq!(once.next(start), None);
    }
}
