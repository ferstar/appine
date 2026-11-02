//! 文件与目录类 syscall。
//!
//! 读写分两层：`read_bytes`/`write_bytes` 对一个打开文件做一次宿主缓冲区上的操作，
//! `read_file`/`write_file` 负责和 guest 内存之间的拷贝。宿主 fd 和管道可能阻塞，
//! 阻塞期间释放 BKL，并且不持有任何打开文件的借用（别的线程可能在用同一个打开文件）。

use std::os::unix::fs::FileExt;
use std::rc::{Rc, Weak};
use std::time::Duration;

use super::SocketRef;
use super::{IO_CHUNK, MAX_COPY, SysResult, get, inotify, put, read_path, tty};
use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC, MAY_READ, MAY_WRITE, cap};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, TimerFd, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_locks, request_wake_object, wake};
use crate::locks::{self, Lock, Owner, Waiter};
use crate::pipe::{PIPE_BUF, PIPE_CAPACITY, Pipe, PipeEnd};
use crate::process::{RLIMIT_FSIZE, Signal};
use crate::pseudo::{self, PseudoNode};
use crate::signal::{self, SigInfo};
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::{self, Device, rdev};
use crate::vfs::store::{DirEntry, FileTime, Ino, Stat, Store, mode};
use crate::vfs::{Dentry, Fs, MountId, Node, Pseudo, TMPFS_PAGE, Vfs, ms, normalize};

pub(super) const AT_FDCWD: i32 = -100;
pub(super) const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_REMOVEDIR: u32 = 0x200;
const AT_EACCESS: u32 = 0x200;
const AT_SYMLINK_FOLLOW: u32 = 0x400;
const AT_EMPTY_PATH: u32 = 0x1000;

const F_DUPFD: u32 = 0;
const F_GETFD: u32 = 1;
const F_SETFD: u32 = 2;
const F_GETFL: u32 = 3;
const F_SETFL: u32 = 4;
const F_GETLK: u32 = 5;
const F_SETLK: u32 = 6;
const F_SETLKW: u32 = 7;
const F_OFD_GETLK: u32 = 36;
const F_OFD_SETLK: u32 = 37;
const F_OFD_SETLKW: u32 = 38;
const F_SETOWN: u32 = 8;
const F_GETOWN: u32 = 9;
const F_SETSIG: u32 = 10;
const F_GETSIG: u32 = 11;
const F_SETOWN_EX: u32 = 15;
const F_GETOWN_EX: u32 = 16;
const F_OWNER_TID: i32 = 0;
const F_OWNER_PID: i32 = 1;
const F_OWNER_PGRP: i32 = 2;
const F_SETLEASE: u32 = 1024;
const F_GETLEASE: u32 = 1025;
const F_DUPFD_CLOEXEC: u32 = 1030;
const F_SETPIPE_SZ: u32 = 1031;
const F_GETPIPE_SZ: u32 = 1032;
const F_ADD_SEALS: u32 = 1033;
const F_GET_SEALS: u32 = 1034;
const FD_CLOEXEC: u64 = 1;

/// memfd 的封印。
const F_SEAL_SEAL: u32 = 0x1;
const F_SEAL_SHRINK: u32 = 0x2;
const F_SEAL_GROW: u32 = 0x4;
const F_SEAL_WRITE: u32 = 0x8;
const F_SEAL_FUTURE_WRITE: u32 = 0x10;
const F_SEAL_EXEC: u32 = 0x20;
const F_SEAL_ALL: u32 = 0x3f;
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;
const SEEK_DATA: u32 = 3;
const SEEK_HOLE: u32 = 4;

const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;

const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

const CLOSE_RANGE_UNSHARE: u32 = 2;
const CLOSE_RANGE_CLOEXEC: u32 = 4;

/// readv/writev 最多接受的 iovec 个数（`UIO_MAXIOV`）。
const UIO_MAXIOV: usize = 1024;

pub(super) use crate::vfs::ROOT_DEV;
/// 管道报告的设备号（Linux 上 pipefs 是一个匿名设备）。
const PIPE_DEV: u64 = 0xd;
/// socket、anon_inode（epoll、eventfd）报告的设备号。
const SOCK_DEV: u64 = 0x8;
const ANON_DEV: u64 = 0xe;
/// statfs 报告的文件系统类型。
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const MQUEUE_MAGIC: u64 = 0x1980_0202;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const NSFS_MAGIC: u64 = 0x6e73_6673;
const PIPEFS_MAGIC: u64 = 0x5049_5045;
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;
const SYSFS_MAGIC: u64 = 0x6265_6572;
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;
/// PTY 主设备的设备号（`/dev/ptmx`）。
const PTMX_RDEV: u64 = rdev(5, 2);

/// 可以透传给宿主终端的 ioctl：编号、参数大小、是否是输出参数。
const TTY_IOCTLS: [(u32, usize, bool); 14] = [
    (0x5401, 36, true),       // TCGETS
    (0x5402, 36, false),      // TCSETS
    (0x5403, 36, false),      // TCSETSW
    (0x5404, 36, false),      // TCSETSF
    (0x5405, 18, true),       // TCGETA
    (0x5406, 18, false),      // TCSETA
    (0x5407, 18, false),      // TCSETAW
    (0x5408, 18, false),      // TCSETAF
    (0x5413, 8, true),        // TIOCGWINSZ
    (0x5414, 8, false),       // TIOCSWINSZ
    (0x802c_542a, 44, true),  // TCGETS2
    (0x402c_542b, 44, false), // TCSETS2
    (0x402c_542c, 44, false), // TCSETSW2
    (0x402c_542d, 44, false), // TCSETSF2
];
const FIONREAD: u32 = 0x541b;
const RNDGETENTCNT: u32 = 0x8004_5200;
const RNDADDTOENTCNT: u32 = 0x4004_5201;
const RNDADDENTROPY: u32 = 0x4008_5203;
const RNDZAPENTCNT: u32 = 0x5204;
const RNDCLEARPOOL: u32 = 0x5206;
const RNDRESEEDCRNG: u32 = 0x5207;
/// 熵池里的比特数（`/proc/sys/kernel/random/entropy_avail`）。
pub(crate) const ENTROPY: i32 = 256;

/// `/dev/random`、`/dev/urandom` 的 ioctl（Linux 6.1 的 `random_ioctl`）。随机数取自宿主，
/// 与 Linux 5.18 起一样熵池总是满的：加进来的数据和熵只检查参数、读一遍，不起作用（seedrng
/// 开机时用 `RNDADDENTROPY` 把上次存的种子加回去）。改熵池的要开机时的命名空间里的
/// `CAP_SYS_ADMIN`；不认识的请求是 EINVAL（tty 的请求也是，与 Linux 一样不是 ENOTTY）。
fn random_ioctl(cx: &mut Cx<'_>, request: u32, arg: u64) -> SysResult {
    let admin = || {
        if cx.cred().capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    };
    match request {
        RNDGETENTCNT => put(cx, arg, ENTROPY.to_le_bytes()).map(|()| 0),
        RNDADDTOENTCNT => {
            admin()?;
            if i32::from_le_bytes(get(cx, arg)?) < 0 {
                return Err(Errno::EINVAL);
            }
            Ok(0)
        }
        // struct rand_pool_info：熵的比特数、数据的字节数，后面是数据；数据要都读得到
        RNDADDENTROPY => {
            admin()?;
            if i32::from_le_bytes(get(cx, arg)?) < 0 {
                return Err(Errno::EINVAL);
            }
            let len = i32::from_le_bytes(get(cx, arg + 4)?);
            let len = usize::try_from(len).map_err(|_| Errno::EFAULT)?;
            let mut buf = [0u8; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(buf.len());
                cx.mm()
                    .read(UserSlice::new(arg + 8 + done as u64, n), &mut buf[..n])?;
                done += n;
            }
            Ok(0)
        }
        RNDZAPENTCNT | RNDCLEARPOOL | RNDRESEEDCRNG => admin().map(|()| 0),
        _ => Err(Errno::EINVAL),
    }
}
const FIONBIO: u32 = 0x5421;
const FIOASYNC: u32 = 0x5452;
const FIOSETOWN: u32 = 0x8901;
const SIOCSPGRP: u32 = 0x8902;
const FIOGETOWN: u32 = 0x8903;
const SIOCGPGRP: u32 = 0x8904;
const FIONCLEX: u32 = 0x5450;
const FIOCLEX: u32 = 0x5451;
const FICLONE: u32 = 0x4004_9409;
const FICLONERANGE: u32 = 0x4020_940d;
const FIDEDUPERANGE: u32 = 0xc018_9436;

const POLLIN: i16 = 0x001;

fn host_errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(Errno::EIO.0))
}

// ---- 路径 ----

/// `*at` 系列的起点目录。路径为空时直接 ENOENT（不看 dirfd），与 Linux 相同。
pub(super) fn start_dir(cx: &Cx<'_>, dirfd: i32, path: &[u8]) -> Result<Node, Errno> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    if path.first() == Some(&b'/') {
        return Ok(cx.root());
    }
    if dirfd == AT_FDCWD {
        return Ok(cx.cwd());
    }
    let file = cx.fds().get(dirfd)?;
    let node = file.borrow().node().ok_or(Errno::ENOTDIR)?;
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    Ok(node)
}

/// 解析 `*at` 系列的路径参数。
pub(super) fn lookup_at(cx: &Cx<'_>, dirfd: i32, path: &[u8], follow: bool) -> Result<Node, Errno> {
    resolve_at(cx, &cx.cred(), dirfd, path, follow)
}

/// 以凭据 `cred` 解析 `*at` 系列的路径参数。
fn resolve_at(
    cx: &Cx<'_>,
    cred: &Cred,
    dirfd: i32,
    path: &[u8],
    follow: bool,
) -> Result<Node, Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    vfs.resolve(&cx.view(&vfs), cred, &cx.root(), &start, path, follow)
}

/// `*at` 系列里"路径为空时作用于 dirfd 本身"（`AT_EMPTY_PATH`）的公共部分。
fn lookup_at_or_fd(
    cx: &Cx<'_>,
    cred: &Cred,
    dirfd: i32,
    path: &[u8],
    flags: u32,
    follow: bool,
) -> Result<Node, Errno> {
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            return Ok(cx.cwd());
        }
        return cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::EBADF);
    }
    resolve_at(cx, cred, dirfd, path, follow)
}

/// 解析要新建或删除的路径：返回父目录经由的挂载、父目录和最后一个组件。只有仓库里的
/// 目录可以修改；挂载是不是只读的由调用方按 Linux 的顺序检查。
pub(super) fn parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<(MountId, Ino, &'p [u8]), Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    let (mnt, dir) = store_dir(&vfs, &dir)?;
    Ok((mnt, dir, name))
}

/// 要新建的名字的父目录（[`parent_at`]）：合成文件系统的目录里名字已经存在时是 EEXIST，
/// 先于目录不能修改的错误（Linux 的 `filename_create`：`mkdir -p /sys/fs/cgroup/x` 要靠
/// `mkdir /sys/fs` 是 EEXIST）。
pub(super) fn create_parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<(MountId, Ino, &'p [u8]), Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    if matches!(dir, Node::Pseudo(..)) && vfs.has_child(&cx.view(&vfs), &dir, name) {
        return Err(Errno::EEXIST);
    }
    let (mnt, dir) = store_dir(&vfs, &dir)?;
    Ok((mnt, dir, name))
}

/// 要在里面新建、删除的目录：仓库里的；合成文件系统里的不能修改，在只读的挂载里
/// （比如 cgroup2）是 EROFS。mqueue 文件系统的根目录里只能新建、删除队列（openat、
/// unlinkat 另外处理），别的（mkdir、mknod、symlink、link、rename）与 Linux 一样是 EPERM：
/// 它的目录没有这些操作。
fn store_dir(vfs: &Vfs, dir: &Node) -> Result<(MountId, Ino), Errno> {
    match dir {
        Node::Store(mnt, ino) => Ok((*mnt, *ino)),
        Node::Pseudo(..) if vfs.readonly(dir) => Err(Errno::EROFS),
        Node::Pseudo(_, PseudoNode::MqRoot(_) | PseudoNode::Cgroup(..)) => Err(Errno::EPERM),
        Node::Pseudo(..) => Err(Errno::EACCES),
    }
}

/// IPC 命名空间 `ns` 的 mqueue 文件系统里 inode 编号是 `ino` 的队列：删掉了的是 ENOENT。
fn mq_queue(cx: &Cx<'_>, ns: u64, ino: u64) -> Result<Rc<RefCell<super::MessageQueue>>, Errno> {
    super::mqueue::find(cx.kr().ipc(ns), ino)
        .map(|(_, q)| q.clone())
        .ok_or(Errno::ENOENT)
}

