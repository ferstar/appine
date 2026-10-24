//! 运行一个 guest 程序。
//!
//! M0：单进程、单线程，还没有信号投递。guest 遇到同步异常时直接终止，
//! 退出状态与 Linux 上"被信号杀死"一致。

use std::fmt;
use std::io;
use std::path::Path;

use appine_engine::{Cpu, Exit, FaultKind};

use crate::loader::{self, LoadError};
use crate::mm::Mm;
use crate::syscall::{self, Flow};

/// 导致 guest 终止的信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// 未定义指令。
    Ill,
    /// `brk` 断点。
    Trap,
    /// 未对齐访问。
    Bus,
    /// 非法内存访问。
    Segv,
}

impl Signal {
    /// Linux 信号编号。
    pub fn number(self) -> i32 {
        match self {
            Signal::Ill => 4,
            Signal::Trap => 5,
            Signal::Bus => 7,
            Signal::Segv => 11,
        }
    }

    /// 信号名。
    pub fn name(self) -> &'static str {
        match self {
            Signal::Ill => "SIGILL",
            Signal::Trap => "SIGTRAP",
            Signal::Bus => "SIGBUS",
            Signal::Segv => "SIGSEGV",
        }
    }
}

/// guest 的结束方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// guest 调用了 `exit` 或 `exit_group`。
    Exited(i32),
    /// guest 被信号终止。
    Signaled {
        /// 信号。
        signal: Signal,
        /// 出错指令的地址。
        pc: u64,
        /// 访存出错时的 guest 地址。
        fault_addr: Option<u64>,
    },
}

/// guest 没能开始运行的原因。
#[derive(Debug)]
pub enum RunError {
    /// 读取程序文件失败。
    Io(io::Error),
    /// 加载失败。
    Load(LoadError),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(err) => err.fmt(f),
            RunError::Load(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RunError {}

/// 从宿主路径 `path` 加载 guest 程序并运行到结束。
pub fn run(path: &Path, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<ExitStatus, RunError> {
    let file = std::fs::read(path).map_err(RunError::Io)?;
    let mut mm = Mm::new();
    let image = loader::load(&mut mm, &file, argv, envp).map_err(RunError::Load)?;

    // SAFETY: cpu 比 mm 后声明、先析构，mm 的页表在 cpu 存续期间一直有效
    let mut cpu = unsafe { Cpu::new(mm.mmu()) };
    cpu.state_mut().pc = image.entry;
    cpu.state_mut().sp = image.sp;

    loop {
        let signal = match cpu.run() {
            Exit::Svc { .. } => match syscall::dispatch(&mut cpu, &mm) {
                Flow::Continue => continue,
                Flow::Exit(code) => return Ok(ExitStatus::Exited(code)),
            },
            Exit::Kick => continue,
            Exit::Undefined { .. } => (Signal::Ill, None),
            Exit::Breakpoint { .. } => (Signal::Trap, None),
            Exit::Fault {
                addr,
                kind: FaultKind::Page,
                ..
            } => (Signal::Segv, Some(addr)),
            Exit::Fault {
                addr,
                kind: FaultKind::Alignment,
                ..
            } => (Signal::Bus, Some(addr)),
        };
        return Ok(ExitStatus::Signaled {
            signal: signal.0,
            pc: cpu.state().pc,
            fault_addr: signal.1,
        });
    }
}
