//! 内核层和引擎自己的 bug（engine 的 map、设计文档 §12 第 5 条）。
//!
//! 一个 guest 程序撞上这样的 bug 时，只结束这个 guest 进程，App 里别的会话照常运行：
//!
//! - 内核层 panic：用 [`catch`] 兜住，之后获取 BKL 时忽略它的中毒标记
//!   （[`crate::kernel::Shared::lock`]）。兜住的地方和出错时结束的是：
//!   - guest 线程每次进入内核、线程开始、线程退出的每一步：这个线程所在的进程；
//!   - 进程结束（最后一个线程退出之后，[`crate::process::finish_group`]）的每一步：进程
//!     已经结束了，只报告，接着做下一步；
//!   - 定时器线程处理一个进程的定时器：那个进程；内存上限的检查、信号驱动的 I/O 分不出
//!     是哪个进程的，出错时关掉这项功能；
//!   - 宿主调用 [`crate::Machine`] 的方法和本库别的公开函数（[`host_call`]）：调用返回
//!     错误，出错的是一个控制台上的调用时挂断那个控制台。
//! - 引擎让宿主收到 SIGSEGV、SIGBUS、SIGILL：引擎返回 [`Exit::HostFault`](appine_engine::Exit)。
//!
//! 两种情况都像 Linux 的 oops 结束当前任务那样用 SIGSEGV 结束进程（这里是整个线程组），
//! 并往宿主的 stderr（Android 上转到 logcat）写一份报告；panic 的消息和位置由 panic hook
//! 先打印，也写进报告（[`install_panic_hook`]）。BKL 保护的状态可能只改了一半，但结束了出问题的进程，大部分不一致也随之消失。
//!
//! debug 构建默认仍然直接终止宿主进程（panic 时 abort，宿主故障按默认动作产生 core
//! dump），方便开发时就地调试；环境变量 `APPINE_ON_BUG=kill` 或 `abort` 可以覆盖默认值。

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::OnceLock;
#[cfg(any(debug_assertions, feature = "test-hooks"))]
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

#[cfg(any(debug_assertions, feature = "test-hooks"))]
use appine_engine::Cpu;

#[cfg(any(debug_assertions, feature = "test-hooks"))]
use crate::errno::Errno;
#[cfg(any(debug_assertions, feature = "test-hooks"))]
use crate::kernel::Cx;
use crate::kernel::{Kernel, Tid};
use crate::process::{ExitStatus, Signal};
use crate::task::Task;

/// 撞上 bug 时只结束 guest 进程，而不是终止宿主进程。
pub(crate) fn contain() -> bool {
    static CONTAIN: OnceLock<bool> = OnceLock::new();
    *CONTAIN.get_or_init(|| match std::env::var("APPINE_ON_BUG").as_deref() {
        Ok("kill") => true,
        Ok("abort") => false,
        _ => !cfg!(debug_assertions),
    })
}

thread_local! {
    /// 这个线程最近一次 panic 的消息和位置（[`install_panic_hook`]），还没写进报告的。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static LAST_PANIC: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// panic 的消息最多写进报告多少字节：`assert_eq!` 的消息带着两边的值，可能很长。
const PANIC_MESSAGE_MAX: usize = 1000;

/// 装上 panic hook（每个宿主进程一次，原来的照样调用）：记下每个线程最近一次 panic 的消息和
/// 位置，结束进程、报告 bug 时写进报告（[`kill`]、[`report`]）。报告进内核日志、交给 App，
/// 用户在 `dmesg` 里、在复制给开发者的报告里就看得到是哪里出的错；原来这些只由默认的 hook
/// 打印到 stderr（Android 上是 logcat，用户拿不到）。
pub(crate) fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(not a string)");
            let mut end = message.len().min(PANIC_MESSAGE_MAX);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            let mut text = message[..end].to_owned();
            if let Some(at) = info.location() {
                text.push_str(&format!(" at {}:{}", at.file(), at.line()));
            }
            // 线程正在退出、线程局部变量已经没了时就不记
            let _ = LAST_PANIC.try_with(|p| {
                if let Ok(mut p) = p.try_borrow_mut() {
                    *p = Some(text);
                }
            });
            previous(info);
        }));
    });
}