/// 在目录 `dir` 里新建 `name` 之前对名字和挂载的检查（Linux 的 `filename_create`）：要能
/// 在目录里查找，名字不存在，然后才是只读的挂载（EROFS）。返回目录的元数据。
fn may_create_name(
    vfs: &Vfs,
    cred: &Cred,
    (mnt, dir): (MountId, Ino),
    name: &[u8],
) -> Result<Stat, Errno> {
    let store = vfs.store();
    let st = store.stat(dir)?;
    cred.permission(&st, MAY_EXEC)?;
    if store.lookup(dir, name)?.is_some() {
        return Err(Errno::EEXIST);
    }
    if vfs.readonly(&Node::Store(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    Ok(st)
}

/// 在目录 `dir` 里新建 `name` 之前的检查（`may_create`）：[`may_create_name`]，再要求
/// 目录可写。返回目录的元数据。
pub(super) fn may_create(
    vfs: &Vfs,
    cred: &Cred,
    dir: (MountId, Ino),
    name: &[u8],
) -> Result<Stat, Errno> {
    let st = may_create_name(vfs, cred, dir, name)?;
    cred.permission(&st, MAY_WRITE | MAY_EXEC)?;
    Ok(st)
}

/// 删除、改名之前：只读的挂载里是 EROFS（Linux 的 `mnt_want_write`，在查找名字之前）。
fn writable_dir(vfs: &Vfs, (mnt, dir): (MountId, Ino)) -> Result<(), Errno> {
    if vfs.readonly(&Node::Store(mnt, dir)) {
        return Err(Errno::EROFS);
    }
    Ok(())
}

/// 要新建的不是目录、名字却以 `/` 结尾：已经存在时是 EEXIST，否则 ENOENT（与 Linux
/// 的 `filename_create` 一样）。
fn reject_trailing_slash(store: &Store, dir: Ino, name: &[u8], path: &[u8]) -> Result<(), Errno> {
    if !path.ends_with(b"/") {
        return Ok(());
    }
    Err(match store.lookup(dir, name)? {
        Some(_) => Errno::EEXIST,
        None => Errno::ENOENT,
    })
}

/// 删除或者移走目录 `dir` 里的 `name` 之前的检查（`may_delete`），返回它的元数据。
fn may_delete(store: &Store, cred: &Cred, dir: Ino, name: &[u8]) -> Result<Stat, Errno> {
    let dir_st = store.stat(dir)?;
    cred.permission(&dir_st, MAY_EXEC)?;
    let victim = store.stat(store.lookup(dir, name)?.ok_or(Errno::ENOENT)?)?;
    cred.may_delete(&dir_st, &victim)?;
    Ok(victim)
}

/// fchmod、fchown 等作用的打开文件在仓库里的 inode：`O_PATH` 打开的文件不行，只读的
/// 挂载里是 EROFS（Linux 的 `mnt_want_write_file`）。
fn file_ino(cx: &Cx<'_>, fd: i32) -> Result<Ino, Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    if f.flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    let node = f.node().ok_or(Errno::EBADF)?;
    let ino = node.ino().ok_or(Errno::EBADF)?;
    if cx.vfs()?.readonly(&node) {
        return Err(Errno::EROFS);
    }
    Ok(ino)
}

/// 要修改的仓库里的 inode：只读的挂载里（包括总是只读的 cgroup2）是 EROFS，合成文件系统
/// 里别的节点不能修改。
fn store_ino(cx: &Cx<'_>, node: &Node) -> Result<Ino, Errno> {
    if cx.vfs()?.readonly(node) {
        return Err(Errno::EROFS);
    }
    node.ino().ok_or(Errno::EACCES)
}

/// 打开时记下的绝对路径（`/proc/<pid>/fd/<n>`、maps）：与 Linux 的 `d_path` 一样解析完
/// 符号链接，按最后找到文件的目录和名字 `found` 算。valgrind 按 `/proc/self/fd` 里的名字
/// 带着 `O_NOFOLLOW` 重新打开 Debian 的 `/lib/ld-linux-aarch64.so.1`（最后一级是链接）。
/// 经由 `/proc/<pid>/fd/<n>` 这种链接到达、不知道名字的文件按词法规范化的路径。
fn display_path(
    cx: &Cx<'_>,
    vfs: &Vfs,
    start: &Node,
    path: &[u8],
    node: &Node,
    found: Option<&Dentry>,
) -> Option<Rc<[u8]>> {
    if let Ok(p) = vfs.found_path(&cx.view(vfs), &cx.root(), node, found) {
        return Some(Rc::from(p));
    }
    let full = if path.first() == Some(&b'/') {
        path.to_vec()
    } else {
        let mut p = vfs.path_of(&cx.view(vfs), &cx.root(), start).ok()?;
        p.push(b'/');
        p.extend_from_slice(path);
        p
    };
    Some(Rc::from(normalize(&full)))
}

// ---- stat ----

fn split_ns(ns: i64) -> (u64, u64) {
    (
        ns.div_euclid(1_000_000_000) as u64,
        ns.rem_euclid(1_000_000_000) as u64,
    )
}

/// 按 asm-generic 的 `struct stat`（128 字节）编码。
pub(super) fn encode_stat(st: &Stat, dev: u64) -> [u8; 128] {
    let mut b = [0u8; 128];
    let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    let (asec, ansec) = split_ns(st.atime_ns);
    let (msec, mnsec) = split_ns(st.mtime_ns);
    let (csec, cnsec) = split_ns(st.ctime_ns);
    put(0, &dev.to_le_bytes());
    put(8, &st.ino.to_le_bytes());
    put(16, &st.mode.to_le_bytes());
    put(20, &st.nlink.to_le_bytes());
    put(24, &st.uid.to_le_bytes());
    put(28, &st.gid.to_le_bytes());
    put(32, &st.rdev.to_le_bytes());
    // 仓库里的目录没有数据，按 ext4 的习惯报告一个块
    let size = if st.is_dir() && dev == ROOT_DEV {
        4096
    } else {
        st.size
    };
    put(48, &size.to_le_bytes());
    put(56, &4096u32.to_le_bytes());
    // sysfs 的文件报告一页的大小，但不占块。短的符号链接存在 inode 里、不占块：ext4 是目标
    // 短于 60 字节（`ext4_symlink`，连同结尾的 NUL 放得进 `i_data`），tmpfs 这类别的是 128
    // 字节以内（`shmem_symlink` 的 `SHORT_SYMLINK_LEN`）
    let inline = if dev == ROOT_DEV { 60 } else { 128 };
    let blocks = if dev == Fs::Sys.dev() || (st.is_symlink() && size < inline) {
        0
    } else {
        size.div_ceil(4096) * 8
    };
    put(64, &blocks.to_le_bytes());
    put(72, &asec.to_le_bytes());
    put(80, &ansec.to_le_bytes());
    put(88, &msec.to_le_bytes());
    put(96, &mnsec.to_le_bytes());
    put(104, &csec.to_le_bytes());
    put(112, &cnsec.to_le_bytes());
    b
}

/// fstat 会报告的 (st_dev, st_ino)，不必查仓库就能知道的部分（epoll 的 fdinfo）。合成
/// 节点的设备号按开机时的实例算（后来挂上的 procfs、devpts 实例另有设备号，[`Vfs::dev_of`]）。
pub(crate) fn dev_ino(f: &OpenFile) -> (u64, u64) {
    match (&f.kind, f.node()) {
        (_, Some(Node::Store(_, ino))) => (ROOT_DEV, ino),
        (FileKind::Pipe(end), None) => (PIPE_DEV, end.pipe.borrow().ino),
        (FileKind::Socket(s), _) => (SOCK_DEV, pseudo::socket_ino(s)),
        (_, Some(Node::Pseudo(_, p))) => (pseudo::fs_of(&p).dev(), pseudo::ino_of(&p)),
        _ => (ANON_DEV, 1),
    }
}

/// 文件系统节点的元数据。
fn node_stat(cx: &Cx<'_>, node: &Node) -> Result<[u8; 128], Errno> {
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    match node {
        Node::Store(_, ino) => Ok(encode_stat(&vfs.store().stat(*ino)?, vfs.dev_of(node))),
        // 命名空间在 nsfs 里
        Node::Pseudo(_, p @ PseudoNode::NsFile(..)) => {
            Ok(encode_stat(&pseudo::stat(&view, p)?, Fs::Nsfs.dev()))
        }
        // 魔法链接指向的打开文件：报告那个文件本身
        Node::Pseudo(_, PseudoNode::OpenFd(pid, fd)) => {
            let file = view.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
            file_stat(cx, &file)
        }
        // 已经删除的程序文件
        Node::Pseudo(_, PseudoNode::OpenExe(pid)) => {
            let exe = exe_file(cx, *pid)?;
            let (Some((mnt, ino)), Some(host)) = (exe.node, &exe.host) else {
                return Err(Errno::ENOENT);
            };
            let st = vfs.store().stat_open(ino, host)?;
            Ok(encode_stat(&st, vfs.dev_of(&Node::Store(mnt, ino))))
        }
        Node::Pseudo(_, p) => Ok(encode_stat(&pseudo::stat(&view, p)?, vfs.dev_of(node))),
    }
}

/// 宿主 fd 的元数据。
// st_* 字段的类型在不同宿主上宽度不同
#[allow(clippy::unnecessary_cast)]
fn host_stat(fd: i32) -> Result<[u8; 128], Errno> {
    let s = appine_hal::fstat(fd).map_err(Errno)?;
    let st = Stat {
        ino: s.st_ino as u64,
        mode: s.st_mode as u32,
        nlink: s.st_nlink as u32,
        uid: s.st_uid,
        gid: s.st_gid,
        rdev: s.st_rdev as u64,
        size: s.st_size as u64,
        atime_ns: s.st_atime as i64 * 1_000_000_000 + s.st_atime_nsec as i64,
        mtime_ns: s.st_mtime as i64 * 1_000_000_000 + s.st_mtime_nsec as i64,
        ctime_ns: s.st_ctime as i64 * 1_000_000_000 + s.st_ctime_nsec as i64,
    };
    Ok(encode_stat(&st, s.st_dev as u64))
}

fn file_stat(cx: &Cx<'_>, file: &FileRef) -> Result<[u8; 128], Errno> {
    let f = file.borrow();
    let anon = |ino: u64, mode: u32, rdev: u64, dev: u64| {
        let st = Stat {
            ino,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        };
        encode_stat(&st, dev)
    };
    match &f.kind {
        FileKind::Host(fd) => host_stat(*fd),
        FileKind::Pipe(end) => match end.pipe.borrow().fifo {
            Some(ino) => {
                let vfs = cx.vfs()?;
                let dev = vfs.dev_of(&Node::Store(f.mnt, ino));
                Ok(encode_stat(&vfs.store().stat(ino)?, dev))
            }
            None => Ok(anon(
                end.pipe.borrow().ino,
                mode::S_IFIFO | 0o600,
                0,
                PIPE_DEV,
            )),
        },
        FileKind::Pty(end) if end.master => Ok(anon(2, mode::S_IFCHR | 0o666, PTMX_RDEV, ROOT_DEV)),
        FileKind::Socket(s) => Ok(anon(
            pseudo::socket_ino(s),
            mode::S_IFSOCK | 0o777,
            0,
            SOCK_DEV,
        )),
        // anon_inode：没有文件类型位
        FileKind::Epoll(_)
        | FileKind::EventFd(_)
        | FileKind::TimerFd(_)
        | FileKind::SignalFd(_)
        | FileKind::Inotify(_)
        | FileKind::Pidfd(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => Ok(anon(1, 0o600, 0, ANON_DEV)),
        // 设备号是队列所在的 IPC 命名空间的 mqueue 文件系统的
        FileKind::MessageQueue(q) => {
            let q = q.borrow();
            Ok(encode_stat(&q.stat(), cx.vfs()?.mq_dev(q.ns)))
        }
        FileKind::Pty(end) => {
            let t = end.tty.borrow();
            let st = Stat {
                ino: pseudo::ino_of(&PseudoNode::Pts(t.fs, t.index)),
                mode: mode::S_IFCHR | t.mode,
                nlink: 1,
                uid: t.owner.0,
                gid: t.owner.1,
                rdev: rdev(PTS_MAJOR, t.index),
                atime_ns: t.atime_ns,
                mtime_ns: t.mtime_ns,
                ctime_ns: t.ctime_ns,
                ..Stat::default()
            };
            // 设备号是终端所在的 devpts 实例的（ttyname 拿它比对 /dev/pts/<n> 的）
            Ok(encode_stat(&st, t.fs))
        }
        FileKind::Regular { ino, file } => {
            let vfs = cx.vfs()?;
            let dev = vfs.dev_of(&Node::Store(f.mnt, *ino));
            Ok(encode_stat(&vfs.store().stat_open(*ino, file)?, dev))
        }
        _ => {
            let node = f.node().ok_or(Errno::EBADF)?;
            drop(f);
            node_stat(cx, &node)
        }
    }
}

// ---- 打开、关闭、复制 ----

pub(super) fn openat(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    let file = open_path(cx, dirfd, path, flags, perm)?;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// openat2（Linux 6.1 的 `fs/open.c`）：`struct open_how` 的 flags、mode、resolve。与 openat
/// 不同，不认识的标志、不该有的 mode 都是 EINVAL，路径解析带着 `resolve` 的限制
/// （[`crate::vfs::resolve`]）。
pub(super) fn openat2(cx: &mut Cx<'_>, dirfd: i32, path: u64, how: u64, size: usize) -> SysResult {
    use crate::vfs::resolve;
    const OPEN_HOW_SIZE_VER0: usize = 24;
    /// `FMODE_NONOTIFY`：用户态不该给，与 `O_CLOEXEC` 一起去掉、不算错。
    const FMODE_NONOTIFY: u64 = 0x400_0000;
    const O_SYNC_BIT: u64 = 0o4000000; // __O_SYNC
    const O_DSYNC: u64 = 0o10000;
    const O_NOATIME: u64 = oflags::O_NOATIME as u64;
    const O_TMPFILE_BIT: u64 = 0o20000000; // __O_TMPFILE
    const VALID_OPEN_FLAGS: u64 = (oflags::O_ACCMODE
        | oflags::O_CREAT
        | oflags::O_EXCL
        | oflags::O_NOCTTY
        | oflags::O_TRUNC
        | oflags::O_APPEND
        | oflags::O_NONBLOCK
        | oflags::O_ASYNC
        | oflags::O_DIRECT
        | oflags::O_LARGEFILE
        | oflags::O_DIRECTORY
        | oflags::O_NOFOLLOW
        | oflags::O_CLOEXEC
        | oflags::O_PATH) as u64
        | O_SYNC_BIT
        | O_DSYNC
        | O_NOATIME
        | O_TMPFILE_BIT;
    const O_PATH_FLAGS: u64 =
        (oflags::O_DIRECTORY | oflags::O_NOFOLLOW | oflags::O_PATH | oflags::O_CLOEXEC) as u64;
    if size < OPEN_HOW_SIZE_VER0 {
        return Err(Errno::EINVAL);
    }
    if size > 4096 {
        return Err(Errno::E2BIG);
    }
    // copy_struct_from_user：比 24 字节长的部分必须全是 0（以后的版本加的字段）
    let mut raw = vec![0u8; size];
    cx.mm().read(UserSlice::new(how, size), &mut raw)?;
    if raw[OPEN_HOW_SIZE_VER0..].iter().any(|&b| b != 0) {
        return Err(Errno::E2BIG);
    }
    let field = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (mut flags, mode, res) = (field(0), field(1), field(2));
    // O_LARGEFILE 只给不是 O_PATH 的
    if flags & oflags::O_PATH as u64 == 0 {
        flags |= oflags::O_LARGEFILE as u64;
    }
    // build_open_flags 的检查，按它的顺序
    let f = flags & !(oflags::O_CLOEXEC as u64 | FMODE_NONOTIFY);
    if f & !VALID_OPEN_FLAGS != 0 || res & !resolve::ALL != 0 {
        return Err(Errno::EINVAL);
    }
    if res & resolve::SCOPED == resolve::SCOPED {
        return Err(Errno::EINVAL);
    }
    let will_create = f & (oflags::O_CREAT as u64 | O_TMPFILE_BIT) != 0;
    if (will_create && mode & !0o7777 != 0) || (!will_create && mode != 0) {
        return Err(Errno::EINVAL);
    }
    // O_TMPFILE 必须带着 O_DIRECTORY、不带 O_CREAT，还要能写
    let o_tmpfile = O_TMPFILE_BIT | oflags::O_DIRECTORY as u64;
    if f & O_TMPFILE_BIT != 0
        && (f & (o_tmpfile | oflags::O_CREAT as u64) != o_tmpfile
            || f & oflags::O_ACCMODE as u64 == oflags::O_RDONLY as u64)
    {
        return Err(Errno::EINVAL);
    }
    if f & oflags::O_PATH as u64 != 0 && f & !O_PATH_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    // RESOLVE_CACHED：新建、截断、O_TMPFILE（它含 O_DIRECTORY，所以 O_DIRECTORY 也算）的
    // 不试；别的照常解析，appine 的解析不等宿主的 I/O 以外的东西
    let busy = (oflags::O_TRUNC | oflags::O_CREAT | oflags::O_DIRECTORY) as u64 | O_TMPFILE_BIT;
    if res & resolve::CACHED != 0 && f & busy != 0 {
        return Err(Errno::EAGAIN);
    }
    let path = read_path(cx, path)?;
    let flags = flags as u32;
    let file = open_path_with(cx, dirfd, path, flags, mode as u32, res)?;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// openat2 的起点和根目录（path_init）。有作用域（`BENEATH`、`IN_ROOT`）时根目录就是起点；
/// `IN_ROOT` 的绝对路径也从 dirfd 起；别的绝对路径不看 dirfd（`BENEATH` 的由解析报 EXDEV）。
fn openat2_start(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u64) -> Result<(Node, Node), Errno> {
    use crate::vfs::resolve;
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    if path[0] == b'/' && flags & resolve::IN_ROOT == 0 {
        let root = cx.root();
        return Ok((root.clone(), root));
    }
    let start = if dirfd == AT_FDCWD {
        cx.cwd()
    } else {
        let file = cx.fds().get(dirfd)?;
        let node = file.borrow().node().ok_or(Errno::ENOTDIR)?;
        let vfs = cx.vfs()?;
        if !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        node
    };
    let root = if flags & resolve::SCOPED != 0 {
        start.clone()
    } else {
        cx.root()
    };
    Ok((start, root))
}

/// 内核自己新建一个文件（core dump）：相对当前目录，已经存在时是 EEXIST，不跟随符号
/// 链接，只写打开。
pub(crate) fn create_exclusive(cx: &mut Cx<'_>, path: &[u8], perm: u32) -> Result<FileRef, Errno> {
    let flags = oflags::O_CREAT | oflags::O_EXCL | oflags::O_NOFOLLOW | oflags::O_WRONLY;
    open_path(cx, AT_FDCWD, path.to_vec(), flags, perm)
}

/// 内核自己删除一个文件（core dump 之前删掉同名的旧文件），相对当前目录。
pub(crate) fn unlink_path(cx: &mut Cx<'_>, path: &[u8]) -> Result<(), Errno> {
    unlink(cx, AT_FDCWD, path.to_vec(), 0)
}

/// 内核自己在 `offset` 处写打开文件（core dump），与 pwrite 相同。
pub(crate) fn write_at(
    cx: &mut Cx<'_>,
    file: &FileRef,
    data: &[u8],
    offset: u64,
) -> Result<usize, Errno> {
    write_bytes(cx, file, data, Some(offset))
}

/// openat 打开（或者新建）`path`，还没有放进 fd 表。
fn open_path(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: Vec<u8>,
    flags: u32,
    perm: u32,
) -> Result<FileRef, Errno> {
    open_path_with(cx, dirfd, path, flags, perm, 0)
}

/// [`open_path`]，路径解析带着 openat2 的限制（[`crate::vfs::resolve`]）。
fn open_path_with(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: Vec<u8>,
    flags: u32,
    perm: u32,
    res: u64,
) -> Result<FileRef, Errno> {
    use crate::vfs::resolve;
    let vfs = cx.vfs()?;
    let (start, root) = if res == 0 {
        (start_dir(cx, dirfd, &path)?, cx.root())
    } else {
        openat2_start(cx, dirfd, &path, res)?
    };
    let create = flags & oflags::O_CREAT != 0;
    let excl = create && flags & oflags::O_EXCL != 0;
    let follow = flags & oflags::O_NOFOLLOW == 0 && !excl;
    let cred = cx.cred();

    let resolved = vfs.walk_resolve(&cx.view(&vfs), &cred, &root, &start, &path, follow, res);
    // mqueue 文件系统里新建了队列（不再检查打开的权限，与仓库里新建的文件一样）
    let mut mq_created = false;
    // parent 是最后经过的目录和名字，报告 inotify 事件用
    let (node, created, parent) = match resolved {
        Ok(_) if excl => return Err(Errno::EEXIST),
        Ok((node, found)) => (node, None, found),
        Err(e) if e == Errno::ENOENT && create => {
            // 最后一个组件是悬空的符号链接时，与 Linux 一样沿着它新建目标文件
            let mut root_set = path.first() == Some(&b'/')
                || res & resolve::SCOPED != 0
                || path.split(|&b| b == b'/').any(|c| c == b"..");
            let (mut start, mut path) = (start.clone(), path.clone());
            let mut links = 0;
            loop {
                let (dir, name) =
                    vfs.resolve_parent_with(&cx.view(&vfs), &cred, &root, &start, &path, res)?;
                if path.ends_with(b"/") || name == b"." || name == b".." {
                    return Err(Errno::EISDIR);
                }
                // mqueue 文件系统里新建的是消息队列（Linux 的 mqueue_create），属性是默认的
                if let Node::Pseudo(mnt, PseudoNode::MqRoot(ns)) = dir {
                    if vfs.readonly(&dir) {
                        return Err(Errno::EROFS);
                    }
                    let ipc = cx.kr().ipc(ns);
                    let dir_st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
                    cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
                    let perm = perm & !cx.umask();
                    let q = super::mqueue::new_queue(ipc, ns, &cred, perm, None)?;
                    let ino = q.borrow().ino;
                    super::mqueue::insert_queue(cx.k().ipc_mut(ns), name.to_vec(), q);
                    mq_created = true;
                    break (
                        Node::Pseudo(mnt, PseudoNode::Mq(ns, ino)),
                        None,
                        Some((dir.clone(), name.to_vec())),
                    );
                }
                // cgroup 的目录里建不了文件（没有 create，Linux 的 vfs_create 是 EACCES）
                if let Node::Pseudo(_, PseudoNode::Cgroup(..)) = dir
                    && !vfs.readonly(&dir)
                {
                    return Err(Errno::EACCES);
                }
                let (mnt, dir) = store_dir(&vfs, &dir)?;
                if let Some(ino) = vfs.store().lookup(dir, name)?
                    && follow
                    && vfs.store().attrs(ino)?.kind() == mode::S_IFLNK
                {
                    links += 1;
                    if links > crate::vfs::MAX_SYMLINKS {
                        return Err(Errno::ELOOP);
                    }
                    // 与 walk 里一样（pick_link、nd_jump_root）：RESOLVE_NO_SYMLINKS、nosymfollow
                    // 的挂载不跟随；绝对路径的目标在 NO_XDEV 时要与根目录在同一个挂载里
                    if res & resolve::NO_SYMLINKS != 0
                        || vfs
                            .mount_flags(&Node::Store(mnt, ino))
                            .is_some_and(|f| f & ms::NOSYMFOLLOW != 0)
                    {
                        return Err(Errno::ELOOP);
                    }
                    path = vfs.store().readlink(ino)?;
                    // 解析时取过根目录没有：近似地按原来的路径看（绝对路径、有作用域、
                    // 有 `..`，或者这里已经跳过一次），前面的符号链接里的不算
                    if path.first() == Some(&b'/')
                        && res & resolve::NO_XDEV != 0
                        && (!root_set || mnt != root.mnt())
                    {
                        return Err(Errno::EXDEV);
                    }
                    root_set |= path.first() == Some(&b'/');
                    start = Node::Store(mnt, dir);
                    continue;
                }
                let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
                let (uid, gid, perm) = cred.new_owner(&dir_st, perm & !cx.umask() & 0o7777, false);
                let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
                let (ino, file) = vfs.store().create_file(dir, name, perm, uid, gid)?;
                tmpfs_created(&vfs, tmpfs, 0);
                break (
                    Node::Store(mnt, ino),
                    Some(file),
                    Some((Node::Store(mnt, dir), name.to_vec())),
                );
            }
        }
        Err(e) => return Err(e),
    };

    let mut display = display_path(cx, &vfs, &start, &path, &node, parent.as_ref());
    // 命名空间在 `/proc/<pid>/fd/<n>` 里是 `mnt:[编号]` 这样；经由 bind 挂载出去的命名空间
    // 文件打开的是挂载点的路径（Linux 的 d_path）
    if let Node::Pseudo(mnt, p @ PseudoNode::NsFile(..)) = &node
        && vfs.mount(*mnt).is_none_or(|m| m.fs != Fs::Nsfs)
    {
        display = Some(Rc::from(cx.view(&vfs).name(*mnt, p)));
    }
    let was_created = created.is_some();
    let via_devpts = matches!(node, Node::Pseudo(_, PseudoNode::Pts(..)));
    let opened_ino = node.ino().filter(|_| flags & oflags::O_PATH == 0);
    // 仓库里的文件记下经由的挂载（[`OpenFile::mnt`]）
    let store_mnt = node.ino().map(|_| node.mnt());
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(node)
    } else {
        match node {
            Node::Store(mnt, ino) => {
                // 刚建好的文件本来就是空的，不截断：ext4 把截断到 0 之后又写过的文件当成
                // 整个替换，关闭时立即开始回写，比写本身还慢
                let flags = if was_created {
                    flags & !oflags::O_TRUNC
                } else {
                    flags
                };
                let beside = parent.as_ref().map(|(dir, _)| dir);
                open_store(cx, &cred, &vfs, (mnt, ino), flags, (created, beside))?
            }
            Node::Pseudo(_, PseudoNode::Mq(ns, ino)) => {
                open_mq(cx, &cred, (ns, ino), flags, mq_created)?
            }
            Node::Pseudo(mnt, p) => open_pseudo(cx, &cred, &vfs, mnt, p, flags)?,
        }
    };
    // /dev/tty 这样的设备节点打开的是具体的某个从设备：路径是它在 devpts 里的（fd 的链接
    // 按经由的挂载算）；主设备、经由 devpts 打开的从设备照常是打开的路径
    if let FileKind::Pty(end) = &kind
        && !end.master
        && !via_devpts
    {
        display = None;
    }
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::with_path(kind, flags & keep | largefile(flags), display);
    if let Some(mnt) = store_mnt {
        file.borrow_mut().mnt = mnt;
    }
    file.borrow_mut().dentry = parent.clone();
    if let Some(ino) = opened_ino {
        open_events(cx, &file, ino, parent, was_created, flags);
    }
    Ok(file)
}

/// open、openat2、open_by_handle_at 打开的文件带 `O_LARGEFILE`（64 位上 Linux 总是加上，
/// `O_PATH` 时去掉），F_GETFL、fdinfo 看得到；管道、socket 这些别的系统调用建的文件没有。
fn largefile(flags: u32) -> u32 {
    if flags & oflags::O_PATH != 0 {
        0
    } else {
        oflags::O_LARGEFILE
    }
}

/// open 的 inotify 事件，顺序与 Linux 相同：新建（报给目录）、`O_TRUNC` 截断、打开。
/// 给打开文件挂上 [`inotify::Opened`]。
fn open_events(
    cx: &Cx<'_>,
    file: &FileRef,
    ino: Ino,
    parent: Option<(Node, Vec<u8>)>,
    created: bool,
    flags: u32,
) {
    let k = cx.kr();
    let parent = match parent {
        Some((Node::Store(_, dir), name)) => Some((dir, name)),
        _ => None,
    };
    if inotify::active(k) {
        open_notify(k, file, ino, parent.as_ref(), created, flags);
    }
    let watch = inotify::opened(k, &file.borrow(), parent);
    file.borrow_mut().watch = watch;
}

fn open_notify(
    k: &Kernel,
    file: &FileRef,
    ino: Ino,
    parent: Option<&(Ino, Vec<u8>)>,
    created: bool,
    flags: u32,
) {
    let parent = parent.map(|(d, n)| (*d, n.as_slice()));
    let (dir, truncated) = {
        let f = file.borrow();
        let regular = matches!(f.kind, FileKind::Regular { .. });
        let dir = matches!(f.kind, FileKind::Directory { .. });
        let truncated = regular && f.writable() && flags & oflags::O_TRUNC != 0;
        (if dir { inotify::IN_ISDIR } else { 0 }, truncated)
    };
    match parent {
        Some((d, name)) if created => inotify::dir_event(k, d, name, inotify::IN_CREATE, 0),
        _ if truncated => inotify::path_event(k, ino, parent, inotify::IN_MODIFY),
        _ => {}
    }
    inotify::path_event(k, ino, parent, inotify::IN_OPEN | dir);
}

/// 文件句柄（`struct file_handle`）里的类型和长度：句柄就是仓库里的 inode 编号。
const FILE_HANDLE_TYPE: i32 = 1;
const FILE_HANDLE_BYTES: u32 = 8;
/// 放不下句柄时写回的类型（Linux 的 `FILEID_INVALID`）。
const FILEID_INVALID: i32 = 255;
const MAX_HANDLE_SZ: u32 = 128;
const AT_HANDLE_FID: u32 = 0x200;

/// name_to_handle_at：仓库里的文件的句柄是它的 inode 编号；合成文件系统不支持句柄。
pub(super) fn name_to_handle_at(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    handle: u64,
    mount_id: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH | AT_HANDLE_FID) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let node = lookup_at_or_fd(
        cx,
        &cx.cred(),
        dirfd,
        &path,
        flags,
        flags & AT_SYMLINK_FOLLOW != 0,
    )?;
    let Node::Store(mnt, ino) = node else {
        return Err(Errno::EOPNOTSUPP);
    };
    let header: [u8; 8] = get(cx, handle)?;
    let room = u32::from_le_bytes(header[..4].try_into().unwrap());
    if room > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    // 放不下时只写回句柄头（需要的长度、FILEID_INVALID），返回 EOVERFLOW。两种情况都写
    // mount_id，它或者句柄写不进去时是 EFAULT（Linux 的 do_sys_name_to_handle）
    let mut b = FILE_HANDLE_BYTES.to_le_bytes().to_vec();
    let ret = if room < FILE_HANDLE_BYTES {
        b.extend_from_slice(&FILEID_INVALID.to_le_bytes());
        Err(Errno::EOVERFLOW)
    } else {
        b.extend_from_slice(&FILE_HANDLE_TYPE.to_le_bytes());
        b.extend_from_slice(&ino.to_le_bytes());
        Ok(0)
    };
    put(cx, mount_id, (mnt as i32).to_le_bytes())?;
    cx.mm().write(UserSlice::new(handle, b.len()), &b)?;
    ret
}

/// open_by_handle_at：按 inode 编号打开，要求 `CAP_DAC_READ_SEARCH`。
pub(super) fn open_by_handle_at(
    cx: &mut Cx<'_>,
    mount_fd: i32,
    handle: u64,
    flags: u32,
) -> SysResult {
    if !cx.cred().capable(cap::DAC_READ_SEARCH) {
        return Err(Errno::EPERM);
    }
    // 与 Linux 的 handle_to_path 一样先读句柄头、查长度，再读整个句柄，最后才看 mount_fd
    let header: [u8; 8] = get(cx, handle)?;
    let bytes = u32::from_le_bytes(header[..4].try_into().unwrap());
    if bytes == 0 || bytes > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    let ty = i32::from_le_bytes(header[4..].try_into().unwrap());
    let mut f_handle = vec![0u8; bytes as usize];
    cx.mm()
        .read(UserSlice::new(handle + 8, f_handle.len()), &mut f_handle)?;
    // 文件经由 mount_fd 所在的挂载打开。句柄只在仓库里解得开：mount_fd 在管道、socket、
    // 合成文件系统上时，与 Linux 上这些文件系统没有 export_operations 一样是 ESTALE
    let node = if mount_fd == AT_FDCWD {
        Some(cx.cwd())
    } else {
        cx.fds().get(mount_fd)?.borrow().node()
    };
    let Some(node @ Node::Store(..)) = node else {
        return Err(Errno::ESTALE);
    };
    let mnt = node.mnt();
    let ino = match f_handle.first_chunk::<8>() {
        Some(b) if bytes == FILE_HANDLE_BYTES && ty == FILE_HANDLE_TYPE => u64::from_le_bytes(*b),
        _ => return Err(Errno::ESTALE),
    };
    let vfs = cx.vfs()?;
    // 文件已经删除（或者句柄不是这里发的）：ESTALE
    let st = match vfs.store().stat(ino) {
        Ok(st) if st.nlink > 0 => st,
        _ => return Err(Errno::ESTALE),
    };
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !st.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(Node::Store(mnt, ino))
    } else {
        open_store(cx, &cx.cred(), &vfs, (mnt, ino), flags, (None, None))?
    };
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::new(kind, flags & keep | largefile(flags));
    file.borrow_mut().mnt = mnt;
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// 设备号对应的 PTY 从设备编号：主设备号 136，次设备号就是编号（Linux 的 pts 驱动登记了
/// 136 的全部 2^20 个次设备号）。
fn pts_index(r: u64) -> Option<u32> {
    let major = ((r >> 8) & 0xfff) as u32;
    let minor = ((r & 0xff) | ((r >> 12) & !0xff)) as u32;
    (major == PTS_MAJOR).then_some(minor)
}

/// 不在 devpts 里的 ptmx 设备节点打开的是哪个 devpts 实例（Linux 的 `devpts_acquire`）：
/// 节点所在的目录 `dir` 里的 `pts` 挂着的那个（它的挂载和设备号）。没有 `pts` 是 ENOENT，
/// 它不是 devpts 的根是 ENODEV；不知道节点在哪个目录里（按句柄打开的）时是开机时的那个。
fn devpts_beside(vfs: &Vfs, dir: Option<&Node>) -> Result<(u64, MountId), Errno> {
    let Some(dir) = dir else {
        return Ok((crate::tty::BOOT_PTS, crate::vfs::PTS_MOUNT));
    };
    let Node::Store(mnt, ino) = dir else {
        return Err(Errno::ENODEV);
    };
    let pts = vfs.store().lookup(*ino, b"pts")?.ok_or(Errno::ENOENT)?;
    match vfs.mounted(Node::Store(*mnt, pts)) {
        Node::Pseudo(m, PseudoNode::PtsRoot(fs)) => Ok((fs, m)),
        _ => Err(Errno::ENODEV),
    }
}

/// 有没有进程正在执行仓库里的程序 `ino`（以写方式打开它时 ETXTBSY）。
pub(super) fn executing(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr()
        .tasks
        .values()
        .any(|t| t.res.borrow().mm.borrow().exe == Some(ino))
}

/// 有没有以写方式打开着的 `ino`（执行它时 ETXTBSY）。
/// memfd_create 得到的 fd 不算（[`OpenFile::pseudo`]）。
pub(super) fn open_for_write(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr().open_files().iter().any(|f| {
        let f = f.borrow();
        f.writable() && !f.pseudo && matches!(f.kind, FileKind::Regular { ino: i, .. } if i == ino)
    })
}

/// 访问权限（`MAY_*`）换成设备过滤的访问（`BPF_DEVCG_ACC_*`）。
fn device_access(mask: u32) -> u32 {
    let mut access = 0;
    if mask & MAY_WRITE != 0 {
        access |= crate::bpf::ACC_WRITE;
    }
    if mask & MAY_READ != 0 {
        access |= crate::bpf::ACC_READ;
    }
    access
}

/// open 要求的访问权限（`MAY_*`）。
fn open_mask(flags: u32) -> u32 {
    match flags & oflags::O_ACCMODE {
        oflags::O_RDONLY => MAY_READ,
        oflags::O_WRONLY => MAY_WRITE,
        _ => MAY_READ | MAY_WRITE,
    }
}

/// 打开经由挂载 `mnt` 的仓库里的 inode。刚刚由这次 open 新建的文件（`created`）不检查
/// 权限。
fn open_store(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    (mnt, ino): (MountId, Ino),
    flags: u32,
    (created, beside): (Option<std::fs::File>, Option<&Node>),
) -> Result<FileKind, Errno> {
    let store = vfs.store();
    let accmode = flags & oflags::O_ACCMODE;
    // 只用到类型、权限、属主和设备号，不必向宿主 stat object
    let st = store.attrs(ino)?;
    if created.is_none() {
        let mnt_flags = vfs.mount_flags(&Node::Store(mnt, ino)).unwrap_or(0);
        let readonly = mnt_flags & ms::RDONLY != 0;
        let mut mask = open_mask(flags);
        match st.kind() {
            mode::S_IFLNK => return Err(Errno::ELOOP),
            mode::S_IFDIR if accmode != oflags::O_RDONLY || flags & oflags::O_CREAT != 0 => {
                return Err(Errno::EISDIR);
            }
            // 截断：只读的挂载里先是 EROFS（Linux 的 do_open 在 may_open 之前）
            mode::S_IFREG if flags & oflags::O_TRUNC != 0 => {
                if readonly {
                    return Err(Errno::EROFS);
                }
                mask |= MAY_WRITE;
            }
            // nodev 的挂载里的设备节点打不开（may_open_dev）
            mode::S_IFCHR | mode::S_IFBLK if mnt_flags & ms::NODEV != 0 => {
                return Err(Errno::EACCES);
            }
            _ => {}
        }
        cred.permission(&st, mask)?;
        // 设备节点再过 cgroup 的设备过滤（Linux 的 devcgroup_inode_permission）
        if matches!(st.kind(), mode::S_IFCHR | mode::S_IFBLK) && st.rdev != 0 {
            super::bpf::check_device(cx, st.kind(), st.rdev, device_access(mask))?;
        }
        if flags & oflags::O_NOATIME != 0 && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
        // 只读的挂载里写方式打开普通文件是 EROFS；设备、FIFO 照常（do_dentry_open 不管
        // special_file）
        if readonly && st.kind() == mode::S_IFREG && accmode != oflags::O_RDONLY {
            return Err(Errno::EROFS);
        }
    }
    Ok(match st.kind() {
        mode::S_IFLNK => return Err(Errno::ELOOP),
        mode::S_IFDIR => {
            if accmode != oflags::O_RDONLY || flags & oflags::O_CREAT != 0 {
                return Err(Errno::EISDIR);
            }
            FileKind::Directory {
                node: Node::Store(mnt, ino),
                entries: None,
                process: None,
            }
        }
        _ if flags & oflags::O_DIRECTORY != 0 => return Err(Errno::ENOTDIR),
        mode::S_IFREG => {
            let write = accmode != oflags::O_RDONLY || flags & oflags::O_TRUNC != 0;
            if write && created.is_none() && executing(cx, ino) {
                return Err(Errno::ETXTBSY);
            }
            // 别人的租约：写方式打开与所有租约冲突，读方式只与写租约冲突
            if created.is_none() {
                let nonblock = flags & oflags::O_NONBLOCK != 0;
                super::lease::break_leases(cx, ino, write, nonblock, None)?;
            }
            let file = match created {
                Some(f) => f,
                None => store.open_object(ino, accmode != oflags::O_RDONLY)?,
            };
            if flags & oflags::O_TRUNC != 0 && accmode != oflags::O_RDONLY {
                remove_privs(cx, ino)?;
                let tmpfs = tmpfs_size(vfs, &Node::Store(mnt, ino), &file)?;
                file.set_len(0).map_err(host_errno)?;
                tmpfs_resized(vfs, tmpfs, 0);
                truncated(cx, &file);
            }
            FileKind::Regular { ino, file }
        }
        mode::S_IFCHR => match st.rdev {
            // /dev/tty、/dev/console：调用者的控制终端
            r if r == rdev(5, 0) || r == rdev(5, 1) => tty::open_ctty(cx)?,
            PTMX_RDEV => {
                let (fs, m) = devpts_beside(vfs, beside)?;
                tty::open_ptmx(cx.k(), (fs, m, false), (cred.fsuid, cred.fsgid))?
            }
            // devpts 之外的终端设备节点：开机时的实例里的
            r => match pts_index(r) {
                Some(index) => {
                    let at = (crate::tty::BOOT_PTS, index, crate::vfs::PTS_MOUNT, false);
                    tty::open_slave(cx, at, flags)?
                }
                None => {
                    let dev = dev::open(store, ino)?;
                    let kmsg = match dev {
                        Device::Kmsg => Some(open_kmsg(cx, accmode)?),
                        _ => None,
                    };
                    FileKind::Device { ino, dev, kmsg }
                }
            },
        },
        mode::S_IFIFO => open_fifo(cx, ino, flags)?,
        // 块设备、socket 文件不能打开（socket 文件用 connect）
        _ => return Err(Errno::ENXIO),
    })
}

/// 打开 `/dev/kmsg`（`devkmsg_open`）：`printk_devkmsg` 为 `off` 时谁都打不开；读方式打开
/// 按 syslog 的读全部查权限（`dmesg_restrict`），只写不查。读的位置从最老的一条开始。
fn open_kmsg(cx: &mut Cx<'_>, accmode: u32) -> Result<Box<crate::klog::KmsgFile>, Errno> {
    let log = cx.kr().klog.clone();
    if log.borrow().devkmsg == crate::klog::Devkmsg::Off {
        return Err(Errno::EPERM);
    }
    if accmode != oflags::O_WRONLY {
        super::misc::check_syslog_permissions(cx, super::misc::syslog_action::READ_ALL, false)?;
    }
    Ok(Box::new(crate::klog::KmsgFile::new(log)))
}

/// 打开命名管道（FIFO）：同一个 inode 的所有打开共用一个管道。与 Linux 相同，只读打开
/// 等到有写者，只写打开等到有读者（`O_NONBLOCK` 时没有读者就返回 ENXIO），读写打开不等。
fn open_fifo(cx: &mut Cx<'_>, ino: Ino, flags: u32) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    let nonblock = flags & oflags::O_NONBLOCK != 0;
    let (read, write) = match accmode {
        oflags::O_RDONLY => (true, false),
        oflags::O_WRONLY => (false, true),
        _ => (true, true),
    };
    let capacity = pipe_capacity(cx);
    let pipe = {
        let k = cx.k();
        k.fifos.retain(|_, w| w.strong_count() > 0);
        match k.fifos.get(&ino).and_then(Weak::upgrade) {
            Some(p) => p,
            None => {
                let p = Pipe::fifo(ino, capacity);
                k.fifos.insert(ino, Rc::downgrade(&p));
                p
            }
        }
    };
    if write && !read && nonblock && pipe.borrow().readers == 0 {
        return Err(Errno::ENXIO);
    }
    let peer_opens = |p: &Pipe| if read { p.writer_opens } else { p.reader_opens };
    let before = peer_opens(&pipe.borrow());
    let mut end = PipeEnd::open(&pipe, read, write);
    if read && !write && nonblock && pipe.borrow().writers == 0 {
        end.hup_after = Some(before);
    }
    if read != write && !nonblock {
        // 等对端打开：对端来过就行，哪怕已经关掉
        loop {
            {
                let p = pipe.borrow();
                let peers = if read { p.writers } else { p.readers };
                if peers > 0 || peer_opens(&p) != before {
                    break;
                }
            }
            if cx.interrupted() {
                return Err(Errno::ERESTARTSYS);
            }
            cx.wait_object(crate::pipe::object(&pipe))?;
        }
    }
    Ok(FileKind::Pipe(end))
}

/// 打开合成文件系统里的节点。
/// 打开 mqueue 文件系统里的队列：得到的 fd 与 mq_open 的一样（Linux 上就是同一种文件），
/// mq_timedsend 这些照样能用。刚新建的不再检查权限。
fn open_mq(
    cx: &Cx<'_>,
    cred: &Cred,
    (ns, ino): (u64, u64),
    flags: u32,
    created: bool,
) -> Result<FileKind, Errno> {
    if flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    let q = mq_queue(cx, ns, ino)?;
    if !created {
        cred.permission(&q.borrow().stat(), open_mask(flags))?;
    }
    Ok(FileKind::MessageQueue(q))
}

fn open_pseudo(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    mnt: crate::vfs::MountId,
    node: PseudoNode,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    if !matches!(node, PseudoNode::OpenFd(..) | PseudoNode::OpenExe(_)) {
        cred.permission(&pseudo::stat(&cx.view(vfs), &node)?, open_mask(flags))?;
    }
    // /proc/<pid>/mem 与 ptrace 一样要求能跟踪那个进程（PTRACE_MODE_ATTACH），auxv、environ、
    // maps、smaps、pagemap 要求能以只读方式跟踪（PTRACE_MODE_READ，规则相同）：同一个用户
    // 的进程不可 dump 时也打不开
    let mut mm = None;
    if let PseudoNode::Entry(
        pid,
        _,
        e @ (pseudo::Entry::Mem
        | pseudo::Entry::Auxv
        | pseudo::Entry::Environ
        | pseudo::Entry::Maps
        | pseudo::Entry::Smaps
        | pseudo::Entry::SmapsRollup
        | pseudo::Entry::Pagemap),
    ) = node
    {
        let group = cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)?;
        if !super::proc::may_ptrace(cx, &group.borrow()) {
            return Err(Errno::EACCES);
        }
        if e == pseudo::Entry::Mem {
            mm = Some(Rc::downgrade(&pseudo::process_mm(cx.kr(), pid)?));
        }
    }
    // 设备节点在 nodev 的挂载里打不开（Linux 的 may_open）
    if matches!(node, PseudoNode::Pts(..) | PseudoNode::PtsPtmx(_))
        && vfs.mount(mnt).is_some_and(|m| m.flags & ms::NODEV != 0)
    {
        return Err(Errno::EACCES);
    }
    // devpts 的终端也过 cgroup 的设备过滤
    let rdev = match node {
        PseudoNode::Pts(_, n) => Some(rdev(PTS_MAJOR, n)),
        PseudoNode::PtsPtmx(_) => Some(PTMX_RDEV),
        _ => None,
    };
    if let Some(r) = rdev {
        super::bpf::check_device(cx, mode::S_IFCHR, r, device_access(open_mask(flags)))?;
    }
    match node {
        PseudoNode::Pts(fs, n) => return tty::open_slave(cx, (fs, n, mnt, true), flags),
        PseudoNode::PtsPtmx(fs) => {
            return tty::open_ptmx(cx.k(), (fs, mnt, true), (cred.fsuid, cred.fsgid));
        }
        PseudoNode::OpenFd(pid, fd) => return reopen(cx, cred, vfs, pid, fd, flags),
        PseudoNode::OpenExe(pid) => return reopen_exe(cx, cred, vfs, pid, flags),
        _ => {}
    }
    let view = cx.view(vfs);
    // O_NOFOLLOW 时停在了符号链接（比如 /proc/self/fd/<n>）上
    if Pseudo::kind(&view, &node)? == mode::S_IFLNK {
        return Err(Errno::ELOOP);
    }
    let full = Node::Pseudo(mnt, node.clone());
    if vfs.is_dir(&view, &full)? {
        if accmode != oflags::O_RDONLY {
            return Err(Errno::EISDIR);
        }
        // /proc/<pid> 记住是哪个进程：Linux 的打开文件引用着 struct pid，进程回收之后
        // 当作 pidfd 用是 ESRCH，即使 pid 已经给了新进程
        let process = match node {
            PseudoNode::Pid(pid, None) => cx.kr().groups.get(&pid).map(Rc::downgrade),
            _ => None,
        };
        return Ok(FileKind::Directory {
            node: full,
            entries: None,
            process,
        });
    }
    if flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    if accmode != oflags::O_RDONLY && vfs.readonly(&full) {
        return Err(Errno::EROFS);
    }
    // cgroup 的文件只按权限位查（root 打得开只读的，写的时候才是 EINVAL）
    if accmode != oflags::O_RDONLY
        && !pseudo::writable(&node)
        && !matches!(node, PseudoNode::Cgroup(..))
    {
        return Err(Errno::EACCES);
    }
    // 网络的 sysctl 读的时候才出错（没有设过的 stable_secret 是 EIO，Linux 的
    // proc_sys_call_handler）：打开时先放空的，第一次读重新生成
    let mut fresh = true;
    let data = if pseudo::unreadable(&node) {
        Vec::new()
    } else {
        match pseudo::content(&view, mnt, &node) {
            Err(_) if matches!(node, PseudoNode::NetConf(..)) => {
                fresh = false;
                Vec::new()
            }
            r => r?,
        }
    };
    // /proc/kmsg（`kmsg_open`）：打开时按 syslog 的打开查权限，之后的读不再查
    let klog = match node {
        PseudoNode::Global(pseudo::Global::Kmsg) => {
            super::misc::do_syslog(cx, super::misc::syslog_action::OPEN, 0, 0, true)?;
            Some(cx.kr().klog.clone())
        }
        _ => None,
    };
    // 命名空间文件拿着那个命名空间
    let ns = match node {
        PseudoNode::NsFile(kind, id) => {
            Some(super::ns::find(cx.kr(), Some(vfs), kind, id).ok_or(Errno::ENOENT)?)
        }
        _ => None,
    };
    Ok(FileKind::Proc {
        mnt,
        node,
        data,
        fresh,
        mm,
        ns,
        klog,
    })
}

/// 经由 `/proc/<pid>/fd/<n>` 重新打开一个没有路径的文件：管道得到同一个管道的
/// 一端，宿主 fd 得到同一个宿主 fd，已经删除的普通文件（memfd、删除之后还开着的）
/// 经由宿主的 `/proc/self/fd` 打开同一个宿主文件（仓库里的 object 已经删了）。
fn reopen(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    pid: i32,
    fd: i32,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    let file = cx.view(vfs).open_file(pid, fd).ok_or(Errno::ENOENT)?;
    let f = file.borrow();
    if let FileKind::Regular { ino, file: host } = &f.kind {
        let ino = *ino;
        let st = vfs.store().stat_open(ino, host)?;
        let truncate = flags & oflags::O_TRUNC != 0;
        let mask = open_mask(flags) | if truncate { MAY_WRITE } else { 0 };
        cred.permission(&st, mask)?;
        if truncate && accmode != oflags::O_RDONLY {
            check_seals(cx, &f, false, |_| 0)?;
        }
        // 宿主 fd 总是可读的，原来以写方式打开时也可写：够用就复制一份（读写都按位置
        // 进行，偏移由各自的打开文件记着）。不够用时经由宿主的 /proc/self/fd 重新打开，
        // 被删除的文件在 virtiofs 这类文件系统上打不开
        let new = if accmode == oflags::O_RDONLY || f.writable() {
            host.try_clone()
        } else {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(
                    "/proc/self/fd/{}",
                    std::os::fd::AsRawFd::as_raw_fd(host)
                ))
        }
        .map_err(host_errno)?;
        let mnt = f.mnt;
        drop(f);
        return open_store(cx, cred, vfs, (mnt, ino), flags, (Some(new), None));
    }
    match &f.kind {
        FileKind::Pipe(end) => Ok(FileKind::Pipe(PipeEnd::open(
            &end.pipe,
            accmode != oflags::O_WRONLY,
            accmode != oflags::O_RDONLY,
        ))),
        FileKind::Host(fd) => Ok(FileKind::Host(*fd)),
        _ => Err(Errno::ENXIO),
    }
}

/// 进程 `pid` 正在执行的程序文件（`/proc/<pid>/exe`）。
pub(super) fn exe_file(cx: &Cx<'_>, pid: i32) -> Result<Rc<crate::process::ExeFile>, Errno> {
    let group = cx.kr().groups.get(&pid).cloned().ok_or(Errno::ENOENT)?;
    group.borrow().exe.clone().ok_or(Errno::ENOENT)
}

/// 经由 `/proc/<pid>/exe` 打开已经删除的程序文件（[`PseudoNode::OpenExe`]）：复制记着的
/// 宿主文件，与 [`reopen`] 打开已经删除的普通文件一样。还在执行的程序不能以写方式打开
/// （ETXTBSY），权限先查（Linux 在 may_open 之后才 get_write_access）。
fn reopen_exe(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    pid: i32,
    flags: u32,
) -> Result<FileKind, Errno> {
    let exe = exe_file(cx, pid)?;
    let (Some((mnt, ino)), Some(host)) = (exe.node, &exe.host) else {
        return Err(Errno::ENOENT);
    };
    let st = vfs.store().stat_open(ino, host)?;
    let truncate = flags & oflags::O_TRUNC != 0;
    let accmode = flags & oflags::O_ACCMODE;
    cred.permission(&st, open_mask(flags) | if truncate { MAY_WRITE } else { 0 })?;
    let write = accmode != oflags::O_RDONLY || truncate;
    if write && executing(cx, ino) {
        return Err(Errno::ETXTBSY);
    }
    let new = if accmode == oflags::O_RDONLY {
        host.try_clone()
    } else {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(&**host)
            ))
    }
    .map_err(host_errno)?;
    open_store(cx, cred, vfs, (mnt, ino), flags, (Some(new), None))
}

