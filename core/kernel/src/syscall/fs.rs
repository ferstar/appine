//! 文件与目录类 syscall。

use std::os::unix::fs::FileExt;

use super::{IO_CHUNK, SysResult, get, put, read_path};
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, oflags};
use crate::process::Process;
use crate::uaccess::UserSlice;
use crate::vfs::store::{DirEntry, Ino, ROOT, Stat, mode};

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_EACCESS: u32 = 0x200;
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
const FD_CLOEXEC: u64 = 1;
const F_UNLCK: i16 = 2;

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;
const SEEK_DATA: u32 = 3;
const SEEK_HOLE: u32 = 4;

/// readv/writev 最多接受的 iovec 个数（`UIO_MAXIOV`）。
const UIO_MAXIOV: usize = 1024;

/// inode 仓库对外报告的设备号。
const ROOT_DEV: u64 = 0x801;

/// 可以透传给宿主终端的 ioctl：编号和参数大小。
const TTY_IOCTLS: [(u32, usize, bool); 6] = [
    (0x5401, 36, true),  // TCGETS
    (0x5402, 36, false), // TCSETS
    (0x5403, 36, false), // TCSETSW
    (0x5404, 36, false), // TCSETSF
    (0x5413, 8, true),   // TIOCGWINSZ
    (0x5414, 8, false),  // TIOCSWINSZ
];

fn host_errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(Errno::EIO.0))
}

/// `*at` 系列的起点目录。
fn start_dir(p: &Process, dirfd: i32, path: &[u8]) -> Result<Ino, Errno> {
    if path.first() == Some(&b'/') {
        return Ok(ROOT);
    }
    if dirfd == AT_FDCWD {
        return Ok(p.cwd);
    }
    let file = p.fds.get(dirfd)?;
    let ino = file.borrow().ino().ok_or(Errno::ENOTDIR)?;
    if !p.vfs()?.store().stat(ino)?.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    Ok(ino)
}

/// 解析 `*at` 系列的路径参数。
fn lookup_at(p: &Process, dirfd: i32, path: &[u8], follow: bool) -> Result<Ino, Errno> {
    let start = start_dir(p, dirfd, path)?;
    p.vfs()?.resolve(start, path, follow)
}

fn split_ns(ns: i64) -> (u64, u64) {
    (
        ns.div_euclid(1_000_000_000) as u64,
        ns.rem_euclid(1_000_000_000) as u64,
    )
}

