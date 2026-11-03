//! 字符设备（kernel/vfs 的 map）。
//!
//! 开机时 `/dev` 上挂一个 tmpfs，标准的设备节点按 Linux 的设备号建在里面（[`populate`]，
//! 相当于 Linux 的 devtmpfs），打开设备节点时按设备号找到这里的实现；guest 自己用 mknod
//! 建的同号节点行为相同。仓库里的 `/dev` 目录是它的挂载点，里面也补齐同样的节点
//! （[`ensure_dev_nodes`]，卸下 `/dev` 之后看到的就是它们）。

use crate::errno::Errno;
use crate::vfs::store::{Ino, ROOT, Store, mode};

/// 进程内实现的字符设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// `/dev/null`：读到 EOF，写入全部丢弃。
    Null,
    /// `/dev/zero`：读到零，写入全部丢弃。
    Zero,
    /// `/dev/full`：读到零，写入失败（ENOSPC）。
    Full,
    /// `/dev/random`、`/dev/urandom`：宿主的随机数。
    Random,
    /// `/dev/kmsg`：内核日志（[`crate::klog`]），每个打开文件有自己的读位置。
    Kmsg,
}

/// Linux 的设备号编码（`new_encode_dev`）。
pub const fn rdev(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as u64
}

impl Device {
    /// 按设备号查找；没有实现的设备返回 ENXIO。
    pub fn from_rdev(rdev_value: u64) -> Result<Device, Errno> {
        match rdev_value {
            v if v == rdev(1, 3) => Ok(Device::Null),
            v if v == rdev(1, 5) => Ok(Device::Zero),
            v if v == rdev(1, 7) => Ok(Device::Full),
            v if v == rdev(1, 8) || v == rdev(1, 9) => Ok(Device::Random),
            v if v == rdev(1, 11) => Ok(Device::Kmsg),
            _ => Err(Errno::ENXIO),
        }
    }
}

/// `/dev` 里的标准节点：名字、设备号、权限。终端设备（tty、console、ptmx）由
/// syscall 层按设备号接到 PTY 上。
const NODES: [(&[u8], u64, u32); 9] = [
    (b"null", rdev(1, 3), 0o666),
    (b"zero", rdev(1, 5), 0o666),
    (b"full", rdev(1, 7), 0o666),
    (b"random", rdev(1, 8), 0o666),
    (b"urandom", rdev(1, 9), 0o666),
    (b"kmsg", rdev(1, 11), 0o644),
    (b"tty", rdev(5, 0), 0o666),
    (b"console", rdev(5, 1), 0o600),
    (b"ptmx", rdev(5, 2), 0o666),
];

/// `/dev` 里的标准符号链接，都指向 procfs。
const LINKS: [(&[u8], &[u8]); 4] = [
    (b"fd", b"/proc/self/fd"),
    (b"stdin", b"/proc/self/fd/0"),
    (b"stdout", b"/proc/self/fd/1"),
    (b"stderr", b"/proc/self/fd/2"),
];

/// `/dev` 里的标准目录：devpts、`/dev/shm` 的 tmpfs、mqueue 的挂载点。
const DIRS: [(&[u8], u32); 3] = [(b"pts", 0o755), (b"shm", 0o1777), (b"mqueue", 0o755)];

/// 确保 `/dev` 和其中的标准设备节点，以及 `/proc`、`/sys`、`/run` 这些挂载点存在（导入
/// rootfs 之后、每次启动时调用）。已经存在的同名项保持不动。
pub fn ensure_dev_nodes(store: &Store) -> Result<(), Errno> {
    store.transaction(|s| {
        for (name, perm) in [(&b"proc"[..], 0o555), (b"sys", 0o555), (b"run", 0o755)] {
            if s.lookup(ROOT, name)?.is_none() {
                s.mkdir(ROOT, name, perm, 0, 0)?;
            }
        }
        let dev = match s.lookup(ROOT, b"dev")? {
            Some(ino) => ino,
            None => s.mkdir(ROOT, b"dev", 0o755, 0, 0)?,
        };
        if !s.stat(dev)?.is_dir() {
            return Ok(());
        }
        populate(s, dev)
    })
}

/// 在目录 `dev` 里建好标准的设备节点、符号链接和目录（开机时挂在 `/dev` 上的 tmpfs、
/// 仓库里的 `/dev`）。已经存在的同名项保持不动。
pub fn populate(store: &Store, dev: Ino) -> Result<(), Errno> {
    store.transaction(|s| {
        let missing = |name: &[u8]| -> Result<bool, Errno> { Ok(s.lookup(dev, name)?.is_none()) };
        for (name, rdev_value, perm) in NODES {
            if missing(name)? {
                s.mknod(dev, name, mode::S_IFCHR | perm, rdev_value, 0, 0)?;
            }
        }
        for (name, target) in LINKS {
            if missing(name)? {
                s.symlink(dev, name, target, 0, 0)?;
            }
        }
        for (name, perm) in DIRS {
            if missing(name)? {
                s.mkdir(dev, name, perm, 0, 0)?;
            }
        }
        Ok(())
    })
}

/// 设备节点的 inode 对应的设备。
pub fn open(store: &Store, ino: Ino) -> Result<Device, Errno> {
    Device::from_rdev(store.stat(ino)?.rdev)
}