pub(super) fn close(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    // 先从表里拿出来再释放：关闭管道的最后一端会唤醒另一端
    let r = cx.fds_mut().close(fd);
    let file = r?;
    let fds = cx.t.fds.clone();
    locks::closed(cx.k(), &fds, std::slice::from_ref(&file));
    crate::unix::released(cx.k(), vec![file]);
    Ok(0)
}

pub(super) fn close_range(cx: &mut Cx<'_>, first: u32, last: u32, flags: u32) -> SysResult {
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return Err(Errno::EINVAL);
    }
    if flags & CLOSE_RANGE_UNSHARE != 0 && Rc::strong_count(&cx.t.fds) > 1 {
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    let closed = cx.fds_mut().close_range(
        first as usize,
        last as usize,
        flags & CLOSE_RANGE_CLOEXEC != 0,
    );
    let fds = cx.t.fds.clone();
    locks::closed(cx.k(), &fds, &closed);
    crate::unix::released(cx.k(), closed);
    Ok(0)
}

pub(super) fn dup(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    cx.fds_mut().install(file, false, 0).map(|fd| fd as u64)
}

pub(super) fn dup3(cx: &mut Cx<'_>, old: i32, new: i32, flags: u32) -> SysResult {
    if flags & !oflags::O_CLOEXEC != 0 || old == new {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(old)?;
    let replaced = cx.fds_mut().install_at(new, file, flags != 0);
    let fds = cx.t.fds.clone();
    let replaced: Vec<_> = replaced?.into_iter().collect();
    locks::closed(cx.k(), &fds, &replaced);
    crate::unix::released(cx.k(), replaced);
    Ok(new as u64)
}

/// 新管道的容量：与 Linux 一样不超过 pipe-max-size，有 `CAP_SYS_RESOURCE` 时除外。
fn pipe_capacity(cx: &Cx<'_>) -> usize {
    if cx.cred().capable(cap::SYS_RESOURCE) {
        PIPE_CAPACITY
    } else {
        PIPE_CAPACITY.min(cx.kr().pipe_max_size)
    }
}

pub(super) fn pipe2(cx: &mut Cx<'_>, fds: u64, flags: u32) -> SysResult {
    const O_DIRECT: u32 = 0o200000;
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK | O_DIRECT) != 0 {
        return Err(Errno::EINVAL);
    }
    let cloexec = flags & oflags::O_CLOEXEC != 0;
    let status = flags & oflags::O_NONBLOCK;
    let (r, w) = PipeEnd::pair(pipe_capacity(cx));
    let r = OpenFile::new(FileKind::Pipe(r), oflags::O_RDONLY | status);
    let w = OpenFile::new(FileKind::Pipe(w), oflags::O_WRONLY | status);
    let rfd = cx.fds_mut().install(r, cloexec, 0)?;
    // 先结束对 fd 表的借用：失败时还要再借一次，关掉读端
    let installed = cx.fds_mut().install(w, cloexec, 0);
    let wfd = match installed {
        Ok(fd) => fd,
        Err(e) => {
            let _ = cx.fds_mut().close(rfd);
            return Err(e);
        }
    };
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&rfd.to_le_bytes());
    out[4..].copy_from_slice(&wfd.to_le_bytes());
    if let Err(e) = put(cx, fds, out) {
        let _ = cx.fds_mut().close(rfd);
        let _ = cx.fds_mut().close(wfd);
        return Err(e);
    }
    Ok(0)
}

