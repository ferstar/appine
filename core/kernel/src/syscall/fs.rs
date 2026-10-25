//! 文件与目录类 syscall。
//!
//! 读写分两层：`read_bytes`/`write_bytes` 对一个打开文件做一次宿主缓冲区上的操作，
//! `read_file`/`write_file` 负责和 guest 内存之间的拷贝。宿主 fd 和管道可能阻塞，
//! 阻塞期间释放 BKL，并且不持有任何打开文件的借用（别的线程可能在用同一个打开文件）。

use std::cell::RefCell;
use std::os::unix::fs::FileExt;
use std::rc::{Rc, Weak};
use std::time::Duration;

use super::SocketRef;
use super::{IO_CHUNK, SysResult, get, inotify, put, read_path, tty};
use crate::cred::{Cred, MAY_EXEC, MAY_READ, MAY_WRITE, cap};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, TimerFd, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_files};
use crate::locks::{self, Lock, Owner, Waiter};
use crate::pipe::{PIPE_BUF, PIPE_CAPACITY, Pipe, PipeEnd};
use crate::process::{RLIMIT_FSIZE, Signal};
use crate::pseudo::{self, PseudoNode};
use crate::signal::{self, SigInfo};
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::{self, Device, rdev};
use crate::vfs::store::{DirEntry, Ino, Stat, Store, mode};
use crate::vfs::{Node, Pseudo, Vfs, normalize};

pub(super) const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
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

/// inode 仓库对外报告的设备号。
const ROOT_DEV: u64 = 0x801;
/// 管道报告的设备号（Linux 上 pipefs 是一个匿名设备）。
const PIPE_DEV: u64 = 0xd;
/// socket、anon_inode（epoll、eventfd）报告的设备号。
const SOCK_DEV: u64 = 0x8;
const ANON_DEV: u64 = 0xe;
/// procfs、devpts 报告的设备号。
const PROC_DEV: u64 = 0x4;
const DEVPTS_DEV: u64 = 0x18;
/// mqueue 文件系统的设备号。
const MQUEUE_DEV: u64 = 0x13;
/// statfs 报告的文件系统类型。
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const PIPEFS_MAGIC: u64 = 0x5049_5045;
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;
/// PTY 主设备的设备号（`/dev/ptmx`）。
const PTMX_RDEV: u64 = rdev(5, 2);

/// 可以透传给宿主终端的 ioctl：编号、参数大小、是否是输出参数。
const TTY_IOCTLS: [(u32, usize, bool); 10] = [
    (0x5401, 36, true),  // TCGETS
    (0x5402, 36, false), // TCSETS
    (0x5403, 36, false), // TCSETSW
    (0x5404, 36, false), // TCSETSF
    (0x5405, 18, true),  // TCGETA
    (0x5406, 18, false), // TCSETA
    (0x5407, 18, false), // TCSETAW
    (0x5408, 18, false), // TCSETAF
    (0x5413, 8, true),   // TIOCGWINSZ
    (0x5414, 8, false),  // TIOCSWINSZ
];
const FIONREAD: u32 = 0x541b;
const RNDGETENTCNT: u32 = 0x8004_5200;
/// 熵池里的比特数（`/proc/sys/kernel/random/entropy_avail`）。
pub(crate) const ENTROPY: i32 = 256;
const FIONBIO: u32 = 0x5421;
const FIOASYNC: u32 = 0x5452;
const FIOSETOWN: u32 = 0x8901;
const SIOCSPGRP: u32 = 0x8902;
const FIOGETOWN: u32 = 0x8903;
const SIOCGPGRP: u32 = 0x8904;
const FIONCLEX: u32 = 0x5450;
const FIOCLEX: u32 = 0x5451;

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

/// 解析要新建或删除的路径：返回父目录和最后一个组件。只有仓库里的目录可以修改。
pub(super) fn parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<(Ino, &'p [u8]), Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path)?;
    Ok((store_ino(&dir)?, name))
}

