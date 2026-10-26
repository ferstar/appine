//! 平均负载（`/proc/loadavg`、sysinfo）：与 Linux 的 `kernel/sched/loadavg.c` 一样，每 5 秒
//! 取一次可运行的任务数（不在内核里等待的 guest 线程，等 BKL 的也算，相当于 Linux 的 R
//! 加 D），按 1、5、15 分钟做指数衰减平均。
//!
//! 取样由定时器线程做。所有线程都在等待时停下来，不为它每 5 秒醒一次；有线程要接着执行
//! guest 代码时叫醒定时器线程（[`Shared::resume_checks`]），停下来期间错过的取样都按 0
//! 补上，相当于 Linux 的 NO_HZ 在空闲的 CPU 上做的事。

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::kernel::{Kernel, Shared};

/// 取样的间隔（Linux 的 `LOAD_FREQ` 是 5 秒多一个滴答）。
const LOAD_FREQ: Duration = Duration::from_secs(5);
/// 定点数的小数位数（`FSHIFT`）。
const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;
/// 1、5、15 分钟的衰减系数：`FIXED_1 / exp(5 秒 / 时长)`。
const EXP: [u64; 3] = [1884, 2014, 2037];
/// sysinfo 的 `loads` 的小数位数（`SI_LOAD_SHIFT`）。
const SI_LOAD_SHIFT: u32 = 16;

/// 平均负载的状态，在 [`Kernel`] 里。
pub(crate) struct LoadAvg {
    /// 1、5、15 分钟的平均值（定点数）。
    avenrun: [u64; 3],
    /// 下一次取样的时刻。
    due: Instant,
    /// 停下来了：上次取样时所有线程都在等待，没有安排下一次取样。
    stopped: bool,
}

impl LoadAvg {
    pub fn new(now: Instant) -> LoadAvg {
        LoadAvg {
            avenrun: [0; 3],
            due: now + LOAD_FREQ,
            stopped: false,
        }
    }

    /// 定时器线程醒来时调用，现在有 `running` 个线程可运行：到了取样的时刻就取样。返回
    /// 下一次要叫醒定时器线程的时刻，停下来时是 None。
    fn update(&mut self, now: Instant, running: u64) -> Option<Instant> {
        if now < self.due {
            // 停下来之后有线程又运行了（它叫醒了定时器线程）
            if self.stopped && running > 0 {
                self.stopped = false;
            }
        } else {
            let n = self.missed(now);
            // 停下来的期间所有线程都在等待
            let active = if self.stopped { 0 } else { running };
            self.avenrun = fold(self.avenrun, active, n);
            self.due += LOAD_FREQ * n as u32;
            self.stopped = running == 0;
        }
        (!self.stopped).then_some(self.due)
    }

    /// 到 `now` 为止错过的取样次数（`now` 不早于 `due`）。
    fn missed(&self, now: Instant) -> u64 {
        let behind = (now - self.due).as_nanos() / LOAD_FREQ.as_nanos();
        1 + u64::try_from(behind)
            .unwrap_or(u64::MAX)
            .min(u32::MAX as u64 - 1)
    }

    /// 现在的平均值。停下来之后定时器线程还没补上的取样（读的线程刚刚醒来）在这里补上。
    fn current(&self, now: Instant) -> [u64; 3] {
        if self.stopped && now >= self.due {
            fold(self.avenrun, 0, self.missed(now))
        } else {
            self.avenrun
        }
    }
}

/// `n` 次取样都是 `active` 个任务之后的平均值（Linux 的 `calc_load_n`）。
fn fold(avenrun: [u64; 3], active: u64, n: u64) -> [u64; 3] {
    let active = active.saturating_mul(FIXED_1);
    let mut out = avenrun;
    for (avg, exp) in out.iter_mut().zip(EXP) {
        *avg = calc_load(*avg, fixed_power(exp, n), active);
    }
    out
}

/// Linux 的 `calc_load`：`load * exp + active * (1 - exp)`，向着 active 的方向取整。
fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut new = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        new += FIXED_1 - 1;
    }
    new / FIXED_1
}

/// 定点数 `x` 的 `n` 次方（Linux 的 `fixed_power_int`）。
fn fixed_power(mut x: u64, mut n: u64) -> u64 {
    let mut result = FIXED_1;
    while n != 0 {
        if n & 1 != 0 {
            result = (result * x + FIXED_1 / 2) >> FSHIFT;
        }
        n >>= 1;
        x = (x * x + FIXED_1 / 2) >> FSHIFT;
    }
    result
}