pub(super) fn fcntl(cx: &mut Cx<'_>, fd: i32, cmd: u32, arg: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // 起点不小于 RLIMIT_NOFILE 是 EINVAL，没有空位才是 EMFILE
            let min = usize::try_from(arg as i32)
                .ok()
                .filter(|&m| m < cx.fds().limit)
                .ok_or(Errno::EINVAL)?;
            cx.fds_mut()
                .install(file, cmd == F_DUPFD_CLOEXEC, min)
                .map(|fd| fd as u64)
        }
        F_GETFD => Ok(cx.fds().cloexec(fd)? as u64),
        F_SETFD => cx
            .fds_mut()
            .set_cloexec(fd, arg & FD_CLOEXEC != 0)
            .map(|()| 0),
        F_GETFL => Ok(file.borrow().flags as u64),
        F_SETFL => {
            let async_on = {
                let mut f = file.borrow_mut();
                let was = f.flags & oflags::O_ASYNC != 0;
                f.flags = (f.flags & !oflags::SETFL_MASK) | (arg as u32 & oflags::SETFL_MASK);
                !was && f.flags & oflags::O_ASYNC != 0
            };
            if async_on {
                super::poll::register_fasync(cx, &file, fd);
            }
            Ok(0)
        }
        F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => {
            record_lock(cx, &file, cmd, arg)
        }
        F_SETOWN => {
            let pid = arg as i32;
            file.borrow_mut().owner = if pid < 0 {
                (F_OWNER_PGRP, -pid)
            } else {
                (F_OWNER_PID, pid)
            };
            Ok(0)
        }
        F_GETOWN => {
            let (ty, pid) = file.borrow().owner;
            Ok(if ty == F_OWNER_PGRP { -pid } else { pid } as i64 as u64)
        }
        F_SETOWN_EX => {
            let b: [u8; 8] = get(cx, arg)?;
            let ty = i32::from_le_bytes(b[..4].try_into().unwrap());
            let pid = i32::from_le_bytes(b[4..].try_into().unwrap());
            if !matches!(ty, F_OWNER_TID | F_OWNER_PID | F_OWNER_PGRP) {
                return Err(Errno::EINVAL);
            }
            file.borrow_mut().owner = (ty, cx.pid_in(pid));
            Ok(0)
        }
        F_GETOWN_EX => {
            let (ty, pid) = file.borrow().owner;
            let pid = cx.pid_out(pid);
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&ty.to_le_bytes());
            b[4..].copy_from_slice(&pid.to_le_bytes());
            put(cx, arg, b).map(|()| 0)
        }
        F_SETSIG => {
            if arg != 0 && Signal::new(arg as i32).is_none() {
                return Err(Errno::EINVAL);
            }
            file.borrow_mut().sig = arg as i32;
            Ok(0)
        }
        F_GETSIG => Ok(file.borrow().sig as u64),
        F_SETLEASE => super::lease::set(cx, &file, fd, arg as i16).map(|()| 0),
        F_GETLEASE => Ok(super::lease::get(cx, &file) as u64),
        F_GETPIPE_SZ | F_SETPIPE_SZ => {
            let f = file.borrow();
            let FileKind::Pipe(end) = &f.kind else {
                return Err(Errno::EBADF);
            };
            let mut p = end.pipe.borrow_mut();
            if cmd == F_GETPIPE_SZ {
                return Ok(p.capacity as u64);
            }
            // 按页向上取整到 2 的幂；超过 pipe-max-size 要求 CAP_SYS_RESOURCE，
            // 不能小于已经缓冲的数据
            let size = (arg as u32 as usize)
                .max(PIPE_BUF)
                .checked_next_power_of_two()
                .filter(|&n| n <= 1 << 31)
                .ok_or(Errno::EINVAL)?;
            if size > cx.kr().pipe_max_size && !cx.cred().capable(cap::SYS_RESOURCE) {
                return Err(Errno::EPERM);
            }
            if size < p.buf.len() {
                return Err(Errno::EBUSY);
            }
            p.capacity = size;
            request_wake_object(crate::pipe::object(&end.pipe));
            Ok(size as u64)
        }
        // 只有 memfd 能加封印
        F_ADD_SEALS | F_GET_SEALS => {
            let f = file.borrow();
            let FileKind::Regular { ino, .. } = f.kind else {
                return Err(Errno::EINVAL);
            };
            let seals = cx.k().seals.get_mut(&ino).ok_or(Errno::EINVAL)?;
            if cmd == F_GET_SEALS {
                return Ok(*seals as u64);
            }
            let add = arg as u32;
            if add & !F_SEAL_ALL != 0 {
                return Err(Errno::EINVAL);
            }
            if !f.writable() || *seals & F_SEAL_SEAL != 0 {
                return Err(Errno::EPERM);
            }
            // 还有可写的共享映射时不能封印写入
            if add & F_SEAL_WRITE != 0 && writably_mapped(cx, &f) {
                return Err(Errno::EBUSY);
            }
            *cx.k().seals.get_mut(&ino).unwrap() |= add;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// fcntl 的记录锁命令（[`crate::locks`]）：`arg` 指向 `struct flock`。
fn record_lock(cx: &mut Cx<'_>, file: &FileRef, cmd: u32, arg: u64) -> SysResult {
    let mut b: [u8; 32] = get(cx, arg)?;
    let half = |off: usize| i16::from_le_bytes(b[off..off + 2].try_into().unwrap());
    let long = |off: usize| i64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let (l_type, whence, l_start, l_len) = (half(0), half(2), long(8), long(16));
    let l_pid = i32::from_le_bytes(b[24..28].try_into().unwrap());
    let ofd = matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
    let getlk = matches!(cmd, F_GETLK | F_OFD_GETLK);
    if ofd && l_pid != 0 {
        return Err(Errno::EINVAL);
    }
    let write = match l_type {
        F_RDLCK => false,
        F_WRLCK => true,
        F_UNLCK if !getlk => false,
        _ => return Err(Errno::EINVAL),
    };
    let unlock = l_type == F_UNLCK;
    if !matches!(whence as u32, SEEK_SET | SEEK_CUR | SEEK_END) {
        return Err(Errno::EINVAL);
    }
    let (ino, start, end) = {
        let f = file.borrow();
        if !getlk && !unlock && !(if write { f.writable() } else { f.readable() }) {
            return Err(Errno::EBADF);
        }
        let Some(ino) = f.ino() else {
            // 不在仓库里的文件（管道、socket）：别的进程看不到它们的锁
            if getlk {
                b[..2].copy_from_slice(&F_UNLCK.to_le_bytes());
                put(cx, arg, b)?;
            }
            return Ok(0);
        };
        let base = match whence as u32 {
            SEEK_SET => 0,
            SEEK_CUR => f.pos,
            SEEK_END => match &f.kind {
                FileKind::Regular { file, .. } => file.metadata().map_err(host_errno)?.len(),
                _ => 0,
            },
            _ => return Err(Errno::EINVAL),
        };
        let start = i64::try_from(base)
            .ok()
            .and_then(|b| b.checked_add(l_start))
            .ok_or(Errno::EOVERFLOW)?;
        // l_len 为 0 表示到文件末尾以及之后，负数表示 [start + l_len, start)
        let (start, end) = match l_len {
            0 => (start, i64::MAX),
            n if n > 0 => (start, start.checked_add(n).ok_or(Errno::EOVERFLOW)?),
            n => (start + n, start),
        };
        if start < 0 {
            return Err(Errno::EINVAL);
        }
        let end = if end == i64::MAX {
            u64::MAX
        } else {
            end as u64
        };
        (ino, start as u64, end)
    };
    let owner = if ofd {
        file.borrow_mut().holds_locks = true;
        Owner::Ofd(Rc::downgrade(file))
    } else {
        cx.fds_mut().holds_locks = true;
        Owner::Posix(Rc::downgrade(&cx.t.fds))
    };
    let req = Lock {
        owner,
        pid: if ofd { -1 } else { cx.tgid() },
        start,
        end,
        write,
    };
    if getlk {
        // 没有冲突时只把 l_type 改成 F_UNLCK，其余字段不变
        match cx.k().locks.conflict(ino, &req) {
            None => b[..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
            Some(l) => {
                let ty = if l.write { F_WRLCK } else { F_RDLCK };
                let len = if l.end == u64::MAX {
                    0
                } else {
                    l.end - l.start
                };
                b[..2].copy_from_slice(&ty.to_le_bytes());
                b[2..4].copy_from_slice(&(SEEK_SET as i16).to_le_bytes());
                b[8..16].copy_from_slice(&l.start.to_le_bytes());
                b[16..24].copy_from_slice(&len.to_le_bytes());
                // OFD 锁是 -1，别的是调用者看到的编号（Linux 的 locks_translate_pid）
                let pid = if l.pid > 0 { cx.pid_out(l.pid) } else { l.pid };
                b[24..28].copy_from_slice(&pid.to_le_bytes());
            }
        }
        return put(cx, arg, b).map(|()| 0);
    }
    loop {
        let locks = &mut cx.k().locks;
        if unlock || locks.conflict(ino, &req).is_none() {
            locks.set_waiting(&req.owner, None);
            locks.set(ino, req, unlock);
            return Ok(0);
        }
        if matches!(cmd, F_SETLK | F_OFD_SETLK) {
            return Err(Errno::EAGAIN);
        }
        if locks.deadlock(ino, &req) {
            locks.set_waiting(&req.owner, None);
            return Err(Errno::EDEADLK);
        }
        locks.set_waiting(
            &req.owner,
            Some(Waiter {
                ino,
                req: req.clone(),
            }),
        );
        if cx.interrupted() {
            cx.k().locks.set_waiting(&req.owner, None);
            return Err(Errno::ERESTARTSYS);
        }
        // 持有锁的一方解锁、关闭文件或者退出时会请求唤醒
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
}

/// 封印禁止的写入（write、pwrite）和长度变化（ftruncate）返回 EPERM。`grow_to` 是
/// 写入或截断之后的长度。
fn check_seals(
    cx: &Cx<'_>,
    f: &OpenFile,
    write: bool,
    grow_to: impl FnOnce(u64) -> u64,
) -> Result<(), Errno> {
    let FileKind::Regular { ino, file } = &f.kind else {
        return Ok(());
    };
    let Some(&seals) = cx.kr().seals.get(ino) else {
        return Ok(());
    };
    if write && seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
        return Err(Errno::EPERM);
    }
    let size = file.metadata().map_err(host_errno)?.len();
    let new = grow_to(size);
    if (new > size && seals & F_SEAL_GROW != 0) || (new < size && seals & F_SEAL_SHRINK != 0) {
        return Err(Errno::EPERM);
    }
    Ok(())
}

pub(super) fn memfd_create(cx: &mut Cx<'_>, name: u64, flags: u32) -> SysResult {
    const MFD_CLOEXEC: u32 = 0x1;
    const MFD_ALLOW_SEALING: u32 = 0x2;
    const MFD_NOEXEC_SEAL: u32 = 0x8;
    const MFD_EXEC: u32 = 0x10;
    // 不支持 MFD_HUGETLB
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL | MFD_EXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let name = cx.mm().read_cstr(name, 249).map_err(|e| {
        if e == Errno::ENAMETOOLONG {
            Errno::EINVAL
        } else {
            e
        }
    })?;
    let noexec = flags & MFD_NOEXEC_SEAL != 0;
    let (ino, file) = cx
        .vfs()?
        .store()
        .create_anonymous(if noexec { 0o666 } else { 0o777 })?;
    let seals = match (flags & MFD_ALLOW_SEALING != 0, noexec) {
        (_, true) => F_SEAL_EXEC,
        (true, false) => 0,
        (false, false) => F_SEAL_SEAL,
    };
    cx.k().seals.insert(ino, seals);
    let mut path = b"/memfd:".to_vec();
    path.extend_from_slice(&name);
    path.extend_from_slice(b" (deleted)");
    let file = OpenFile::with_path(
        FileKind::Regular { ino, file },
        oflags::O_RDWR | oflags::O_LARGEFILE,
        Some(Rc::from(path)),
    );
    file.borrow_mut().pseudo = true;
    cx.fds_mut()
        .install(file, flags & MFD_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

pub(super) fn ioctl(cx: &mut Cx<'_>, fd: i32, request: u32, arg: u64) -> SysResult {
    let file = io_file(cx, fd)?;
    match request {
        FIOCLEX | FIONCLEX => {
            return cx.fds_mut().set_cloexec(fd, request == FIOCLEX).map(|()| 0);
        }
        FIONBIO => {
            let on = u32::from_le_bytes(get(cx, arg)?) != 0;
            let mut f = file.borrow_mut();
            f.flags = (f.flags & !oflags::O_NONBLOCK) | if on { oflags::O_NONBLOCK } else { 0 };
            return Ok(0);
        }
        FIOASYNC => {
            let on = u32::from_le_bytes(get(cx, arg)?) != 0;
            let was = {
                let mut f = file.borrow_mut();
                let was = f.flags & oflags::O_ASYNC != 0;
                f.flags = (f.flags & !oflags::O_ASYNC) | if on { oflags::O_ASYNC } else { 0 };
                was
            };
            if on && !was {
                super::poll::register_fasync(cx, &file, fd);
            }
            return Ok(0);
        }
        // 与 Linux 一样在看文件类型之前处理（do_vfs_ioctl）
        FICLONE | FICLONERANGE => {
            let src = if request == FICLONE {
                arg as i32
            } else {
                // struct file_clone_range 的第一项是源 fd（s64）
                let b: [u8; 32] = get(cx, arg)?;
                i64::from_le_bytes(b[..8].try_into().unwrap()) as i32
            };
            return clone_range(&io_file(cx, src)?, &file);
        }
        FIDEDUPERANGE => {
            // struct file_dedupe_range 的头：两个 u64，dest_count（u16）和两个保留项
            let b: [u8; 24] = get(cx, arg)?;
            if b[18..24].iter().any(|&x| x != 0) {
                return Err(Errno::EINVAL);
            }
            return match file.borrow().kind {
                FileKind::Regular { .. } => Err(Errno::EOPNOTSUPP),
                FileKind::Directory { .. } => Err(Errno::EISDIR),
                _ => Err(Errno::EINVAL),
            };
        }
        // socket 的属主（与 F_SETOWN、F_GETOWN 相同，负数是进程组）
        FIOSETOWN | SIOCSPGRP | FIOGETOWN | SIOCGPGRP
            if matches!(file.borrow().kind, FileKind::Socket(_)) =>
        {
            if matches!(request, FIOSETOWN | SIOCSPGRP) {
                let pid = i32::from_le_bytes(get(cx, arg)?);
                file.borrow_mut().owner = if pid < 0 {
                    (F_OWNER_PGRP, cx.pid_in(pid.saturating_neg()))
                } else {
                    (F_OWNER_PID, cx.pid_in(pid))
                };
                return Ok(0);
            }
            let (ty, pid) = file.borrow().owner;
            let pid = cx.pid_out(pid);
            let pid = if ty == F_OWNER_PGRP { -pid } else { pid };
            return put(cx, arg, pid.to_le_bytes()).map(|()| 0);
        }
        _ => {}
    }
    let ns = match &file.borrow().kind {
        FileKind::Proc {
            mnt,
            node: PseudoNode::NsFile(kind, _),
            ns: Some(r),
            ..
        } => Some((*mnt, *kind, r.clone())),
        _ => None,
    };
    if let Some((mnt, kind, r)) = ns {
        return super::ns::ioctl(cx, mnt, kind, &r, request, arg);
    }
    // 取出要用的对象再放开打开文件的借用：NOTIF_RECV 会等，等的时候别的线程可能要改这个
    // 打开文件（fcntl、FIONBIO）
    let notify = match &file.borrow().kind {
        FileKind::SeccompNotify(l) => Some(l.filter()),
        _ => None,
    };
    if let Some(f) = notify {
        return crate::seccomp::ioctl(cx, &f, request, arg);
    }
    let pty = match &file.borrow().kind {
        FileKind::Pty(end) => Some((end.tty.clone(), (end.master, end.born), end.mnt)),
        _ => None,
    };
    if let Some((t, side, mnt)) = pty {
        if request == tty::TIOCGPTPEER {
            return tty::open_peer(cx, &t, side, mnt, arg as u32);
        }
        return tty::ioctl(cx, &t, side, request, arg);
    }
    let sock = match &file.borrow().kind {
        FileKind::Socket(s) => Some(s.clone()),
        _ => None,
    };
    if let Some(s) = sock {
        return super::net::ioctl(cx, &s, request, arg);
    }
    let host_fd = match &file.borrow().kind {
        FileKind::Host(fd) => *fd,
        FileKind::Pipe(end) if request == FIONREAD => {
            let n = end.pipe.borrow().buf.len() as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        FileKind::Inotify(i) if request == FIONREAD => {
            let n = i.borrow().pending() as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        FileKind::Device {
            dev: Device::Random,
            ..
        } => return random_ioctl(cx, request, arg),
        _ => return Err(Errno::ENOTTY),
    };
    let Some(&(_, size, out)) = TTY_IOCTLS.iter().find(|(r, ..)| *r == request) else {
        return Err(Errno::ENOTTY);
    };
    let mut buf = vec![0u8; size];
    if !out {
        cx.mm().read(UserSlice::new(arg, size), &mut buf)?;
    }
    let rc = appine_hal::ioctl(host_fd, request as u64, &mut buf).map_err(Errno)?;
    if out {
        cx.mm().write(UserSlice::new(arg, size), &buf)?;
    }
    Ok(rc as u64)
}

/// ioctl 和 FICLONE 的源 fd 对应的打开文件：与 Linux 的 `fdget` 一样不接受 `O_PATH` 打开的
/// （EBADF）。
fn io_file(cx: &Cx<'_>, fd: i32) -> Result<FileRef, Errno> {
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    Ok(file)
}

/// FICLONE、FICLONERANGE：按 Linux 的顺序检查（`ioctl_file_clone`、`generic_file_rw_checks`），
/// 最后和宿主上的 ext4、f2fs 一样不支持 reflink（EOPNOTSUPP），程序退回普通的复制
/// （Python 3.14 的 `pathlib.Path.copy`、`cp --reflink=auto`）。仓库之外的文件和仓库里的
/// 不在同一个挂载点（EXDEV）。
fn clone_range(src: &FileRef, dst: &FileRef) -> SysResult {
    let (src, dst) = (src.borrow(), dst.borrow());
    let in_store = |f: &OpenFile| {
        matches!(
            f.kind,
            FileKind::Regular { .. } | FileKind::Directory { .. }
        )
    };
    if !in_store(&src) || !in_store(&dst) {
        return Err(Errno::EXDEV);
    }
    if [&src, &dst]
        .iter()
        .any(|f| matches!(f.kind, FileKind::Directory { .. }))
    {
        return Err(Errno::EISDIR);
    }
    if !src.readable() || !dst.writable() || dst.flags & oflags::O_APPEND != 0 {
        return Err(Errno::EBADF);
    }
    Err(Errno::EOPNOTSUPP)
}

// ---- 元数据 ----

pub(super) fn faccessat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    amode: u32,
    flags: u32,
) -> SysResult {
    if amode & !7 != 0 || flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    // 默认用真实用户 ID 检查（包括路径上的搜索权限），AT_EACCESS 时用有效用户 ID
    let cred = if flags & AT_EACCESS != 0 {
        cx.cred()
    } else {
        cx.cred().for_access()
    };
    let node = lookup_at_or_fd(
        cx,
        &cred,
        dirfd,
        &path,
        flags,
        flags & AT_SYMLINK_NOFOLLOW == 0,
    )?;
    if amode == 0 {
        return Ok(0);
    }
    let b = node_stat(cx, &node)?;
    let word = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let st = Stat {
        mode: word(16),
        uid: word(24),
        gid: word(28),
        ..Stat::default()
    };
    // R_OK、W_OK、X_OK 与 MAY_READ、MAY_WRITE、MAY_EXEC 的取值相同
    cred.permission(&st, amode)?;
    // 只读的挂载里的普通文件、目录、符号链接问能不能写是 EROFS（在权限之后）
    let special = !matches!(
        st.mode & mode::S_IFMT,
        mode::S_IFREG | mode::S_IFDIR | mode::S_IFLNK
    );
    if amode & MAY_WRITE != 0 && !special && cx.vfs()?.readonly(&node) {
        return Err(Errno::EROFS);
    }
    Ok(0)
}

pub(super) fn chdir(cx: &mut Cx<'_>, path: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    set_cwd(cx, node)
}

pub(super) fn fchdir(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    let node = cx.fds().get(fd)?.borrow().node().ok_or(Errno::ENOTDIR)?;
    set_cwd(cx, node)
}

/// chroot：目标必须是可以搜索的目录，要求 `CAP_SYS_CHROOT`。当前目录不变（可能在
/// 新的根目录外面，与 Linux 相同）。
pub(super) fn chroot(cx: &mut Cx<'_>, path: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    let cred = cx.cred();
    if let Node::Store(_, ino) = node {
        cred.permission(&vfs.store().attrs(ino)?, MAY_EXEC)?;
    }
    if !cred.capable_here(cap::SYS_CHROOT) {
        return Err(Errno::EPERM);
    }
    cx.t.fs.borrow_mut().root = node;
    cx.shared.fast_changed();
    Ok(0)
}

/// chdir、fchdir：目标必须是可以搜索的目录。
fn set_cwd(cx: &mut Cx<'_>, node: Node) -> SysResult {
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    if let Node::Store(_, ino) = node {
        cx.cred().permission(&vfs.store().attrs(ino)?, MAY_EXEC)?;
    }
    cx.t.fs.borrow_mut().cwd = node;
    cx.shared.fast_changed();
    Ok(0)
}

pub(super) fn getcwd(cx: &mut Cx<'_>, buf: u64, size: u64) -> SysResult {
    let vfs = cx.vfs()?;
    let (path, reached) = vfs.reach(&cx.view(&vfs), &cx.root(), &cx.cwd())?;
    // 与 Linux 一样，当前目录不在根目录下面时以 (unreachable) 开头（musl、glibc 当作 ENOENT）
    let mut path = if reached {
        path
    } else {
        [&b"(unreachable)"[..], &path].concat()
    };
    path.push(0);
    if (size as usize) < path.len() {
        return Err(Errno::ERANGE);
    }
    cx.mm().write(UserSlice::new(buf, path.len()), &path)?;
    Ok(path.len() as u64)
}

pub(super) fn readlinkat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    buf: u64,
    size: usize,
) -> SysResult {
    if size as isize <= 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    // 路径为空时读 dirfd 本身（以 O_PATH | O_NOFOLLOW 打开的符号链接），`AT_FDCWD` 是当前
    // 目录；它不是符号链接时是 ENOENT（Linux 的 do_readlinkat：`empty ? -ENOENT : -EINVAL`）
    let empty = path.is_empty();
    let node = if !empty {
        lookup_at(cx, dirfd, &path, false)?
    } else if dirfd == AT_FDCWD {
        cx.cwd()
    } else {
        cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::ENOENT)?
    };
    let vfs = cx.vfs()?;
    let target = match vfs.readlink(&cx.view(&vfs), &node) {
        Err(e) if empty && e == Errno::EINVAL => return Err(Errno::ENOENT),
        r => r?,
    };
    let n = target.len().min(size);
    cx.mm().write(UserSlice::new(buf, n), &target[..n])?;
    Ok(n as u64)
}

pub(super) fn newfstatat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    buf: u64,
    flags: u32,
) -> SysResult {
    newfstatat_flags(flags)?;
    let path = read_path(cx, path)?;
    let st = stat_at(cx, dirfd, &path, flags)?;
    put(cx, buf, st).map(|()| 0)
}

/// newfstatat 的 `flags` 检查。
pub(super) fn newfstatat_flags(flags: u32) -> Result<(), Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// newfstatat、statx 共用：`*at` 风格地找到对象，返回 `struct stat`。
fn stat_at(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Result<[u8; 128], Errno> {
    stat_at_kernel(cx, dirfd, path, flags).map(|st| owner_out(cx, st))
}

/// stat 结果里的属主、属组换成调用者的用户命名空间里的（映射不到是 65534）。
fn owner_out(cx: &Cx<'_>, mut st: [u8; 128]) -> [u8; 128] {
    let cred = cx.cred();
    if cred.user_ns.level > 0 {
        let word =
            |b: &[u8; 128], off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        let (uid, gid) = (cred.uid_out(word(&st, 24)), cred.gid_out(word(&st, 28)));
        st[24..28].copy_from_slice(&uid.to_le_bytes());
        st[28..32].copy_from_slice(&gid.to_le_bytes());
    }
    st
}

/// [`stat_at`]，属主、属组是内核 ID。
fn stat_at_kernel(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Result<[u8; 128], Errno> {
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        file_stat(cx, &cx.fds().get(dirfd)?)
    } else {
        // 解析和取属性共用一个读事务（只在缓存没命中时才真正开始）
        let vfs = cx.vfs()?;
        vfs.store().snapshot(|_| {
            let cred = cx.cred();
            let node = lookup_at_or_fd(
                cx,
                &cred,
                dirfd,
                path,
                flags,
                flags & AT_SYMLINK_NOFOLLOW == 0,
            )?;
            node_stat(cx, &node)
        })
    }
}

pub(super) fn statx(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    flags: u32,
    mask: u32,
    buf: u64,
) -> SysResult {
    let flags = statx_flags(flags, mask)?;
    let path = read_path(cx, path)?;
    let st = stat_at(cx, dirfd, &path, flags)?;
    put(cx, buf, encode_statx(&st)).map(|()| 0)
}

/// statx 的 `flags` 和 `mask` 检查，返回 [`stat_at`] 用的 flags。
pub(super) fn statx_flags(flags: u32, mask: u32) -> Result<u32, Errno> {
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_STATX_SYNC_TYPE: u32 = 0x6000;
    const STATX_RESERVED: u32 = 0x8000_0000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return Err(Errno::EINVAL);
    }
    Ok(flags & (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH))
}

/// 由 `struct stat` 得到 `struct statx`：总是给出全部基本字段（`STATX_BASIC_STATS`），
/// 没有创建时间。
pub(super) fn encode_statx(st: &[u8; 128]) -> [u8; 256] {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    let u32_at = |o: usize| u32::from_le_bytes(st[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(st[o..o + 8].try_into().unwrap());
    // glibc、musl 的 major()/minor()
    let major = |d: u64| (((d >> 8) & 0xfff) | ((d >> 32) & !0xfff)) as u32;
    let minor = |d: u64| ((d & 0xff) | ((d >> 12) & !0xff)) as u32;
    let mut b = [0u8; 256];
    let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    put(0, &STATX_BASIC_STATS.to_le_bytes());
    put(4, &u32_at(56).to_le_bytes()); // blksize
    put(16, &u32_at(20).to_le_bytes()); // nlink
    put(20, &u32_at(24).to_le_bytes()); // uid
    put(24, &u32_at(28).to_le_bytes()); // gid
    put(28, &(u32_at(16) as u16).to_le_bytes()); // mode
    put(32, &u64_at(8).to_le_bytes()); // ino
    put(40, &u64_at(48).to_le_bytes()); // size
    put(48, &u64_at(64).to_le_bytes()); // blocks
    // atime、ctime、mtime：秒和纳秒（statx_timestamp 是 i64 + u32 + 保留）
    for (dst, src) in [(64, 72), (96, 104), (112, 88)] {
        put(dst, &u64_at(src).to_le_bytes());
        put(dst + 8, &(u64_at(src + 8) as u32).to_le_bytes());
    }
    let (rdev, dev) = (u64_at(32), u64_at(0));
    put(128, &major(rdev).to_le_bytes());
    put(132, &minor(rdev).to_le_bytes());
    put(136, &major(dev).to_le_bytes());
    put(140, &minor(dev).to_le_bytes());
    b
}

pub(super) fn fstat(cx: &mut Cx<'_>, fd: i32, buf: u64) -> SysResult {
    let st = owner_out(cx, file_stat(cx, &cx.fds().get(fd)?)?);
    put(cx, buf, st).map(|()| 0)
}

fn encode_statfs(magic: u64, s: &appine_hal::FsStats, flags: u64) -> [u8; 120] {
    // asm-generic 的 struct statfs（64 位）
    let mut b = [0u8; 120];
    let words = [
        magic,
        s.block_size,
        s.blocks,
        s.free,
        s.available,
        s.files,
        s.files_free,
        0, // f_fsid
        255,
        s.block_size,
        statfs_flags(flags),
    ];
    for (i, w) in words.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    b
}

/// statfs 的 `f_flags`（Linux 的 `calculate_f_flags`）：挂载的标志加上 `ST_VALID`。
fn statfs_flags(flags: u64) -> u64 {
    const ST_VALID: u64 = 0x20;
    const ST_RELATIME: u64 = 0x1000;
    const ST_NOSYMFOLLOW: u64 = 0x2000;
    // RDONLY、NOSUID、NODEV、NOEXEC、NOATIME、NODIRATIME 的取值与 MS_* 相同
    let same = ms::RDONLY | ms::NOSUID | ms::NODEV | ms::NOEXEC | ms::NOATIME | ms::NODIRATIME;
    let mut f = ST_VALID | (flags & same);
    if flags & ms::RELATIME != 0 {
        f |= ST_RELATIME;
    }
    if flags & ms::NOSYMFOLLOW != 0 {
        f |= ST_NOSYMFOLLOW;
    }
    f
}

/// 节点所在文件系统的 statfs。
fn statfs_of(cx: &Cx<'_>, node: &Node) -> Result<[u8; 120], Errno> {
    let empty = appine_hal::FsStats {
        block_size: 4096,
        ..Default::default()
    };
    let vfs = cx.vfs()?;
    let flags = vfs.mount_flags(node).unwrap_or(0);
    // tmpfs 按页数、inode 数（Linux 的 shmem_statfs）：不限的那一项都是 0；用量现数一遍
    if let Some(dev) = vfs.tmpfs_dev(node) {
        let (p, pages, inodes) = vfs.tmpfs_usage(dev, true)?;
        let free = p.blocks.saturating_sub(pages);
        let s = appine_hal::FsStats {
            block_size: TMPFS_PAGE,
            blocks: p.blocks,
            free,
            available: free,
            files: p.inodes,
            files_free: p.inodes.saturating_sub(inodes),
        };
        return Ok(encode_statfs(TMPFS_MAGIC, &s, flags));
    }
    Ok(match node {
        Node::Store(..) => encode_statfs(EXT4_SUPER_MAGIC, &vfs.store().fs_stats()?, flags),
        Node::Pseudo(_, PseudoNode::NsFile(..)) => encode_statfs(NSFS_MAGIC, &empty, 0),
        Node::Pseudo(_, p) => {
            let magic = match pseudo::fs_of(p) {
                Fs::Store => EXT4_SUPER_MAGIC,
                Fs::Tmpfs => TMPFS_MAGIC,
                Fs::Proc => PROC_SUPER_MAGIC,
                Fs::Pts => DEVPTS_SUPER_MAGIC,
                Fs::Sys => SYSFS_MAGIC,
                Fs::Cgroup => CGROUP2_SUPER_MAGIC,
                Fs::Nsfs => NSFS_MAGIC,
                Fs::Mqueue => MQUEUE_MAGIC,
            };
            encode_statfs(magic, &empty, flags)
        }
    })
}

pub(super) fn statfs(cx: &mut Cx<'_>, path: u64, buf: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    let b = statfs_of(cx, &node)?;
    put(cx, buf, b).map(|()| 0)
}

pub(super) fn fstatfs(cx: &mut Cx<'_>, fd: i32, buf: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let (pipe, node) = {
        let f = file.borrow();
        (matches!(f.kind, FileKind::Pipe(_)), f.node())
    };
    // 消息队列的 fd 在 mqueue 文件系统里
    if matches!(file.borrow().kind, FileKind::MessageQueue(_)) {
        let empty = appine_hal::FsStats {
            block_size: 4096,
            ..Default::default()
        };
        return put(cx, buf, encode_statfs(MQUEUE_MAGIC, &empty, 0)).map(|()| 0);
    }
    let b = match (pipe, node) {
        (true, _) => encode_statfs(
            PIPEFS_MAGIC,
            &appine_hal::FsStats {
                block_size: 4096,
                ..Default::default()
            },
            0,
        ),
        (false, Some(node)) => statfs_of(cx, &node)?,
        (false, None) => statfs_of(cx, &Node::ROOT)?,
    };
    put(cx, buf, b).map(|()| 0)
}

// ---- 修改命名空间 ----

pub(super) fn mkdirat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    if let Some(r) = cgroup_mkdir(cx, dirfd, &path) {
        return r.map(|()| 0);
    }
    let (mnt, dir, name) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    let (uid, gid, perm) = cred.new_owner(&dir_st, perm & !cx.umask() & 0o7777, true);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    vfs.store().mkdir(dir, name, perm, uid, gid)?;
    tmpfs_created(&vfs, tmpfs, 0);
    inotify::dir_event(
        cx.kr(),
        dir,
        name,
        inotify::IN_CREATE | inotify::IN_ISDIR,
        0,
    );
    Ok(0)
}

pub(super) fn mknodat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    mode_arg: u32,
    dev: u64,
) -> SysResult {
    let path = read_path(cx, path)?;
    let (mnt, dir, name) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let kind = mode_arg & mode::S_IFMT;
    match kind {
        0 | mode::S_IFREG | mode::S_IFCHR | mode::S_IFBLK | mode::S_IFIFO | mode::S_IFSOCK => {}
        mode::S_IFDIR => return Err(Errno::EPERM),
        _ => return Err(Errno::EINVAL),
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    reject_trailing_slash(vfs.store(), dir, name, &path)?;
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    // whiteout（字符设备 0:0）不要 CAP_MKNOD，也不过设备过滤（Linux 的 vfs_mknod）
    let whiteout = kind == mode::S_IFCHR && dev & 0xffff_ffff == 0;
    if matches!(kind, mode::S_IFCHR | mode::S_IFBLK) && !whiteout {
        if !cred.capable(cap::MKNOD) {
            return Err(Errno::EPERM);
        }
        super::bpf::check_device(cx, kind, dev & 0xffff_ffff, crate::bpf::ACC_MKNOD)?;
    }
    let (uid, gid, perm) = cred.new_owner(&dir_st, mode_arg & !cx.umask() & 0o7777, false);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    if kind == 0 || kind == mode::S_IFREG {
        vfs.store().create_file(dir, name, perm, uid, gid)?;
    } else {
        vfs.store()
            .mknod(dir, name, kind | perm, dev & 0xffff_ffff, uid, gid)?;
    }
    tmpfs_created(&vfs, tmpfs, 0);
    inotify::dir_event(cx.kr(), dir, name, inotify::IN_CREATE, 0);
    Ok(0)
}

/// 删除 mqueue 文件系统里的队列（与 mq_unlink 一样）：父目录是 mqueue 的根目录时返回
/// Some。顺序照 Linux 的 do_unlinkat、do_rmdir：`.`、`..`，只读的挂载，找不到，unlink 以
/// `/` 结尾的名字，目录的写权限，粘滞位（EPERM），rmdir 的 ENOTDIR。
fn unlink_mq(cx: &mut Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Option<Result<(), Errno>> {
    let start = start_dir(cx, dirfd, path).ok()?;
    let vfs = cx.vfs().ok()?;
    let cred = cx.cred();
    let (dir, name) = vfs
        .resolve_parent(&cx.view(&vfs), &cred, &cx.root(), &start, path)
        .ok()?;
    let Node::Pseudo(_, PseudoNode::MqRoot(ns)) = dir else {
        return None;
    };
    let rmdir = flags & AT_REMOVEDIR != 0;
    Some((|| {
        match name {
            b"." if rmdir => return Err(Errno::EINVAL),
            b".." if rmdir => return Err(Errno::ENOTEMPTY),
            b"." | b".." => return Err(Errno::EISDIR),
            _ => {}
        }
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let ipc = cx.kr().ipc(ns);
        if !ipc.mqueues.contains_key(name) {
            return Err(Errno::ENOENT);
        }
        // unlink 以 / 结尾的名字在 may_delete 之前就是 ENOTDIR
        if !rmdir && path.ends_with(b"/") {
            return Err(Errno::ENOTDIR);
        }
        let dir_st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
        cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
        if rmdir {
            // rmdir 先查粘滞位（may_delete），队列不是目录
            let q = ipc.mqueues[name].borrow().stat();
            let dir_uid = ipc.mq_dir.borrow().uid;
            if cred.fsuid != q.uid && cred.fsuid != dir_uid && !cred.capable_wrt(&q, cap::FOWNER) {
                return Err(Errno::EPERM);
            }
            return Err(Errno::ENOTDIR);
        }
        super::mqueue::remove_queue(cx.k().ipc_mut(ns), &cred, name)
    })())
}

/// 父目录是 cgroup 的目录时：`name` 是那个 cgroup 的编号、它的元数据和最后一个组件。
fn cgroup_parent<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Option<(Node, u64, Stat, &'p [u8])> {
    let start = start_dir(cx, dirfd, path).ok()?;
    let vfs = cx.vfs().ok()?;
    let (dir, name) = vfs
        .resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)
        .ok()?;
    let Node::Pseudo(_, PseudoNode::Cgroup(id, b"")) = dir else {
        return None;
    };
    let st = pseudo::stat(&cx.view(&vfs), &PseudoNode::Cgroup(id, b"")).ok()?;
    Some((dir, id, st, name))
}

/// 在 cgroup 的目录里 mkdir：建子 cgroup（[`crate::cgroup::Cgroups::mkdir`]）。顺序照 Linux：
/// `.`、`..` 和已经有的名字是 EEXIST，只读的挂载 EROFS，没有目录的写权限 EACCES。
fn cgroup_mkdir(cx: &mut Cx<'_>, dirfd: i32, path: &[u8]) -> Option<Result<(), Errno>> {
    let (dir, id, st, name) = cgroup_parent(cx, dirfd, path)?;
    Some((|| {
        let vfs = cx.vfs()?;
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let cgroups = &cx.kr().cgroups;
        let exists = cgroups
            .get(id)
            .is_some_and(|g| g.children.contains_key(name))
            || cgroups.files(id).any(|f| f == name);
        if name == b"." || name == b".." || exists {
            return Err(Errno::EEXIST);
        }
        let cred = cx.cred();
        cred.permission(&st, MAY_WRITE | MAY_EXEC)?;
        cx.k().cgroups.mkdir(id, name, &cred).map(|_| ())
    })())
}

/// 在 cgroup 的目录里 unlink、rmdir：rmdir 删掉子 cgroup（[`crate::cgroup::rmdir`]），文件
/// 删不了（EPERM，cgroup 的目录没有 unlink）。
fn cgroup_unlink(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: &[u8],
    flags: u32,
) -> Option<Result<(), Errno>> {
    let (dir, id, st, name) = cgroup_parent(cx, dirfd, path)?;
    let rmdir = flags & AT_REMOVEDIR != 0;
    Some((|| {
        match name {
            b"." if rmdir => return Err(Errno::EINVAL),
            b".." if rmdir => return Err(Errno::ENOTEMPTY),
            b"." | b".." => return Err(Errno::EISDIR),
            _ => {}
        }
        let vfs = cx.vfs()?;
        if vfs.readonly(&dir) {
            return Err(Errno::EROFS);
        }
        let cgroups = &cx.kr().cgroups;
        let child = cgroups.get(id).and_then(|g| g.children.get(name).copied());
        let file = cgroups.files(id).any(|f| f == name);
        if child.is_none() && !file {
            return Err(Errno::ENOENT);
        }
        cx.cred().permission(&st, MAY_WRITE | MAY_EXEC)?;
        match (rmdir, child) {
            (true, Some(c)) => crate::cgroup::rmdir(cx.k(), c),
            (true, None) => Err(Errno::ENOTDIR),
            (false, Some(_)) => Err(Errno::EISDIR),
            (false, None) => Err(Errno::EPERM),
        }
    })())
}

pub(super) fn unlinkat(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    unlink(cx, dirfd, path, flags).map(|()| 0)
}

fn unlink(cx: &mut Cx<'_>, dirfd: i32, path: Vec<u8>, flags: u32) -> Result<(), Errno> {
    if let Some(r) = unlink_mq(cx, dirfd, &path, flags) {
        return r;
    }
    if let Some(r) = cgroup_unlink(cx, dirfd, &path, flags) {
        return r;
    }
    let (mnt, dir, name) = parent_at(cx, dirfd, &path)?;
    let store_vfs = cx.vfs()?;
    let store = store_vfs.store();
    let cred = cx.cred();
    // 有 inotify 监视时先记下要删除的 inode
    let victim = match name {
        b"." | b".." => None,
        _ if inotify::active(cx.kr()) => store.lookup(dir, name)?,
        _ => None,
    };
    if flags & AT_REMOVEDIR != 0 {
        match name {
            b"." => return Err(Errno::EINVAL),
            b".." => return Err(Errno::ENOTEMPTY),
            _ => {
                writable_dir(&store_vfs, (mnt, dir))?;
                may_delete(store, &cred, dir, name)?;
                let gone = not_mount_point(cx, &store_vfs, dir, name)?;
                let tmpfs = tmpfs_victim(&store_vfs, (mnt, dir), name)?;
                store.rmdir(dir, name)?;
                tmpfs_removed(&store_vfs, tmpfs);
                detach_points(cx, &store_vfs, gone, (dir, name));
            }
        }
        if let Some(ino) = victim {
            let k = cx.kr();
            inotify::deleted(k, ino, dir, name);
            inotify::dir_event(k, dir, name, inotify::IN_DELETE | inotify::IN_ISDIR, 0);
        }
    } else {
        if name == b"." || name == b".." {
            return Err(Errno::EISDIR);
        }
        writable_dir(&store_vfs, (mnt, dir))?;
        // 以 / 结尾的名字只能是目录：是目录时 EISDIR，否则 ENOTDIR
        if path.ends_with(b"/") {
            let ino = store.lookup(dir, name)?.ok_or(Errno::ENOENT)?;
            return Err(if store.stat(ino)?.is_dir() {
                Errno::EISDIR
            } else {
                Errno::ENOTDIR
            });
        }
        may_delete(store, &cred, dir, name)?;
        let gone = not_mount_point(cx, &store_vfs, dir, name)?;
        let tmpfs = tmpfs_victim(&store_vfs, (mnt, dir), name)?;
        store.unlink(dir, name)?;
        tmpfs_removed(&store_vfs, tmpfs);
        detach_points(cx, &store_vfs, gone, (dir, name));
        if let Some(ino) = victim {
            unlinked_events(cx, store, ino, (dir, name));
            inotify::dir_event(cx.kr(), dir, name, inotify::IN_DELETE, 0);
        }
    }
    Ok(())
}

/// 删除、改名之前：目录 `dir` 里的 `name` 上挂着调用者的命名空间里的挂载时是 EBUSY
/// （Linux 的 `is_local_mountpoint`）。返回它的 inode，删掉之后用来卸下别的命名空间里挂在
/// 上面的（[`detach_points`]）。
fn not_mount_point(cx: &Cx<'_>, vfs: &Vfs, dir: Ino, name: &[u8]) -> Result<Option<Ino>, Errno> {
    match vfs.store().lookup(dir, name)? {
        Some(ino) if vfs.is_store_point((ino, dir, name), cx.mnt_ns()) => Err(Errno::EBUSY),
        found => Ok(found),
    }
}

/// 删掉了目录 `dir` 里的 `name`（原来是 `ino`）：别的命名空间里挂在它上面的挂载卸下
/// （Linux 的 vfs_rmdir、vfs_unlink、vfs_rename 里的 `detach_mounts`）。
fn detach_points(cx: &Cx<'_>, vfs: &Vfs, ino: Option<Ino>, (dir, name): (Ino, &[u8])) {
    if let Some(ino) = ino {
        vfs.detach_store_point((ino, dir, name));
        cx.shared.fast_changed();
    }
}

/// 去掉了文件 `ino` 的链接 `name`（unlink、被 rename 覆盖）：链接数变了（IN_ATTRIB），
/// 最后一个链接也没了时 IN_DELETE_SELF。这两个事件只报给文件自己。
fn unlinked_events(cx: &Cx<'_>, store: &Store, ino: Ino, (dir, name): (Ino, &[u8])) {
    let k = cx.kr();
    inotify::path_event(k, ino, None, inotify::IN_ATTRIB);
    if store.stat(ino).map_or(0, |st| st.nlink) == 0 {
        inotify::deleted(k, ino, dir, name);
    }
}

pub(super) fn symlinkat(cx: &mut Cx<'_>, target: u64, dirfd: i32, path: u64) -> SysResult {
    let target = read_path(cx, target)?;
    if target.is_empty() {
        return Err(Errno::ENOENT);
    }
    let path = read_path(cx, path)?;
    let (mnt, dir, name) = create_parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    reject_trailing_slash(vfs.store(), dir, name, &path)?;
    let dir_st = may_create(&vfs, &cred, (mnt, dir), name)?;
    let (uid, gid, _) = cred.new_owner(&dir_st, 0o777, false);
    // 长的符号链接在 tmpfs 里另占一页（Linux 的 SHORT_SYMLINK_LEN）
    let pages = u64::from(target.len() >= 128);
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), pages)?;
    vfs.store().symlink(dir, name, &target, uid, gid)?;
    tmpfs_created(&vfs, tmpfs, pages);
    inotify::dir_event(cx.kr(), dir, name, inotify::IN_CREATE, 0);
    Ok(0)
}