/// 在目录 `dir` 里新建 `name` 之前的检查（`may_create`）：要能在目录里查找，名字
/// 不存在，目录可写。返回目录的元数据。
pub(super) fn may_create(store: &Store, cred: &Cred, dir: Ino, name: &[u8]) -> Result<Stat, Errno> {
    let st = store.stat(dir)?;
    cred.permission(&st, MAY_EXEC)?;
    if store.lookup(dir, name)?.is_some() {
        return Err(Errno::EEXIST);
    }
    cred.permission(&st, MAY_WRITE | MAY_EXEC)?;
    Ok(st)
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

/// fchmod、fchown 等作用的打开文件在仓库里的 inode：`O_PATH` 打开的文件不行。
fn file_ino(cx: &Cx<'_>, fd: i32) -> Result<Ino, Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    if f.flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    f.ino().ok_or(Errno::EBADF)
}

/// 仓库里的 inode；合成文件系统里的节点不能修改。
fn store_ino(node: &Node) -> Result<Ino, Errno> {
    match node {
        Node::Store(ino) => Ok(*ino),
        Node::Pseudo(_) => Err(Errno::EACCES),
    }
}

/// 打开时记下的绝对路径（`/proc/<pid>/fd/<n>`）：词法上规范化，不跟随符号链接。
fn display_path(cx: &Cx<'_>, vfs: &Vfs, start: &Node, path: &[u8]) -> Option<Rc<[u8]>> {
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
fn encode_stat(st: &Stat, dev: u64) -> [u8; 128] {
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
    put(64, &(size.div_ceil(4096) * 8).to_le_bytes());
    put(72, &asec.to_le_bytes());
    put(80, &ansec.to_le_bytes());
    put(88, &msec.to_le_bytes());
    put(96, &mnsec.to_le_bytes());
    put(104, &csec.to_le_bytes());
    put(112, &cnsec.to_le_bytes());
    b
}

/// fstat 会报告的 (st_dev, st_ino)，不必查仓库就能知道的部分（epoll 的 fdinfo）。
pub(crate) fn dev_ino(f: &OpenFile) -> (u64, u64) {
    match (&f.kind, f.node()) {
        (_, Some(Node::Store(ino))) => (ROOT_DEV, ino),
        (FileKind::Pipe(end), None) => (PIPE_DEV, end.pipe.borrow().ino),
        (FileKind::Socket(s), _) => (SOCK_DEV, pseudo::socket_ino(s)),
        (_, Some(Node::Pseudo(p))) => (PROC_DEV, pseudo::ino_of(&p)),
        _ => (ANON_DEV, 1),
    }
}

/// 文件系统节点的元数据。
fn node_stat(cx: &Cx<'_>, node: &Node) -> Result<[u8; 128], Errno> {
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    match node {
        Node::Store(ino) => Ok(encode_stat(&vfs.store().stat(*ino)?, ROOT_DEV)),
        // 魔法链接指向的打开文件：报告那个文件本身
        Node::Pseudo(PseudoNode::OpenFd(pid, fd)) => {
            let file = view.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
            file_stat(cx, &file)
        }
        Node::Pseudo(p) => {
            let dev = match p {
                PseudoNode::PtsRoot | PseudoNode::Pts(_) | PseudoNode::PtsPtmx => DEVPTS_DEV,
                _ => PROC_DEV,
            };
            Ok(encode_stat(&pseudo::stat(&view, p)?, dev))
        }
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
            Some(ino) => Ok(encode_stat(&cx.vfs()?.store().stat(ino)?, ROOT_DEV)),
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
        | FileKind::Pidfd(_) => Ok(anon(1, 0o600, 0, ANON_DEV)),
        FileKind::MessageQueue(q) => Ok(encode_stat(&q.borrow().stat(), MQUEUE_DEV)),
        FileKind::Pty(end) => {
            let n = end.tty.borrow().index;
            Ok(anon(
                pseudo::ino_of(&PseudoNode::Pts(n)),
                mode::S_IFCHR | 0o620,
                rdev(PTS_MAJOR, n),
                DEVPTS_DEV,
            ))
        }
        FileKind::Regular { ino, file } => Ok(encode_stat(
            &cx.vfs()?.store().stat_open(*ino, file)?,
            ROOT_DEV,
        )),
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
    let vfs = cx.vfs()?;
    let start = start_dir(cx, dirfd, &path)?;
    let create = flags & oflags::O_CREAT != 0;
    let excl = create && flags & oflags::O_EXCL != 0;
    let follow = flags & oflags::O_NOFOLLOW == 0 && !excl;
    let cred = cx.cred();

    let root = cx.root();
    let resolved = vfs.walk(&cx.view(&vfs), &cred, &root, &start, &path, follow);
    // parent 是最后经过的目录和名字，报告 inotify 事件用
    let (node, created, parent) = match resolved {
        Ok(_) if excl => return Err(Errno::EEXIST),
        Ok((node, found)) => (node, None, found),
        Err(e) if e == Errno::ENOENT && create => {
            // 最后一个组件是悬空的符号链接时，与 Linux 一样沿着它新建目标文件
            let (mut start, mut path) = (start.clone(), path.clone());
            let mut links = 0;
            loop {
                let (dir, name) =
                    vfs.resolve_parent(&cx.view(&vfs), &cred, &root, &start, &path)?;
                if path.ends_with(b"/") || name == b"." || name == b".." {
                    return Err(Errno::EISDIR);
                }
                let dir = store_ino(&dir)?;
                if let Some(ino) = vfs.store().lookup(dir, name)?
                    && follow
                    && vfs.store().stat(ino)?.kind() == mode::S_IFLNK
                {
                    links += 1;
                    if links > crate::vfs::MAX_SYMLINKS {
                        return Err(Errno::ELOOP);
                    }
                    path = vfs.store().readlink(ino)?;
                    start = Node::Store(dir);
                    continue;
                }
                let dir_st = may_create(vfs.store(), &cred, dir, name)?;
                let (uid, gid, perm) = cred.new_owner(&dir_st, perm & !cx.umask() & 0o7777, false);
                let (ino, file) = vfs.store().create_file(dir, name, perm, uid, gid)?;
                break (
                    Node::Store(ino),
                    Some(file),
                    Some((Node::Store(dir), name.to_vec())),
                );
            }
        }
        Err(e) => return Err(e),
    };

    let mut display = display_path(cx, &vfs, &start, &path);
    let was_created = created.is_some();
    let opened_ino = match &node {
        Node::Store(ino) if flags & oflags::O_PATH == 0 => Some(*ino),
        _ => None,
    };
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(node)
    } else {
        match node {
            Node::Store(ino) => open_store(cx, &cred, vfs.store(), ino, flags, created)?,
            Node::Pseudo(p) => open_pseudo(cx, &cred, &vfs, p, flags)?,
        }
    };
    if let FileKind::Pty(end) = &kind {
        // /dev/tty、/dev/ptmx 打开的是具体的某个 PTY
        display = (!end.master)
            .then(|| Rc::from(format!("/dev/pts/{}", end.tty.borrow().index).into_bytes()));
    }
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::with_path(kind, flags & keep, display);
    if let Some(ino) = opened_ino {
        open_events(cx, &file, ino, parent, was_created, flags);
    }
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
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
        Some((Node::Store(dir), name)) => Some((dir, name)),
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
    let Node::Store(ino) = node else {
        return Err(Errno::EOPNOTSUPP);
    };
    let room = u32::from_le_bytes(get(cx, handle)?);
    if room > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    // 放不下时写回需要的长度
    if room < FILE_HANDLE_BYTES {
        put(cx, handle, FILE_HANDLE_BYTES.to_le_bytes())?;
        return Err(Errno::EOVERFLOW);
    }
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&FILE_HANDLE_BYTES.to_le_bytes());
    b[4..8].copy_from_slice(&FILE_HANDLE_TYPE.to_le_bytes());
    b[8..].copy_from_slice(&ino.to_le_bytes());
    put(cx, handle, b)?;
    // 根文件系统的挂载 id
    put(cx, mount_id, 1i32.to_le_bytes())?;
    Ok(0)
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
    if mount_fd != AT_FDCWD {
        cx.fds().get(mount_fd)?;
    }
    let b: [u8; 16] = get(cx, handle)?;
    let bytes = u32::from_le_bytes(b[..4].try_into().unwrap());
    if bytes == 0 || bytes > MAX_HANDLE_SZ {
        return Err(Errno::EINVAL);
    }
    let ty = i32::from_le_bytes(b[4..8].try_into().unwrap());
    let ino = u64::from_le_bytes(b[8..].try_into().unwrap());
    let vfs = cx.vfs()?;
    // 文件已经删除（或者句柄不是这里发的）：ESTALE
    let st = match vfs.store().stat(ino) {
        Ok(st) if bytes == FILE_HANDLE_BYTES && ty == FILE_HANDLE_TYPE && st.nlink > 0 => st,
        _ => return Err(Errno::ESTALE),
    };
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !st.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(Node::Store(ino))
    } else {
        open_store(cx, &cx.cred(), vfs.store(), ino, flags, None)?
    };
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::new(kind, flags & keep);
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// 设备号对应的 PTY 从设备编号（主设备号 136–143）。
fn pts_index(r: u64) -> Option<u32> {
    let major = ((r >> 8) & 0xfff) as u32;
    let minor = ((r & 0xff) | ((r >> 12) & !0xff)) as u32;
    (PTS_MAJOR..PTS_MAJOR + 8)
        .contains(&major)
        .then(|| ((major - PTS_MAJOR) << 8) + minor)
}

/// 有没有进程正在执行仓库里的程序 `ino`（以写方式打开它时 ETXTBSY）。
pub(super) fn executing(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr()
        .tasks
        .values()
        .any(|t| t.res.borrow().mm.borrow().exe == Some(ino))
}

/// 有没有以写方式打开着的 `ino`（执行它时 ETXTBSY）。
pub(super) fn open_for_write(cx: &Cx<'_>, ino: Ino) -> bool {
    cx.kr().open_files().iter().any(|f| {
        let f = f.borrow();
        f.writable() && matches!(f.kind, FileKind::Regular { ino: i, .. } if i == ino)
    })
}

/// open 要求的访问权限（`MAY_*`）。
fn open_mask(flags: u32) -> u32 {
    match flags & oflags::O_ACCMODE {
        oflags::O_RDONLY => MAY_READ,
        oflags::O_WRONLY => MAY_WRITE,
        _ => MAY_READ | MAY_WRITE,
    }
}

/// 打开仓库里的 inode。刚刚由这次 open 新建的文件（`created`）不检查权限。
fn open_store(
    cx: &mut Cx<'_>,
    cred: &Cred,
    store: &Store,
    ino: Ino,
    flags: u32,
    created: Option<std::fs::File>,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    // 已经打开的（刚建好的，或者重新打开的已删除文件）从打开的 object 取属性
    let st = match &created {
        Some(f) => store.stat_open(ino, f)?,
        None => store.stat(ino)?,
    };
    if created.is_none() {
        let mut mask = open_mask(flags);
        match st.kind() {
            mode::S_IFLNK => return Err(Errno::ELOOP),
            mode::S_IFDIR if accmode != oflags::O_RDONLY || flags & oflags::O_CREAT != 0 => {
                return Err(Errno::EISDIR);
            }
            mode::S_IFREG if flags & oflags::O_TRUNC != 0 => mask |= MAY_WRITE,
            _ => {}
        }
        cred.permission(&st, mask)?;
        if flags & oflags::O_NOATIME != 0 && !cred.owns(&st) {
            return Err(Errno::EPERM);
        }
    }
    Ok(match st.kind() {
        mode::S_IFLNK => return Err(Errno::ELOOP),
        mode::S_IFDIR => {
            if accmode != oflags::O_RDONLY || flags & oflags::O_CREAT != 0 {
                return Err(Errno::EISDIR);
            }
            FileKind::Directory {
                node: Node::Store(ino),
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
                file.set_len(0).map_err(host_errno)?;
                truncated(cx, &file);
            }
            FileKind::Regular { ino, file }
        }
        mode::S_IFCHR => match st.rdev {
            // /dev/tty、/dev/console：调用者的控制终端
            r if r == rdev(5, 0) || r == rdev(5, 1) => tty::open_ctty(cx)?,
            PTMX_RDEV => tty::open_ptmx(cx.k(), (cred.fsuid, cred.fsgid))?,
            r => match pts_index(r) {
                Some(index) => tty::open_slave(cx, index, flags)?,
                None => FileKind::Device {
                    ino,
                    dev: dev::open(store, ino)?,
                },
            },
        },
        mode::S_IFIFO => open_fifo(cx, ino, flags)?,
        // 块设备、socket 文件不能打开（socket 文件用 connect）
        _ => return Err(Errno::ENXIO),
    })
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
    let end = PipeEnd::open(&pipe, read, write);
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
            cx.wait(None, &mut [])?;
        }
    }
    Ok(FileKind::Pipe(end))
}

