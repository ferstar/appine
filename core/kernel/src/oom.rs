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
//!
//! 设了 `memory.max` 的 cgroup（[`crate::cgroup`]）一起检查：量子树里的进程的驻留内存
//! （mincore，几十 MB 的进程要一两百微秒，node 这样映射多的 200 MB 要一毫秒多，所以只量
//! 设了上限的，间隔至少是耗时的 20 倍），超出时与 Linux 的 memcg OOM 一样从它下面挑一个
//! 进程结束，`memory.oom.group` 时连同那一组。

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use appine_engine::PAGE_SIZE;

use crate::cell::RefCell;
use crate::cgroup::ROOT;
use crate::kernel::{INIT, Kernel, Shared};
use crate::process::{ExitStatus, Signal, ThreadGroup};

/// 有线程在运行时检查的间隔。
const INTERVAL: Duration = Duration::from_millis(20);
/// 用量超过上限的 3/4 之后检查的间隔。
const NEAR_INTERVAL: Duration = Duration::from_millis(5);
/// 检查的间隔至少是一次检查的耗时的这么多倍（量 cgroup 的内存要 mincore，见 [`check`]）。
const MEASURE_SHARE: u32 = 20;
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
    /// 下一次检查的时刻；None 表示所有线程都在等待，停下来了。
    next: Option<Instant>,
    /// 最近结束的进程和结束它的时刻。
    victim: Option<(Weak<RefCell<ThreadGroup>>, Instant)>,
    /// 下一次拆代码缓存的可写视图的时刻；None 表示所有线程都在等待，停下来了。
    code_views: Option<Instant>,
}

/// `memory.events` 里的计数：量到超出上限的次数，因此要挑进程结束的次数（上次挑中的
/// 还没退完时不算），结束了的进程数，因为 `memory.oom.group` 整组结束的次数。记在 cgroup
/// 上（[`crate::cgroup::memory_event`]），App 的上限的记在根上。
#[derive(Clone, Copy, Default)]
pub(crate) struct Events {
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
    pub oom_group_kill: u64,
}