/// 取走这个线程最近一次 panic 的消息和位置（[`install_panic_hook`]）。
fn take_panic() -> Option<String> {
    LAST_PANIC
        .try_with(|p| p.try_borrow_mut().ok()?.take())
        .ok()
        .flatten()
}

/// `what` 后面接上这个线程刚才 panic 的消息和位置（有的话）。
fn with_panic(what: fmt::Arguments<'_>) -> String {
    match take_panic() {
        Some(panic) => format!("{what}: {panic}"),
        None => what.to_string(),
    }
}

/// 执行内核代码 `f`，兜住其中的 panic：返回 None，由调用方收拾（结束出问题的进程等）。
/// 不兜（[`contain`]）时直接终止宿主进程。panic 的消息和位置已经由 panic hook 打印。
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) if contain() => None,
        Err(_) => {
            eprintln!("appine: internal error, aborting");
            std::process::abort()
        }
    }
}

/// 执行宿主调用（[`crate::Machine`] 的方法和本库别的公开函数）`what` 里的内核代码：内核层
/// panic 时报告，返回 None，由调用方返回错误。
pub(crate) fn host_call<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    let r = catch(|| {
        trap(Trap::HostCall, HOST);
        f()
    });
    if r.is_none() {
        report(format_args!("kernel panic in {what}"));
    }
    r
}

/// 内核层结束了一个 guest 进程（App 据此在它的标签页里说明原因，设计文档 §11.2、§12）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillReport {
    /// 进程号（开机时的 pid 命名空间里的）。
    pub pid: i32,
    /// 进程的名字（comm）。
    pub comm: String,
    /// 控制终端是 App 的控制台时是它的编号（`Machine::open_console` 返回的）。
    pub console: Option<u32>,
    pub reason: KillReason,
}

/// 为什么结束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillReason {
    /// 内核层、引擎的 bug：`report` 是报告的那一行（也写进了内核日志），`log` 是内核日志里
    /// 它前面的几条和它自己（`dmesg` 的写法，每行一项）：出错之前有没有别的 bug、OOM、guest
    /// 写的日志，在复制给开发者的报告里就看得到。
    Bug { report: String, log: Vec<String> },
    /// OOM killer：App 内存的上限（`cgroup` 是 None）或者某个 cgroup 的 `memory.max` 不够了；
    /// 用量和上限是字节。
    OutOfMemory {
        cgroup: Option<String>,
        used: u64,
        limit: u64,
    },
}

/// 最多攒这么多份报告，App 没来得及取走时丢掉最早的。
const MAX_KILLS: usize = 32;

/// 记下结束进程 `tgid` 的报告，等 App 取走。
pub(crate) fn record_kill(k: &Kernel, tgid: Tid, comm: String, reason: KillReason) {
    // 撞上 bug 时线程组可能正被借着：借不到就当作没有控制终端，报告照样记下
    let console = k.groups.get(&tgid).and_then(|g| {
        let tty = g.try_borrow().ok()?.ctty.clone()?;
        k.consoles
            .iter()
            .find(|(_, c)| Rc::ptr_eq(&c.master.tty, &tty))
            .map(|(id, _)| *id)
    });
    let Ok(mut kills) = k.kills.try_borrow_mut() else {
        return;
    };
    if kills.len() >= MAX_KILLS {
        kills.remove(0);
    }
    kills.push(KillReport {
        pid: tgid,
        comm,
        console,
        reason,
    });
}

/// 结束撞上 bug 的线程所在的进程：报告 `what`，用 SIGSEGV 结束整个线程组。`pc` 是这个
/// 线程最后一次进入内核时的 guest pc。
pub(crate) fn kill(k: &Kernel, task: &Task, pc: u64, what: fmt::Arguments<'_>) {
    let comm = crate::pseudo::comm_str(Some(task), task.tgid);
    let report = format!(
        "appine: internal error in pid {} ({comm}), thread {}, last guest pc {pc:#x}: {}; \
         killed the process with SIGSEGV",
        task.tgid,
        task.tid.get(),
        with_panic(what),
    );
    eprintln!("{report}");
    let log = log(k, &report);
    record_kill(k, task.tgid, comm, KillReason::Bug { report, log });
    task.killed.set(true);
    segv(k, task.tgid);
}

