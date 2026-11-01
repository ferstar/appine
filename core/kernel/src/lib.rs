//! appine 内核层：在进程内实现 Linux 用户态 ABI（设计文档 §3、§6–§8）。
//!
//! 每个 guest 线程对应一个宿主线程，内核层由一把大内核锁保护；宿主经由 [`Machine`]
//! 启动进程、开控制台（Android 的终端标签页、`appine-run`）。根文件系统是 inode 仓库。

mod bpf;
mod bug;
mod cell;
mod cgroup;
mod coredump;
mod cred;
mod debug;
pub mod errno;
mod fd;
mod futex;
mod ipc;
mod itimer;
mod kernel;
mod klog;
mod loadavg;
mod loader;
mod locks;
mod machine;
mod mm;
mod net;
mod netconf;
mod netdev;
mod netns;
mod nft;
mod nsproxy;
mod oom;
mod pidns;
mod pipe;
mod portmap;
mod process;
mod pseudo;
mod ptrace;
mod rawicmp;
mod route;
mod seccomp;
mod signal;
mod syscall;
mod task;
mod terminal;
mod traffic;
mod tty;
mod uaccess;
mod unix;
mod vdso;
pub mod vfs;

pub use appine_engine::DbtStats;
pub use appine_hal::net::Route as HostRoute;
pub use loader::LoadError;
pub use machine::{Machine, MachineOptions, Stdio, run};
pub use portmap::ListenEvent;
pub use process::{ExitStatus, RunError, RunOptions, RunReport, Signal, Tier};
pub use terminal::Terminal;

use std::path::Path;

use errno::Errno;
use vfs::import::ImportReport;
use vfs::store::Store;

/// 在 inode 仓库里写一个文件（宿主准备配置文件用，比如首次启动时的
/// `/etc/profile.d/appine.sh`）；`path` 是 guest 路径。见 [`vfs::write_file`]。
pub fn write_file(
    store: &Path,
    path: &str,
    data: &[u8],
    mode: u32,
    overwrite: bool,
) -> Result<(), Errno> {
    bug::host_call("write_file", || {
        let store = Store::open(store, &[])?;
        vfs::write_file(&store, path.as_bytes(), data, mode, overwrite)
    })
    .unwrap_or(Err(Errno::EIO))
}

/// 在宿主目录 `store` 新建 inode 仓库，并导入 rootfs 的 tar.gz（设计文档 §11.4）。
///
/// 目录里已经有仓库时返回 `EEXIST`。导入失败时仓库保持为空。
pub fn import_rootfs(store: &Path, tarball: &Path) -> Result<ImportReport, Errno> {
    bug::host_call("import_rootfs", || {
        let file = std::fs::File::open(tarball)
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
        let store = Store::create(store)?;
        let report = vfs::import::import_tar_gz(&store, std::io::BufReader::new(file))?;
        vfs::dev::ensure_dev_nodes(&store)?;
        Ok(report)
    })
    .unwrap_or(Err(Errno::EIO))
}
