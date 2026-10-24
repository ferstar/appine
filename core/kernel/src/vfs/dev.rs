//! 字符设备（设计文档 §8）。
//!
//! M3a 的 `/dev` 是 inode 仓库里的普通目录，设备节点按 Linux 的设备号建在里面
//! （[`ensure_dev_nodes`]），打开设备节点时按设备号找到这里的实现。guest 自己
//! 用 mknod 建的同号节点行为相同。终端设备随 PTY 一起在 M3b 实现。

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
            _ => Err(Errno::ENXIO),
        }
    }
}

/// `/dev` 里的标准节点：名字、设备号、权限。
const NODES: [(&[u8], u64, u32); 6] = [
    (b"null", rdev(1, 3), 0o666),
    (b"zero", rdev(1, 5), 0o666),
    (b"full", rdev(1, 7), 0o666),
    (b"random", rdev(1, 8), 0o666),
    (b"urandom", rdev(1, 9), 0o666),
    (b"tty", rdev(5, 0), 0o666),
];

/// `/dev` 里的标准符号链接，procfs 在 M3b 实现之前它们是悬空的，与真实系统的布局一致。
const LINKS: [(&[u8], &[u8]); 4] = [
    (b"fd", b"/proc/self/fd"),
    (b"stdin", b"/proc/self/fd/0"),
    (b"stdout", b"/proc/self/fd/1"),
    (b"stderr", b"/proc/self/fd/2"),
];

/// 确保 `/dev` 和其中的标准设备节点存在（导入 rootfs 之后、每次启动时调用）。
/// 已经存在的同名项保持不动。
pub fn ensure_dev_nodes(store: &Store) -> Result<(), Errno> {
    store.transaction(|s| {
        let dev = match s.lookup(ROOT, b"dev")? {
            Some(ino) => ino,
            None => s.mkdir(ROOT, b"dev", 0o755, 0, 0)?,
        };
        if !s.stat(dev)?.is_dir() {
            return Ok(());
        }
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
        for (name, perm) in [(&b"shm"[..], 0o1777), (b"pts", 0o755)] {
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