pub(super) fn linkat(
    cx: &mut Cx<'_>,
    olddirfd: i32,
    oldpath: u64,
    newdirfd: i32,
    newpath: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let oldpath = read_path(cx, oldpath)?;
    let cred = cx.cred();
    let src = lookup_at_or_fd(
        cx,
        &cred,
        olddirfd,
        &oldpath,
        flags,
        flags & AT_SYMLINK_FOLLOW != 0,
    )?;
    let newpath = read_path(cx, newpath)?;
    let start = start_dir(cx, newdirfd, &newpath)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cred, &cx.root(), &start, &newpath)?;
    if matches!(dir, Node::Pseudo(..)) && vfs.has_child(&cx.view(&vfs), &dir, name) {
        return Err(Errno::EEXIST);
    }
    // 合成文件系统是另一个"设备"：跨设备不能建硬链接
    let (Node::Store(src_mnt, src), Node::Store(mnt, dir)) = (src, dir) else {
        return Err(Errno::EXDEV);
    };
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    reject_trailing_slash(vfs.store(), dir, name, &newpath)?;
    // 两边经由不同的挂载（同一个文件系统 bind 到两处也是）是 EXDEV，在名字已经存在、
    // 只读的挂载之后，在目录可写之前（Linux 的 do_linkat）
    let dir_st = may_create_name(&vfs, &cred, (mnt, dir), name)?;
    if src_mnt != mnt {
        return Err(Errno::EXDEV);
    }
    // fs.protected_hardlinks（Linux 的 may_linkat，在 EXDEV 之后、目录的权限之前）：不是属主、
    // 也没有 CAP_FOWNER 时，只能链接能读写的普通文件，不能是 setuid 的、可执行的 setgid 的
    if cx.kr().protected_hardlinks {
        let st = vfs.store().stat(src)?;
        let setgid_exec = mode::S_ISGID | 0o010;
        let safe = st.mode & mode::S_IFMT == mode::S_IFREG
            && st.mode & mode::S_ISUID == 0
            && st.mode & setgid_exec != setgid_exec
            && cred.permission(&st, MAY_READ | MAY_WRITE).is_ok();
        if !safe && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
    }
    cred.permission(&dir_st, MAY_WRITE | MAY_EXEC)?;
    // tmpfs 里每个硬链接也占一个 inode（Linux 的 shmem_link）
    let tmpfs = tmpfs_reserve(&vfs, (mnt, dir), 0)?;
    vfs.store().link(dir, name, src)?;
    tmpfs_created(&vfs, tmpfs, 0);
    inotify::path_event(cx.kr(), src, None, inotify::IN_ATTRIB);
    inotify::dir_event(cx.kr(), dir, name, inotify::IN_CREATE, 0);
    Ok(0)
}

pub(super) fn renameat2(
    cx: &mut Cx<'_>,
    olddirfd: i32,
    oldpath: u64,
    newdirfd: i32,
    newpath: u64,
    flags: u32,
) -> SysResult {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0
        || flags == RENAME_NOREPLACE | RENAME_EXCHANGE
    {
        return Err(Errno::EINVAL);
    }
    let oldpath = read_path(cx, oldpath)?;
    let newpath = read_path(cx, newpath)?;
    let (old_mnt, old_dir, old_name) = parent_at(cx, olddirfd, &oldpath)?;
    let (new_mnt, new_dir, new_name) = parent_at(cx, newdirfd, &newpath)?;
    // 两边经由不同的挂载是 EXDEV，然后才看 `.`、`..` 和只读的挂载（Linux 的 do_renameat2）
    if old_mnt != new_mnt {
        return Err(Errno::EXDEV);
    }
    if [old_name, new_name]
        .iter()
        .any(|n| *n == b"." || *n == b"..")
    {
        return Err(Errno::EBUSY);
    }
    let vfs = cx.vfs()?;
    writable_dir(&vfs, (old_mnt, old_dir))?;
    // 任何一边以 / 结尾，被移动的就必须是目录
    if (oldpath.ends_with(b"/") || newpath.ends_with(b"/"))
        && let Some(src) = vfs.store().lookup(old_dir, old_name)?
        && !vfs.store().attrs(src)?.is_dir()
    {
        return Err(Errno::ENOTDIR);
    }
    let exchange = flags & RENAME_EXCHANGE != 0;
    may_rename(
        vfs.store(),
        &cx.cred(),
        (old_dir, old_name),
        (new_dir, new_name),
        exchange,
    )?;
    not_mount_point(cx, &vfs, old_dir, old_name)?;
    let overwritten = not_mount_point(cx, &vfs, new_dir, new_name)?;
    // 有 inotify 监视时先记下两边的 inode 和它们是不是目录
    let entry = |dir, name| -> Result<Option<(Ino, u32)>, Errno> {
        match vfs.store().lookup(dir, name)? {
            Some(ino) if vfs.store().attrs(ino)?.is_dir() => Ok(Some((ino, inotify::IN_ISDIR))),
            Some(ino) => Ok(Some((ino, 0))),
            None => Ok(None),
        }
    };
    let (src, dst) = if inotify::active(cx.kr()) {
        (entry(old_dir, old_name)?, entry(new_dir, new_name)?)
    } else {
        (None, None)
    };
    // 覆盖掉的名字在 tmpfs 里空出来（改名成自己的另一个硬链接什么也不做）
    let replaced = match exchange {
        false
            if vfs.store().lookup(old_dir, old_name)?
                != vfs.store().lookup(new_dir, new_name)? =>
        {
            tmpfs_victim(&vfs, (new_mnt, new_dir), new_name)?
        }
        _ => None,
    };
    let moving = vfs.store().lookup(old_dir, old_name)?;
    vfs.store().rename(
        old_dir,
        old_name,
        new_dir,
        new_name,
        flags & RENAME_NOREPLACE != 0,
        exchange,
    )?;
    tmpfs_removed(&vfs, replaced);
    // 覆盖掉的目标上别的命名空间里的挂载卸下
    if !exchange && overwritten != moving {
        detach_points(cx, &vfs, overwritten, (new_dir, new_name));
    }
    // 经由移动的名字打开着的文件跟着改名
    let k = cx.kr();
    let store = vfs.store();
    let lookup = |dir, name| move || store.lookup(dir, name).ok().flatten();
    inotify::renamed(
        k,
        (old_dir, old_name),
        (new_dir, new_name),
        lookup(new_dir, new_name),
    );
    if exchange {
        inotify::renamed(
            k,
            (new_dir, new_name),
            (old_dir, old_name),
            lookup(old_dir, old_name),
        );
    }
    // 顺序与 Linux 的 fsnotify_move 相同；交换时另一边再移动一次
    let moved = |isdir: u32, from: (Ino, &[u8]), to: (Ino, &[u8])| {
        let cookie = inotify::cookie(k);
        inotify::dir_event(k, from.0, from.1, inotify::IN_MOVED_FROM | isdir, cookie);
        inotify::dir_event(k, to.0, to.1, inotify::IN_MOVED_TO | isdir, cookie);
    };
    if let Some(src) = src {
        moved(src.1, (old_dir, old_name), (new_dir, new_name));
        match dst {
            Some(dst) if exchange => {
                inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF);
                moved(dst.1, (new_dir, new_name), (old_dir, old_name));
                inotify::path_event(k, dst.0, None, inotify::IN_MOVE_SELF);
            }
            Some((replaced, isdir)) => {
                inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF);
                if isdir != 0 {
                    inotify::deleted(k, replaced, new_dir, new_name);
                } else {
                    unlinked_events(cx, vfs.store(), replaced, (new_dir, new_name));
                }
            }
            None => inotify::path_event(k, src.0, None, inotify::IN_MOVE_SELF),
        }
    }
    Ok(0)
}

/// rename 之前的检查：两边的目录都要可写并遵守粘滞位；目录换了上级时，它自己也要
/// 可写（要修改它的 `..`）。
fn may_rename(
    store: &Store,
    cred: &Cred,
    (old_dir, old_name): (Ino, &[u8]),
    (new_dir, new_name): (Ino, &[u8]),
    exchange: bool,
) -> Result<(), Errno> {
    let src = may_delete(store, cred, old_dir, old_name)?;
    let dst = match store.lookup(new_dir, new_name)? {
        Some(_) => Some(may_delete(store, cred, new_dir, new_name)?),
        None => {
            cred.permission(&store.stat(new_dir)?, MAY_WRITE | MAY_EXEC)?;
            None
        }
    };
    if old_dir != new_dir {
        let moved = [Some(src), dst.filter(|_| exchange)];
        for st in moved.iter().flatten().filter(|st| st.is_dir()) {
            cred.permission(st, MAY_WRITE)?;
        }
    }
    Ok(())
}

pub(super) fn fchmodat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    let target = Attrs::of(cx, &lookup_at(cx, dirfd, &path, true)?)?;
    chmod(cx, &target, perm)?;
    if let Attrs::Store(ino) = target {
        attrib_at(cx, ino, dirfd, &path, true);
    }
    Ok(0)
}

pub(super) fn fchmod(cx: &mut Cx<'_>, fd: i32, perm: u32) -> SysResult {
    let target = Attrs::of_fd(cx, fd)?;
    chmod(cx, &target, perm)?;
    attrib_fd(cx, fd);
    Ok(0)
}

/// chmod、chown、utimensat 作用的对象：仓库里的 inode，或者 `/dev/pts/<n>`（属主、权限和
/// 时间记在终端上，与 Linux 的 devpts 一样）。合成文件系统里别的节点不能修改。
enum Attrs {
    Store(Ino),
    Pts(Rc<RefCell<Tty>>),
    /// socket、匿名管道的 fd：Linux 上改的是 sockfs、pipefs 里它们自己的 inode，属主是
    /// 创建它的进程（libuv 的 `uv_pipe_chmod` 先 fchmod socket 的 fd）。这里当作调用者
    /// 的，改了不记下来，fstat 报告的属主和权限照旧是合成的。
    Anon,
    /// POSIX 消息队列：mqueue 文件系统里的文件，或者 mq_open 得到的 fd。
    Mq(Rc<RefCell<super::MessageQueue>>),
    /// IPC 命名空间（编号）的 mqueue 文件系统的根目录。
    MqDir(u64),
}

impl Attrs {
    fn of(cx: &Cx<'_>, node: &Node) -> Result<Attrs, Errno> {
        match node {
            Node::Pseudo(_, PseudoNode::Pts(..)) if cx.vfs()?.readonly(node) => Err(Errno::EROFS),
            Node::Pseudo(_, PseudoNode::Pts(fs, n)) => pseudo::pty(cx.kr(), *fs, *n)
                .map(Attrs::Pts)
                .ok_or(Errno::ENOENT),
            Node::Pseudo(_, PseudoNode::MqRoot(_) | PseudoNode::Mq(..))
                if cx.vfs()?.readonly(node) =>
            {
                Err(Errno::EROFS)
            }
            Node::Pseudo(_, PseudoNode::MqRoot(ns)) => Ok(Attrs::MqDir(*ns)),
            Node::Pseudo(_, PseudoNode::Mq(ns, ino)) => mq_queue(cx, *ns, *ino).map(Attrs::Mq),
            _ => store_ino(cx, node).map(Attrs::Store),
        }
    }

    /// 打开的文件：终端的从设备，或者仓库里的文件（`O_PATH` 打开的不行）。
    fn of_fd(cx: &Cx<'_>, fd: i32) -> Result<Attrs, Errno> {
        let file = cx.fds().get(fd)?;
        let f = file.borrow();
        match &f.kind {
            FileKind::Pty(end) if !end.master && f.flags & oflags::O_PATH == 0 => {
                Ok(Attrs::Pts(end.tty.clone()))
            }
            FileKind::Socket(_) => Ok(Attrs::Anon),
            FileKind::Pipe(end) if end.pipe.borrow().fifo.is_none() => Ok(Attrs::Anon),
            FileKind::MessageQueue(q) => Ok(Attrs::Mq(q.clone())),
            _ => {
                drop(f);
                file_ino(cx, fd).map(Attrs::Store)
            }
        }
    }

    fn stat(&self, cx: &Cx<'_>) -> Result<Stat, Errno> {
        match self {
            Attrs::Store(ino) => cx.vfs()?.store().attrs(*ino),
            Attrs::Pts(t) => {
                let t = t.borrow();
                Ok(Stat {
                    mode: mode::S_IFCHR | t.mode,
                    uid: t.owner.0,
                    gid: t.owner.1,
                    ..Stat::default()
                })
            }
            Attrs::Anon => {
                let cred = cx.cred();
                Ok(Stat {
                    uid: cred.fsuid,
                    gid: cred.fsgid,
                    ..Stat::default()
                })
            }
            Attrs::Mq(q) => Ok(q.borrow().stat()),
            Attrs::MqDir(ns) => {
                let ipc = cx.kr().ipc(*ns);
                Ok(ipc.mq_dir.borrow().stat(ipc.mqueues.len()))
            }
        }
    }
}

/// 按路径改了元数据：报告 IN_ATTRIB（也报给所在的目录）。
pub(super) fn attrib_at(cx: &Cx<'_>, ino: Ino, dirfd: i32, path: &[u8], follow: bool) {
    if inotify::active(cx.kr()) {
        let parent = inotify::watched_parent(cx, dirfd, path, follow);
        let parent = parent.as_ref().map(|(d, n)| (*d, n.as_slice()));
        let dir = match cx.vfs().map(|v| v.store().kind(ino)) {
            Ok(Ok(mode::S_IFDIR)) => inotify::IN_ISDIR,
            _ => 0,
        };
        inotify::path_event(cx.kr(), ino, parent, inotify::IN_ATTRIB | dir);
    }
}

/// 按 fd 改了元数据：报告 IN_ATTRIB。
pub(super) fn attrib_fd(cx: &Cx<'_>, fd: i32) {
    if let Ok(file) = cx.fds().get(fd) {
        inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_ATTRIB);
    }
}

/// 只有属主（或者有 `CAP_FOWNER`）能修改权限位；不在文件属组里的调用者不能设置
/// setgid 位（没有 `CAP_FSETID` 时静默去掉）。
fn chmod(cx: &Cx<'_>, target: &Attrs, perm: u32) -> SysResult {
    let st = target.stat(cx)?;
    let cred = cx.cred();
    if !cred.owns(&st) {
        return Err(Errno::EPERM);
    }
    let mut perm = perm & 0o7777;
    if !cred.in_group(st.gid) && !cred.capable_wrt(&st, cap::FSETID) {
        perm &= !mode::S_ISGID;
    }
    match target {
        Attrs::Store(ino) => cx.vfs()?.store().set_mode(*ino, perm)?,
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            t.mode = perm;
            t.changed();
        }
        Attrs::Anon => {}
        Attrs::Mq(q) => q.borrow_mut().set_mode(perm),
        Attrs::MqDir(ns) => {
            let ipc = cx.kr().ipc(*ns);
            let mut d = ipc.mq_dir.borrow_mut();
            d.mode = perm;
            d.ctime_ns = crate::tty::wall_ns();
        }
    }
    Ok(0)
}

