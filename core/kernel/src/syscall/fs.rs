//! 文件与目录类 syscall。
//!
//! 读写分两层：`read_bytes`/`write_bytes` 对一个打开文件做一次宿主缓冲区上的操作，
//! `read_file`/`write_file` 负责和 guest 内存之间的拷贝。宿主 fd 和管道可能阻塞，
//! 阻塞期间释放 BKL，并且不持有任何打开文件的借用（别的线程可能在用同一个打开文件）。

use std::cell::RefCell;
use std::os::unix::fs::FileExt;
use std::rc::Rc;
use std::time::Duration;

use super::SocketRef;
use super::{IO_CHUNK, SysResult, get, put, read_path, tty};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Cx, request_wake};
use crate::pipe::{PIPE_BUF, PIPE_CAPACITY, Pipe, PipeEnd};
use crate::process::Signal;
use crate::pseudo::{self, PseudoNode};
use crate::signal::{self, SigInfo};
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::{self, Device, rdev};
use crate::vfs::store::{DirEntry, Ino, Stat, Store, mode};
use crate::vfs::{Node, Vfs, normalize};

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
/// statfs 报告的文件系统类型。
const EXT4_SUPER_MAGIC: u64 = 0xef53;
const PIPEFS_MAGIC: u64 = 0x5049_5045;
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;
/// PTY 主设备的设备号（`/dev/ptmx`）。
const PTMX_RDEV: u64 = rdev(5, 2);

/// 可以透传给宿主终端的 ioctl：编号、参数大小、是否是输出参数。
const TTY_IOCTLS: [(u32, usize, bool); 6] = [
    (0x5401, 36, true),  // TCGETS
    (0x5402, 36, false), // TCSETS
    (0x5403, 36, false), // TCSETSW
    (0x5404, 36, false), // TCSETSF
    (0x5413, 8, true),   // TIOCGWINSZ
    (0x5414, 8, false),  // TIOCSWINSZ
];
const FIONREAD: u32 = 0x541b;
const FIONBIO: u32 = 0x5421;
const FIONCLEX: u32 = 0x5450;
const FIOCLEX: u32 = 0x5451;

const POLLIN: i16 = 0x001;

fn host_errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(Errno::EIO.0))
}

// ---- 路径 ----

