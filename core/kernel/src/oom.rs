//! guest 自己的 OOM killer（设计文档 §12 第 4 条）。
//!
//! Android 的 LMK 只认 App 进程：内存吃紧时它杀掉整个 App，所有会话一起丢掉。所以给 App
//! 进程的匿名内存设一个上限（默认是物理内存的一半），超出时像 Linux 的 OOM killer 那样
//! 挑一个 guest 进程用 SIGKILL 结束。量的是整个宿主进程（[`appine_hal::memory_footprint`]）：
//! LMK 看的就是它，guest 的私有、共享内存之外，内核层为 guest 付出的翻译缓存、元数据缓存
//! 也在里面；宿主的文件页（仓库里的文件、映射的程序）可以回收，不算。
//!
//! guest 碰新的页由宿主内核直接处理，内核层看不到缺页，所以只能定时检查：有 guest 线程
//! 不在内核里等待时，定时器线程每 20 毫秒量一次（读一次 `/proc/self/status` 约 2 微秒），
//! 用量超过上限的 3/4 之后改成每 5 毫秒；全都在等待时内存不会再涨，停下来，等某个线程
//! 要接着执行 guest 代码时再叫醒它（[`Shared::resume_oom_checks`]）。不停分配内存的程序
//! 在被发现之前还能多用几十 MB（Tier 1 下每秒能碰好几 GB 的新页），所以上限要留出余量。
//! 结束一个进程之后，等它的线程都退出、内存还给宿主，再决定要不要结束下一个。

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use appine_engine::PAGE_SIZE;

use crate::kernel::{INIT, Kernel, Shared};
use crate::process::{ExitStatus, Signal, ThreadGroup};

/// 有线程在运行时检查的间隔。
const INTERVAL: Duration = Duration::from_millis(20);
/// 用量超过上限的 3/4 之后检查的间隔。
const NEAR_INTERVAL: Duration = Duration::from_millis(5);
/// 结束的进程这么久还没退完（卡在宿主 I/O 里）时，不再等它，与 Linux 的 oom_reaper 放弃
/// 一个进程的时间差不多。
const VICTIM_TIMEOUT: Duration = Duration::from_secs(1);
/// `oom_score_adj` 的下限：这样的进程不会被选中。
const OOM_SCORE_ADJ_MIN: i32 = -1000;

/// OOM killer 的状态，在 [`Kernel`] 里。
pub(crate) struct Oom {
    /// 上限（字节），`u64::MAX` 表示不限制。
    pub limit: u64,
    /// 下一次检查的时刻；None 表示所有线程都在等待，停下来了。
    next: Option<Instant>,
    /// 最近结束的进程和结束它的时刻。
    victim: Option<(Weak<RefCell<ThreadGroup>>, Instant)>,
}

impl Default for Oom {
    fn default() -> Oom {
        Oom {
            limit: u64::MAX,
            next: None,
            victim: None,
        }
    }
}

/// 默认的上限：宿主物理内存的一半。
pub(crate) fn default_limit() -> u64 {
    match appine_hal::memory_info().0 {
        0 => u64::MAX,
        total => total / 2,
    }
}

/// 定时器线程每次醒来时调用：到了检查的时刻就量一次 App 的内存，超出上限时结束一个
/// guest 进程。返回下一次要检查的时刻。
pub(crate) fn check(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    if k.oom.limit == u64::MAX {
        return None;
    }
    let now = Instant::now();
    if let Some(next) = k.oom.next
        && now < next
    {
        return Some(next);
    }
    let idle = k.tasks.values().all(|t| t.sleeping.get());
    shared.oom_idle.store(idle, Ordering::Relaxed);
    if idle {
        k.oom.next = None;
        return None;
    }
    let Some(used) = appine_hal::memory_footprint() else {
        eprintln!("appine: cannot measure the memory use, the memory limit is off");
        k.oom.limit = u64::MAX;
        return None;
    };
    let near = used > k.oom.limit / 4 * 3;
    k.oom.next = Some(now + if near { NEAR_INTERVAL } else { INTERVAL });
    if used <= k.oom.limit {
        return k.oom.next;
    }
    // 上次挑中的进程还没退完：它的内存马上就会还回来。超时还没退完的（卡在宿主 I/O 里）
    // 不再等，也不再挑它
    let mut stuck = None;
    if let Some((victim, at)) = &k.oom.victim
        && let Some(g) = victim.upgrade()
        && !g.borrow().threads.is_empty()
    {
        if now < *at + VICTIM_TIMEOUT {
            return k.oom.next;
        }
        stuck = Some(g);
    }
    let Some((victim, rss)) = select(k, stuck.as_ref()) else {
        return k.oom.next;
    };
    k.oom.victim = Some((Rc::downgrade(&victim), now));
    // 已经在退出（exit_group、致命信号）的不用再结束，与 Linux 一样等它退完
    if victim.borrow().exiting.is_none() {
        let g = victim.borrow();
        let task = g.threads.first().and_then(|t| k.tasks.get(t));
        eprintln!(
            "appine: out of memory: killed process {} ({}) rss:{}kB oom_score_adj:{}; \
             App memory {}kB, limit {}kB",
            g.tgid,
            crate::pseudo::comm_str(task.map(|t| &**t), g.tgid),
            rss * PAGE_SIZE / 1024,
            g.oom_score_adj,
            used / 1024,
            k.oom.limit / 1024,
        );
        drop(g);
        let status = ExitStatus::Signaled {
            signal: Signal::KILL,
            pc: None,
            fault_addr: None,
        };
        crate::signal::group_exit(k, &victim, status);
    }
    k.oom.next
}

/// 挑要结束的进程（跳过 `skip`），按 Linux 的 oom_badness：驻留的页数，加上
/// `oom_score_adj` 按物理内存折算的页数（1000 相当于全部内存，所以总是先挑它）。返回
/// 进程和它驻留的 4K 页数。
fn select(
    k: &Kernel,
    skip: Option<&Rc<RefCell<ThreadGroup>>>,
) -> Option<(Rc<RefCell<ThreadGroup>>, u64)> {
    let total = (appine_hal::memory_info().0.max(k.oom.limit) / PAGE_SIZE) as i64;
    let mut best: Option<(Rc<RefCell<ThreadGroup>>, i64, u64)> = None;
    for group in k.groups.values() {
        if skip.is_some_and(|s| Rc::ptr_eq(s, group)) {
            continue;
        }
        let g = group.borrow();
        if g.tgid == INIT || g.oom_score_adj == OOM_SCORE_ADJ_MIN {
            continue;
        }
        let Some(task) = g.threads.first().and_then(|t| k.tasks.get(t)) else {
            continue;
        };
        let rss = task.res.borrow().mm.borrow().resident_pages();
        let points = (rss as i64 + g.oom_score_adj as i64 * total / 1000).max(1);
        if best.as_ref().is_none_or(|b| points > b.1) {
            best = Some((group.clone(), points, rss));
        }
    }
    best.map(|(g, _, rss)| (g, rss))
}
