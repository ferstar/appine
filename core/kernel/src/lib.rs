//! appine 内核层：在进程内实现 Linux 用户态 ABI（设计文档 §3、§6–§8）。
//!
//! M1b：单进程、单线程；加载静态或经由 `ld-musl` 动态链接的程序，根文件系统是
//! inode 仓库，提供文件、内存、时间等基础 syscall。

mod debug;
pub mod errno;
mod fd;
mod loader;
mod mm;
mod process;
mod syscall;
mod uaccess;
pub mod vfs;

pub use appine_engine::DbtStats;
pub use loader::LoadError;
pub use process::{ExitStatus, RunError, RunOptions, RunReport, Signal, Tier, run};

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
    vfs::import::import_tar_gz(&store, std::io::BufReader::new(file))
}