/// 打开合成文件系统里的节点。
fn open_pseudo(
    cx: &mut Cx<'_>,
    cred: &Cred,
    vfs: &Vfs,
    node: PseudoNode,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    if !matches!(node, PseudoNode::OpenFd(..)) {
        cred.permission(&pseudo::stat(&cx.view(vfs), &node)?, open_mask(flags))?;
    }
    match node {
        PseudoNode::Pts(n) => return tty::open_slave(cx, n, flags),
        PseudoNode::PtsPtmx => return tty::open_ptmx(cx.k(), (cred.fsuid, cred.fsgid)),
        PseudoNode::OpenFd(pid, fd) => return reopen(cx, cred, vfs, pid, fd, flags),
        _ => {}
    }
    let view = cx.view(vfs);
    // O_NOFOLLOW 时停在了符号链接（比如 /proc/self/fd/<n>）上
    if Pseudo::kind(&view, &node)? == mode::S_IFLNK {
        return Err(Errno::ELOOP);
    }
    if vfs.is_dir(&view, &Node::Pseudo(node.clone()))? {
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
            node: Node::Pseudo(node),
            entries: None,
            process,
        });
    }
    if flags & oflags::O_DIRECTORY != 0 {
        return Err(Errno::ENOTDIR);
    }
    if accmode != oflags::O_RDONLY && !pseudo::writable(&node) {
        return Err(Errno::EACCES);
    }
    let data = pseudo::content(&view, &node)?;
    Ok(FileKind::Proc { node, data })
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
        drop(f);
        return open_store(cx, cred, vfs.store(), ino, flags, Some(new));
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
        F_GETFL => Ok((file.borrow().flags | oflags::O_LARGEFILE) as u64),
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
            file.borrow_mut().owner = (ty, pid);
            Ok(0)
        }
        F_GETOWN_EX => {
            let (ty, pid) = file.borrow().owner;
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
            request_wake_files();
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
        Owner::Ofd(Rc::downgrade(file))
    } else {
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
                b[24..28].copy_from_slice(&l.pid.to_le_bytes());
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
        cx.wait(None, &mut [])?;
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
    cx.fds_mut()
        .install(file, flags & MFD_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

pub(super) fn ioctl(cx: &mut Cx<'_>, fd: i32, request: u32, arg: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
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
        // socket 的属主（与 F_SETOWN、F_GETOWN 相同，负数是进程组）
        FIOSETOWN | SIOCSPGRP | FIOGETOWN | SIOCGPGRP
            if matches!(file.borrow().kind, FileKind::Socket(_)) =>
        {
            if matches!(request, FIOSETOWN | SIOCSPGRP) {
                let pid = i32::from_le_bytes(get(cx, arg)?);
                file.borrow_mut().owner = if pid < 0 {
                    (F_OWNER_PGRP, -pid)
                } else {
                    (F_OWNER_PID, pid)
                };
                return Ok(0);
            }
            let (ty, pid) = file.borrow().owner;
            let pid = if ty == F_OWNER_PGRP { -pid } else { pid };
            return put(cx, arg, pid.to_le_bytes()).map(|()| 0);
        }
        _ => {}
    }
    let pty = match &file.borrow().kind {
        FileKind::Pty(end) => Some((end.tty.clone(), end.master)),
        _ => None,
    };
    if let Some((t, master)) = pty {
        return tty::ioctl(cx, &t, master, request, arg);
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
        // 与 Linux 5.18 起相同，熵池总是满的
        FileKind::Device {
            dev: Device::Random,
            ..
        } if request == RNDGETENTCNT => {
            return put(cx, arg, ENTROPY.to_le_bytes()).map(|()| 0);
        }
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
    cred.permission(&st, amode).map(|()| 0)
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
    if let Node::Store(ino) = node {
        cred.permission(&vfs.store().stat(ino)?, MAY_EXEC)?;
    }
    if !cred.capable(cap::SYS_CHROOT) {
        return Err(Errno::EPERM);
    }
    cx.t.fs.borrow_mut().root = node;
    Ok(0)
}

/// chdir、fchdir：目标必须是可以搜索的目录。
fn set_cwd(cx: &mut Cx<'_>, node: Node) -> SysResult {
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    if let Node::Store(ino) = node {
        cx.cred().permission(&vfs.store().stat(ino)?, MAY_EXEC)?;
    }
    cx.t.fs.borrow_mut().cwd = node;
    Ok(0)
}

pub(super) fn getcwd(cx: &mut Cx<'_>, buf: u64, size: u64) -> SysResult {
    let vfs = cx.vfs()?;
    let mut path = vfs.path_of(&cx.view(&vfs), &cx.root(), &cx.cwd())?;
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
    // 路径为空时读 dirfd 本身（以 O_PATH | O_NOFOLLOW 打开的符号链接）
    let node = if path.is_empty() {
        cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::ENOENT)?
    } else {
        lookup_at(cx, dirfd, &path, false)?
    };
    let vfs = cx.vfs()?;
    let target = vfs.readlink(&cx.view(&vfs), &node)?;
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
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let st = stat_at(cx, dirfd, &path, flags)?;
    put(cx, buf, st).map(|()| 0)
}

/// newfstatat、statx 共用：`*at` 风格地找到对象，返回 `struct stat`。
fn stat_at(cx: &Cx<'_>, dirfd: i32, path: &[u8], flags: u32) -> Result<[u8; 128], Errno> {
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
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_STATX_SYNC_TYPE: u32 = 0x6000;
    const STATX_RESERVED: u32 = 0x8000_0000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let st = stat_at(
        cx,
        dirfd,
        &path,
        flags & (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH),
    )?;
    put(cx, buf, encode_statx(&st)).map(|()| 0)
}

/// 由 `struct stat` 得到 `struct statx`：总是给出全部基本字段（`STATX_BASIC_STATS`），
/// 没有创建时间。
fn encode_statx(st: &[u8; 128]) -> [u8; 256] {
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
    let st = file_stat(cx, &cx.fds().get(fd)?)?;
    put(cx, buf, st).map(|()| 0)
}

fn encode_statfs(magic: u64, s: &appine_hal::FsStats) -> [u8; 120] {
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
        0x20, // ST_VALID
    ];
    for (i, w) in words.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    b
}