/// 可运行的线程数：不在内核里等待的 guest 线程。
pub(crate) fn running(k: &Kernel) -> u64 {
    k.tasks
        .values()
        .filter(|t| !t.sleeping.load(Ordering::Relaxed))
        .count() as u64
}

/// 定时器线程每次醒来时调用。返回下一次要取样的时刻；停下来时返回 None，并让下一个
/// 要执行 guest 代码的线程叫醒定时器线程。
pub(crate) fn sample(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let now = Instant::now();
    // 定时器线程有别的事时也会醒来：不到取样的时刻就不必数线程
    if !k.load.stopped && now < k.load.due {
        return Some(k.load.due);
    }
    let running = running(k);
    let next = k.load.update(now, running);
    if next.is_none() {
        shared.checks_idle.store(true, Ordering::Relaxed);
    }
    next
}

/// `/proc/loadavg` 的内容。
pub(crate) fn proc_loadavg(k: &Kernel) -> String {
    // 与 Linux 一样加上 0.005 再截断成两位小数
    let avg = k.load.current(Instant::now()).map(|a| a + FIXED_1 / 200);
    let int = |a: u64| a >> FSHIFT;
    let frac = |a: u64| ((a & (FIXED_1 - 1)) * 100) >> FSHIFT;
    format!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        int(avg[0]),
        frac(avg[0]),
        int(avg[1]),
        frac(avg[1]),
        int(avg[2]),
        frac(avg[2]),
        running(k).max(1),
        k.tasks.len(),
        k.last_pid(),
    )
}

/// sysinfo 的 `loads`。
pub(crate) fn sysinfo_loads(k: &Kernel) -> [u64; 3] {
    k.load
        .current(Instant::now())
        .map(|a| a << (SI_LOAD_SHIFT - FSHIFT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenths(a: u64) -> u64 {
        (a * 100) >> FSHIFT
    }

    #[test]
    fn samples_every_five_seconds() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        // 还没到取样的时刻
        assert_eq!(
            l.update(t0 + Duration::from_secs(1), 2),
            Some(t0 + LOAD_FREQ)
        );
        assert_eq!(l.avenrun, [0; 3]);
        // 一个任务跑了一分钟：1 分钟的平均值约是 1 - 1/e
        let mut now = t0;
        for _ in 0..12 {
            now += LOAD_FREQ;
            assert!(l.update(now, 1).is_some());
        }
        assert_eq!(tenths(l.avenrun[0]), 63);
        assert!(l.avenrun[1] < l.avenrun[0] && l.avenrun[2] < l.avenrun[1]);
    }

    #[test]
    fn stopped_samples_count_as_idle() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        let mut now = t0;
        for _ in 0..24 {
            now += LOAD_FREQ;
            l.update(now, 4);
        }
        let busy = l.avenrun;
        // 所有线程都在等待：停下来
        now += LOAD_FREQ;
        assert_eq!(l.update(now, 0), None);
        let first_idle = l.avenrun;
        assert!(first_idle[0] < busy[0]);
        // 过了一分钟有线程醒来：错过的 12 次取样按 0 补上
        now += Duration::from_secs(60) + Duration::from_millis(1);
        assert_eq!(l.current(now), fold(first_idle, 0, 12));
        assert!(l.update(now, 1).is_some());
        assert_eq!(l.avenrun, fold(first_idle, 0, 12));
        // 与一次次取样的结果相同：求幂时每次平方都舍入，可能差千分之几
        let mut step = first_idle;
        for _ in 0..12 {
            step = fold(step, 0, 1);
        }
        for i in 0..3 {
            assert!(
                l.avenrun[i].abs_diff(step[i]) <= 16,
                "{:?} {:?}",
                l.avenrun,
                step
            );
        }
        // 下一次取样照常进行，按那时的线程数
        let due = l.due;
        assert!(l.update(due, 1).is_some());
        assert_eq!(l.avenrun, fold(fold(first_idle, 0, 12), 1, 1));
    }

    #[test]
    fn stopped_sampler_restarts_before_the_next_sample() {
        let t0 = Instant::now();
        let mut l = LoadAvg::new(t0);
        assert_eq!(l.update(t0 + LOAD_FREQ, 0), None);
        // 停下来之后、下一次取样之前有线程又运行了
        let next = t0 + LOAD_FREQ * 2;
        assert_eq!(
            l.update(t0 + LOAD_FREQ + Duration::from_secs(1), 1),
            Some(next)
        );
        l.update(next, 1);
        assert!(l.avenrun[0] > 0);
    }
}