fn owner_arg(id: u32) -> Option<u32> {
    (id != u32::MAX).then_some(id)
}

pub(super) fn fchownat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    uid: u32,
    gid: u32,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let node = lookup_at_or_fd(cx, &cx.cred(), dirfd, &path, flags, follow)?;
    let target = Attrs::of(cx, &node)?;
    chown(cx, &target, uid, gid)?;
    if path.is_empty() {
        attrib_fd(cx, dirfd);
    } else if let Attrs::Store(ino) = target {
        attrib_at(cx, ino, dirfd, &path, follow);
    }
    Ok(0)
}

pub(super) fn fchown(cx: &mut Cx<'_>, fd: i32, uid: u32, gid: u32) -> SysResult {
    let target = Attrs::of_fd(cx, fd)?;
    chown(cx, &target, uid, gid)?;
    attrib_fd(cx, fd);
    Ok(0)
}

/// 没有 `CAP_CHOWN` 时，属主只能把属组改成自己所在的组，不能修改属主。ID 是调用者的用户
/// 命名空间里的，映射不到是 EINVAL；`CAP_CHOWN` 要文件原来的属主、属组在里面有映射。
fn chown(cx: &Cx<'_>, target: &Attrs, uid: u32, gid: u32) -> SysResult {
    let cred = cx.cred();
    let uid = owner_arg(uid)
        .map(|u| cred.uid_in(u).ok_or(Errno::EINVAL))
        .transpose()?;
    let gid = owner_arg(gid)
        .map(|g| cred.gid_in(g).ok_or(Errno::EINVAL))
        .transpose()?;
    let st = target.stat(cx)?;
    let owner = cred.fsuid == st.uid;
    let uid_ok = uid.is_none_or(|u| owner && u == st.uid);
    let gid_ok = gid.is_none_or(|g| owner && (g == st.gid || cred.in_group(g)));
    if !(cred.capable_wrt(&st, cap::CHOWN) || uid_ok && gid_ok) {
        return Err(Errno::EPERM);
    }
    match target {
        Attrs::Store(ino) => cx.vfs()?.store().set_owner(*ino, uid, gid)?,
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            t.owner = (uid.unwrap_or(t.owner.0), gid.unwrap_or(t.owner.1));
            t.changed();
        }
        Attrs::Anon => {}
        Attrs::Mq(q) => q.borrow_mut().set_owner(uid, gid),
        Attrs::MqDir(ns) => {
            let ipc = cx.kr().ipc(*ns);
            let mut d = ipc.mq_dir.borrow_mut();
            (d.uid, d.gid) = (uid.unwrap_or(d.uid), gid.unwrap_or(d.gid));
            d.ctime_ns = crate::tty::wall_ns();
        }
    }
    Ok(0)
}

pub(super) fn utimensat(
    cx: &mut Cx<'_>,
    dirfd: i32,
    path: u64,
    times: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    // path 为 NULL 时作用于 dirfd 本身（futimens）
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let (target, path) = if path == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        (Attrs::of_fd(cx, dirfd)?, Vec::new())
    } else {
        let path = read_path(cx, path)?;
        let node = lookup_at_or_fd(cx, &cx.cred(), dirfd, &path, flags, follow)?;
        (Attrs::of(cx, &node)?, path)
    };
    // times[0] 是 atime，times[1] 是 mtime。现在的时间由仓库取（普通文件交给宿主）
    let (atime, mtime, explicit) = if times == 0 {
        (Some(FileTime::Now), Some(FileTime::Now), false)
    } else {
        let b: [u8; 32] = get(cx, times)?;
        let word = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        for nsec in [word(1), word(3)] {
            if nsec != UTIME_NOW && nsec != UTIME_OMIT && !(0..1_000_000_000).contains(&nsec) {
                return Err(Errno::EINVAL);
            }
        }
        if word(1) == UTIME_OMIT && word(3) == UTIME_OMIT {
            return Ok(0);
        }
        let explicit = [word(1), word(3)]
            .iter()
            .any(|&n| n != UTIME_NOW && n != UTIME_OMIT);
        let time = |sec: i64, nsec: i64| match nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(FileTime::Now),
            nsec => Some(FileTime::At(sec * 1_000_000_000 + nsec)),
        };
        (time(word(0), word(1)), time(word(2), word(3)), explicit)
    };
    // 设成指定的时间只有属主可以，设成现在的时间有写权限也可以
    let st = target.stat(cx)?;
    let cred = cx.cred();
    if !cred.owns(&st) {
        if explicit {
            return Err(Errno::EPERM);
        }
        cred.permission(&st, MAY_WRITE)?;
    }
    let ino = match target {
        Attrs::Store(ino) => ino,
        // 终端的时间记在终端上（与 Linux 的 devpts 一样可以改）
        Attrs::Pts(t) => {
            let mut t = t.borrow_mut();
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            t.atime_ns = atime.map_or(t.atime_ns, ns);
            t.mtime_ns = mtime.map_or(t.mtime_ns, ns);
            t.changed();
            return Ok(0);
        }
        Attrs::Anon => return Ok(0),
        // 消息队列和 mqueue 的根目录的时间记在内核层里
        Attrs::Mq(q) => {
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            q.borrow_mut().set_times(atime.map(ns), mtime.map(ns));
            return Ok(0);
        }
        Attrs::MqDir(id) => {
            let now = crate::tty::wall_ns();
            let ns = |time: FileTime| match time {
                FileTime::Now => now,
                FileTime::At(ns) => ns,
            };
            let ipc = cx.kr().ipc(id);
            let mut d = ipc.mq_dir.borrow_mut();
            d.atime_ns = atime.map_or(d.atime_ns, ns);
            d.mtime_ns = mtime.map_or(d.mtime_ns, ns);
            d.ctime_ns = now;
            return Ok(0);
        }
    };
    // 按 fd 设置的普通文件经由打开的 object 设置：删除之后仍然打开着的文件也能用
    let open = match (path.is_empty(), dirfd) {
        (true, fd) if fd != AT_FDCWD => Some(cx.fds().get(fd)?),
        _ => None,
    };
    let vfs = cx.vfs()?;
    if let Some(f) = open.as_ref().map(|f| f.borrow())
        && let FileKind::Regular { file, .. } = &f.kind
    {
        vfs.store().set_times_open(ino, file, atime, mtime)?;
    } else {
        vfs.store().set_times(ino, atime, mtime)?;
    }
    if path.is_empty() {
        attrib_fd(cx, dirfd);
    } else {
        attrib_at(cx, ino, dirfd, &path, follow);
    }
    Ok(0)
}