/// 结束进程 `tgid`：bug 不是在它自己的线程上撞上的（定时器线程）。
pub(crate) fn kill_process(k: &Kernel, tgid: Tid, what: fmt::Arguments<'_>) {
    let task = k.groups.get(&tgid).and_then(|g| {
        let first = g.borrow().threads.first().copied();
        first.and_then(|t| k.tasks.get(&t).cloned())
    });
    let comm = crate::pseudo::comm_str(task.as_deref(), tgid);
    let report = format!(
        "appine: internal error in pid {tgid} ({comm}): {}; killed the process with SIGSEGV",
        with_panic(what),
    );
    eprintln!("{report}");
    let log = log(k, &report);
    record_kill(k, tgid, comm, KillReason::Bug { report, log });
    segv(k, tgid);
}

/// 交给 App 的报告带上内核日志里它前面的几条。
const LOG_BEFORE: u64 = 10;

/// 结束进程的报告也记进内核日志（[`crate::klog`]），guest 里用 `dmesg` 看得到进程为什么
/// 以 SIGSEGV 结束。Linux 的 oops 用 KERN_EMERG，那是整个内核出了错；这里只结束一个进程，
/// 与 OOM killer 结束进程一样用 KERN_ERR。报告里的换行（panic 的消息可能有）各成一行。
///
/// 返回日志里报告前面的 [`LOG_BEFORE`] 条和报告自己（[`crate::klog::Klog::lines_from`]）：
/// 报告的时间戳和前面几条的放在一起，看得出隔了多久。日志正被借用时（在日志自己的代码里
/// 出了错）是空的。
fn log(k: &Kernel, report: &str) -> Vec<String> {
    let start = k.klog.try_borrow().map(|l| l.upcoming_seq()).ok();
    for line in report.lines() {
        crate::klog::printk(&k.klog, crate::klog::level::ERR, format_args!("{line}"));
    }
    start
        .and_then(|start| {
            let l = k.klog.try_borrow().ok()?;
            Some(l.lines_from(start.saturating_sub(LOG_BEFORE)))
        })
        .unwrap_or_default()
}

/// 报告一个不能归到某个 guest 进程上的 bug。
pub(crate) fn report(what: fmt::Arguments<'_>) {
    eprintln!("appine: internal error: {}", with_panic(what));
}

fn segv(k: &Kernel, tgid: Tid) {
    if let Some(group) = k.groups.get(&tgid) {
        let status = ExitStatus::Signaled {
            signal: Signal::SEGV,
            pc: None,
            fault_addr: None,
            core_dumped: false,
        };
        crate::signal::group_exit(k, group, status);
    }
}

/// 测试钩子可以布下 bug 的地方（[`trap`]）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Trap {
    /// 线程开始运行时：布给一个进程（pid），它下一个开始的线程 panic。
    Start,
    /// 线程退出时：布给一个线程（tid）。
    Exit,
    /// 定时器线程处理一个进程（pid）的定时器时。
    Timers,
    /// 宿主下一次调用 [`crate::Machine`] 的方法（只读一张表的除外）或本库别的公开函数时。
    HostCall,
    /// 进程结束（[`crate::process::finish_group`]）的一步：布给一个进程（pid），哪一步记在
    /// `FINISH_STEP` 里。
    Finish,
    /// 线程下一次不持 BKL 的系统调用（`syscall::fast`）：布给一个线程（tid）。
    Fast,
    /// 在 futex 上等待（已经登记成等待者之后）：布给一个进程（pid）。
    FutexWait,
}

/// 测试钩子布下的 bug：每个 [`Trap`] 上布给的 tid 或 pid，0 表示没有。
#[cfg(any(debug_assertions, feature = "test-hooks"))]
static ARMED: [AtomicI32; 7] = [const { AtomicI32::new(0) }; 7];

/// [`Trap::Finish`] 布在第几步（从 1 开始）。
#[cfg(any(debug_assertions, feature = "test-hooks"))]
static FINISH_STEP: AtomicU32 = AtomicU32::new(0);

/// 宿主调用不属于任何 guest 进程，布给它时用这个编号。
pub(crate) const HOST: Tid = -1;