/// `*at` 系列的起点目录。
fn start_dir(cx: &Cx<'_>, dirfd: i32, path: &[u8]) -> Result<Node, Errno> {
    if path.first() == Some(&b'/') {
        return Ok(Node::ROOT);
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
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    vfs.resolve(&cx.view(&vfs), &start, path, follow)
}

/// `*at` 系列里"路径为空时作用于 dirfd 本身"（`AT_EMPTY_PATH`）的公共部分。
fn lookup_at_or_fd(
    cx: &Cx<'_>,
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
    lookup_at(cx, dirfd, path, follow)
}

/// 解析要新建或删除的路径：返回父目录和最后一个组件。只有仓库里的目录可以修改。
pub(super) fn parent_at<'p>(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &'p [u8],
) -> Result<(Ino, &'p [u8]), Errno> {
    let start = start_dir(cx, dirfd, path)?;
    let vfs = cx.vfs()?;
    let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &start, path)?;
    Ok((store_ino(&dir)?, name))
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
        let mut p = vfs.path_of(&cx.view(vfs), start).ok()?;
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
    // 不记录 atime，用 mtime 代替
    put(72, &msec.to_le_bytes());
    put(80, &mnsec.to_le_bytes());
    put(88, &msec.to_le_bytes());
    put(96, &mnsec.to_le_bytes());
    put(104, &csec.to_le_bytes());
    put(112, &cnsec.to_le_bytes());
    b
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
            mtime_ns: 0,
            ctime_ns: 0,
        };
        encode_stat(&st, dev)
    };
    match &f.kind {
        FileKind::Host(fd) => host_stat(*fd),
        FileKind::Pipe(end) => Ok(anon(
            end.pipe.borrow().ino,
            mode::S_IFIFO | 0o600,
            0,
            PIPE_DEV,
        )),
        FileKind::Pty(end) if end.master => Ok(anon(2, mode::S_IFCHR | 0o666, PTMX_RDEV, ROOT_DEV)),
        FileKind::Socket(s) => Ok(anon(
            Rc::as_ptr(s) as u64 >> 4,
            mode::S_IFSOCK | 0o777,
            0,
            SOCK_DEV,
        )),
        // anon_inode：没有文件类型位
        FileKind::Epoll(_) | FileKind::EventFd(_) => Ok(anon(1, 0o600, 0, ANON_DEV)),
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

    let resolved = vfs.resolve(&cx.view(&vfs), &start, &path, follow);
    let (node, created) = match resolved {
        Ok(_) if excl => return Err(Errno::EEXIST),
        Ok(node) => (node, None),
        Err(e) if e == Errno::ENOENT && create => {
            let (dir, name) = vfs.resolve_parent(&cx.view(&vfs), &start, &path)?;
            if path.ends_with(b"/") || name == b"." || name == b".." {
                return Err(Errno::EISDIR);
            }
            let dir = store_ino(&dir)?;
            let perm = perm & !cx.umask() & 0o7777;
            let (ino, file) = vfs.store().create_file(dir, name, perm, 0, 0)?;
            (Node::Store(ino), Some(file))
        }
        Err(e) => return Err(e),
    };

    let mut display = display_path(cx, &vfs, &start, &path);
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !vfs.is_dir(&cx.view(&vfs), &node)? {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(node)
    } else {
        match node {
            Node::Store(ino) => open_store(cx, vfs.store(), ino, flags, created)?,
            Node::Pseudo(p) => open_pseudo(cx, &vfs, p, flags)?,
        }
    };
    if let FileKind::Pty(end) = &kind {
        // /dev/tty、/dev/ptmx 打开的是具体的某个 PTY
        display = (!end.master)
            .then(|| Rc::from(format!("/dev/pts/{}", end.tty.borrow().index).into_bytes()));
    }
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::with_path(kind, flags & keep, display);
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

/// 打开仓库里的 inode。
fn open_store(
    cx: &mut Cx<'_>,
    store: &Store,
    ino: Ino,
    flags: u32,
    created: Option<std::fs::File>,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    let st = store.stat(ino)?;
    Ok(match st.kind() {
        mode::S_IFLNK => return Err(Errno::ELOOP),
        mode::S_IFDIR => {
            if accmode != oflags::O_RDONLY || flags & oflags::O_CREAT != 0 {
                return Err(Errno::EISDIR);
            }
            FileKind::Directory {
                node: Node::Store(ino),
                entries: None,
            }
        }
        _ if flags & oflags::O_DIRECTORY != 0 => return Err(Errno::ENOTDIR),
        mode::S_IFREG => {
            let file = match created {
                Some(f) => f,
                None => store.open_object(ino, accmode != oflags::O_RDONLY)?,
            };
            if flags & oflags::O_TRUNC != 0 && accmode != oflags::O_RDONLY {
                file.set_len(0).map_err(host_errno)?;
            }
            FileKind::Regular { ino, file }
        }
        mode::S_IFCHR => match st.rdev {
            // /dev/tty、/dev/console：调用者的控制终端
            r if r == rdev(5, 0) || r == rdev(5, 1) => tty::open_ctty(cx)?,
            PTMX_RDEV => tty::open_ptmx(cx.k())?,
            r => match pts_index(r) {
                Some(index) => tty::open_slave(cx, index, flags)?,
                None => FileKind::Device {
                    ino,
                    dev: dev::open(store, ino)?,
                },
            },
        },
        // 块设备、FIFO、socket 文件还不支持
        _ => return Err(Errno::ENXIO),
    })
}

/// 打开合成文件系统里的节点。
fn open_pseudo(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    node: PseudoNode,
    flags: u32,
) -> Result<FileKind, Errno> {
    let accmode = flags & oflags::O_ACCMODE;
    match node {
        PseudoNode::Pts(n) => return tty::open_slave(cx, n, flags),
        PseudoNode::PtsPtmx => return tty::open_ptmx(cx.k()),
        PseudoNode::OpenFd(pid, fd) => return reopen(cx, vfs, pid, fd, accmode),
        _ => {}
    }
    let view = cx.view(vfs);
    if vfs.is_dir(&view, &Node::Pseudo(node.clone()))? {
        if accmode != oflags::O_RDONLY {
            return Err(Errno::EISDIR);
        }
        return Ok(FileKind::Directory {
            node: Node::Pseudo(node),
            entries: None,
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

/// 经由 `/proc/<pid>/fd/<n>` 重新打开一个不在仓库里的文件：管道得到同一个管道的
/// 一端，宿主 fd 得到同一个宿主 fd。
fn reopen(cx: &Cx<'_>, vfs: &Vfs, pid: i32, fd: i32, accmode: u32) -> Result<FileKind, Errno> {
    let file = cx.view(vfs).open_file(pid, fd).ok_or(Errno::ENOENT)?;
    let f = file.borrow();
    match &f.kind {
        FileKind::Pipe(end) => {
            if accmode == oflags::O_RDWR {
                return Err(Errno::EINVAL);
            }
            let write = accmode == oflags::O_WRONLY;
            Ok(FileKind::Pipe(PipeEnd::reopen(&end.pipe, write)))
        }
        FileKind::Host(fd) => Ok(FileKind::Host(*fd)),
        _ => Err(Errno::ENXIO),
    }
}

pub(super) fn close(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    // 先从表里拿出来再释放：关闭管道的最后一端会唤醒另一端
    let r = cx.fds_mut().close(fd);
    r.map(|()| 0)
}

pub(super) fn close_range(cx: &mut Cx<'_>, first: u32, last: u32, flags: u32) -> SysResult {
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return Err(Errno::EINVAL);
    }
    if flags & CLOSE_RANGE_UNSHARE != 0 && Rc::strong_count(&cx.t.fds) > 1 {
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    cx.fds_mut().close_range(
        first as usize,
        last as usize,
        flags & CLOSE_RANGE_CLOEXEC != 0,
    );
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
    cx.fds_mut()
        .install_at(new, file, flags != 0)
        .map(|()| new as u64)
}

pub(super) fn pipe2(cx: &mut Cx<'_>, fds: u64, flags: u32) -> SysResult {
    const O_DIRECT: u32 = 0o200000;
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK | O_DIRECT) != 0 {
        return Err(Errno::EINVAL);
    }
    let cloexec = flags & oflags::O_CLOEXEC != 0;
    let status = flags & oflags::O_NONBLOCK;
    let (r, w) = PipeEnd::pair();
    let r = OpenFile::new(FileKind::Pipe(r), oflags::O_RDONLY | status);
    let w = OpenFile::new(FileKind::Pipe(w), oflags::O_WRONLY | status);
    let rfd = cx.fds_mut().install(r, cloexec, 0)?;
    let wfd = match cx.fds_mut().install(w, cloexec, 0) {
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
            let min = usize::try_from(arg as i32).map_err(|_| Errno::EINVAL)?;
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
            let mut f = file.borrow_mut();
            f.flags = (f.flags & !oflags::SETFL_MASK) | (arg as u32 & oflags::SETFL_MASK);
            Ok(0)
        }
        // 记录锁还没有实现：F_GETLK 总是报告没有锁，加锁总是成功
        F_GETLK => put(cx, arg, F_UNLCK.to_le_bytes()).map(|()| 0),
        F_SETLK | F_SETLKW => Ok(0),
        F_GETPIPE_SZ | F_SETPIPE_SZ => match file.borrow().kind {
            FileKind::Pipe(_) => Ok(PIPE_CAPACITY as u64),
            _ => Err(Errno::EBADF),
        },
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
            *seals |= add;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
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
    let node = lookup_at_or_fd(cx, dirfd, &path, flags, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    let st = node_stat(cx, &node)?;
    let st_mode = u32::from_le_bytes(st[16..20].try_into().unwrap());
    // guest 以 root 运行：读写总是允许，执行要求至少一个 x 位（目录除外）
    if amode & 1 != 0 && st_mode & mode::S_IFMT != mode::S_IFDIR && st_mode & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(0)
}

pub(super) fn chdir(cx: &mut Cx<'_>, path: u64) -> SysResult {
    let path = read_path(cx, path)?;
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    let node = vfs.resolve(&view, &cx.cwd(), &path, true)?;
    if !vfs.is_dir(&view, &node)? {
        return Err(Errno::ENOTDIR);
    }
    cx.t.fs.borrow_mut().cwd = node;
    Ok(0)
}

pub(super) fn fchdir(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    let node = cx.fds().get(fd)?.borrow().node().ok_or(Errno::ENOTDIR)?;
    let vfs = cx.vfs()?;
    if !vfs.is_dir(&cx.view(&vfs), &node)? {
        return Err(Errno::ENOTDIR);
    }
    cx.t.fs.borrow_mut().cwd = node;
    Ok(0)
}

pub(super) fn getcwd(cx: &mut Cx<'_>, buf: u64, size: u64) -> SysResult {
    let vfs = cx.vfs()?;
    let mut path = vfs.path_of(&cx.view(&vfs), &cx.cwd())?;
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
    let node = lookup_at(cx, dirfd, &path, false)?;
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
        let node = lookup_at_or_fd(cx, dirfd, path, flags, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        node_stat(cx, &node)
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
    let vfs = cx.vfs()?;
    let node = vfs.resolve(&cx.view(&vfs), &cx.cwd(), &path, true)?;
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
    cx.vfs()?
        .store()
        .mkdir(dir, name, perm & !cx.umask() & 0o7777, 0, 0)?;
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
    let perm = mode_arg & !cx.umask() & 0o7777;
    let vfs = cx.vfs()?;
    match mode_arg & mode::S_IFMT {
        0 | mode::S_IFREG => {
            vfs.store().create_file(dir, name, perm, 0, 0)?;
        }
        kind @ (mode::S_IFCHR | mode::S_IFBLK | mode::S_IFIFO | mode::S_IFSOCK) => {
            vfs.store()
                .mknod(dir, name, kind | perm, dev & 0xffff_ffff, 0, 0)?;
        }
        mode::S_IFDIR => return Err(Errno::EPERM),
        _ => return Err(Errno::EINVAL),
    }
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
    if flags & AT_REMOVEDIR != 0 {
        match name {
            b"." => return Err(Errno::EINVAL),
            b".." => return Err(Errno::ENOTEMPTY),
            _ => store.rmdir(dir, name)?,
        }
    } else {
        if name == b"." || name == b".." || path.ends_with(b"/") {
            return Err(Errno::EISDIR);
        }
        store.unlink(dir, name)?;
    }
    Ok(0)
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
    cx.vfs()?.store().symlink(dir, name, &target, 0, 0)?;
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
    let src = store_ino(&lookup_at_or_fd(
        cx,
        olddirfd,
        &oldpath,
        flags,
        flags & AT_SYMLINK_FOLLOW != 0,
    )?)?;
    let newpath = read_path(cx, newpath)?;
    let (dir, name) = parent_at(cx, newdirfd, &newpath)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    cx.vfs()?.store().link(dir, name, src)?;
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
    cx.vfs()?.store().rename(
        old_dir,
        old_name,
        new_dir,
        new_name,
        flags & RENAME_NOREPLACE != 0,
        flags & RENAME_EXCHANGE != 0,
    )?;
    Ok(0)
}

pub(super) fn fchmodat(cx: &mut Cx<'_>, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(cx, path)?;
    let ino = store_ino(&lookup_at(cx, dirfd, &path, true)?)?;
    cx.vfs()?.store().set_mode(ino, perm)?;
    Ok(0)
}

pub(super) fn fchmod(cx: &mut Cx<'_>, fd: i32, perm: u32) -> SysResult {
    let ino = cx.fds().get(fd)?.borrow().ino().ok_or(Errno::EBADF)?;
    cx.vfs()?.store().set_mode(ino, perm)?;
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
    let node = lookup_at_or_fd(cx, dirfd, &path, flags, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    cx.vfs()?
        .store()
        .set_owner(store_ino(&node)?, owner_arg(uid), owner_arg(gid))?;
    Ok(0)
}

pub(super) fn fchown(cx: &mut Cx<'_>, fd: i32, uid: u32, gid: u32) -> SysResult {
    let ino = cx.fds().get(fd)?.borrow().ino().ok_or(Errno::EBADF)?;
    cx.vfs()?
        .store()
        .set_owner(ino, owner_arg(uid), owner_arg(gid))?;
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
    let ino = if path == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        cx.fds().get(dirfd)?.borrow().ino().ok_or(Errno::EBADF)?
    } else {
        let path = read_path(cx, path)?;
        let node = lookup_at_or_fd(cx, dirfd, &path, flags, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        store_ino(&node)?
    };
    let now = || {
        appine_hal::clock_gettime(0)
            .map(|(s, ns)| s * 1_000_000_000 + ns)
            .map_err(Errno)
    };
    // 只记录 mtime（atime 用 mtime 代替）：times[1] 是 mtime
    let mtime = if times == 0 {
        Some(now()?)
    } else {
        let b: [u8; 32] = get(cx, times)?;
        let word = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        for nsec in [word(1), word(3)] {
            if nsec != UTIME_NOW && nsec != UTIME_OMIT && !(0..1_000_000_000).contains(&nsec) {
                return Err(Errno::EINVAL);
            }
        }
        match word(3) {
            UTIME_OMIT => None,
            UTIME_NOW => Some(now()?),
            nsec => Some(word(2) * 1_000_000_000 + nsec),
        }
    };
    if let Some(t) = mtime {
        cx.vfs()?.store().set_mtime(ino, t)?;
    }
    Ok(0)
}

pub(super) fn truncate(cx: &mut Cx<'_>, path: u64, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let path = read_path(cx, path)?;
    let vfs = cx.vfs()?;
    let ino = store_ino(&vfs.resolve(&cx.view(&vfs), &cx.cwd(), &path, true)?)?;
    match vfs.store().stat(ino)?.kind() {
        mode::S_IFDIR => Err(Errno::EISDIR),
        mode::S_IFREG => vfs.store().truncate(ino, len).map(|()| 0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ftruncate(cx: &mut Cx<'_>, fd: i32, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| Errno::EINVAL)?;
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        FileKind::Regular { file, .. } if f.writable() => {
            check_seals(cx, &f, false, |_| len)?;
            file.set_len(len).map_err(host_errno).map(|()| 0)
        }
        FileKind::Path(_) => Err(Errno::EBADF),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn fallocate(cx: &mut Cx<'_>, fd: i32, mode: u32, offset: i64, len: i64) -> SysResult {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x1;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x2;
    const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
    let (Ok(offset), Ok(len)) = (u64::try_from(offset), u64::try_from(len)) else {
        return Err(Errno::EINVAL);
    };
    let end = offset
        .checked_add(len)
        .filter(|_| len > 0)
        .ok_or(Errno::EINVAL)?;
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
            request_wake();
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

pub(super) fn fsync(cx: &mut Cx<'_>, fd: i32) -> SysResult {
    // 仓库的数据直接写在宿主文件里；为了 apk 这类频繁 fsync 的负载，不逐个刷盘
    cx.fds().get(fd).map(|_| 0)
}

// ---- 目录 ----

pub(super) fn getdents64(cx: &mut Cx<'_>, fd: i32, buf: u64, count: usize) -> SysResult {
    let file = cx.fds().get(fd)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let FileKind::Directory { node, entries } = kind else {
        return Err(Errno::ENOTDIR);
    };
    if entries.is_none() {
        let vfs = cx.vfs()?;
        let view = cx.view(&vfs);
        let ino_of = |n: &Node| match n {
            Node::Store(i) => *i,
            Node::Pseudo(p) => pseudo::ino_of(p),
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
        | FileKind::EventFd(_) => return Err(Errno::ESPIPE),
        FileKind::Proc { data, .. } => match whence {
            SEEK_SET => 0,
            SEEK_CUR => *pos as i64,
            SEEK_END => data.len() as i64,
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
    Local,
}

fn io_path(f: &OpenFile) -> IoPath {
    match &f.kind {
        FileKind::Host(fd) => IoPath::Host(*fd),
        FileKind::Pipe(end) => IoPath::Pipe(end.pipe.clone()),
        FileKind::Pty(end) => IoPath::Pty(end.tty.clone(), end.master),
        FileKind::Socket(s) => IoPath::Socket(s.clone()),
        FileKind::EventFd(e) => IoPath::EventFd(e.clone()),
        _ => IoPath::Local,
    }
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
                request_wake();
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
                request_wake();
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
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        _ => return Err(Errno::EBADF),
    };
    if offset.is_none() && matches!(f.kind, FileKind::Regular { .. } | FileKind::Proc { .. }) {
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
        // 可写的 procfs 文件：接受写入，但不起作用
        FileKind::Proc { .. } => Ok(data.len()),
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
        if !f.readable() {
            return Err(Errno::EBADF);
        }
        (io_path(&f), f.nonblocking())
    };
    match path {
        IoPath::Host(_)
        | IoPath::Pipe(_)
        | IoPath::Pty(..)
        | IoPath::Socket(_)
        | IoPath::EventFd(_)
            if offset.is_some() =>
        {
            Err(Errno::ESPIPE)
        }
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
        IoPath::Local => local_read(&mut file.borrow_mut(), buf, offset),
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
        if !f.writable() {
            return Err(Errno::EBADF);
        }
        (io_path(&f), f.nonblocking())
    };
    match path {
        IoPath::Host(_)
        | IoPath::Pipe(_)
        | IoPath::Pty(..)
        | IoPath::Socket(_)
        | IoPath::EventFd(_)
            if offset.is_some() =>
        {
            Err(Errno::ESPIPE)
        }
        IoPath::Host(fd) => match cx.unlocked(|| appine_hal::write(fd, data)) {
            Ok(n) => Ok(n),
            Err(e) if e == Errno::EPIPE.0 => Err(broken_pipe(cx)),
            Err(e) => Err(Errno(e)),
        },
        IoPath::Pipe(pipe) => pipe_write(cx, &pipe, nonblock, data),
        IoPath::Pty(t, master) => tty::write(cx, &t, master, nonblock, data),
        IoPath::Socket(s) => super::net::send(cx, &s, nonblock, data, &[], &[], 0),
        IoPath::EventFd(e) => super::poll::eventfd_write(cx, &e, nonblock, data),
        IoPath::Local => {
            check_seals(cx, &file.borrow(), true, |size| {
                let f = file.borrow();
                let pos = match offset {
                    Some(off) => off,
                    None if f.flags & oflags::O_APPEND != 0 => size,
                    None => f.pos,
                };
                size.max(pos + data.len() as u64)
            })?;
            local_write(&mut file.borrow_mut(), data, offset)
        }
    }
}

/// 从打开文件读到 guest 缓冲区（read、pread、readv）。
fn read_file(
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
fn write_file(
    cx: &mut Cx<'_>,
    file: &FileRef,
    buf: u64,
    len: usize,
    offset: Option<u64>,
) -> SysResult {
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
fn vectored(
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
    // RWF_HIPRI、RWF_DSYNC、RWF_SYNC 不影响结果；RWF_NOWAIT、RWF_APPEND 不支持
    const RWF_SUPPORTED: u32 = 0x7;
    const RWF_ALL: u32 = 0x3f;
    let offset = match flags {
        Some(f) if f & !RWF_ALL != 0 => return Err(Errno::EINVAL),
        Some(f) if f & !RWF_SUPPORTED != 0 => return Err(Errno::EOPNOTSUPP),
        Some(_) if offset == -1 => None,
        _ => checked_offset(offset)?,
    };
    vectored(cx, fd, iov, count, offset, write)
}
