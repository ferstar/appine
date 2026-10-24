//! 扩展属性：setxattr、getxattr、listxattr、removexattr，以及按符号链接本身（`l`）
//! 和按 fd（`f`）的变体。
//!
//! 扩展属性存在 inode 仓库的数据库里（设计文档 §11）。合成文件系统（/proc、/dev/pts）
//! 的节点没有扩展属性，管道、socket 等不在文件系统里的文件不支持。

use super::fs::{AT_FDCWD, lookup_at};
use super::{SysResult, read_path};
use crate::errno::Errno;
use crate::fd::oflags;
use crate::kernel::Cx;
use crate::uaccess::UserSlice;
use crate::vfs::Node;
use crate::vfs::store::{Ino, mode};

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
        Node::Pseudo(_) => Err(pseudo),
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
    // 与 Linux 相同，user.* 只能加在普通文件和目录上
    if name.starts_with(b"user.") && !matches!(store.kind(ino)?, mode::S_IFREG | mode::S_IFDIR) {
        return Err(Errno::EPERM);
    }
    store.set_xattr(
        ino,
        &name,
        &v,
        flags & XATTR_CREATE != 0,
        flags & XATTR_REPLACE != 0,
    )?;
    Ok(0)
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
    let v = cx.vfs()?.store().get_xattr(ino, &name)?;
    reply(cx, value, size, &v)
}

pub(super) fn listxattr(cx: &mut Cx<'_>, target: Target, list: u64, size: usize) -> SysResult {
    let names = match node_of(cx, target)? {
        Node::Store(ino) => cx.vfs()?.store().list_xattr(ino)?,
        Node::Pseudo(_) => Vec::new(),
    };
    let mut out = Vec::new();
    for n in names {
        out.extend_from_slice(&n);
        out.push(0);
    }
    reply(cx, list, size, &out)
}

pub(super) fn removexattr(cx: &mut Cx<'_>, target: Target, name: u64) -> SysResult {
    let name = read_name(cx, name)?;
    let ino = store_ino(node_of(cx, target)?, Errno::EOPNOTSUPP)?;
    cx.vfs()?.store().remove_xattr(ino, &name).map(|()| 0)
}
