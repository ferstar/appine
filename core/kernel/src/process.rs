//! 运行一个 guest 程序。
//!
//! M0：单进程、单线程，还没有信号投递。guest 遇到同步异常时直接终止，
//! 退出状态与 Linux 上"被信号杀死"一致。

use std::fmt;
use std::io;
use std::path::Path;

use appine_engine::{Cpu, Dbt, DbtStats, Exit, FaultKind};

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

/// 执行引擎（设计文档 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Tier 0：解释器。
    #[default]
    Interpreter,
    /// Tier 1：同 ISA 直通式 DBT（M2 原型，只支持 aarch64 Linux 宿主）。
    Dbt,
}

/// 运行选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// 执行引擎。
    pub tier: Tier,
}

/// 一次运行的结果。
#[derive(Debug, Clone, Copy)]
pub struct RunReport {
    /// guest 的结束方式。
    pub status: ExitStatus,
    /// 使用 Tier 1 时的统计。
    pub dbt_stats: Option<DbtStats>,
}

/// guest 没能开始运行的原因。
#[derive(Debug)]
pub enum RunError {
    /// 读取程序文件失败。
    Io(io::Error),
    /// 加载失败。
    Load(LoadError),
    /// 当前宿主不支持所选的执行引擎。
    TierUnavailable,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(err) => err.fmt(f),
            RunError::Load(err) => err.fmt(f),
            RunError::TierUnavailable => {
                f.write_str("the selected tier is not supported on this host")
            }
        }
    }
}

impl std::error::Error for RunError {}

/// 从宿主路径 `path` 加载 guest 程序并运行到结束。
pub fn run(
    path: &Path,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    let mut dbt = match options.tier {
        Tier::Interpreter => None,
        Tier::Dbt => Some(Dbt::new().ok_or(RunError::TierUnavailable)?),
    };
    let file = std::fs::read(path).map_err(RunError::Io)?;
    let mut mm = Mm::new();
    let image = loader::load(&mut mm, &file, argv, envp).map_err(RunError::Load)?;

    // SAFETY: cpu 比 mm 后声明、先析构，mm 的页表在 cpu 存续期间一直有效
    let mut cpu = unsafe { Cpu::new(mm.mmu()) };
    cpu.state_mut().pc = image.entry;
    cpu.state_mut().sp = image.sp;

    let status = loop {
        let exit = match &mut dbt {
            Some(dbt) => dbt.run(&mut cpu),
            None => cpu.run(),
        };
        let signal = match exit {
            Exit::Svc { .. } => match syscall::dispatch(&mut cpu, &mm) {
                Flow::Continue => continue,
                Flow::Exit(code) => break ExitStatus::Exited(code),
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
        break ExitStatus::Signaled {
            signal: signal.0,
            pc: cpu.state().pc,
            fault_addr: signal.1,
        };
    };
    Ok(RunReport {
        status,
        dbt_stats: dbt.map(|d| d.stats()),
    })
}
