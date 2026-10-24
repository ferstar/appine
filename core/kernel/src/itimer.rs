//! 间隔定时器 `ITIMER_REAL`（setitimer、getitimer；musl 的 alarm 也经由 setitimer）。
//!
//! 到期时刻记在线程组里。宿主主线程在等待会话结束的同时负责它们（[`expire`]）：等到
//! 最早的到期时刻，给到期的进程发 SIGALRM，有间隔的重新计时。设置定时器时请求唤醒，
//! 主线程随之重新计算要等多久。按 CPU 时间计时的 `ITIMER_VIRTUAL`、`ITIMER_PROF` 没有实现。

use std::time::{Duration, Instant};

use crate::kernel::Kernel;
use crate::process::Signal;
use crate::signal::{self, SigInfo};

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

/// 给定时器到期的进程发 SIGALRM，返回下一个到期时刻。
pub(crate) fn expire(k: &Kernel) -> Option<Instant> {
    let now = Instant::now();
    let mut next: Option<Instant> = None;
    let mut fired = Vec::new();
    for (&tgid, group) in &k.groups {
        let mut g = group.borrow_mut();
        let Some(t) = g.itimer else {
            continue;
        };
        if g.threads.is_empty() {
            g.itimer = None;
            continue;
        }
        if t.deadline <= now {
            fired.push(tgid);
            g.itimer = t.next(now);
        }
        if let Some(t) = g.itimer {
            next = Some(next.map_or(t.deadline, |n| n.min(t.deadline)));
        }
    }
    for tgid in fired {
        signal::send_to_group(k, tgid, Signal::ALRM, SigInfo::kernel());
    }
    next
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
