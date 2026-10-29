//! 扩展属性：setxattr、getxattr、listxattr、removexattr，以及按符号链接本身（`l`）
//! 和按 fd（`f`）的变体。
//!
//! 扩展属性存在 inode 仓库的数据库里（设计文档 §11）。合成文件系统（/proc、/dev/pts）
//! 的节点没有扩展属性，管道、socket 等不在文件系统里的文件不支持。权限按 Linux 的
//! `xattr_permission` 和 commoncap 的钩子检查（[`permission`]）；程序文件的能力
//! `security.capability` 写的时候检查格式，读的时候按 [`FileCaps::xattr_for_reader`] 转换。

use super::fs::{AT_FDCWD, attrib_at, attrib_fd, lookup_at};
use super::{SysResult, inotify, read_path};
use crate::cred::{FileCaps, MAY_READ, MAY_WRITE, XATTR_NAME_CAPS, cap};
use crate::errno::Errno;
use crate::fd::oflags;
use crate::kernel::Cx;
use crate::uaccess::UserSlice;
use crate::vfs::Node;
use crate::vfs::store::{Ino, Stat, mode};

const XATTR_CREATE: u32 = 1;
const XATTR_REPLACE: u32 = 2;
/// 名字的最大长度。
const XATTR_NAME_MAX: usize = 255;
/// 值的最大长度。
const XATTR_SIZE_MAX: usize = 65536;
/// 名字必须带其中一个命名空间前缀。
const NAMESPACES: [&[u8]; 4] = [b"security.", b"system.", b"trusted.", b"user."];

/// 操作的对象。
#[derive(Clone, Copy)]
pub(super) enum Target {
    /// 路径，以及是否跟随最后一个符号链接。
    Path(u64, bool),
    Fd(i32),
}

/// 找到对象的节点；不在文件系统里的文件返回 EOPNOTSUPP。
fn node_of(cx: &Cx<'_>, target: Target) -> Result<Node, Errno> {
    match target {
        Target::Path(path, follow) => {
            let path = read_path(cx, path)?;
            lookup_at(cx, AT_FDCWD, &path, follow)
        }
        Target::Fd(fd) => {
            let file = cx.fds().get(fd)?;
            let f = file.borrow();
            if f.flags & oflags::O_PATH != 0 {
                return Err(Errno::EBADF);
            }
            f.node().ok_or(Errno::EOPNOTSUPP)
        }
    }
}

/// 仓库里的 inode；合成文件系统的节点返回 `pseudo`。
fn store_ino(node: Node, pseudo: Errno) -> Result<Ino, Errno> {
    match node {
        Node::Store(ino) => Ok(ino),
        Node::Pseudo(..) => Err(pseudo),
    }
}

