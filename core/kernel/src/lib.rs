//! appine 内核层：在进程内实现 Linux 用户态 ABI（设计文档 §3、§6–§8）。
//!
//! M0 只有最小骨架：加载静态 ELF、单线程运行循环、`write`/`exit`/`exit_group`。

mod debug;
pub mod errno;
mod loader;
mod mm;
mod process;
mod syscall;
mod uaccess;

pub use appine_engine::DbtStats;
pub use loader::LoadError;
pub use process::{ExitStatus, RunError, RunOptions, RunReport, Signal, Tier, run};