impl Default for Oom {
    fn default() -> Oom {
        Oom {
            limit: u64::MAX,
            peak: Cell::new(0),
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

    /// 改了 cgroup 的 `memory.max`：下一次醒来就检查（新的上限可能已经超出）。
    pub fn recheck(&mut self) {
        self.next = None;
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

/// 定时器线程每次醒来时调用：到了检查的时刻就量一次 App 的内存和设了 `memory.max` 的
/// cgroup 的内存，超出上限时结束一个 guest 进程（cgroup 的从它里面挑）。返回下一次要检查
/// 的时刻。
pub(crate) fn check(k: &mut Kernel, shared: &Shared) -> Option<Instant> {
    let limits = crate::cgroup::memory_limits(k);
    if k.oom.limit == u64::MAX && limits.is_empty() {
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
    // 超出的范围（根是整个 App）、用量、上限
    let mut over = None;
    let mut near = false;
    if k.oom.limit != u64::MAX {
        match footprint() {
            Some(used) => {
                k.oom.note(used);
                near = used > k.oom.limit / 4 * 3;
                if used > k.oom.limit {
                    over = Some((ROOT, used, k.oom.limit));
                }
            }
            None => {
                eprintln!("appine: cannot measure the memory use, the memory limit is off");
                k.oom.limit = u64::MAX;
            }
        }
    }
    for (id, max) in limits {
        if over.is_some() {
            break;
        }
        let used = crate::cgroup::measure_memory(k, id);
        near |= used > max / 4 * 3;
        if used > max {
            over = Some((id, used, max));
        }
    }
    // 量 cgroup 拿着 BKL，进程多、映射大时要一两毫秒：间隔至少是这次耗时的
    // MEASURE_SHARE 倍，占用不超过 BKL 时间的 1/MEASURE_SHARE
    let interval = if near { NEAR_INTERVAL } else { INTERVAL };
    k.oom.next = Some(now + interval.max(now.elapsed() * MEASURE_SHARE));
    // 刚退出的进程的内存在它放开 BKL 之后才交还宿主（mm::release_unmapped）
    let Some((domain, used, limit)) = over else {
        return k.oom.next;
    };
    // 上次挑中的进程还在退出：它的内存马上就会还回来，不算又碰到上限
    if crate::mm::UNMAPPING.load(Ordering::Acquire) != 0 || waiting_for_victim(k, now) {
        return k.oom.next;
    }
    crate::cgroup::memory_event(k, domain, |e| e.max += 1);
    out_of_memory(k, (domain, used, limit));
    k.oom.next
}

/// 上次挑中的进程还没退完，也还没到 [`VICTIM_TIMEOUT`]。
fn waiting_for_victim(k: &Kernel, now: Instant) -> bool {
    matches!(&k.oom.victim, Some((victim, at))
        if now < *at + VICTIM_TIMEOUT
            && victim.upgrade().is_some_and(|g| !g.borrow().threads.is_empty()))
}

/// OOM 的范围 `domain`（cgroup，根是整个 App）的用量 `used` 超出了上限 `limit`：从它下面挑
/// 一个进程结束（Linux 的 `out_of_memory`、`oom_kill_process`），`memory.oom.group` 时连同
/// 那一组。上次挑中的进程还没退完时等它（它的内存马上就会还回来），超时还没退完的（卡在
/// 宿主 I/O 里）不再等，也不再挑它。
pub(crate) fn out_of_memory(k: &mut Kernel, (domain, used, limit): (u64, u64, u64)) {
    let now = Instant::now();
    if waiting_for_victim(k, now) {
        return;
    }
    // 超时还没退完的不再挑
    let stuck = k.oom.victim.as_ref().and_then(|(v, _)| v.upgrade());
    let stuck = stuck.filter(|g| !g.borrow().threads.is_empty());
    let Some((victim, rss)) = select(k, stuck.as_ref(), (domain, limit)) else {
        return;
    };
    crate::cgroup::memory_event(k, domain, |e| e.oom += 1);
    k.oom.victim = Some((Rc::downgrade(&victim), now));
    // 已经在退出（exit_group、致命信号）的不用再结束，与 Linux 一样等它退完
    if victim.borrow().exiting.is_some() {
        return;
    }
    {
        let g = victim.borrow();
        let task = g.threads.first().and_then(|t| k.tasks.get(t));
        eprintln!(
            "appine: out of memory: killed process {} ({}) rss:{}kB oom_score_adj:{}; \
             {} memory {}kB, limit {}kB",
            g.tgid,
            crate::pseudo::comm_str(task.map(|t| &**t), g.tgid),
            rss * PAGE_SIZE / 1024,
            g.oom_score_adj,
            if domain == ROOT {
                "App".to_owned()
            } else {
                format!("cgroup {}", k.cgroups.path(domain, ROOT))
            },
            used / 1024,
            limit / 1024,
        );
        log_kill(k, &g, task.map(|t| &**t), (domain, used, limit));
    }
    let group = crate::cgroup::oom_group(k, victim.borrow().cgroup, domain);
    kill(k, &victim);
    // memory.oom.group：连同那个 cgroup 里的进程一起结束（Linux 的 mem_cgroup_scan_tasks，
    // 挑中的那个也再算一次），oom_score_adj 是下限的除外
    if let Some(group) = group {
        crate::cgroup::memory_event(k, group, |e| e.oom_group_kill += 1);
        let members: Vec<_> = k
            .groups
            .values()
            .filter(|g| {
                let g = g.borrow();
                g.tgid != INIT
                    && !g.threads.is_empty()
                    && g.oom_score_adj != OOM_SCORE_ADJ_MIN
                    && k.cgroups.is_within(g.cgroup, group)
            })
            .cloned()
            .collect();
        for g in members {
            {
                let gb = g.borrow();
                let task = gb.threads.first().and_then(|t| k.tasks.get(t));
                log_killed(k, &gb, task.map(|t| &**t));
            }
            kill(k, &g);
        }
    }
}

/// 用 SIGKILL 结束进程 `g`，记进它所在的 cgroup 的 `oom_kill`（已经在退出的照样算，与
/// Linux 一样）。
fn kill(k: &mut Kernel, g: &Rc<RefCell<ThreadGroup>>) {
    let cgroup = g.borrow().cgroup;
    if g.borrow().exiting.is_none() {
        let status = ExitStatus::Signaled {
            signal: Signal::KILL,
            pc: None,
            fault_addr: None,
            core_dumped: false,
        };
        crate::signal::group_exit(k, g, status);
    }
    crate::cgroup::memory_event(k, cgroup, |e| e.oom_kill += 1);
}

/// 结束进程时记进内核日志（[`crate::klog`]）：App 的上限在 guest 看来是它所在的 cgroup 的
/// `memory.max`，所以按 Linux 的 memcg OOM 写（`mem_cgroup_print_oom_meminfo` 的第一行、
/// `dump_oom_summary`、`__oom_kill_process`），`dmesg` 里看得到是谁、为什么被结束。调用的
/// 栈、各个进程的表这些略去。驻留的页按映射的种类分开（与 smaps 一样按宿主内存算），没有
/// 页表的计数（`VmPTE` 也是 0）。
fn log_kill(
    k: &Kernel,
    g: &ThreadGroup,
    task: Option<&crate::task::Task>,
    (domain, used, limit): (u64, u64, u64),
) {
    use crate::klog::{level, printk};
    let comm = crate::pseudo::comm_str(task, g.tgid);
    let (pid, uid) = (g.tgid, g.cred.ruid);
    let failcnt = k.cgroups.get(domain).map_or(0, |c| c.mem_events.max);
    let log = &k.klog;
    printk(
        log,
        level::INFO,
        format_args!(
            "memory: usage {}kB, limit {}kB, failcnt {failcnt}",
            used / 1024,
            limit / 1024,
        ),
    );
    printk(
        log,
        level::INFO,
        format_args!(
            "oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,\
             oom_memcg={},task_memcg={},task={comm},pid={pid},uid={uid}",
            k.cgroups.path(domain, ROOT),
            k.cgroups.path(g.cgroup, ROOT),
        ),
    );
    log_killed(k, g, task);
}

/// 结束一个进程的那一行（`__oom_kill_process`）。驻留的页按映射的种类分开（与 smaps 一样
/// 按宿主内存算），没有页表的计数。
fn log_killed(k: &Kernel, g: &ThreadGroup, task: Option<&crate::task::Task>) {
    let comm = crate::pseudo::comm_str(task, g.tgid);
    let (pid, uid) = (g.tgid, g.cred.ruid);
    let (mut vm, mut anon, mut file, mut shmem) = (0, 0, 0, 0);
    if let Some(t) = task {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        vm = mm.size() / 1024;
        for r in mm.regions() {
            let pages = mm
                .residency(r.start, r.end - r.start)
                .map_or(0, |v| v.iter().filter(|&&p| p).count() as u64);
            match (r.file, r.shared) {
                (Some(_), _) => file += pages * 4,
                (None, true) => shmem += pages * 4,
                (None, false) => anon += pages * 4,
            }
        }
    }
    crate::klog::printk(
        &k.klog,
        crate::klog::level::ERR,
        format_args!(
            "Memory cgroup out of memory: Killed process {pid} ({comm}) total-vm:{vm}kB, \
             anon-rss:{anon}kB, file-rss:{file}kB, shmem-rss:{shmem}kB, UID:{uid} \
             pgtables:0kB oom_score_adj:{}",
            g.oom_score_adj
        ),
    );
}

/// 挑要结束的进程（跳过 `skip`）：OOM 的范围是 cgroup `domain`（根是整个 App），从它下面的
/// 进程里挑，按 Linux 的 oom_badness：驻留的页数，加上 `oom_score_adj` 按上限 `limit`
/// （根的按物理内存和上限里大的那个）折算的页数（1000 相当于全部，所以总是先挑它）。返回
/// 进程和它驻留的 4K 页数。
fn select(
    k: &Kernel,
    skip: Option<&Rc<RefCell<ThreadGroup>>>,
    (domain, limit): (u64, u64),
) -> Option<(Rc<RefCell<ThreadGroup>>, u64)> {
    let total = if domain == ROOT {
        appine_hal::memory_info().0.max(limit)
    } else {
        limit
    };
    let total = (total / PAGE_SIZE) as i64;
    let mut best: Option<(Rc<RefCell<ThreadGroup>>, i64, u64)> = None;
    for group in k.groups.values() {
        if skip.is_some_and(|s| Rc::ptr_eq(s, group)) {
            continue;
        }
        let g = group.borrow();
        if g.tgid == INIT
            || g.oom_score_adj == OOM_SCORE_ADJ_MIN
            || !k.cgroups.is_within(g.cgroup, domain)
        {
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