/// 测试钩子在 `at` 上为 `id`（tid、pid 或 [`HOST`]）布下了 bug 时 panic，只触发一次。
/// 没有测试钩子的构建里什么也不做。
#[inline]
pub(crate) fn trap(at: Trap, id: Tid) {
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    {
        let armed = &ARMED[at as usize];
        if armed.load(Ordering::Relaxed) == id
            && armed
                .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            panic!("panic injected by the test hook at {at:?}");
        }
    }
    #[cfg(not(any(debug_assertions, feature = "test-hooks")))]
    let _ = (at, id);
}

/// 进程 `tgid` 结束的第 `step` 步：测试钩子在这一步为它布下了 bug 时 panic，只触发一次。
#[inline]
pub(crate) fn trap_finish(tgid: Tid, step: u32) {
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    if FINISH_STEP.load(Ordering::Relaxed) != step {
        return;
    }
    #[cfg(not(any(debug_assertions, feature = "test-hooks")))]
    let _ = step;
    trap(Trap::Finish, tgid);
}

/// 测试钩子的 syscall 编号，远在 Linux 的编号之外。只在 debug 构建和打开了 `test-hooks`
/// 特性的构建（Android 的 debug 版 App）里存在，并且要设置 `APPINE_TEST_HOOKS=1`。
/// 端到端测试用它模拟内核层和引擎的 bug。
#[cfg(any(debug_assertions, feature = "test-hooks"))]
pub(crate) const TEST_SYSCALL: u64 = 0xa991;

/// 测试钩子，`what` 是：
///
/// 1. 内核层马上 panic；
/// 2. 让 `addr` 所在的页在页表里指向一个不能访问的宿主页，guest 下次访问它时引擎在宿主上
///    出错，就像翻译错了访存指令；
/// 3. 调用的线程退出时 panic；
/// 4. 这个进程下一个开始运行的线程 panic；
/// 5. 定时器线程下次处理这个进程的定时器时 panic；
/// 6. 宿主下一次调用 Machine 的方法时 panic；
/// 7. 这个进程结束时（[`crate::process::finish_group`]）第 `addr` 步 panic；
/// 8. 放开 BKL 时（[`Cx::unlocked`]）马上 panic。展开到这里时必须已经重新拿到 BKL，否则
///    终止宿主进程：外层丢掉的 `Rc` 会和别的线程同时改计数；
/// 9. 调用的线程下一次不持 BKL 的系统调用（`syscall::fast`）panic；
/// 10. 这个进程下一次在 futex 上等待时，登记成等待者之后 panic。
#[cfg(any(debug_assertions, feature = "test-hooks"))]
pub(crate) fn test_hook(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    what: u64,
    addr: u64,
) -> Result<u64, Errno> {
    if std::env::var_os("APPINE_TEST_HOOKS").is_none_or(|v| v != "1") {
        return Err(Errno::ENOSYS);
    }
    let arm = |at: Trap, id: Tid| ARMED[at as usize].store(id, Ordering::SeqCst);
    match what {
        1 => panic!("panic injected by the test hook"),
        2 => {
            cx.mm_mut().break_page(addr)?;
            cpu.tlb_flush();
        }
        3 => arm(Trap::Exit, cx.tid()),
        4 => arm(Trap::Start, cx.tgid()),
        5 => {
            arm(Trap::Timers, cx.tgid());
            cx.shared.timers.signal();
        }
        6 => {
            arm(Trap::HostCall, HOST);
            // 让在 Machine 的方法里等着的宿主线程重新检查
            crate::kernel::request_wake();
        }
        7 if addr != 0 => {
            FINISH_STEP.store(addr as u32, Ordering::SeqCst);
            arm(Trap::Finish, cx.tgid());
        }
        8 => {
            struct MustHoldBkl;
            impl Drop for MustHoldBkl {
                fn drop(&mut self) {
                    if !crate::kernel::bkl_held() {
                        eprintln!("appine: unwound out of Cx::unlocked without the BKL");
                        std::process::abort();
                    }
                }
            }
            let _check = MustHoldBkl;
            cx.unlocked(|| panic!("panic injected by the test hook while the BKL is released"));
        }
        9 => arm(Trap::Fast, cx.tid()),
        10 => arm(Trap::FutexWait, cx.tgid()),
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}