pub(super) fn truncate(cx: &mut Cx<'_>, path: u64, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let path = read_path(cx, path)?;
    let vfs = cx.vfs()?;
    let node = lookup_at(cx, AT_FDCWD, &path, true)?;
    let ino = store_ino(cx, &node)?;
    let st = vfs.store().attrs(ino)?;
    match st.kind() {
        mode::S_IFDIR => Err(Errno::EISDIR),
        mode::S_IFREG => {
            cx.cred().permission(&st, MAY_WRITE)?;
            if executing(cx, ino) {
                return Err(Errno::ETXTBSY);
            }
            check_fsize(cx, len)?;
            super::lease::break_leases(cx, ino, true, false, None)?;
            remove_privs(cx, ino)?;
            let host = vfs.store().open_object(ino, true)?;
            let tmpfs = tmpfs_size(&vfs, &node, &host)?;
            host.set_len(len).map_err(host_errno)?;
            tmpfs_resized(&vfs, tmpfs, len);
            truncated(cx, &host);
            if inotify::active(cx.kr()) {
                let parent = inotify::watched_parent(cx, AT_FDCWD, &path, true);
                let parent = parent.as_ref().map(|(d, n)| (*d, n.as_slice()));
                inotify::path_event(cx.kr(), ino, parent, inotify::IN_MODIFY);
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ftruncate(cx: &mut Cx<'_>, fd: i32, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let file = cx.fds().get(fd)?;
    if let FileKind::Regular { ino, .. } = file.borrow().kind
        && file.borrow().writable()
    {
        super::lease::break_leases(cx, ino, true, false, Some(&file))?;
    }
    let f = file.borrow();
    match &f.kind {
        FileKind::Regular { ino, file } if f.writable() => {
            check_fsize(cx, len)?;
            check_seals(cx, &f, false, |_| len)?;
            remove_privs(cx, *ino)?;
            let vfs = cx.vfs()?;
            let tmpfs = tmpfs_size(&vfs, &Node::Store(f.mnt, *ino), file)?;
            file.set_len(len).map_err(host_errno)?;
            tmpfs_resized(&vfs, tmpfs, len);
            truncated(cx, file);
            inotify::file_event(cx.kr(), &f, inotify::IN_MODIFY);
            Ok(0)
        }
        FileKind::Path(_) => Err(Errno::EBADF),
        _ => Err(Errno::EINVAL),
    }
}

/// 文件变短之后调用：所有地址空间里映射它的页，超出新末尾的从页表里拿掉
/// （[`crate::mm::Mm::file_truncated`]）。当前进程的地址空间在 syscall 结束时同步，
/// 别的进程的在这里同步。
/// 有没有哪个地址空间以可写的共享映射映射着这个打开的文件。
fn writably_mapped(cx: &Cx<'_>, f: &OpenFile) -> bool {
    use std::os::unix::fs::MetadataExt;
    let FileKind::Regular { file, .. } = &f.kind else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    cx.kr().tasks.values().any(|t| {
        t.res
            .borrow()
            .mm
            .borrow()
            .writably_maps(meta.dev(), meta.ino())
    })
}

fn truncated(cx: &Cx<'_>, host: &std::fs::File) {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = host.metadata() else {
        return;
    };
    let sync = cx.task.sync.clone();
    let mut seen = Vec::new();
    for t in cx.kr().tasks.values() {
        let mm = t.res.borrow().mm.clone();
        if seen.contains(&Rc::as_ptr(&mm)) {
            continue;
        }
        seen.push(Rc::as_ptr(&mm));
        let Ok(mut m) = mm.try_borrow_mut() else {
            crate::debug::log(format_args!("truncate: an address space is busy"));
            continue;
        };
        if m.file_truncated(meta.dev(), meta.ino(), meta.len()) && !Rc::ptr_eq(&mm, &cx.t.mm) {
            m.sync(&sync);
        }
    }
}

pub(super) fn fallocate(cx: &mut Cx<'_>, fd: i32, mode: u32, offset: i64, len: i64) -> SysResult {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x1;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x2;
    const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
    let (Ok(offset), Ok(len)) = (u64::try_from(offset), u64::try_from(len)) else {
        return Err(Errno::EINVAL);
    };
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    // 超过最大的文件长度（i64::MAX）
    let end = offset
        .checked_add(len)
        .filter(|&e| e <= i64::MAX as u64)
        .ok_or(Errno::EFBIG)?;
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    if !f.writable() {
        return Err(Errno::EBADF);
    }
    let (ino, host) = match &f.kind {
        FileKind::Regular { ino, file } => (*ino, file),
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        FileKind::Pipe(_) | FileKind::Socket(_) => return Err(Errno::ESPIPE),
        _ => return Err(Errno::ENODEV),
    };
    let size = host.metadata().map_err(host_errno)?.len();
    let keep = mode & FALLOC_FL_KEEP_SIZE != 0;
    // 仓库的 object 是宿主上的普通文件：预留空间就是把文件加长；打洞和清零写入 0
    let zero = match mode & !FALLOC_FL_KEEP_SIZE {
        0 => false,
        FALLOC_FL_PUNCH_HOLE if keep => true,
        FALLOC_FL_ZERO_RANGE => true,
        _ => return Err(Errno::EOPNOTSUPP),
    };
    if !keep {
        check_fsize(cx, end)?;
    }
    check_seals(cx, &f, zero, |size| if keep { size } else { size.max(end) })?;
    // tmpfs 里要分到这一段的页（Linux 的 shmem_fallocate），打洞不用；不过记账只按大小
    let vfs = cx.vfs()?;
    let tmpfs = tmpfs_size(&vfs, &Node::Store(f.mnt, ino), host)?;
    if let Some((dev, own)) = tmpfs
        && mode & FALLOC_FL_PUNCH_HOLE == 0
        && vfs
            .tmpfs_room(dev, own)?
            .is_some_and(|room| end.div_ceil(TMPFS_PAGE) > room.max(own))
    {
        return Err(Errno::ENOSPC);
    }
    remove_privs(cx, ino)?;
    if zero {
        let zeros = vec![0u8; IO_CHUNK];
        let mut pos = offset;
        while pos < end.min(size) {
            let n = (end.min(size) - pos).min(IO_CHUNK as u64) as usize;
            host.write_all_at(&zeros[..n], pos).map_err(host_errno)?;
            pos += n as u64;
        }
    }
    if !keep && end > size {
        host.set_len(end).map_err(host_errno)?;
        tmpfs_resized(&vfs, tmpfs, end);
    }
    inotify::file_event(cx.kr(), &f, inotify::IN_MODIFY);
    Ok(0)
}

pub(super) fn flock(cx: &mut Cx<'_>, fd: i32, op: u32) -> SysResult {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;
    let file = cx.fds().get(fd)?;
    // 不在仓库里的文件（管道、socket）没有可以共享的锁：总是成功
    let Some(ino) = file.borrow().ino() else {
        return Ok(0);
    };
    let me = Rc::downgrade(&file);
    let exclusive = match op & !LOCK_NB {
        LOCK_SH => false,
        LOCK_EX => true,
        LOCK_UN => {
            if let Some(locks) = cx.k().flocks.get_mut(&ino) {
                locks.retain(|(f, _)| !f.ptr_eq(&me));
            }
            request_wake_locks();
            return Ok(0);
        }
        _ => return Err(Errno::EINVAL),
    };
    loop {
        let locks = cx.k().flocks.entry(ino).or_default();
        locks.retain(|(f, _)| f.strong_count() > 0);
        // 同一个打开文件再次加锁是转换锁的类型
        let conflict = locks
            .iter()
            .any(|(f, ex)| !f.ptr_eq(&me) && (exclusive || *ex));
        if !conflict {
            // 转换锁的类型时先放开原来的（与 Linux 一样）：排他锁降成共享锁，等共享锁的
            // 一方就可以拿到了
            let before = locks.len();
            locks.retain(|(f, _)| !f.ptr_eq(&me));
            if locks.len() != before {
                request_wake_locks();
            }
            locks.push((me, exclusive));
            file.borrow_mut().holds_locks = true;
            return Ok(0);
        }
        if op & LOCK_NB != 0 {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // 持有锁的文件解锁、转换或关闭（OpenFile 析构）时会请求唤醒
        cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
    }
}

/// sync_file_range、readahead、syncfs：只检查参数（数据直接写在宿主文件里，读取
/// 也由宿主缓存）。`kind` 区分三者：0 是 sync_file_range，1 是 readahead，2 是 syncfs。
pub(super) fn cache_hint(
    cx: &mut Cx<'_>,
    fd: i32,
    offset: i64,
    len: i64,
    flags: u32,
    kind: u32,
) -> SysResult {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match kind {
        0 => {
            if flags & !7 != 0 || offset < 0 || len < 0 || offset.checked_add(len).is_none() {
                return Err(Errno::EINVAL);
            }
            match &f.kind {
                // 交给宿主的 object：dpkg 这类先让一批文件开始写回、再逐个 fsync，合成一次
                // 日志提交。打开文件由 file 保活，放锁期间不碰它
                FileKind::Regular { file: host, .. } if flags != 0 => {
                    let fd = std::os::fd::AsRawFd::as_raw_fd(host);
                    drop(f);
                    cx.unlocked(|| appine_hal::sync_file_range(fd, offset, len, flags))
                        .map(|()| 0)
                        .map_err(Errno)
                }
                FileKind::Regular { .. } | FileKind::Directory { .. } => Ok(0),
                FileKind::Path(_) => Err(Errno::EBADF),
                _ => Err(Errno::ESPIPE),
            }
        }
        1 => match &f.kind {
            _ if !f.readable() => Err(Errno::EBADF),
            FileKind::Regular { .. } if offset >= 0 => Ok(0),
            _ => Err(Errno::EINVAL),
        },
        _ => Ok(0),
    }
}

/// cachestat（Linux 6.5）：文件在页缓存里的状况。仓库里的文件看宿主文件的页缓存
/// （[`appine_hal::cachestat`]，只有在缓存里的页数），其余的文件没有页缓存，全是 0。
pub(super) fn cachestat(cx: &mut Cx<'_>, fd: i32, range: u64, out: u64, flags: u32) -> SysResult {
    let file = cx.fds().get(fd)?;
    if matches!(file.borrow().kind, FileKind::Path(_)) {
        return Err(Errno::EBADF);
    }
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let r: [u8; 16] = get(cx, range)?;
    let off = u64::from_le_bytes(r[..8].try_into().unwrap());
    let len = u64::from_le_bytes(r[8..].try_into().unwrap());
    let stats = match &file.borrow().kind {
        FileKind::Regular { file, .. } => {
            appine_hal::cachestat(std::os::fd::AsRawFd::as_raw_fd(file), off, len).map_err(Errno)?
        }
        _ => [0; 5],
    };
    let mut b = [0u8; 40];
    for (i, n) in stats.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&n.to_le_bytes());
    }
    put(cx, out, b).map(|()| 0)
}

/// fsync(2)、fdatasync(2)（`data_only`）：普通文件的数据在宿主的 object 里，让宿主把它刷到
/// 盘上；元数据（名字、权限……）在仓库的数据库里，提交推迟的写事务，再等 WAL 落盘
/// （[`durable`]）。刷盘时放开 BKL。
pub(super) fn fsync(cx: &mut Cx<'_>, fd: i32, data_only: bool) -> SysResult {
    let file = cx.fds().get(fd)?;
    let host = {
        let f = file.borrow();
        match &f.kind {
            _ if f.flags & oflags::O_PATH != 0 => return Err(Errno::EBADF),
            // 只有普通文件和目录能同步（宿主 fd 交给宿主）
            FileKind::Regular { file, .. } => Some(std::os::fd::AsRawFd::as_raw_fd(file)),
            FileKind::Directory { .. } => None,
            FileKind::Host(_) => return Ok(0),
            _ => return Err(Errno::EINVAL),
        }
    };
    // 打开文件由 file 保活，放锁期间不碰它
    if let Some(fd) = host {
        cx.unlocked(|| appine_hal::fsync(fd, data_only))
            .map_err(Errno)?;
    }
    durable(cx, false)
}

/// sync(2)：提交推迟的写事务，把仓库所在的整个宿主文件系统写到盘上。
pub(super) fn sync(cx: &mut Cx<'_>) -> SysResult {
    durable(cx, true)
}

/// 提交仓库推迟提交的写事务，等它落盘（[`Store::commit`] 只写进 WAL）：`all` 时连同所有
/// object 的数据对整个宿主文件系统 syncfs，否则只等 WAL。放开 BKL 等。
fn durable(cx: &mut Cx<'_>, all: bool) -> SysResult {
    let Ok(vfs) = cx.vfs() else {
        return Ok(0);
    };
    let store = vfs.store();
    store.commit()?;
    let meta = store.meta().clone();
    drop(vfs);
    cx.unlocked(move || {
        if all {
            meta.sync_all()
        } else {
            meta.sync_metadata()
        }
    })?;
    Ok(0)
}

// ---- 目录 ----

pub(super) fn getdents64(cx: &mut Cx<'_>, fd: i32, buf: u64, count: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let FileKind::Directory { node, entries, .. } = kind else {
        return Err(Errno::ENOTDIR);
    };
    if entries.is_none() {
        let vfs = cx.vfs()?;
        let view = cx.view(&vfs);
        let ino_of = |n: &Node| match n {
            Node::Store(_, i) => *i,
            Node::Pseudo(_, p) => pseudo::node_ino(view.k, p),
        };
        let parent = vfs.parent(&view, node)?;
        let mut list = vec![
            DirEntry {
                name: b".".to_vec(),
                ino: ino_of(node),
                mode: mode::S_IFDIR,
            },
            DirEntry {
                name: b"..".to_vec(),
                ino: ino_of(&parent),
                mode: mode::S_IFDIR,
            },
        ];
        match node {
            Node::Store(_, ino) => list.extend(vfs.store().readdir(*ino)?),
            Node::Pseudo(id, p) => list.extend(pseudo::readdir(&view, *id, p)?),
        }
        *entries = Some(list);
    }
    let list = entries.as_ref().unwrap();
    let mut out = Vec::new();
    let mut next = *pos as usize;
    while let Some(e) = list.get(next) {
        // struct linux_dirent64：d_ino、d_off、d_reclen、d_type、d_name，按 8 字节对齐
        let reclen = (19 + e.name.len() + 1).next_multiple_of(8);
        if out.len() + reclen > count {
            break;
        }
        let start = out.len();
        out.extend_from_slice(&e.ino.to_le_bytes());
        out.extend_from_slice(&(next as u64 + 1).to_le_bytes());
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push((e.mode >> 12) as u8 & 0xf); // DT_* 就是 S_IFMT >> 12
        out.extend_from_slice(&e.name);
        out.resize(start + reclen, 0);
        next += 1;
    }
    if out.is_empty() && next < list.len() {
        return Err(Errno::EINVAL);
    }
    cx.mm().write(UserSlice::new(buf, out.len()), &out)?;
    *pos = next as u64;
    inotify::file_event(cx.kr(), &f, inotify::IN_ACCESS);
    Ok(out.len() as u64)
}

pub(super) fn lseek(cx: &mut Cx<'_>, fd: i32, offset: i64, whence: u32) -> SysResult {
    let file = cx.fds().get(fd)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let base = match kind {
        FileKind::Host(fd) => return appine_hal::lseek(*fd, offset, whence as i32).map_err(Errno),
        FileKind::Path(_) => return Err(Errno::EBADF),
        FileKind::Pipe(_)
        | FileKind::Pty(_)
        | FileKind::Socket(_)
        | FileKind::Epoll(_)
        | FileKind::EventFd(_)
        | FileKind::TimerFd(_)
        | FileKind::SignalFd(_)
        | FileKind::Inotify(_)
        | FileKind::Pidfd(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => return Err(Errno::ESPIPE),
        FileKind::Proc { data, .. } => match whence {
            SEEK_SET => 0,
            SEEK_CUR => *pos as i64,
            SEEK_END => data.len() as i64,
            _ => return Err(Errno::EINVAL),
        },
        FileKind::MessageQueue(q) => match whence {
            SEEK_SET => 0,
            SEEK_CUR => *pos as i64,
            SEEK_END => q.borrow().status().len() as i64,
            _ => return Err(Errno::EINVAL),
        },
        // /dev/kmsg 按记录定位（`devkmsg_llseek`）：只能是偏移 0，SEEK_SET 到最老的一条，
        // SEEK_DATA 到 syslog 上次清空之后的第一条（`dmesg -c` 之后），SEEK_END 到下一条；
        // 文件位置不变，返回 0
        FileKind::Device {
            kmsg: Some(user), ..
        } => {
            if offset != 0 {
                return Err(Errno::ESPIPE);
            }
            let log = user.log.borrow();
            user.seq = match whence {
                SEEK_SET => log.first_seq(),
                SEEK_DATA => log.clear_seq(),
                SEEK_END => log.next_seq(),
                _ => return Err(Errno::EINVAL),
            };
            return Ok(0);
        }
        // 与 Linux 的 /dev/null 等一样，seek 总是成功并停在 0
        FileKind::Device { .. } => return Ok(0),
        FileKind::Directory { entries, .. } => {
            let new = match whence {
                SEEK_SET => offset,
                SEEK_CUR => (*pos as i64).checked_add(offset).ok_or(Errno::EINVAL)?,
                _ => return Err(Errno::EINVAL),
            };
            if new < 0 {
                return Err(Errno::EINVAL);
            }
            if new == 0 {
                *entries = None;
            }
            *pos = new as u64;
            return Ok(new as u64);
        }
        FileKind::Regular { file, .. } => {
            let size = file.metadata().map_err(host_errno)?.len() as i64;
            match whence {
                SEEK_SET => 0,
                SEEK_CUR => *pos as i64,
                SEEK_END => size,
                SEEK_DATA if offset >= size => return Err(Errno::ENXIO),
                SEEK_DATA => 0,
                SEEK_HOLE if offset >= size => return Err(Errno::ENXIO),
                SEEK_HOLE => {
                    *pos = size as u64;
                    return Ok(size as u64);
                }
                _ => return Err(Errno::EINVAL),
            }
        }
    };
    let new = base
        .checked_add(offset)
        .filter(|&v| v >= 0)
        .ok_or(Errno::EINVAL)?;
    *pos = new as u64;
    Ok(new as u64)
}

// ---- 读写 ----

/// 一次读写要走的路径，在不持有打开文件借用的情况下进行。
enum IoPath {
    Host(i32),
    Pipe(Rc<RefCell<Pipe>>),
    /// 终端，打开文件是不是主设备、打开时挂断过几次（[`crate::tty::PtyEnd::born`]）。
    Pty(Rc<RefCell<Tty>>, (bool, u32)),
    Socket(SocketRef),
    EventFd(Rc<RefCell<EventFd>>),
    TimerFd(Rc<RefCell<TimerFd>>),
    SignalFd(Rc<RefCell<crate::fd::SignalFd>>),
    Inotify(Rc<RefCell<super::Inotify>>),
    /// `/proc/<pid>/pagemap`：按读的位置现算。
    Pagemap(Tid),
    /// `/proc/<pid>/mem`：位置就是地址。
    /// `/proc/<pid>/mem` 打开时的地址空间，已经释放时是 None。
    Mem(Option<Rc<RefCell<crate::mm::Mm>>>),
    /// `/dev/kmsg`：按记录读写，读的位置在打开文件里（[`crate::klog::KmsgFile`]）。
    Kmsg,
    /// `/proc/kmsg`：syslog 的读。
    ProcKmsg(Rc<RefCell<crate::klog::Klog>>),
    Local,
}

fn io_path(f: &OpenFile) -> IoPath {
    match &f.kind {
        FileKind::Host(fd) => IoPath::Host(*fd),
        FileKind::Pipe(end) => IoPath::Pipe(end.pipe.clone()),
        FileKind::Pty(end) => IoPath::Pty(end.tty.clone(), (end.master, end.born)),
        FileKind::Socket(s) => IoPath::Socket(s.clone()),
        FileKind::EventFd(e) => IoPath::EventFd(e.clone()),
        FileKind::TimerFd(t) => IoPath::TimerFd(t.clone()),
        FileKind::SignalFd(s) => IoPath::SignalFd(s.clone()),
        FileKind::Inotify(i) => IoPath::Inotify(i.clone()),
        FileKind::Proc {
            node: PseudoNode::Entry(pid, _, pseudo::Entry::Pagemap),
            ..
        } => IoPath::Pagemap(*pid),
        FileKind::Proc {
            node: PseudoNode::Entry(_, _, pseudo::Entry::Mem),
            mm,
            ..
        } => IoPath::Mem(mm.as_ref().and_then(std::rc::Weak::upgrade)),
        FileKind::Device { kmsg: Some(_), .. } => IoPath::Kmsg,
        FileKind::Proc {
            klog: Some(log), ..
        } => IoPath::ProcKmsg(log.clone()),
        _ => IoPath::Local,
    }
}

/// 能不能按位置读写（AIO 对不能定位的文件不管偏移）。
pub(super) fn seekable(f: &OpenFile) -> bool {
    matches!(
        io_path(f),
        IoPath::Local | IoPath::Pagemap(_) | IoPath::Mem(_)
    )
}

/// 读写走哪条路径。带偏移的读写（pread、pwritev）遇到不能定位的文件是 ESPIPE：与
/// Linux 一样先于打开方式检查（只读打开的管道 pwrite 也是 ESPIPE），`O_PATH` 除外。
fn positional(f: &OpenFile, offset: Option<u64>) -> Result<IoPath, Errno> {
    let path = io_path(f);
    // /dev/kmsg、/proc/kmsg 的 pread、pwrite 不看偏移，与 read、write 一样
    let seekable = matches!(
        path,
        IoPath::Local | IoPath::Pagemap(_) | IoPath::Mem(_) | IoPath::Kmsg | IoPath::ProcKmsg(_)
    );
    if offset.is_some() && !seekable && f.flags & oflags::O_PATH == 0 {
        return Err(Errno::ESPIPE);
    }
    Ok(path)
}

/// 等宿主 fd（`appine-run` 交给 guest 的宿主 stdio）可读或可写；非阻塞时不等。只等宿主
/// fd 本身，不登记任何对象：进程内没有修改会让它就绪，原来系统里每一次管道、PTY 的读写
/// 都把阻塞在宿主 stdin 上的线程叫醒一遍。
fn wait_host(cx: &mut Cx<'_>, fd: i32, events: i16, nonblock: bool) -> Result<(), Errno> {
    let mut pfd = [(fd, events, 0)];
    loop {
        appine_hal::poll(&mut pfd, Some(Duration::ZERO)).map_err(Errno)?;
        if pfd[0].2 != 0 {
            return Ok(());
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut pfd, crate::kernel::wake::FILE, &[])?;
        if pfd[0].2 != 0 {
            return Ok(());
        }
    }
}

fn pipe_read(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        {
            let mut p = pipe.borrow_mut();
            if !p.buf.is_empty() || buf.is_empty() {
                let n = buf.len().min(p.buf.len());
                let before = p.space();
                for (d, s) in buf.iter_mut().zip(p.buf.drain(..n)) {
                    *d = s;
                }
                if p.drained(before) {
                    request_wake_object(crate::pipe::object(pipe));
                }
                return Ok(n);
            }
            if p.writers == 0 {
                return Ok(0);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// 等管道里有数据：返回 false 表示写端已经全部关闭（EOF）。
fn pipe_wait_data(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
) -> Result<bool, Errno> {
    loop {
        {
            let p = pipe.borrow();
            if !p.buf.is_empty() {
                return Ok(true);
            }
            if p.writers == 0 {
                return Ok(false);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// 打开文件是管道时返回管道，以及它是不是读端。
fn pipe_of(file: &FileRef) -> Option<(Rc<RefCell<Pipe>>, bool)> {
    match &file.borrow().kind {
        FileKind::Pipe(end) => Some((end.pipe.clone(), end.read)),
        _ => None,
    }
}

const SPLICE_F_NONBLOCK: u32 = 2;

/// splice：在管道和另一个文件（或两个管道）之间搬数据。经由宿主缓冲区复制，不是零拷贝；
/// 从管道取数据时先复制、写成功之后才从管道里去掉，写失败时数据不会丢。
pub(super) fn splice(
    cx: &mut Cx<'_>,
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
    flags: u32,
) -> SysResult {
    let input = cx.fds().get(fd_in)?;
    let output = cx.fds().get(fd_out)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    let (in_pipe, out_pipe) = (pipe_of(&input), pipe_of(&output));
    // 目录、eventfd、epoll 这类对象不支持 splice（Linux 的 splice_read、splice_write）
    let splices = |f: &FileRef| {
        matches!(
            f.borrow().kind,
            FileKind::Regular { .. }
                | FileKind::Pipe(_)
                | FileKind::Socket(_)
                | FileKind::Device { kmsg: None, .. }
                | FileKind::Pty(_)
                | FileKind::Proc { .. }
                | FileKind::Host(_)
        )
    };
    if in_pipe.is_none() && out_pipe.is_none()
        || output.borrow().flags & oflags::O_APPEND != 0
        || !splices(&input)
        || !splices(&output)
    {
        return Err(Errno::EINVAL);
    }
    if in_pipe.is_some() && off_in != 0 || out_pipe.is_some() && off_out != 0 {
        return Err(Errno::ESPIPE);
    }
    let read_off = |cx: &Cx<'_>, addr: u64| -> Result<Option<u64>, Errno> {
        match addr {
            0 => Ok(None),
            a => Ok(Some(
                u64::try_from(i64::from_le_bytes(get(cx, a)?)).map_err(|_| Errno::EINVAL)?,
            )),
        }
    };
    let (src, dst) = (read_off(cx, off_in)?, read_off(cx, off_out)?);
    if len == 0 {
        return Ok(0);
    }
    let nonblock = flags & SPLICE_F_NONBLOCK != 0;
    let wrote = if let Some((pipe, _)) = in_pipe {
        if !pipe_wait_data(cx, &pipe, nonblock || input.borrow().nonblocking())? {
            return Ok(0);
        }
        let data: Vec<u8> = pipe.borrow().buf.iter().take(len).copied().collect();
        let n = write_bytes(cx, &output, &data, dst)?;
        let mut p = pipe.borrow_mut();
        let before = p.space();
        p.buf.drain(..n);
        if p.drained(before) {
            request_wake_object(crate::pipe::object(&pipe));
        }
        n
    } else {
        let mut buf = vec![0u8; len.min(crate::pipe::PIPE_CAPACITY)];
        // 与 Linux 相同，没有连接的 socket 不能 splice 出数据：EINVAL
        let got = match read_bytes(cx, &input, &mut buf, src) {
            Err(e) if e == Errno::ENOTCONN => return Err(Errno::EINVAL),
            r => r?,
        };
        let n = write_bytes(cx, &output, &buf[..got], None)?;
        // 没写完的部分退回输入文件
        if src.is_none() && n < got {
            input.borrow_mut().pos -= (got - n) as u64;
        }
        n
    };
    if let Some(s) = src {
        put(cx, off_in, (s + wrote as u64).to_le_bytes())?;
    }
    if let Some(d) = dst {
        put(cx, off_out, (d + wrote as u64).to_le_bytes())?;
    }
    Ok(wrote as u64)
}

/// tee：把一个管道里的数据复制到另一个管道，不从源管道里取走。
pub(super) fn tee(cx: &mut Cx<'_>, fd_in: i32, fd_out: i32, len: usize, flags: u32) -> SysResult {
    let input = cx.fds().get(fd_in)?;
    let output = cx.fds().get(fd_out)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    let (Some((src, _)), Some((dst, _))) = (pipe_of(&input), pipe_of(&output)) else {
        return Err(Errno::EINVAL);
    };
    if Rc::ptr_eq(&src, &dst) {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if !pipe_wait_data(cx, &src, flags & SPLICE_F_NONBLOCK != 0)? {
        return Ok(0);
    }
    let data: Vec<u8> = src.borrow().buf.iter().take(len).copied().collect();
    write_bytes(cx, &output, &data, None).map(|n| n as u64)
}

/// vmsplice：管道的读端相当于 readv；写端等到有空间，然后写进放得下的部分就返回
/// （与 Linux 相同，不等全部写完）。
pub(super) fn vmsplice(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize, flags: u32) -> SysResult {
    let file = cx.fds().get(fd)?;
    let Some((pipe, read)) = pipe_of(&file) else {
        return Err(Errno::EBADF);
    };
    if read {
        return vectored(cx, fd, iov, count, None, false);
    }
    let iovecs = iovecs(cx, iov, count)?;
    let nonblock = flags & SPLICE_F_NONBLOCK != 0 || file.borrow().nonblocking();
    let space = loop {
        {
            let p = pipe.borrow();
            if p.readers == 0 {
                return Err(broken_pipe(cx));
            }
            if p.space() > 0 {
                break p.space();
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        pipe.borrow_mut().want_space(1);
        cx.wait_object(crate::pipe::object(&pipe))?;
    };
    let mut data = Vec::new();
    for (base, len) in iovecs {
        let n = len.min(space - data.len());
        let start = data.len();
        data.resize(start + n, 0);
        cx.mm().read(UserSlice::new(base, n), &mut data[start..])?;
        if data.len() == space {
            break;
        }
    }
    {
        let mut p = pipe.borrow_mut();
        p.buf.extend(&data);
        p.writes += 1;
    }
    request_wake_object(crate::pipe::object(&pipe));
    Ok(data.len() as u64)
}

/// 写端没有读者了：EPIPE，并给当前线程发 SIGPIPE。
fn broken_pipe(cx: &mut Cx<'_>) -> Errno {
    let task = cx.task.clone();
    signal::send_to_task(cx.kr(), &task, Signal::PIPE, SigInfo::kernel());
    Errno::EPIPE
}

fn pipe_write(
    cx: &mut Cx<'_>,
    pipe: &Rc<RefCell<Pipe>>,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    loop {
        let progress = {
            let mut p = pipe.borrow_mut();
            if p.readers == 0 {
                drop(p);
                let e = broken_pipe(cx);
                return if done > 0 { Ok(done) } else { Err(e) };
            }
            let want = data.len() - done;
            // 不超过 PIPE_BUF 的写入必须一次写完，不能和别人的写入交错
            let space = p.space();
            if (data.len() <= PIPE_BUF && space >= want) || (data.len() > PIPE_BUF && space > 0) {
                let n = want.min(space);
                // 读端只在管道空着时等，poll 也只在空着时报告不可读；不空时只有边沿触发
                // 的 epoll 要知道又来了数据
                if p.buf.is_empty() {
                    request_wake_object(crate::pipe::object(pipe));
                } else {
                    super::poll::arrived(cx);
                }
                p.buf.extend(&data[done..done + n]);
                p.writes += 1;
                done += n;
                true
            } else {
                // 要等读端腾出空间：小写入要一次写完的空间；大写入等到至少能写一页，
                // 免得读端每读一点就叫醒一次
                p.want_space(if data.len() <= PIPE_BUF {
                    want
                } else {
                    want.min(PIPE_BUF)
                });
                false
            }
        };
        if done == data.len() {
            return Ok(done);
        }
        if progress {
            continue;
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_object(crate::pipe::object(pipe))?;
    }
}

/// `/dev/kmsg` 的读（`devkmsg_read`）：一次读一条记录（[`crate::klog::Klog::ext_text`]），
/// 没有新的记录时等（非阻塞时 EAGAIN）。要读的那条已经被挤掉时是 EPIPE，位置跳到最老的
/// 一条；缓冲区放不下这条是 EINVAL，这条也算读过了，与 Linux 相同。
fn kmsg_read(
    cx: &mut Cx<'_>,
    file: &FileRef,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        let object = {
            let mut f = file.borrow_mut();
            let FileKind::Device {
                kmsg: Some(user), ..
            } = &mut f.kind
            else {
                return Err(Errno::EBADF);
            };
            let log = user.log.clone();
            let l = log.borrow();
            if let Some(r) = l.ext_text(user.seq) {
                if r.seq != user.seq {
                    user.seq = r.seq;
                    return Err(Errno::EPIPE);
                }
                user.seq = r.seq + 1;
                if r.text.len() > buf.len() {
                    return Err(Errno::EINVAL);
                }
                buf[..r.text.len()].copy_from_slice(&r.text);
                return Ok(r.text.len());
            }
            wake::of(&*log)
        };
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(object)?;
    }
}

/// `/dev/kmsg` 的写（`devkmsg_write`）：一次写是一条记录（[`crate::klog::KmsgFile::write`]），
/// 超过 `LOG_LINE_MAX` 是 EINVAL；没有前缀时的级别是 `/proc/sys/kernel/printk` 的默认
/// 消息级别。
fn kmsg_write(cx: &mut Cx<'_>, file: &FileRef, data: &[u8]) -> Result<usize, Errno> {
    if data.len() > crate::klog::LOG_LINE_MAX {
        return Err(Errno::EINVAL);
    }
    let level = cx.kr().printk[1];
    if let FileKind::Device {
        kmsg: Some(user), ..
    } = &mut file.borrow_mut().kind
    {
        user.write(data, level);
    }
    Ok(data.len())
}

/// `/proc/kmsg` 的读（`kmsg_read`）：syslog 的读，与 syslog(2) 共用读的位置，读走就没了；
/// 非阻塞时没有未读的记录是 EAGAIN。
fn proc_kmsg_read(
    cx: &mut Cx<'_>,
    log: &Rc<RefCell<crate::klog::Klog>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if nonblock && log.borrow_mut().size_unread(true) == 0 {
        return Err(Errno::EAGAIN);
    }
    if buf.is_empty() {
        return Ok(0);
    }
    while !log.borrow().syslog_readable() {
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_object(wake::of(&**log))?;
    }
    let text = log.borrow_mut().syslog_read(buf.len());
    buf[..text.len()].copy_from_slice(&text);
    Ok(text.len())
}

/// 仓库里的文件和设备：不会阻塞，直接在持有借用时完成。
/// 普通文件的宿主 fd：读写它时可以放开 BKL（[`unlocked_io`]）。
fn regular_fd(f: &OpenFile) -> Option<i32> {
    match &f.kind {
        FileKind::Regular { file, .. } => Some(std::os::fd::AsRawFd::as_raw_fd(file)),
        _ => None,
    }
}

/// 不持 BKL 读写普通文件：冷数据的宿主 pread/pwrite 要等闪存（百微秒到毫秒），持锁时
/// 所有进程的系统调用都得跟着等。`offset` 为 None 时用文件位置，同一个打开文件同时只有
/// 一次这样的读写（Linux 的 f_pos_lock），别的等它做完。`io` 在放开 BKL 时以宿主 fd 和
/// 位置调用，不能访问任何内核对象；打开文件由调用方手里的 `file` 保活，放锁期间不碰它的
/// 引用计数和借用。
fn unlocked_io(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    offset: Option<u64>,
    io: impl FnOnce(i32, u64) -> Result<usize, i32> + Send,
) -> Result<usize, Errno> {
    let pos = match offset {
        Some(off) => off,
        None => {
            while file.borrow().pos_busy != 0 {
                if cx.task.killed.get() {
                    return Err(Errno::EINTR);
                }
                file.borrow_mut().pos_busy = 2;
                cx.wait_on(None, &mut [], crate::kernel::wake::LOCK)?;
            }
            let mut f = file.borrow_mut();
            f.pos_busy = 1;
            f.pos
        }
    };
    let r = cx.unlocked(|| io(fd, pos));
    if offset.is_none() {
        let mut f = file.borrow_mut();
        if let Ok(n) = r {
            f.pos = pos + n as u64;
        }
        if std::mem::take(&mut f.pos_busy) == 2 {
            request_wake_locks();
        }
    }
    r.map_err(Errno)
}

/// 与 Linux 的 seq_file 一样（`seq_read_iter` 在偏移 0 时从头再来），从偏移 0 读 procfs、
/// sysfs、cgroup2 的文件时重新生成内容：一直开着、seek 回 0 再读的程序（procps，runc 写了
/// `cgroup.freeze` 再读回来确认）读到的是现在的。打开时刚生成、还没有读写过的不再生成。
fn refresh_pseudo(cx: &mut Cx<'_>, file: &FileRef, offset: Option<u64>) -> Result<(), Errno> {
    let (mnt, node) = {
        let mut f = file.borrow_mut();
        let pos = offset.unwrap_or(f.pos);
        match &mut f.kind {
            FileKind::Proc {
                mnt,
                node,
                fresh,
                ns: None,
                klog: None,
                ..
            } if pos == 0 && !pseudo::unreadable(node) => {
                if std::mem::take(fresh) {
                    return Ok(());
                }
                (*mnt, node.clone())
            }
            _ => return Ok(()),
        }
    };
    let vfs = cx.vfs()?;
    let content = pseudo::content(&cx.view(&vfs), mnt, &node)?;
    if let FileKind::Proc { data, .. } = &mut file.borrow_mut().kind {
        *data = content;
    }
    Ok(())
}

fn local_read(f: &mut OpenFile, buf: &mut [u8], offset: Option<u64>) -> Result<usize, Errno> {
    let pos = offset.unwrap_or(f.pos);
    let n = match &f.kind {
        FileKind::Regular { file, .. } => file.read_at(buf, pos).map_err(host_errno)?,
        FileKind::Device { dev, .. } => match dev {
            Device::Null => 0,
            Device::Zero | Device::Full => {
                buf.fill(0);
                buf.len()
            }
            Device::Random => {
                appine_hal::fill_random(buf).map_err(Errno)?;
                buf.len()
            }
            Device::Kmsg => return Err(Errno::EBADF),
        },
        // 命名空间（nsfs）读不了
        FileKind::Proc {
            node: PseudoNode::NsFile(..),
            ..
        } => return Err(Errno::EINVAL),
        FileKind::Proc { node, .. } if pseudo::unreadable(node) => return Err(Errno::EINVAL),
        FileKind::Proc { data, .. } => {
            let start = (pos as usize).min(data.len());
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        }
        // 消息队列读到的是状态行
        FileKind::MessageQueue(q) => {
            let data = q.borrow().status();
            let start = (pos as usize).min(data.len());
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        }
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        FileKind::Epoll(_) | FileKind::Pidfd(_) | FileKind::SeccompNotify(_) | FileKind::Bpf(_) => {
            return Err(Errno::EINVAL);
        }
        _ => return Err(Errno::EBADF),
    };
    let positioned = matches!(
        f.kind,
        FileKind::Regular { .. } | FileKind::Proc { .. } | FileKind::MessageQueue(_)
    );
    if offset.is_none() && positioned {
        f.pos = pos + n as u64;
    }
    Ok(n)
}

fn local_write(f: &mut OpenFile, data: &[u8], offset: Option<u64>) -> Result<usize, Errno> {
    match &f.kind {
        FileKind::Regular { file, .. } => {
            let pos = match offset {
                Some(off) => off,
                None if f.flags & oflags::O_APPEND != 0 => {
                    file.metadata().map_err(host_errno)?.len()
                }
                None => f.pos,
            };
            let n = file.write_at(data, pos).map_err(host_errno)?;
            if offset.is_none() {
                f.pos = pos + n as u64;
            }
            Ok(n)
        }
        FileKind::Device { dev, .. } => match dev {
            Device::Full => Err(Errno::ENOSPC),
            _ => Ok(data.len()),
        },
        // 可写的 procfs 文件：内容由 pseudo::write 处理过了
        FileKind::Proc { .. } => Ok(data.len()),
        // 没有 write 操作的匿名 inode
        FileKind::Epoll(_)
        | FileKind::Pidfd(_)
        | FileKind::MessageQueue(_)
        | FileKind::SeccompNotify(_)
        | FileKind::Bpf(_) => Err(Errno::EINVAL),
        _ => Err(Errno::EBADF),
    }
}

/// 从打开文件做一次读，读进宿主缓冲区。`offset` 为 None 时使用并推进文件位置。
fn read_bytes(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: &mut [u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let (path, nonblock) = {
        let f = file.borrow();
        let path = positional(&f, offset)?;
        if !f.readable() {
            return Err(Errno::EBADF);
        }
        (path, f.nonblocking())
    };
    match path {
        IoPath::Host(fd) => {
            wait_host(cx, fd, POLLIN, nonblock)?;
            cx.unlocked(|| appine_hal::read(fd, buf)).map_err(Errno)
        }
        IoPath::Pipe(pipe) => pipe_read(cx, &pipe, nonblock, buf),
        IoPath::Pty(t, side) => tty::read(cx, &t, side, nonblock, buf),
        IoPath::Socket(s) => {
            let m = super::net::recv(cx, &s, nonblock, buf.len(), 0, 0)?;
            buf[..m.data.len()].copy_from_slice(&m.data);
            Ok(m.data.len())
        }
        IoPath::EventFd(e) => super::poll::eventfd_read(cx, &e, nonblock, buf),
        IoPath::TimerFd(t) => super::timerfd::read(cx, &t, nonblock, buf),
        IoPath::SignalFd(s) => super::sig::signalfd_read(cx, &s, nonblock, buf),
        IoPath::Inotify(i) => super::inotify::read(cx, &i, nonblock, buf),
        IoPath::Pagemap(pid) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let n = pseudo::read_pagemap(cx.kr(), pid, buf, pos)?;
            if offset.is_none() {
                file.borrow_mut().pos = pos + n as u64;
            }
            Ok(n)
        }
        IoPath::Kmsg => kmsg_read(cx, file, nonblock, buf),
        IoPath::ProcKmsg(log) => proc_kmsg_read(cx, &log, nonblock, buf),
        IoPath::Mem(mm) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let n = pseudo::read_mem(mm, buf, pos)?;
            if offset.is_none() {
                file.borrow_mut().pos = pos.wrapping_add(n as u64);
            }
            Ok(n)
        }
        IoPath::Local => {
            let regular = regular_fd(&file.borrow());
            let n = match regular {
                Some(fd) => unlocked_io(cx, file, fd, offset, |fd, pos| {
                    appine_hal::pread(fd, buf, pos)
                })?,
                None => {
                    refresh_pseudo(cx, file, offset)?;
                    local_read(&mut file.borrow_mut(), buf, offset)?
                }
            };
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_ACCESS);
            }
            Ok(n)
        }
    }
}

/// 向打开文件做一次写。`offset` 为 None 时使用并推进文件位置，`O_APPEND` 时先移到文件末尾。
fn write_bytes(
    cx: &mut Cx<'_>,
    file: &FileRef,
    data: &[u8],
    offset: Option<u64>,
) -> Result<usize, Errno> {
    let (path, nonblock) = {
        let f = file.borrow();
        let path = positional(&f, offset)?;
        if !f.writable() {
            return Err(Errno::EBADF);
        }
        (path, f.nonblocking())
    };
    match path {
        IoPath::Host(fd) => match cx.unlocked(|| appine_hal::write(fd, data)) {
            Ok(n) => Ok(n),
            Err(e) if e == Errno::EPIPE.0 => Err(broken_pipe(cx)),
            Err(e) => Err(Errno(e)),
        },
        IoPath::Pipe(pipe) => pipe_write(cx, &pipe, nonblock, data),
        IoPath::Pty(t, side) => tty::write(cx, &t, side, nonblock, data),
        IoPath::Socket(s) => super::net::send(cx, &s, nonblock, data, &[], &[], 0),
        IoPath::EventFd(e) => super::poll::eventfd_write(cx, &e, nonblock, data),
        IoPath::TimerFd(_) | IoPath::SignalFd(_) | IoPath::Inotify(_) | IoPath::Pagemap(_) => {
            Err(Errno::EINVAL)
        }
        IoPath::Kmsg => kmsg_write(cx, file, data),
        // 只读打开（没有 proc_write）
        IoPath::ProcKmsg(_) => Err(Errno::EBADF),
        IoPath::Mem(mm) => {
            let pos = offset.unwrap_or(file.borrow().pos);
            let sync = cx.task.sync.clone();
            let n = pseudo::write_mem(mm, data, pos, &sync)?;
            if offset.is_none() {
                file.borrow_mut().pos = pos.wrapping_add(n as u64);
            }
            Ok(n)
        }
        IoPath::Local => {
            let data = &data[..fsize_allowed(cx, &file.borrow(), offset, data.len())?];
            let (n, tmpfs) = tmpfs_write_room(cx, &file.borrow(), offset, data.len())?;
            let data = &data[..n];
            check_seals(cx, &file.borrow(), true, |size| {
                let f = file.borrow();
                let pos = match offset {
                    Some(off) => off,
                    None if f.flags & oflags::O_APPEND != 0 => size,
                    None => f.pos,
                };
                size.max(pos + data.len() as u64)
            })?;
            let proc_node = match &file.borrow().kind {
                FileKind::Proc { node, .. } => Some(node.clone()),
                _ => None,
            };
            let mut written = data.len();
            if let Some(node) = proc_node {
                let cred = cx.cred();
                let (ns, pid_ns) = (cx.ns(), cx.pid_ns());
                let tid = cx.tid();
                pseudo::write(cx.k(), (tid, &cred), (&ns, &pid_ns), &node, data)?;
                written = pseudo::written(&node, data.len());
                if let FileKind::Proc { fresh, .. } = &mut file.borrow_mut().kind {
                    *fresh = false;
                }
                // 改了 cgroup 的内存上限：叫醒定时器线程马上量一次
                if let PseudoNode::Cgroup(_, b"memory.max") = node {
                    cx.k().oom.recheck();
                    crate::kernel::signal_later(&cx.shared.timers);
                }
            }
            let ino = match file.borrow().kind {
                FileKind::Regular { ino, .. } => Some(ino),
                _ => None,
            };
            if let Some(ino) = ino.filter(|_| !data.is_empty()) {
                remove_privs(cx, ino)?;
            }
            // 普通文件不持 BKL 写；O_APPEND 的仍持锁做（包括带偏移的写，[`append_offset`]），
            // 几个进程同时追加时不会互相覆盖
            let regular =
                regular_fd(&file.borrow()).filter(|_| file.borrow().flags & oflags::O_APPEND == 0);
            let n = match regular {
                Some(fd) => unlocked_io(cx, file, fd, offset, |fd, pos| {
                    appine_hal::pwrite(fd, data, pos)
                })?,
                None => local_write(&mut file.borrow_mut(), data, offset)?.min(written),
            };
            if let (Some(t), FileKind::Regular { file: host, .. }) = (tmpfs, &file.borrow().kind)
                && let Ok(meta) = host.metadata()
            {
                tmpfs_resized(&*cx.vfs()?, Some(t), meta.len());
            }
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_MODIFY);
            }
            Ok(n)
        }
    }
}

/// 写、截短普通文件 `ino` 之前去掉它的特权（[`Store::remove_privs`]）：setuid、setgid 位在
/// 调用者没有 `CAP_FSETID` 时去掉，文件能力总是去掉。
fn remove_privs(cx: &Cx<'_>, ino: Ino) -> Result<(), Errno> {
    let keep_setid = cx.t.group.borrow().cred.capable(cap::FSETID);
    cx.vfs()?.store().remove_privs(ino, keep_setid)
}

/// `RLIMIT_FSIZE`：普通文件写到超过限制的位置时只写到限制为止；一个字节都写不了时
/// 给当前线程发 SIGXFSZ 并返回 EFBIG。返回允许写的字节数。
fn fsize_allowed(
    cx: &Cx<'_>,
    f: &OpenFile,
    offset: Option<u64>,
    len: usize,
) -> Result<usize, Errno> {
    let FileKind::Regular { file, .. } = &f.kind else {
        return Ok(len);
    };
    let limit = cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0;
    if limit == u64::MAX || len == 0 {
        return Ok(len);
    }
    let pos = match offset {
        Some(off) => off,
        None if f.flags & oflags::O_APPEND != 0 => file.metadata().map_err(host_errno)?.len(),
        None => f.pos,
    };
    if pos >= limit {
        return Err(file_too_big(cx));
    }
    Ok(len.min((limit - pos) as usize))
}

/// 写 tmpfs 里的普通文件：写到实例满了为止（Linux 的 `shmem_write_begin` 一页页地要，
/// 要不到时已经写了的照样返回），一页也写不了时 ENOSPC；写在原来的大小以内的不受限制。
/// 返回允许写的字节数，和写完之后记账要的设备号、写之前的页数（[`tmpfs_resized`]）。
fn tmpfs_write_room(
    cx: &Cx<'_>,
    f: &OpenFile,
    offset: Option<u64>,
    len: usize,
) -> Result<(usize, Option<(u64, u64)>), Errno> {
    let FileKind::Regular { ino, file } = &f.kind else {
        return Ok((len, None));
    };
    let vfs = cx.vfs()?;
    let Some(t @ (dev, own)) = tmpfs_size(&vfs, &Node::Store(f.mnt, *ino), file)? else {
        return Ok((len, None));
    };
    let Some(room) = vfs.tmpfs_room(dev, own)?.filter(|_| len > 0) else {
        return Ok((len, Some(t)));
    };
    let pos = match offset {
        Some(off) => off,
        None if f.flags & oflags::O_APPEND != 0 => file.metadata().map_err(host_errno)?.len(),
        None => f.pos,
    };
    let end = room.max(own).saturating_mul(TMPFS_PAGE);
    if pos >= end {
        return Err(Errno::ENOSPC);
    }
    Ok((
        len.min((end - pos).try_into().unwrap_or(usize::MAX)),
        Some(t),
    ))
}

/// `node`（普通文件，宿主上是 `host`）在 tmpfs 里时，那个实例的设备号和文件现在的页数：
/// 改大小之后交给 [`tmpfs_resized`] 记账。
fn tmpfs_size(vfs: &Vfs, node: &Node, host: &std::fs::File) -> Result<Option<(u64, u64)>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(node) else {
        return Ok(None);
    };
    let size = host.metadata().map_err(host_errno)?.len();
    Ok(Some((dev, size.div_ceil(TMPFS_PAGE))))
}

/// tmpfs 里的普通文件改成了 `size` 字节：按页数的差记账（[`tmpfs_size`] 记下的是原来的）。
fn tmpfs_resized(vfs: &Vfs, tmpfs: Option<(u64, u64)>, size: u64) {
    if let Some((dev, own)) = tmpfs {
        let pages = size.div_ceil(TMPFS_PAGE) as i64 - own as i64;
        vfs.tmpfs_charge(dev, pages, 0);
    }
}

/// 在目录 `dir`（经由的挂载和 inode）里新建一个名字（新的 inode，或者硬链接）之前：在 tmpfs
/// 里时看放不放得下，放不下是 ENOSPC（Linux 的 `shmem_reserve_inode`；`pages` 是长的符号
/// 链接要的一页）。返回实例的设备号，建好之后交给 [`tmpfs_created`]。
pub(super) fn tmpfs_reserve(
    vfs: &Vfs,
    (mnt, dir): (MountId, Ino),
    pages: u64,
) -> Result<Option<u64>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(&Node::Store(mnt, dir)) else {
        return Ok(None);
    };
    vfs.tmpfs_check(dev, pages, 1)?;
    Ok(Some(dev))
}

/// 建好了 [`tmpfs_reserve`] 放得下的名字：记账。
pub(super) fn tmpfs_created(vfs: &Vfs, dev: Option<u64>, pages: u64) {
    if let Some(dev) = dev {
        vfs.tmpfs_charge(dev, pages as i64, 1);
    }
}

/// 删掉目录 `dir` 里的 `name`（unlink、rmdir、被 rename 覆盖）之前：在 tmpfs 里时记下它
/// 占的，名字一个 inode，删的是最后一个链接时还有它的页（Linux 等最后一次关掉才释放，这里
/// 删掉时就释放）。删掉之后交给 [`tmpfs_removed`]。
fn tmpfs_victim(
    vfs: &Vfs,
    (mnt, dir): (MountId, Ino),
    name: &[u8],
) -> Result<Option<(u64, u64)>, Errno> {
    let Some(dev) = vfs.tmpfs_dev(&Node::Store(mnt, dir)) else {
        return Ok(None);
    };
    let Some(ino) = vfs.store().lookup(dir, name)? else {
        return Ok(None);
    };
    let st = vfs.store().stat(ino)?;
    let pages = if st.is_dir() || st.nlink <= 1 {
        crate::vfs::tmpfs_pages(&st)
    } else {
        0
    };
    Ok(Some((dev, pages)))
}

/// 删掉了 [`tmpfs_victim`] 记下的名字：记账。
fn tmpfs_removed(vfs: &Vfs, victim: Option<(u64, u64)>) {
    if let Some((dev, pages)) = victim {
        vfs.tmpfs_charge(dev, -(pages as i64), -1);
    }
}

/// 超过 `RLIMIT_FSIZE`：发 SIGXFSZ，返回 EFBIG。
fn file_too_big(cx: &Cx<'_>) -> Errno {
    signal::send_to_task(cx.kr(), cx.task, Signal::XFSZ, SigInfo::kernel());
    Errno::EFBIG
}

/// 截断、预留空间之后的长度不能超过 `RLIMIT_FSIZE`。
fn check_fsize(cx: &Cx<'_>, len: u64) -> Result<(), Errno> {
    if len > cx.t.group.borrow().rlimits[RLIMIT_FSIZE as usize].0 {
        return Err(file_too_big(cx));
    }
    Ok(())
}

/// 从打开文件读到 guest 缓冲区（read、pread、readv）。
pub(super) fn read_file(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: u64,
    len: usize,
    offset: Option<u64>,
) -> SysResult {
    // 终端、管道每次 read 只返回已经就绪的数据，不能为了凑满缓冲区再次阻塞；/dev/zero、
    // /dev/urandom 与 Linux 一样一次读满（`dd bs=256M` 读到的是整块）
    let fill = matches!(
        file.borrow().kind,
        FileKind::Regular { .. }
            | FileKind::Proc { klog: None, .. }
            | FileKind::Device {
                dev: Device::Zero | Device::Full | Device::Random,
                ..
            }
    );
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let k = match read_bytes(cx, file, &mut chunk[..n], offset.map(|o| o + done as u64)) {
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        if k > 0 {
            file.borrow_mut().reads += 1;
            // 信号驱动的 I/O 由定时器线程检查（check_fasync）：只叫醒它
            if file.borrow().flags & oflags::O_ASYNC != 0 {
                crate::kernel::signal_later(&cx.shared.timers);
            }
        }
        if let Err(e) = cx.mm().write(
            UserSlice::new(buf.wrapping_add(done as u64), k),
            &chunk[..k],
        ) {
            // 数据已经读走：与 Linux 一样，已拷贝一部分时返回部分长度
            if done == 0 {
                return Err(e);
            }
            break;
        }
        done += k;
        if k < n || !fill {
            break;
        }
    }
    Ok(done as u64)
}

/// 把 guest 缓冲区写进打开文件（write、pwrite、writev）。
pub(super) fn write_file(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: u64,
    len: usize,
    offset: Option<u64>,
) -> SysResult {
    // 缓冲区读不了之前先报 ESPIPE、EBADF
    positional(&file.borrow(), offset)?;
    if !file.borrow().writable() {
        return Err(Errno::EBADF);
    }
    // write(2) 不跳过长度 0 的写：/dev/kmsg 因此记一条空记录
    if len == 0 && matches!(file.borrow().kind, FileKind::Device { kmsg: Some(_), .. }) {
        return kmsg_write(cx, file, &[]).map(|n| n as u64);
    }
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let copied = cx.mm().read(
            UserSlice::new(buf.wrapping_add(done as u64), n),
            &mut chunk[..n],
        );
        let step = copied
            .and_then(|()| write_bytes(cx, file, &chunk[..n], offset.map(|o| o + done as u64)));
        match step {
            Ok(k) => {
                done += k;
                if k < n {
                    break;
                }
            }
            // 已经写出一部分时，像 Linux 一样返回已写的字节数
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        }
    }
    if done > 0 {
        write_sync(cx, file, 0)?;
    }
    Ok(done as u64)
}

/// `O_DSYNC`、`O_SYNC` 打开的普通文件，或者 pwritev2 带 `RWF_DSYNC`、`RWF_SYNC`（`rwf`）：
/// 写完之后像 fdatasync、fsync 一样等它落盘（Linux 的 generic_write_sync）。数据、长度和
/// 时间都在宿主的 object 里，交给宿主刷，放开 BKL 等；名字不在其列，Linux 上也要另外
/// fsync 目录。
fn write_sync(cx: &mut Cx<'_>, file: &FileRef, rwf: u32) -> Result<(), Errno> {
    const O_DSYNC: u32 = 0o10000;
    const O_SYNC_BIT: u32 = 0o4000000; // __O_SYNC，O_SYNC 是它加上 O_DSYNC
    const RWF_DSYNC: u32 = 2;
    const RWF_SYNC: u32 = 4;
    let (fd, data_only) = {
        let f = file.borrow();
        let FileKind::Regular { file: host, .. } = &f.kind else {
            return Ok(());
        };
        if f.flags & O_DSYNC == 0 && rwf & (RWF_DSYNC | RWF_SYNC) == 0 {
            return Ok(());
        }
        let full = f.flags & O_SYNC_BIT != 0 || rwf & RWF_SYNC != 0;
        (std::os::fd::AsRawFd::as_raw_fd(host), !full)
    };
    // 打开文件由 file 保活，放锁期间不碰它
    cx.unlocked(|| appine_hal::fsync(fd, data_only))
        .map_err(Errno)
}

pub(super) fn sendfile(
    cx: &mut Cx<'_>,
    out_fd: i32,
    in_fd: i32,
    offset: u64,
    count: usize,
) -> SysResult {
    let input = cx.fds().get(in_fd)?;
    let output = cx.fds().get(out_fd)?;
    if !input.borrow().readable() || !output.borrow().writable() {
        return Err(Errno::EBADF);
    }
    if !matches!(
        input.borrow().kind,
        FileKind::Regular { .. } | FileKind::Device { .. }
    ) {
        return Err(Errno::EINVAL);
    }
    let mut off = match offset {
        0 => None,
        addr => Some(u64::try_from(i64::from_le_bytes(get(cx, addr)?)).map_err(|_| Errno::EINVAL)?),
    };
    if let Some(r) = copy_regular(cx, &input, off, &output, None, count) {
        let n = r?;
        if let Some(o) = off {
            put(cx, offset, (o + n as u64).to_le_bytes())?;
        }
        return Ok(n as u64);
    }
    let mut chunk = vec![0u8; count.min(IO_CHUNK)];
    let mut done = 0;
    while done < count {
        let n = (count - done).min(IO_CHUNK);
        let got = match read_bytes(cx, &input, &mut chunk[..n], off) {
            Ok(0) => break,
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        let wrote = match write_bytes(cx, &output, &chunk[..got], None) {
            Ok(k) => k,
            Err(_) if done > 0 => 0,
            Err(e) => {
                if off.is_none() {
                    input.borrow_mut().pos -= got as u64;
                }
                return Err(e);
            }
        };
        // 没写出去的部分要还给输入端
        match &mut off {
            Some(o) => *o += wrote as u64,
            None => input.borrow_mut().pos -= (got - wrote) as u64,
        }
        done += wrote;
        if wrote < got || got < n {
            break;
        }
    }
    if let Some(o) = off {
        put(cx, offset, o.to_le_bytes())?;
    }
    Ok(done as u64)
}

/// sendfile、copy_file_range 在两个普通文件之间拷贝时交给宿主内核（[`appine_hal::copy_file`]）：
/// 数据不经过内核层，也只要一次宿主调用，不持 BKL。`src`、`dst` 为 None 时用并推进文件
/// 位置；输出 fd 的宿主位置也要用，所以总是像 [`unlocked_io`] 那样占住输出的位置，等的
/// 时候两个都不占，免得两个方向的拷贝互相等。不适用时（不是两个不同的普通文件、追加写、
/// 有封印、宿主不支持）返回 None，由调用方逐块读写。
fn copy_regular(
    cx: &mut Cx<'_>,
    input: &FileRef,
    src: Option<u64>,
    output: &FileRef,
    dst: Option<u64>,
    len: usize,
) -> Option<Result<usize, Errno>> {
    let (fd_in, fd_out, ino) = {
        let (i, o) = (input.borrow(), output.borrow());
        if o.flags & oflags::O_APPEND != 0 || i.ino() == o.ino() {
            return None;
        }
        (regular_fd(&i)?, regular_fd(&o)?, o.ino()?)
    };
    // 封印要逐块检查；tmpfs 要逐块记账、写满了停下（write_bytes）
    let in_tmpfs = cx.vfs().is_ok_and(|v| {
        v.tmpfs_dev(&Node::Store(output.borrow().mnt, ino))
            .is_some()
    });
    if cx.kr().seals.contains_key(&ino) || in_tmpfs {
        return None;
    }
    let len = match fsize_allowed(cx, &output.borrow(), dst, len.min(MAX_COPY)) {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    if len > 0
        && let Err(e) = remove_privs(cx, ino)
    {
        return Some(Err(e));
    }
    let busy = |f: &FileRef, used: bool| used && f.borrow().pos_busy != 0;
    while busy(input, src.is_none()) || busy(output, true) {
        if cx.task.killed.get() {
            return Some(Err(Errno::EINTR));
        }
        for (f, used) in [(input, src.is_none()), (output, true)] {
            if busy(f, used) {
                f.borrow_mut().pos_busy = 2;
            }
        }
        if let Err(e) = cx.wait_on(None, &mut [], crate::kernel::wake::LOCK) {
            return Some(Err(e));
        }
    }
    let from = src.unwrap_or_else(|| input.borrow().pos);
    let to = dst.unwrap_or_else(|| output.borrow().pos);
    if src.is_none() {
        input.borrow_mut().pos_busy = 1;
    }
    output.borrow_mut().pos_busy = 1;
    let r = cx.unlocked(|| appine_hal::copy_file(fd_in, from, fd_out, to, len));
    let mut wake = false;
    for (f, used, dst, at) in [(input, src.is_none(), src, from), (output, true, dst, to)] {
        if used {
            let mut f = f.borrow_mut();
            if let (Ok(n), None) = (r, dst) {
                f.pos = at + n as u64;
            }
            wake |= std::mem::take(&mut f.pos_busy) == 2;
        }
    }
    if wake {
        request_wake_locks();
    }
    match r.map_err(Errno) {
        // 宿主不支持这一对文件：调用方逐块读写
        Err(Errno::EINVAL | Errno::ENOSYS | Errno::EOPNOTSUPP | Errno::EXDEV) => None,
        Err(e) => Some(Err(e)),
        Ok(n) => {
            if n > 0 {
                inotify::file_event(cx.kr(), &input.borrow(), inotify::IN_ACCESS);
                inotify::file_event(cx.kr(), &output.borrow(), inotify::IN_MODIFY);
            }
            Some(Ok(n))
        }
    }
}

/// copy_file_range：在两个普通文件之间拷贝，不经过 guest 内存。偏移指针为空时使用并
/// 推进文件位置，否则使用并更新指针指向的偏移。
pub(super) fn copy_file_range(
    cx: &mut Cx<'_>,
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let input = cx.fds().get(fd_in)?;
    let output = cx.fds().get(fd_out)?;
    {
        let (i, o) = (input.borrow(), output.borrow());
        if !i.readable() || !o.writable() || o.flags & oflags::O_APPEND != 0 {
            return Err(Errno::EBADF);
        }
        for f in [&i, &o] {
            match f.kind {
                FileKind::Regular { .. } => {}
                FileKind::Directory { .. } => return Err(Errno::EISDIR),
                _ => return Err(Errno::EINVAL),
            }
        }
    }
    let read_off = |addr: u64| -> Result<Option<u64>, Errno> {
        match addr {
            0 => Ok(None),
            a => Ok(Some(
                u64::try_from(i64::from_le_bytes(get(cx, a)?)).map_err(|_| Errno::EINVAL)?,
            )),
        }
    };
    let (mut src, mut dst) = (read_off(off_in)?, read_off(off_out)?);
    let same = input.borrow().ino() == output.borrow().ino();
    if same {
        // 同一个文件里的两段不能重叠
        let s = src.unwrap_or(input.borrow().pos);
        let d = dst.unwrap_or(output.borrow().pos);
        if s < d + len as u64 && d < s + len as u64 {
            return Err(Errno::EINVAL);
        }
    }
    if let Some(r) = copy_regular(cx, &input, src, &output, dst, len) {
        let n = r? as u64;
        if let Some(s) = src {
            put(cx, off_in, (s + n).to_le_bytes())?;
        }
        if let Some(d) = dst {
            put(cx, off_out, (d + n).to_le_bytes())?;
        }
        return Ok(n);
    }
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let got = read_bytes(cx, &input, &mut chunk[..n], src)?;
        if got == 0 {
            break;
        }
        let wrote = write_bytes(cx, &output, &chunk[..got], dst)?;
        if let Some(s) = &mut src {
            *s += wrote as u64;
        } else {
            input.borrow_mut().pos -= (got - wrote) as u64;
        }
        if let Some(d) = &mut dst {
            *d += wrote as u64;
        }
        done += wrote;
        if wrote < got {
            break;
        }
    }
    if let Some(s) = src {
        put(cx, off_in, s.to_le_bytes())?;
    }
    if let Some(d) = dst {
        put(cx, off_out, d.to_le_bytes())?;
    }
    Ok(done as u64)
}

/// posix_fadvise：只影响性能，检查参数之后忽略。
pub(super) fn fadvise64(cx: &mut Cx<'_>, fd: i32, len: i64, advice: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    if matches!(file.borrow().kind, FileKind::Pipe(_) | FileKind::Socket(_)) {
        return Err(Errno::ESPIPE);
    }
    if len < 0 || !(0..=5).contains(&advice) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

pub(super) fn read(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    read_file(cx, &file, buf, len, None)
}

pub(super) fn write(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    write_file(cx, &file, buf, len, None)
}

fn checked_offset(offset: i64) -> Result<Option<u64>, Errno> {
    u64::try_from(offset).map(Some).map_err(|_| Errno::EINVAL)
}

pub(super) fn pread(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    let file = cx.fds().get(fd)?;
    read_file(cx, &file, buf, len, checked_offset(offset)?)
}

pub(super) fn pwrite(cx: &mut Cx<'_>, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let offset = append_offset(&file, checked_offset(offset)?)?;
    write_file(cx, &file, buf, len, offset)
}

/// 带偏移的写在 `O_APPEND` 的普通文件上照样写到末尾，文件位置不变（Linux 的
/// generic_write_checks，pwrite(2) 的 BUGS 一节），只有 pwritev2 的 `RWF_NOAPPEND` 写在给的
/// 偏移处。这样的写持着 BKL 做（[`write_bytes`]），与别的追加不会写到同一个位置。
fn append_offset(file: &FileRef, offset: Option<u64>) -> Result<Option<u64>, Errno> {
    let f = file.borrow();
    match &f.kind {
        FileKind::Regular { file, .. } if offset.is_some() && f.flags & oflags::O_APPEND != 0 => {
            Ok(Some(file.metadata().map_err(host_errno)?.len()))
        }
        _ => Ok(offset),
    }
}

/// 读取 iovec 数组。
fn iovecs(cx: &Cx<'_>, iov: u64, count: usize) -> Result<Vec<(u64, usize)>, Errno> {
    if count > UIO_MAXIOV {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(count);
    let mut total: usize = 0;
    for i in 0..count as u64 {
        let raw: [u8; 16] = get(cx, iov.wrapping_add(i * 16))?;
        let base = u64::from_le_bytes(raw[..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..].try_into().unwrap()) as usize;
        total = total
            .checked_add(len)
            .filter(|&t| t <= isize::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        out.push((base, len));
    }
    Ok(out)
}

/// readv、writev 和带偏移的变体：按顺序处理每个 iovec，遇到不完整的一段就停下。
/// `offset` 为 None 时使用并推进文件位置。
pub(super) fn vectored(
    cx: &mut Cx<'_>,
    fd: i32,
    iov: u64,
    count: usize,
    offset: Option<u64>,
    write: bool,
) -> SysResult {
    let file = cx.fds().get(fd)?;
    // /dev/kmsg 的 write_iter：整个 writev 是一条记录（systemd 这样写日志）
    if write && matches!(file.borrow().kind, FileKind::Device { kmsg: Some(_), .. }) {
        let vecs = iovecs(cx, iov, count)?;
        let total: usize = vecs.iter().map(|&(_, len)| len).sum();
        if !file.borrow().writable() {
            return Err(Errno::EBADF);
        }
        // writev 总长为 0 时不调用文件的写（do_iter_write）
        if total == 0 {
            return Ok(0);
        }
        if total > crate::klog::LOG_LINE_MAX {
            return Err(Errno::EINVAL);
        }
        let mut data = Vec::new();
        for (base, len) in vecs {
            let start = data.len();
            data.resize(start + len, 0);
            cx.mm()
                .read(UserSlice::new(base, len), &mut data[start..])?;
        }
        return kmsg_write(cx, &file, &data).map(|n| n as u64);
    }
    let mut total = 0;
    for (base, len) in iovecs(cx, iov, count)? {
        let at = offset.map(|o| o + total);
        let r = if write {
            write_file(cx, &file, base, len, at)
        } else {
            read_file(cx, &file, base, len, at)
        };
        match r {
            Ok(n) => {
                total += n;
                if (n as usize) < len {
                    break;
                }
            }
            Err(_) if total > 0 => break,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

pub(super) fn readv(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize) -> SysResult {
    vectored(cx, fd, iov, count, None, false)
}

pub(super) fn writev(cx: &mut Cx<'_>, fd: i32, iov: u64, count: usize) -> SysResult {
    vectored(cx, fd, iov, count, None, true)
}

/// preadv、pwritev；`flags` 不为 None 时是 preadv2、pwritev2，偏移为 -1 表示使用文件位置。
pub(super) fn pvectored(
    cx: &mut Cx<'_>,
    fd: i32,
    iov: u64,
    count: usize,
    offset: i64,
    flags: Option<u32>,
    write: bool,
) -> SysResult {
    // RWF_HIPRI 不影响结果，RWF_DSYNC、RWF_SYNC 写完等它落盘（[`write_sync`]）；RWF_NOAPPEND
    // （Linux 6.9 起，musl 的 pwrite 用它）让带偏移的写在 O_APPEND 的文件上也写在给的偏移处；
    // RWF_NOWAIT、RWF_APPEND 不支持，与 Linux 一样，不认识的标志都是 EOPNOTSUPP
    const RWF_NOAPPEND: u32 = 0x20;
    const RWF_SUPPORTED: u32 = 0x7 | RWF_NOAPPEND;
    let mut offset = match flags {
        Some(f) if f & !RWF_SUPPORTED != 0 => return Err(Errno::EOPNOTSUPP),
        Some(_) if offset == -1 => None,
        _ => checked_offset(offset)?,
    };
    if write && flags.is_none_or(|f| f & RWF_NOAPPEND == 0) {
        offset = append_offset(&cx.fds().get(fd)?, offset)?;
    }
    let n = vectored(cx, fd, iov, count, offset, write)?;
    if let Some(rwf) = flags.filter(|_| write && n > 0) {
        let file = cx.fds().get(fd)?;
        write_sync(cx, &file, rwf)?;
    }
    Ok(n)
}