/// 按 asm-generic 的 `struct stat`（128 字节）编码。
fn encode_stat(st: &Stat) -> [u8; 128] {
    let mut b = [0u8; 128];
    let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    let (msec, mnsec) = split_ns(st.mtime_ns);
    let (csec, cnsec) = split_ns(st.ctime_ns);
    put(0, &ROOT_DEV.to_le_bytes());
    put(8, &st.ino.to_le_bytes());
    put(16, &st.mode.to_le_bytes());
    put(20, &st.nlink.to_le_bytes());
    put(24, &st.uid.to_le_bytes());
    put(28, &st.gid.to_le_bytes());
    put(32, &st.rdev.to_le_bytes());
    // 目录在仓库里没有数据，按 ext4 的习惯报告一个块
    let size = if st.is_dir() { 4096 } else { st.size };
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

/// 宿主 fd 的元数据。
fn host_stat(fd: i32) -> Result<[u8; 128], Errno> {
    let s = appine_hal::fstat(fd).map_err(Errno)?;
    let mut b = encode_stat(&Stat {
        ino: s.st_ino as u64,
        mode: s.st_mode as u32,
        nlink: s.st_nlink as u32,
        uid: s.st_uid,
        gid: s.st_gid,
        rdev: s.st_rdev as u64,
        size: s.st_size as u64,
        mtime_ns: s.st_mtime as i64 * 1_000_000_000 + s.st_mtime_nsec as i64,
        ctime_ns: s.st_ctime as i64 * 1_000_000_000 + s.st_ctime_nsec as i64,
    });
    b[..8].copy_from_slice(&(s.st_dev as u64).to_le_bytes());
    Ok(b)
}

fn file_stat(p: &Process, file: &FileRef) -> Result<[u8; 128], Errno> {
    match &file.borrow().kind {
        FileKind::Host(fd) => host_stat(*fd),
        f => {
            let ino = match f {
                FileKind::Regular { ino, .. }
                | FileKind::Directory { ino, .. }
                | FileKind::Path(ino) => *ino,
                FileKind::Host(_) => unreachable!(),
            };
            Ok(encode_stat(&p.vfs()?.store().stat(ino)?))
        }
    }
}

pub(super) fn openat(p: &mut Process, dirfd: i32, path: u64, flags: u32, perm: u32) -> SysResult {
    let path = read_path(p, path)?;
    let vfs = p.vfs()?.clone();
    let store = vfs.store();
    let start = start_dir(p, dirfd, &path)?;
    let create = flags & oflags::O_CREAT != 0;
    let excl = create && flags & oflags::O_EXCL != 0;
    let follow = flags & oflags::O_NOFOLLOW == 0 && !excl;

    let (ino, created) = match vfs.resolve(start, &path, follow) {
        Ok(_) if excl => return Err(Errno::EEXIST),
        Ok(ino) => (ino, None),
        Err(e) if e == Errno::ENOENT && create => {
            let (dir, name) = vfs.resolve_parent(start, &path)?;
            if path.ends_with(b"/") || name == b"." || name == b".." {
                return Err(Errno::EISDIR);
            }
            let (ino, file) = store.create_file(dir, name, perm & !p.umask & 0o7777, 0, 0)?;
            (ino, Some(file))
        }
        Err(e) => return Err(e),
    };

    let accmode = flags & oflags::O_ACCMODE;
    let st = store.stat(ino)?;
    let kind = if flags & oflags::O_PATH != 0 {
        if flags & oflags::O_DIRECTORY != 0 && !st.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        FileKind::Path(ino)
    } else {
        match st.kind() {
            mode::S_IFLNK => return Err(Errno::ELOOP),
            mode::S_IFDIR => {
                if accmode != oflags::O_RDONLY || create {
                    return Err(Errno::EISDIR);
                }
                FileKind::Directory { ino, entries: None }
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
            // 设备节点、FIFO、socket 文件随 devfs 和管道一起实现
            _ => return Err(Errno::ENXIO),
        }
    };
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK | oflags::O_PATH;
    let file = OpenFile::new(kind, flags & keep);
    p.fds
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

pub(super) fn close(p: &mut Process, fd: i32) -> SysResult {
    p.fds.close(fd).map(|()| 0)
}

pub(super) fn dup(p: &mut Process, fd: i32) -> SysResult {
    let file = p.fds.get(fd)?;
    p.fds.install(file, false, 0).map(|fd| fd as u64)
}

pub(super) fn dup3(p: &mut Process, old: i32, new: i32, flags: u32) -> SysResult {
    if flags & !oflags::O_CLOEXEC != 0 || old == new {
        return Err(Errno::EINVAL);
    }
    let file = p.fds.get(old)?;
    p.fds.install_at(new, file, flags != 0).map(|()| new as u64)
}

pub(super) fn fcntl(p: &mut Process, fd: i32, cmd: u32, arg: u64) -> SysResult {
    let file = p.fds.get(fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let min = usize::try_from(arg as i32).map_err(|_| Errno::EINVAL)?;
            p.fds
                .install(file, cmd == F_DUPFD_CLOEXEC, min)
                .map(|fd| fd as u64)
        }
        F_GETFD => Ok(p.fds.cloexec(fd)? as u64),
        F_SETFD => p.fds.set_cloexec(fd, arg & FD_CLOEXEC != 0).map(|()| 0),
        F_GETFL => Ok((file.borrow().flags | oflags::O_LARGEFILE) as u64),
        F_SETFL => {
            let mut f = file.borrow_mut();
            f.flags = (f.flags & !oflags::SETFL_MASK) | (arg as u32 & oflags::SETFL_MASK);
            Ok(0)
        }
        // 单进程里记录锁不会冲突：F_GETLK 总是报告没有锁
        F_GETLK => put(p, arg, F_UNLCK.to_le_bytes()).map(|()| 0),
        F_SETLK | F_SETLKW => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ioctl(p: &mut Process, fd: i32, request: u32, arg: u64) -> SysResult {
    let file = p.fds.get(fd)?;
    let FileKind::Host(host_fd) = file.borrow().kind else {
        return Err(Errno::ENOTTY);
    };
    let Some(&(_, size, out)) = TTY_IOCTLS.iter().find(|(r, ..)| *r == request) else {
        return Err(Errno::ENOTTY);
    };
    let mut buf = vec![0u8; size];
    if !out {
        p.mm.read(UserSlice::new(arg, size), &mut buf)?;
    }
    let rc = appine_hal::ioctl(host_fd, request as u64, &mut buf).map_err(Errno)?;
    if out {
        p.mm.write(UserSlice::new(arg, size), &buf)?;
    }
    Ok(rc as u64)
}

pub(super) fn faccessat(
    p: &mut Process,
    dirfd: i32,
    path: u64,
    amode: u32,
    flags: u32,
) -> SysResult {
    if amode & !7 != 0 || flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(p, path)?;
    let st = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        let file = p.fds.get(dirfd)?;
        let ino = file.borrow().ino().ok_or(Errno::EBADF)?;
        p.vfs()?.store().stat(ino)?
    } else {
        let ino = lookup_at(p, dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        p.vfs()?.store().stat(ino)?
    };
    // guest 以 root 运行：读写总是允许，执行要求至少一个 x 位（目录除外）
    if amode & 1 != 0 && !st.is_dir() && st.mode & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(0)
}

pub(super) fn chdir(p: &mut Process, path: u64) -> SysResult {
    let path = read_path(p, path)?;
    let ino = p.vfs()?.resolve(p.cwd, &path, true)?;
    if !p.vfs()?.store().stat(ino)?.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    p.cwd = ino;
    Ok(0)
}

pub(super) fn fchdir(p: &mut Process, fd: i32) -> SysResult {
    let ino = p.fds.get(fd)?.borrow().ino().ok_or(Errno::ENOTDIR)?;
    if !p.vfs()?.store().stat(ino)?.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    p.cwd = ino;
    Ok(0)
}

pub(super) fn getcwd(p: &mut Process, buf: u64, size: u64) -> SysResult {
    let mut path = p.vfs()?.path_of(p.cwd)?;
    path.push(0);
    if (size as usize) < path.len() {
        return Err(Errno::ERANGE);
    }
    p.mm.write(UserSlice::new(buf, path.len()), &path)?;
    Ok(path.len() as u64)
}

pub(super) fn mkdirat(p: &mut Process, dirfd: i32, path: u64, perm: u32) -> SysResult {
    let path = read_path(p, path)?;
    let start = start_dir(p, dirfd, &path)?;
    let vfs = p.vfs()?;
    let (dir, name) = vfs.resolve_parent(start, &path)?;
    if name == b"." || name == b".." {
        return Err(Errno::EEXIST);
    }
    vfs.store()
        .mkdir(dir, name, perm & !p.umask & 0o7777, 0, 0)?;
    Ok(0)
}

pub(super) fn readlinkat(
    p: &mut Process,
    dirfd: i32,
    path: u64,
    buf: u64,
    size: usize,
) -> SysResult {
    if size as isize <= 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(p, path)?;
    let ino = lookup_at(p, dirfd, &path, false)?;
    let target = p.vfs()?.store().readlink(ino)?;
    let n = target.len().min(size);
    p.mm.write(UserSlice::new(buf, n), &target[..n])?;
    Ok(n as u64)
}

pub(super) fn newfstatat(
    p: &mut Process,
    dirfd: i32,
    path: u64,
    buf: u64,
    flags: u32,
) -> SysResult {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(p, path)?;
    let st = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            encode_stat(&p.vfs()?.store().stat(p.cwd)?)
        } else {
            file_stat(p, &p.fds.get(dirfd)?)?
        }
    } else {
        let ino = lookup_at(p, dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        encode_stat(&p.vfs()?.store().stat(ino)?)
    };
    put(p, buf, st).map(|()| 0)
}

pub(super) fn fstat(p: &mut Process, fd: i32, buf: u64) -> SysResult {
    let st = file_stat(p, &p.fds.get(fd)?)?;
    put(p, buf, st).map(|()| 0)
}

pub(super) fn getdents64(p: &mut Process, fd: i32, buf: u64, count: usize) -> SysResult {
    let file = p.fds.get(fd)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let FileKind::Directory { ino, entries } = kind else {
        return Err(Errno::ENOTDIR);
    };
    if entries.is_none() {
        let store = p.vfs()?.store();
        let mut list = vec![
            DirEntry {
                name: b".".to_vec(),
                ino: *ino,
                mode: mode::S_IFDIR,
            },
            DirEntry {
                name: b"..".to_vec(),
                ino: store.parent(*ino)?,
                mode: mode::S_IFDIR,
            },
        ];
        list.extend(store.readdir(*ino)?);
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
    p.mm.write(UserSlice::new(buf, out.len()), &out)?;
    *pos = next as u64;
    Ok(out.len() as u64)
}

pub(super) fn lseek(p: &mut Process, fd: i32, offset: i64, whence: u32) -> SysResult {
    let file = p.fds.get(fd)?;
    let mut f = file.borrow_mut();
    let OpenFile { kind, pos, .. } = &mut *f;
    let base = match kind {
        FileKind::Host(fd) => return appine_hal::lseek(*fd, offset, whence as i32).map_err(Errno),
        FileKind::Path(_) => return Err(Errno::EBADF),
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

/// 对打开文件做一次读，读进宿主缓冲区。`offset` 为 None 时使用并推进文件位置。
fn file_read(f: &mut OpenFile, buf: &mut [u8], offset: Option<u64>) -> Result<usize, Errno> {
    if !f.readable() {
        return Err(Errno::EBADF);
    }
    let pos = offset.unwrap_or(f.pos);
    let n = match &f.kind {
        FileKind::Host(_) if offset.is_some() => return Err(Errno::ESPIPE),
        FileKind::Host(fd) => return appine_hal::read(*fd, buf).map_err(Errno),
        FileKind::Regular { file, .. } => file.read_at(buf, pos).map_err(host_errno)?,
        FileKind::Directory { .. } => return Err(Errno::EISDIR),
        FileKind::Path(_) => return Err(Errno::EBADF),
    };
    if offset.is_none() {
        f.pos = pos + n as u64;
    }
    Ok(n)
}

/// 对打开文件做一次写。`offset` 为 None 时使用并推进文件位置，`O_APPEND` 时先移到文件末尾。
fn file_write(f: &mut OpenFile, data: &[u8], offset: Option<u64>) -> Result<usize, Errno> {
    if !f.writable() {
        return Err(Errno::EBADF);
    }
    let n = match &f.kind {
        FileKind::Host(_) if offset.is_some() => return Err(Errno::ESPIPE),
        FileKind::Host(fd) => return appine_hal::write(*fd, data).map_err(Errno),
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
            n
        }
        FileKind::Directory { .. } | FileKind::Path(_) => return Err(Errno::EBADF),
    };
    Ok(n)
}

/// 从打开文件读到 guest 缓冲区（read、pread）。
fn read_file(p: &Process, file: &FileRef, buf: u64, len: usize, offset: Option<u64>) -> SysResult {
    let mut f = file.borrow_mut();
    // 终端每次 read 只返回已经就绪的数据，不能为了凑满缓冲区再次阻塞
    let fill = matches!(f.kind, FileKind::Regular { .. });
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let k = match file_read(&mut f, &mut chunk[..n], offset.map(|o| o + done as u64)) {
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        if let Err(e) = p.mm.write(
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

/// 把 guest 缓冲区写进打开文件（write、pwrite）。
fn write_file(p: &Process, file: &FileRef, buf: u64, len: usize, offset: Option<u64>) -> SysResult {
    let mut f = file.borrow_mut();
    if !f.writable() {
        return Err(Errno::EBADF);
    }
    let mut chunk = vec![0u8; len.min(IO_CHUNK)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(IO_CHUNK);
        let step =
            p.mm.read(
                UserSlice::new(buf.wrapping_add(done as u64), n),
                &mut chunk[..n],
            )
            .and_then(|()| file_write(&mut f, &chunk[..n], offset.map(|o| o + done as u64)));
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
    p: &mut Process,
    out_fd: i32,
    in_fd: i32,
    offset: u64,
    count: usize,
) -> SysResult {
    let input = p.fds.get(in_fd)?;
    let output = p.fds.get(out_fd)?;
    if !matches!(input.borrow().kind, FileKind::Regular { .. }) {
        return Err(Errno::EINVAL);
    }
    let mut off = match offset {
        0 => None,
        addr => Some(u64::try_from(i64::from_le_bytes(get(p, addr)?)).map_err(|_| Errno::EINVAL)?),
    };
    let mut chunk = vec![0u8; count.min(IO_CHUNK)];
    let mut done = 0;
    while done < count {
        let n = (count - done).min(IO_CHUNK);
        // 输入和输出可能是同一个打开文件，所以分两次借用
        let got = match file_read(&mut input.borrow_mut(), &mut chunk[..n], off) {
            Ok(0) => break,
            Ok(k) => k,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        let wrote = match file_write(&mut output.borrow_mut(), &chunk[..got], None) {
            Ok(k) => k,
            Err(_) if done > 0 => 0,
            Err(e) => return Err(e),
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
        put(p, offset, o.to_le_bytes())?;
    }
    Ok(done as u64)
}

pub(super) fn read(p: &mut Process, fd: i32, buf: u64, len: usize) -> SysResult {
    read_file(p, &p.fds.get(fd)?, buf, len, None)
}

pub(super) fn write(p: &mut Process, fd: i32, buf: u64, len: usize) -> SysResult {
    write_file(p, &p.fds.get(fd)?, buf, len, None)
}

fn checked_offset(offset: i64) -> Result<Option<u64>, Errno> {
    u64::try_from(offset).map(Some).map_err(|_| Errno::EINVAL)
}

pub(super) fn pread(p: &mut Process, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    read_file(p, &p.fds.get(fd)?, buf, len, checked_offset(offset)?)
}

pub(super) fn pwrite(p: &mut Process, fd: i32, buf: u64, len: usize, offset: i64) -> SysResult {
    write_file(p, &p.fds.get(fd)?, buf, len, checked_offset(offset)?)
}

/// 读取 iovec 数组。
fn iovecs(p: &Process, iov: u64, count: usize) -> Result<Vec<(u64, usize)>, Errno> {
    if count > UIO_MAXIOV {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(count);
    let mut total: usize = 0;
    for i in 0..count as u64 {
        let raw: [u8; 16] = get(p, iov.wrapping_add(i * 16))?;
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

pub(super) fn readv(p: &mut Process, fd: i32, iov: u64, count: usize) -> SysResult {
    let file = p.fds.get(fd)?;
    let mut total = 0;
    for (base, len) in iovecs(p, iov, count)? {
        match read_file(p, &file, base, len, None) {
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

pub(super) fn writev(p: &mut Process, fd: i32, iov: u64, count: usize) -> SysResult {
    let file = p.fds.get(fd)?;
    let mut total = 0;
    for (base, len) in iovecs(p, iov, count)? {
        match write_file(p, &file, base, len, None) {
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

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLNVAL: i16 = 0x020;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;

pub(super) fn ppoll(p: &mut Process, fds: u64, nfds: usize, timeout: u64) -> SysResult {
    if nfds > p.rlimits[crate::process::RLIMIT_NOFILE as usize].0 as usize {
        return Err(Errno::EINVAL);
    }
    let timeout_ms = match timeout {
        0 => -1,
        addr => {
            let b: [u8; 16] = get(p, addr)?;
            let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
            let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
            if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
                return Err(Errno::EINVAL);
            }
            sec.saturating_mul(1000)
                .saturating_add((nsec + 999_999) / 1_000_000)
                .min(i32::MAX as i64) as i32
        }
    };
    let mut raw = vec![0u8; nfds * 8];
    p.mm.read(UserSlice::new(fds, raw.len()), &mut raw)?;
    // struct pollfd：fd、events、revents
    let mut entries: Vec<(i32, i16, i16)> = raw
        .chunks_exact(8)
        .map(|e| {
            (
                i32::from_le_bytes(e[..4].try_into().unwrap()),
                i16::from_le_bytes([e[4], e[5]]),
                0,
            )
        })
        .collect();
    // 仓库里的文件总是就绪；宿主 fd 交给宿主 poll，已经有就绪项时不再等待
    let mut host = Vec::new();
    for (i, (fd, events, revents)) in entries.iter_mut().enumerate() {
        if *fd < 0 {
            continue;
        }
        match p.fds.get(*fd) {
            Err(_) => *revents = POLLNVAL,
            Ok(file) => match file.borrow().kind {
                FileKind::Host(host_fd) => host.push((i, host_fd)),
                _ => *revents = *events & (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM),
            },
        }
    }
    let ready_now = entries.iter().any(|e| e.2 != 0);
    if !host.is_empty() || !ready_now {
        let mut polled: Vec<(i32, i16, i16)> =
            host.iter().map(|&(i, fd)| (fd, entries[i].1, 0)).collect();
        appine_hal::poll(&mut polled, if ready_now { 0 } else { timeout_ms }).map_err(Errno)?;
        for (&(i, _), &(_, _, revents)) in host.iter().zip(&polled) {
            entries[i].2 = revents;
        }
    }
    for (e, out) in entries.iter().zip(raw.chunks_exact_mut(8)) {
        out[6..8].copy_from_slice(&e.2.to_le_bytes());
    }
    p.mm.write(UserSlice::new(fds, raw.len()), &raw)?;
    Ok(entries.iter().filter(|e| e.2 != 0).count() as u64)
}