/// 节点所在文件系统的 statfs。
fn statfs_of(cx: &Cx<'_>, node: &Node) -> Result<[u8; 120], Errno> {
    let empty = appine_hal::FsStats {
        block_size: 4096,
        ..Default::default()
    };
    Ok(match node {
        Node::Store(_) => encode_statfs(EXT4_SUPER_MAGIC, &cx.vfs()?.store().fs_stats()?),
        Node::Pseudo(PseudoNode::PtsRoot | PseudoNode::Pts(_) | PseudoNode::PtsPtmx) => {
            encode_statfs(DEVPTS_SUPER_MAGIC, &empty)
        }
        Node::Pseudo(_) => encode_statfs(PROC_SUPER_MAGIC, &empty),
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
    let b = match (pipe, node) {
        (true, _) => encode_statfs(
            PIPEFS_MAGIC,
            &appine_hal::FsStats {
                block_size: 4096,
                ..Default::default()
            },
        ),
        (false, Some(node)) => statfs_of(cx, &node)?,
        (false, None) => statfs_of(cx, &Node::ROOT)?,
    };
    put(cx, buf, b).map(|()| 0)
}

// ---- 修改命名空间 ----

pub(super) fn mkdirat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    let (dir, name) = parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    let dir_st = may_create(vfs.store(), &cred, dir, name)?;
    let (uid, gid, perm) = cred.new_owner(&dir_st, perm & !cx.umask() & 0o7777, true);
    vfs.store().mkdir(dir, name, perm, uid, gid)?;
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
    let (dir, name) = parent_at(cx, dirfd, &path)?;
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
    let dir_st = may_create(vfs.store(), &cred, dir, name)?;
    if matches!(kind, mode::S_IFCHR | mode::S_IFBLK) && !cred.capable(cap::MKNOD) {
        return Err(Errno::EPERM);
    }
    let (uid, gid, perm) = cred.new_owner(&dir_st, mode_arg & !cx.umask() & 0o7777, false);
    if kind == 0 || kind == mode::S_IFREG {
        vfs.store().create_file(dir, name, perm, uid, gid)?;
    } else {
        vfs.store()
            .mknod(dir, name, kind | perm, dev & 0xffff_ffff, uid, gid)?;
    }
    inotify::dir_event(cx.kr(), dir, name, inotify::IN_CREATE, 0);
    Ok(0)
}

pub(super) fn unlinkat(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    let (dir, name) = parent_at(cx, dirfd, &path)?;
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
                may_delete(store, &cred, dir, name)?;
                store.rmdir(dir, name)?;
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
        store.unlink(dir, name)?;
        if let Some(ino) = victim {
            unlinked_events(cx, store, ino, (dir, name));
            inotify::dir_event(cx.kr(), dir, name, inotify::IN_DELETE, 0);
        }
    }
    Ok(0)
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
    let (dir, name) = parent_at(cx, dirfd, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    let vfs = cx.vfs()?;
    let cred = cx.cred();
    reject_trailing_slash(vfs.store(), dir, name, &path)?;
    let dir_st = may_create(vfs.store(), &cred, dir, name)?;
    let (uid, gid, _) = cred.new_owner(&dir_st, 0o777, false);
    vfs.store().symlink(dir, name, &target, uid, gid)?;
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
    // 合成文件系统是另一个"设备"：跨设备不能建硬链接
    let (Node::Store(src), Node::Store(dir)) = (src, dir) else {
        return Err(Errno::EXDEV);
    };
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    reject_trailing_slash(vfs.store(), dir, name, &newpath)?;
    may_create(vfs.store(), &cred, dir, name)?;
    vfs.store().link(dir, name, src)?;
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
    let (old_dir, old_name) = parent_at(cx, olddirfd, &oldpath)?;
    let (new_dir, new_name) = parent_at(cx, newdirfd, &newpath)?;
    if [old_name, new_name]
        .iter()
        .any(|n| *n == b"." || *n == b"..")
    {
        return Err(Errno::EBUSY);
    }
    let vfs = cx.vfs()?;
    // 任何一边以 / 结尾，被移动的就必须是目录
    if (oldpath.ends_with(b"/") || newpath.ends_with(b"/"))
        && let Some(src) = vfs.store().lookup(old_dir, old_name)?
        && !vfs.store().stat(src)?.is_dir()
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
    // 有 inotify 监视时先记下两边的 inode 和它们是不是目录
    let entry = |dir, name| -> Result<Option<(Ino, u32)>, Errno> {
        match vfs.store().lookup(dir, name)? {
            Some(ino) if vfs.store().stat(ino)?.is_dir() => Ok(Some((ino, inotify::IN_ISDIR))),
            Some(ino) => Ok(Some((ino, 0))),
            None => Ok(None),
        }
    };
    let (src, dst) = if inotify::active(cx.kr()) {
        (entry(old_dir, old_name)?, entry(new_dir, new_name)?)
    } else {
        (None, None)
    };
    vfs.store().rename(
        old_dir,
        old_name,
        new_dir,
        new_name,
        flags & RENAME_NOREPLACE != 0,
        exchange,
    )?;
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
    let ino = store_ino(&lookup_at(cx, dirfd, &path, true)?)?;
    chmod(cx, ino, perm)?;
    attrib_at(cx, ino, dirfd, &path, true);
    Ok(0)
}

pub(super) fn fchmod(cx: &mut Cx<'_>, fd: i32, perm: u32) -> SysResult {
    let ino = file_ino(cx, fd)?;
    chmod(cx, ino, perm)?;
    attrib_fd(cx, fd);
    Ok(0)
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
fn chmod(cx: &Cx<'_>, ino: Ino, perm: u32) -> SysResult {
    let vfs = cx.vfs()?;
    let st = vfs.store().stat(ino)?;
    let cred = cx.cred();
    if !cred.owns(&st) {
        return Err(Errno::EPERM);
    }
    let mut perm = perm & 0o7777;
    if !cred.in_group(st.gid) && !cred.capable(cap::FSETID) {
        perm &= !mode::S_ISGID;
    }
    vfs.store().set_mode(ino, perm)?;
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
    let ino = store_ino(&node)?;
    chown(cx, ino, uid, gid)?;
    if path.is_empty() {
        attrib_fd(cx, dirfd);
    } else {
        attrib_at(cx, ino, dirfd, &path, follow);
    }
    Ok(0)
}

pub(super) fn fchown(cx: &mut Cx<'_>, fd: i32, uid: u32, gid: u32) -> SysResult {
    let ino = file_ino(cx, fd)?;
    chown(cx, ino, uid, gid)?;
    attrib_fd(cx, fd);
    Ok(0)
}

/// 没有 `CAP_CHOWN` 时，属主只能把属组改成自己所在的组，不能修改属主。
fn chown(cx: &Cx<'_>, ino: Ino, uid: u32, gid: u32) -> SysResult {
    let (uid, gid) = (owner_arg(uid), owner_arg(gid));
    let vfs = cx.vfs()?;
    let st = vfs.store().stat(ino)?;
    let cred = cx.cred();
    let owner = cred.fsuid == st.uid;
    let uid_ok = uid.is_none_or(|u| owner && u == st.uid);
    let gid_ok = gid.is_none_or(|g| owner && (g == st.gid || cred.in_group(g)));
    if !(cred.capable(cap::CHOWN) || uid_ok && gid_ok) {
        return Err(Errno::EPERM);
    }
    vfs.store().set_owner(ino, uid, gid)?;
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
    let (ino, path) = if path == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        (file_ino(cx, dirfd)?, Vec::new())
    } else {
        let path = read_path(cx, path)?;
        let node = lookup_at_or_fd(cx, &cx.cred(), dirfd, &path, flags, follow)?;
        (store_ino(&node)?, path)
    };
    let now = || {
        appine_hal::clock_gettime(0)
            .map(|(s, ns)| s * 1_000_000_000 + ns)
            .map_err(Errno)
    };
    // times[0] 是 atime，times[1] 是 mtime
    let (atime, mtime, explicit) = if times == 0 {
        let t = now()?;
        (Some(t), Some(t), false)
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
            UTIME_OMIT => Ok(None),
            UTIME_NOW => now().map(Some),
            nsec => Ok(Some(sec * 1_000_000_000 + nsec)),
        };
        (time(word(0), word(1))?, time(word(2), word(3))?, explicit)
    };
    // 设成指定的时间只有属主可以，设成现在的时间有写权限也可以
    let st = cx.vfs()?.store().stat(ino)?;
    let cred = cx.cred();
    if !cred.owns(&st) {
        if explicit {
            return Err(Errno::EPERM);
        }
        cred.permission(&st, MAY_WRITE)?;
    }
    cx.vfs()?.store().set_times(ino, atime, mtime)?;
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
    let ino = store_ino(&lookup_at(cx, AT_FDCWD, &path, true)?)?;
    let st = vfs.store().stat(ino)?;
    match st.kind() {
        mode::S_IFDIR => Err(Errno::EISDIR),
        mode::S_IFREG => {
            cx.cred().permission(&st, MAY_WRITE)?;
            if executing(cx, ino) {
                return Err(Errno::ETXTBSY);
            }
            check_fsize(cx, len)?;
            super::lease::break_leases(cx, ino, true, false, None)?;
            let host = vfs.store().open_object(ino, true)?;
            host.set_len(len).map_err(host_errno)?;
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
        FileKind::Regular { file, .. } if f.writable() => {
            check_fsize(cx, len)?;
            check_seals(cx, &f, false, |_| len)?;
            file.set_len(len).map_err(host_errno)?;
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
    let host = match &f.kind {
        FileKind::Regular { file, .. } => file,
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
            request_wake_files();
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
            locks.retain(|(f, _)| !f.ptr_eq(&me));
            locks.push((me, exclusive));
            return Ok(0);
        }
        if op & LOCK_NB != 0 {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // 持有锁的文件解锁或关闭（OpenFile 析构）时会请求唤醒
        cx.wait(None, &mut [])?;
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

pub(super) fn fsync(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    // 仓库的数据直接写在宿主文件里；为了 apk 这类频繁 fsync 的负载，不逐个刷盘
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        _ if f.flags & oflags::O_PATH != 0 => Err(Errno::EBADF),
        // 只有普通文件和目录能同步（宿主 fd 交给宿主）
        FileKind::Regular { .. } | FileKind::Directory { .. } | FileKind::Host(_) => Ok(0),
        _ => Err(Errno::EINVAL),
    }
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
            Node::Store(i) => *i,
            Node::Pseudo(p) => pseudo::node_ino(view.k, p),
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
            Node::Store(ino) => list.extend(vfs.store().readdir(*ino)?),
            Node::Pseudo(p) => list.extend(pseudo::readdir(&view, p)?),
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
        | FileKind::Pidfd(_) => return Err(Errno::ESPIPE),
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
    Pty(Rc<RefCell<Tty>>, bool),
    Socket(SocketRef),
    EventFd(Rc<RefCell<EventFd>>),
    TimerFd(Rc<RefCell<TimerFd>>),
    SignalFd(Rc<RefCell<crate::fd::SignalFd>>),
    Inotify(Rc<RefCell<super::Inotify>>),
    /// `/proc/<pid>/pagemap`：按读的位置现算。
    Pagemap(Tid),
    Local,
}

fn io_path(f: &OpenFile) -> IoPath {
    match &f.kind {
        FileKind::Host(fd) => IoPath::Host(*fd),
        FileKind::Pipe(end) => IoPath::Pipe(end.pipe.clone()),
        FileKind::Pty(end) => IoPath::Pty(end.tty.clone(), end.master),
        FileKind::Socket(s) => IoPath::Socket(s.clone()),
        FileKind::EventFd(e) => IoPath::EventFd(e.clone()),
        FileKind::TimerFd(t) => IoPath::TimerFd(t.clone()),
        FileKind::SignalFd(s) => IoPath::SignalFd(s.clone()),
        FileKind::Inotify(i) => IoPath::Inotify(i.clone()),
        FileKind::Proc {
            node: PseudoNode::Entry(pid, _, pseudo::Entry::Pagemap),
            ..
        } => IoPath::Pagemap(*pid),
        _ => IoPath::Local,
    }
}

/// 能不能按位置读写（AIO 对不能定位的文件不管偏移）。
pub(super) fn seekable(f: &OpenFile) -> bool {
    matches!(io_path(f), IoPath::Local | IoPath::Pagemap(_))
}

/// 读写走哪条路径。带偏移的读写（pread、pwritev）遇到不能定位的文件是 ESPIPE：与
/// Linux 一样先于打开方式检查（只读打开的管道 pwrite 也是 ESPIPE），`O_PATH` 除外。
fn positional(f: &OpenFile, offset: Option<u64>) -> Result<IoPath, Errno> {
    let path = io_path(f);
    let seekable = matches!(path, IoPath::Local | IoPath::Pagemap(_));
    if offset.is_some() && !seekable && f.flags & oflags::O_PATH == 0 {
        return Err(Errno::ESPIPE);
    }
    Ok(path)
}

/// 等宿主 fd 可读或可写；非阻塞时不等。
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
        cx.wait(None, &mut pfd)?;
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
                for (d, s) in buf.iter_mut().zip(p.buf.drain(..n)) {
                    *d = s;
                }
                request_wake_files();
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
        cx.wait(None, &mut [])?;
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
        cx.wait(None, &mut [])?;
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
                | FileKind::Device { .. }
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
        pipe.borrow_mut().buf.drain(..n);
        request_wake_files();
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
        cx.wait(None, &mut [])?;
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
    pipe.borrow_mut().buf.extend(&data);
    request_wake_files();
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
                p.buf.extend(&data[done..done + n]);
                done += n;
                request_wake_files();
                true
            } else {
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
        cx.wait(None, &mut [])?;
    }
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
    io: impl FnOnce(i32, u64) -> Result<usize, i32>,
) -> Result<usize, Errno> {
    let pos = match offset {
        Some(off) => off,
        None => {
            while file.borrow().pos_busy != 0 {
                if cx.task.killed.get() {
                    return Err(Errno::EINTR);
                }
                file.borrow_mut().pos_busy = 2;
                cx.wait_on(None, &mut [], crate::kernel::wake::FILE)?;
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
            request_wake_files();
        }
    }
    r.map_err(Errno)
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
        },
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
        FileKind::Epoll(_) | FileKind::Pidfd(_) => return Err(Errno::EINVAL),
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
        FileKind::Epoll(_) | FileKind::Pidfd(_) | FileKind::MessageQueue(_) => Err(Errno::EINVAL),
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
        IoPath::Pty(t, master) => tty::read(cx, &t, master, nonblock, buf),
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
        IoPath::Local => {
            let regular = regular_fd(&file.borrow());
            let n = match regular {
                Some(fd) => unlocked_io(cx, file, fd, offset, |fd, pos| {
                    appine_hal::pread(fd, buf, pos)
                })?,
                None => local_read(&mut file.borrow_mut(), buf, offset)?,
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
        IoPath::Pty(t, master) => tty::write(cx, &t, master, nonblock, data),
        IoPath::Socket(s) => super::net::send(cx, &s, nonblock, data, &[], &[], 0),
        IoPath::EventFd(e) => super::poll::eventfd_write(cx, &e, nonblock, data),
        IoPath::TimerFd(_) | IoPath::SignalFd(_) | IoPath::Inotify(_) | IoPath::Pagemap(_) => {
            Err(Errno::EINVAL)
        }
        IoPath::Local => {
            let data = &data[..fsize_allowed(cx, &file.borrow(), offset, data.len())?];
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
            if let Some(node) = proc_node {
                let cred = cx.cred();
                pseudo::write(cx.k(), &cred, &node, data)?;
            }
            // 普通文件不持 BKL 写；O_APPEND 仍持锁做，几个进程同时追加时不会互相覆盖
            let regular = regular_fd(&file.borrow())
                .filter(|_| offset.is_some() || file.borrow().flags & oflags::O_APPEND == 0);
            let n = match regular {
                Some(fd) => unlocked_io(cx, file, fd, offset, |fd, pos| {
                    appine_hal::pwrite(fd, data, pos)
                })?,
                None => local_write(&mut file.borrow_mut(), data, offset)?,
            };
            if n > 0 {
                inotify::file_event(cx.kr(), &file.borrow(), inotify::IN_MODIFY);
            }
            Ok(n)
        }
    }
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
    // 终端、管道每次 read 只返回已经就绪的数据，不能为了凑满缓冲区再次阻塞
    let fill = matches!(
        file.borrow().kind,
        FileKind::Regular { .. } | FileKind::Proc { .. }
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
            if file.borrow().flags & oflags::O_ASYNC != 0 {
                request_wake_files();
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
    Ok(done as u64)
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
    write_file(cx, &file, buf, len, checked_offset(offset)?)
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
    // RWF_HIPRI、RWF_DSYNC、RWF_SYNC 不影响结果；带偏移的写本来就不管 O_APPEND，所以
    // RWF_NOAPPEND 也不影响结果（musl 的 pwrite 用它）；RWF_NOWAIT、RWF_APPEND 不支持，
    // 与 Linux 一样，不认识的标志都是 EOPNOTSUPP
    const RWF_SUPPORTED: u32 = 0x7 | 0x20;
    let offset = match flags {
        Some(f) if f & !RWF_SUPPORTED != 0 => return Err(Errno::EOPNOTSUPP),
        Some(_) if offset == -1 => None,
        _ => checked_offset(offset)?,
    };
    vectored(cx, fd, iov, count, offset, write)
}
