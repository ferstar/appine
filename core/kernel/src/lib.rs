//! appine 内核层：在进程内实现 Linux 用户态 ABI（设计文档 §3、§6–§8）。
//!
//! M3a：多进程、多线程。每个 guest 线程对应一个宿主线程，内核层由一把大内核锁
//! 保护；支持 fork/vfork/线程、execve、wait、管道、完整的信号投递和 vDSO。根文件
//! 系统是 inode 仓库。

mod debug;
pub mod errno;
mod fd;
mod futex;
mod itimer;
mod kernel;
mod loader;
mod mm;
mod net;
mod pipe;
mod process;
mod pseudo;
mod signal;
mod syscall;
mod task;
mod terminal;
mod tty;
mod uaccess;
mod unix;
mod vdso;
pub mod vfs;

pub use appine_engine::DbtStats;
pub use loader::LoadError;
pub use process::{ExitStatus, RunError, RunOptions, RunReport, Signal, Tier, run};
pub use terminal::Terminal;

use std::path::Path;

use errno::Errno;
use vfs::import::ImportReport;
use vfs::store::Store;

/// 在宿主目录 `store` 新建 inode 仓库，并导入 rootfs 的 tar.gz（设计文档 §11.4）。
///
/// 目录里已经有仓库时返回 `EEXIST`。导入失败时仓库保持为空。
pub fn import_rootfs(store: &Path, tarball: &Path) -> Result<ImportReport, Errno> {
    let file = std::fs::File::open(tarball)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    let store = Store::create(store)?;
    let report = vfs::import::import_tar_gz(&store, std::io::BufReader::new(file))?;
    vfs::dev::ensure_dev_nodes(&store)?;
    Ok(report)
}
