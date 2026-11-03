//! appine 内核层：在进程内实现 Linux 用户态 ABI（设计文档 §3 和各模块的 map）。
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
mod forward;
mod futex;
mod inittab;
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
mod origdst;
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
pub use bug::{KillReason, KillReport};
pub use inittab::Power;
pub use loader::LoadError;
pub use machine::{Machine, MachineOptions, SHUTDOWN_TIMEOUT, Stdio, run};
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

/// 读 inode 仓库里的一个普通文件（宿主看配置文件用）；`path` 是 guest 路径。见 [`vfs::read_file`]。
pub fn read_file(store: &Path, path: &str) -> Result<Vec<u8>, Errno> {
    bug::host_call("read_file", || {
        let store = Store::open(store, &[])?;
        vfs::read_file(&store, path.as_bytes())
    })
    .unwrap_or(Err(Errno::EIO))
}

/// 宿主浏览仓库（App 的 DocumentsProvider，android/documents 的 map）：另外打开一次仓库，对它调用
/// `f`（[`vfs::browse`] 里的函数）。内核运行着也可以：另外打开仓库时内核先提交推迟的写事务，
/// 看到的是最新的；这个连接开着期间内核不再推迟提交，所以每次用完就关，不长期开着。
pub fn browse<R>(store: &Path, f: impl FnOnce(&Store) -> Result<R, Errno>) -> Result<R, Errno> {
    bug::host_call("browse", || {
        let store = Store::open(store, &[])?;
        f(&store)
    })
    .unwrap_or(Err(Errno::EIO))
}

/// 宿主修改仓库（App 的 DocumentsProvider 新建、删除、改名、移动、写）：与 [`browse`] 一样
/// 另外打开一次仓库，`f` 把改了什么记进 `changes`（[`vfs::browse::Change`]）。仓库关闭
/// （提交）之后，把它们交给用着同一个仓库、正在运行的内核，由它报 inotify 事件
/// （[`machine::store_changed`]）：guest 里监视着的程序知道"文件"App 改了东西。中途出错
/// 时已经做了的照样交。
pub fn browse_changing<R>(
    store: &Path,
    f: impl FnOnce(&Store, &mut Vec<vfs::browse::Change>) -> Result<R, Errno>,
) -> Result<R, Errno> {
    bug::host_call("browse", || {
        let mut changes = Vec::new();
        let r = Store::open(store, &[]).and_then(|s| f(&s, &mut changes));
        machine::store_changed(store, &changes);
        r
    })
    .unwrap_or(Err(Errno::EIO))
}

/// 把宿主目录 `store` 里的 inode 仓库（连同所在的整个宿主文件系统）写到盘上：导入
/// rootfs、写好配置之后，改名就位之前（android/setup 的 map）。不然断电之后仓库在了，里面的
/// object 却可能还是空的。
pub fn sync_store(store: &Path) -> Result<(), Errno> {
    let dir =
        std::fs::File::open(store).map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    appine_hal::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)).map_err(Errno)
}

/// 在宿主目录 `store` 新建 inode 仓库，并导入 rootfs 的 tar.gz（android/setup 的 map）。
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
