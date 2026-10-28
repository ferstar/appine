//! 内核层和引擎自己的 bug（设计文档 §7.4、§12 第 5 条）。
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
//! 先打印。BKL 保护的状态可能只改了一半，但结束了出问题的进程，大部分不一致也随之消失。
//!
//! debug 构建默认仍然直接终止宿主进程（panic 时 abort，宿主故障按默认动作产生 core
//! dump），方便开发时就地调试；环境变量 `APPINE_ON_BUG=kill` 或 `abort` 可以覆盖默认值。

use std::fmt;
use std::panic::AssertUnwindSafe;
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

/// 结束撞上 bug 的线程所在的进程：报告 `what`，用 SIGSEGV 结束整个线程组。`pc` 是这个
/// 线程最后一次进入内核时的 guest pc。
pub(crate) fn kill(k: &Kernel, task: &Task, pc: u64, what: fmt::Arguments<'_>) {
    eprintln!(
        "appine: internal error in pid {} ({}), thread {}, last guest pc {pc:#x}: {what}; \
         killed the process with SIGSEGV",
        task.tgid,
        crate::pseudo::comm_str(Some(task), task.tgid),
        task.tid.get(),
    );
    task.killed.set(true);
    segv(k, task.tgid);
}

/// 结束进程 `tgid`：bug 不是在它自己的线程上撞上的（定时器线程）。
pub(crate) fn kill_process(k: &Kernel, tgid: Tid, what: fmt::Arguments<'_>) {
    let task = k.groups.get(&tgid).and_then(|g| {
        let first = g.borrow().threads.first().copied();
        first.and_then(|t| k.tasks.get(&t).cloned())
    });
    eprintln!(
        "appine: internal error in pid {tgid} ({}): {what}; killed the process with SIGSEGV",
        crate::pseudo::comm_str(task.as_deref(), tgid),
    );
    segv(k, tgid);
}

/// 报告一个不能归到某个 guest 进程上的 bug。
pub(crate) fn report(what: fmt::Arguments<'_>) {
    eprintln!("appine: internal error: {what}");
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
