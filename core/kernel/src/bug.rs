//! 内核层和引擎自己的 bug（设计文档 §7.4、§12 第 5 条）。
//!
//! 一个 guest 程序撞上这样的 bug 时，只结束这个 guest 进程，App 里别的会话照常运行：
//!
//! - 内核层 panic：运行循环在每次进入内核的地方兜住（`catch_unwind`），之后获取 BKL 时
//!   忽略它的中毒标记（[`crate::kernel::Shared::lock`]）。
//! - 引擎让宿主收到 SIGSEGV、SIGBUS、SIGILL：引擎返回 [`Exit::HostFault`](appine_engine::Exit)。
//!
//! 两种情况都像 Linux 的 oops 结束当前任务那样用 SIGSEGV 结束进程（这里是整个线程组），
//! 并往宿主的 stderr（Android 上转到 logcat）写一份报告；panic 的消息和位置由 panic hook
//! 先打印。BKL 保护的状态可能只改了一半，但结束了出问题的进程，大部分不一致也随之消失。
//!
//! debug 构建默认仍然直接终止宿主进程（panic 时 abort，宿主故障按默认动作产生 core
//! dump），方便开发时就地调试；环境变量 `APPINE_ON_BUG=kill` 或 `abort` 可以覆盖默认值。

use std::fmt;
use std::sync::OnceLock;

#[cfg(debug_assertions)]
use appine_engine::Cpu;

#[cfg(debug_assertions)]
use crate::errno::Errno;
#[cfg(debug_assertions)]
use crate::kernel::Cx;
use crate::kernel::Kernel;
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
    if let Some(group) = k.groups.get(&task.tgid) {
        let status = ExitStatus::Signaled {
            signal: Signal::SEGV,
            pc: None,
            fault_addr: None,
        };
        crate::signal::group_exit(k, group, status);
    }
}

/// 测试钩子的 syscall 编号，远在 Linux 的编号之外。只在 debug 构建、设置了
/// `APPINE_TEST_HOOKS=1` 时有效，端到端测试用它模拟内核层和引擎的 bug。
#[cfg(debug_assertions)]
pub(crate) const TEST_SYSCALL: u64 = 0xa991;

/// 测试钩子：`what` 为 1 时内核层 panic；为 2 时让 `addr` 所在的页在页表里指向一个
/// 不能访问的宿主页，guest 下次访问它时引擎在宿主上出错，就像翻译错了访存指令。
#[cfg(debug_assertions)]
pub(crate) fn test_hook(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    what: u64,
    addr: u64,
) -> Result<u64, Errno> {
    if std::env::var_os("APPINE_TEST_HOOKS").is_none_or(|v| v != "1") {
        return Err(Errno::ENOSYS);
    }
    match what {
        1 => panic!("panic injected by the test hook"),
        2 => {
            cx.mm_mut().break_page(addr)?;
            cpu.tlb_flush();
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