fn read_name(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    let name = cx.mm().read_cstr(addr, XATTR_NAME_MAX).map_err(|e| {
        if e == Errno::ENAMETOOLONG {
            Errno::ERANGE
        } else {
            e
        }
    })?;
    if name.is_empty() {
        return Err(Errno::ERANGE);
    }
    if !NAMESPACES.iter().any(|ns| name.starts_with(ns)) {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(name)
}

/// 读（`write` 为 false）或者写名为 `name` 的扩展属性的权限：
///
/// - `trusted.*` 要 `CAP_SYS_ADMIN`，没有时写是 EPERM，读是 ENODATA（看不到它们）；
/// - `user.*` 只能在普通文件和目录上（不然写是 EPERM，读是 ENODATA），粘滞位目录上只有
///   属主能写，读写都要有文件本身的读写权限；
/// - `security.*` 写要 `CAP_SYS_ADMIN`（`security.capability` 由调用方检查 `CAP_SETFCAP`），
///   读不限；`system.*` 不限。
fn permission(cx: &Cx<'_>, st: &Stat, name: &[u8], write: bool) -> Result<(), Errno> {
    let cred = cx.cred();
    let hidden = if write { Errno::EPERM } else { Errno::ENODATA };
    if name.starts_with(b"trusted.") {
        return if cred.capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(hidden)
        };
    }
    if name.starts_with(b"user.") {
        let dir = st.kind() == mode::S_IFDIR;
        if !dir && st.kind() != mode::S_IFREG {
            return Err(hidden);
        }
        if dir && st.mode & mode::S_ISVTX != 0 && write && !cred.owns(st) {
            return Err(Errno::EPERM);
        }
        return cred.permission(st, if write { MAY_WRITE } else { MAY_READ });
    }
    if write
        && name.starts_with(b"security.")
        && name != XATTR_NAME_CAPS
        && !cred.capable(cap::SYS_ADMIN)
    {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// 把结果写回 guest：`size` 为 0 时只返回长度，缓冲区不够时返回 ERANGE。
fn reply(cx: &Cx<'_>, buf: u64, size: usize, data: &[u8]) -> SysResult {
    if size == 0 {
        return Ok(data.len() as u64);
    }
    if size < data.len() {
        return Err(Errno::ERANGE);
    }
    cx.mm().write(UserSlice::new(buf, data.len()), data)?;
    Ok(data.len() as u64)
}

pub(super) fn setxattr(
    cx: &mut Cx<'_>,
    target: Target,
    name: u64,
    value: u64,
    size: usize,
    flags: u32,
) -> SysResult {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        return Err(Errno::EINVAL);
    }
    let name = read_name(cx, name)?;
    if size > XATTR_SIZE_MAX {
        return Err(Errno::E2BIG);
    }
    let mut v = vec![0u8; size];
    cx.mm().read(UserSlice::new(value, size), &mut v)?;
    let ino = store_ino(node_of(cx, target)?, Errno::EOPNOTSUPP)?;
    let vfs = cx.vfs()?;
    let store = vfs.store();
    // 程序文件的能力：只接受 v2、v3，要有 CAP_SETFCAP（cap_convert_nscap，在别的检查之前；
    // 长度为 0 时 Linux 不检查）
    if name == XATTR_NAME_CAPS && size > 0 {
        if !FileCaps::valid_xattr(&v) {
            return Err(Errno::EINVAL);
        }
        if !cx.cred().capable(cap::SETFCAP) {
            return Err(Errno::EPERM);
        }
    }
    permission(cx, &store.attrs(ino)?, &name, true)?;
    store.set_xattr(
        ino,
        &name,
        &v,
        flags & XATTR_CREATE != 0,
        flags & XATTR_REPLACE != 0,
    )?;
    attrib(cx, target, ino);
    Ok(0)
}

/// 改了扩展属性：报告 inotify 的 IN_ATTRIB。
fn attrib(cx: &Cx<'_>, target: Target, ino: Ino) {
    match target {
        Target::Path(path, follow) if inotify::active(cx.kr()) => {
            if let Ok(path) = read_path(cx, path) {
                attrib_at(cx, ino, AT_FDCWD, &path, follow);
            }
        }
        Target::Path(..) => {}
        Target::Fd(fd) => attrib_fd(cx, fd),
    }
}

pub(super) fn getxattr(
    cx: &mut Cx<'_>,
    target: Target,
    name: u64,
    value: u64,
    size: usize,
) -> SysResult {
    let name = read_name(cx, name)?;
    let ino = store_ino(node_of(cx, target)?, Errno::ENODATA)?;
    let vfs = cx.vfs()?;
    permission(cx, &vfs.store().attrs(ino)?, &name, false)?;
    let mut v = vfs.store().get_xattr(ino, &name)?;
    if name == XATTR_NAME_CAPS {
        v = FileCaps::xattr_for_reader(&v)?;
    }
    reply(cx, value, size, &v)
}

pub(super) fn listxattr(cx: &mut Cx<'_>, target: Target, list: u64, size: usize) -> SysResult {
    let names = match node_of(cx, target)? {
        Node::Store(ino) => cx.vfs()?.store().list_xattr(ino)?,
        Node::Pseudo(..) => Vec::new(),
    };
    // 与 Linux 的 trusted 处理函数一样，没有 CAP_SYS_ADMIN 时不列出 trusted.*
    let trusted = cx.cred().capable(cap::SYS_ADMIN);
    let mut out = Vec::new();
    for n in names
        .into_iter()
        .filter(|n| trusted || !n.starts_with(b"trusted."))
    {
        out.extend_from_slice(&n);
        out.push(0);
    }
    reply(cx, list, size, &out)
}

pub(super) fn removexattr(cx: &mut Cx<'_>, target: Target, name: u64) -> SysResult {
    let name = read_name(cx, name)?;
    let ino = store_ino(node_of(cx, target)?, Errno::EOPNOTSUPP)?;
    let vfs = cx.vfs()?;
    permission(cx, &vfs.store().attrs(ino)?, &name, true)?;
    // 去掉程序文件的能力要 CAP_SETFCAP（cap_inode_removexattr）
    if name == XATTR_NAME_CAPS && !cx.cred().capable(cap::SETFCAP) {
        return Err(Errno::EPERM);
    }
    vfs.store().remove_xattr(ino, &name)?;
    attrib(cx, target, ino);
    Ok(0)
}
