//! guest 自己的 OOM killer（设计文档 §12 第 4 条）。
//!
//! Android 的 LMK 只认 App 进程：内存吃紧时它杀掉整个 App，所有会话一起丢掉。所以给 App
//! 进程的匿名内存设一个上限（默认是物理内存的一半），超出时像 Linux 的 OOM killer 那样
//! 挑一个 guest 进程用 SIGKILL 结束。量的是整个宿主进程的匿名内存（[`app_memory`]）：
//! LMK 看的就是它，guest 的私有、共享内存之外，内核层为 guest 付出的翻译缓存、元数据缓存
//! 也在里面；宿主的文件页（仓库里的文件、映射的程序）可以回收，不算。
//!
//! guest 碰新的页由宿主内核直接处理，内核层看不到缺页，所以只能定时检查：有 guest 线程
//! 不在内核里等待时，定时器线程每 20 毫秒量一次（读一次 `/proc/self/status` 约 2 微秒），
//! 用量超过上限的 3/4 之后改成每 5 毫秒；全都在等待时内存不会再涨，停下来，等某个线程
//! 要接着执行 guest 代码时再叫醒它（[`Shared::resume_checks`]）。不停分配内存的程序
//! 在被发现之前还能多用几十 MB（Tier 1 下每秒能碰好几 GB 的新页），所以上限要留出余量。
//! 结束一个进程之后，等它的线程都退出、内存还给宿主，再决定要不要结束下一个。

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use appine_engine::PAGE_SIZE;

use crate::cell::RefCell;
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
/// 多久拆一次代码缓存在可写视图里写过的页（[`unmap_code_views`]）。
const CODE_VIEW_INTERVAL: Duration = Duration::from_secs(1);

/// OOM killer 的状态，在 [`Kernel`] 里。
pub(crate) struct Oom {
    /// 上限（字节），`u64::MAX` 表示不限制。
    pub limit: u64,
    /// 量到过的最大用量（`/sys/fs/cgroup/memory.peak`）。
    pub peak: Cell<u64>,
    /// 超出上限、要结束进程、结束了进程的次数（`/sys/fs/cgroup/memory.events`）。
    pub events: Cell<Events>,
    /// 下一次检查的时刻；None 表示所有线程都在等待，停下来了。
    next: Option<Instant>,
    /// 最近结束的进程和结束它的时刻。
    victim: Option<(Weak<RefCell<ThreadGroup>>, Instant)>,
    /// 下一次拆代码缓存的可写视图的时刻；None 表示所有线程都在等待，停下来了。
    code_views: Option<Instant>,
}

/// `memory.events` 里的计数：量到超出上限的次数，因此要挑进程结束的次数（上次挑中的
/// 还没退完时不算），结束了的进程数。
#[derive(Clone, Copy, Default)]
pub(crate) struct Events {
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

impl Default for Oom {
    fn default() -> Oom {
        Oom {
            limit: u64::MAX,
            peak: Cell::new(0),
            events: Cell::new(Events::default()),
            next: None,
            victim: None,
            code_views: None,
        }
    }
}

impl Oom {
    /// 现在 App 的内存用量（量不了时是 0），顺便记下峰值。
    pub fn measure(&self) -> u64 {
        let used = footprint().unwrap_or(0);
        self.note(used);
        used
    }

    fn note(&self, used: u64) {
        self.peak.set(self.peak.get().max(used));
    }

    fn count(&self, f: impl FnOnce(&mut Events)) {
        let mut e = self.events.get();
        f(&mut e);
        self.events.set(e);
    }
}

/// 默认的上限：宿主物理内存的一半。
pub(crate) fn default_limit() -> u64 {
    match appine_hal::memory_info().0 {
        0 => u64::MAX,
        total => total / 2,
    }
}

/// App 的内存用量，分项（[`appine_hal::memory_usage`]）。Tier 1 的代码缓存在可写视图里
/// 写过、还没拆掉的页（[`unmap_code_views`]）在宿主的 RssShmem 里算了两次，扣掉一次。
pub(crate) fn app_memory() -> Option<appine_hal::MemoryUsage> {
    let mut u = appine_hal::memory_usage()?;
    u.shmem = u.shmem.saturating_sub(appine_engine::written_code_bytes());
    Some(u)
}

/// App 的内存用量（字节）：[`app_memory`] 的各项之和。读不了时返回 None。
fn footprint() -> Option<u64> {
    app_memory().map(|u| u.anon + u.shmem + u.swap)
}

/// 所有 guest 线程都在内核里等待：内存不会再涨，代码缓存也不会再写。
fn all_sleeping(k: &Kernel) -> bool {
    k.tasks.values().all(|t| t.sleeping.load(Ordering::Relaxed))
}

/// 定时器线程每次醒来时调用：有 guest 线程在运行时每秒拆一次 Tier 1 的代码缓存在可写
/// 视图里写过的页，所有线程都等待之后再拆一次，然后停下（[`appine_engine::unmap_written_code`]）。
/// 代码缓存按 W^X 映射两次，宿主的 RSS 按映射数页，两个视图都碰过的页算两次：上面量的
/// App 用量（[`app_memory`] 扣掉了还没拆的）和 LMK 看的 RSS 里翻译多算一倍。按时间拆而
/// 不是写多少拆多少：JIT 负载每秒往几千个不同的页里回填链接、作废块，拆了之后这些页再写
/// 都要重新缺页。返回下一次的时刻。
pub(crate) fn unmap_code_views(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let now = Instant::now();
    if let Some(next) = k.oom.code_views
        && now < next
    {
        return Some(next);
    }
    let idle = all_sleeping(k);
    // 停下来之后被别的事叫醒：这期间没有线程运行，没有新写的页
    if !idle || k.oom.code_views.is_some() {
        appine_engine::unmap_written_code();
    }
    if idle {
        shared.checks_idle.store(true, Ordering::Relaxed);
        k.oom.code_views = None;
        return None;
    }
    k.oom.code_views = Some(now + CODE_VIEW_INTERVAL);
    k.oom.code_views
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
    let idle = all_sleeping(k);
    shared.checks_idle.store(idle, Ordering::Relaxed);
    if idle {
        k.oom.next = None;
        return None;
    }
    let Some(used) = footprint() else {
        eprintln!("appine: cannot measure the memory use, the memory limit is off");
        k.oom.limit = u64::MAX;
        return None;
    };
    k.oom.note(used);
    let near = used > k.oom.limit / 4 * 3;
    k.oom.next = Some(now + if near { NEAR_INTERVAL } else { INTERVAL });
    // 刚退出的进程的内存在它放开 BKL 之后才交还宿主（mm::release_unmapped）
    if used <= k.oom.limit || crate::mm::UNMAPPING.load(Ordering::Acquire) != 0 {
        return k.oom.next;
    }
    k.oom.count(|e| e.max += 1);
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
    k.oom.count(|e| e.oom += 1);
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
            core_dumped: false,
        };
        crate::signal::group_exit(k, &victim, status);
        k.oom.count(|e| e.oom_kill += 1);
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
