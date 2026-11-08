//! Extended attributes: setxattr, getxattr, listxattr, removexattr, and the variants that act on
//! the symlink itself (`l`) and on an fd (`f`).
//!
//! Extended attributes live in the inode store's database (the kernel/vfs map). Nodes of
//! synthetic filesystems (/proc, /dev/pts) have no extended attributes, and files not in a
//! filesystem, such as pipes and sockets, do not support them. Permissions are checked as in
//! Linux's `xattr_permission` and commoncap's hooks ([`permission`]); program file capabilities,
//! `security.capability`, are format-checked on write and converted on read by
//! [`FileCaps::xattr_for_reader`].

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
/// Maximum name length.
const XATTR_NAME_MAX: usize = 255;
/// Maximum value length.
const XATTR_SIZE_MAX: usize = 65536;
/// Names must carry one of these namespace prefixes.
const NAMESPACES: [&[u8]; 4] = [b"security.", b"system.", b"trusted.", b"user."];

/// The object operated on.
#[derive(Clone, Copy)]
pub(super) enum Target {
    /// A path, and whether to follow the last symlink.
    Path(u64, bool),
    Fd(i32),
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
    let ino = writable_ino(cx, node_of(cx, target)?, &name)?;
    let vfs = cx.vfs()?;
    let store = vfs.store();
    // Program file capabilities: only v2 and v3 are accepted, CAP_SETFCAP is needed over the file
    // (in the writer's namespace, with its owner and group mapped there), and the value is stored
    // with the root of the writer's namespace (cap_convert_nscap, before the other checks; Linux
    // does not check when the length is 0)
    if name == XATTR_NAME_CAPS && size > 0 {
        if !FileCaps::valid_xattr(&v) {
            return Err(Errno::EINVAL);
        }
        let cred = cx.cred();
        if !cred.capable_wrt(&store.attrs(ino)?, cap::SETFCAP) {
            return Err(Errno::EPERM);
        }
        v = FileCaps::xattr_to_store(&v, &cred)?;
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

pub(super) fn getxattr(
    cx: &mut Cx<'_>,
    target: Target,
    name: u64,
    value: u64,
    size: usize,
) -> SysResult {
    let name = read_name(cx, name)?;
    let ino = readable_ino(cx, node_of(cx, target)?, &name)?;
    let vfs = cx.vfs()?;
    permission(cx, &vfs.store().attrs(ino)?, &name, false)?;
    let mut v = vfs.store().get_xattr(ino, &name)?;
    if name == XATTR_NAME_CAPS {
        v = FileCaps::xattr_for_reader(&v, &cx.cred())?;
    }
    reply(cx, value, size, &v)
}

pub(super) fn listxattr(cx: &mut Cx<'_>, target: Target, list: u64, size: usize) -> SysResult {
    let names = match node_of(cx, target)? {
        Node::Store(_, ino) => cx.vfs()?.store().list_xattr(ino)?,
        Node::Pseudo(..) => Vec::new(),
        // The overlay's own attributes are not listed (Linux's ovl_listxattr)
        Node::Overlay(_, o) => {
            let vfs = cx.vfs()?;
            let ovl = vfs.overlay();
            let mut names = vfs.store().list_xattr(ovl.node(o).real())?;
            names.retain(|n| !ovl.is_private_xattr(o, n));
            names
        }
    };
    // As in Linux's trusted handler, trusted.* is not listed without CAP_SYS_ADMIN
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
    let ino = writable_ino(cx, node_of(cx, target)?, &name)?;
    let vfs = cx.vfs()?;
    permission(cx, &vfs.store().attrs(ino)?, &name, true)?;
    // Removing program file capabilities needs CAP_SETFCAP over the file (cap_inode_removexattr)
    if name == XATTR_NAME_CAPS
        && !cx
            .cred()
            .capable_wrt(&vfs.store().attrs(ino)?, cap::SETFCAP)
    {
        return Err(Errno::EPERM);
    }
    vfs.store().remove_xattr(ino, &name)?;
    attrib(cx, target, ino);
    Ok(0)
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

/// Finds the object's node; EOPNOTSUPP for files not in a filesystem.
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

/// The inode whose extended attribute `name` is about to change: EROFS on a read-only mount
/// (Linux's `mnt_want_write`, before the other checks). In an overlay the attribute changes in
/// the upper layer, after a copy-up that needs the permission to change it on what the overlay
/// shows; the overlay's own attributes cannot be changed (EOPNOTSUPP, 6.1's
/// `ovl_own_xattr_set`).
fn writable_ino(cx: &Cx<'_>, node: Node, name: &[u8]) -> Result<Ino, Errno> {
    let vfs = cx.vfs()?;
    if vfs.readonly(&node) {
        return Err(Errno::EROFS);
    }
    if let Node::Overlay(_, o) = node {
        let real = {
            let ovl = vfs.overlay();
            if ovl.is_private_xattr(o, name) {
                return Err(Errno::EOPNOTSUPP);
            }
            ovl.node(o).real()
        };
        permission(cx, &vfs.store().attrs(real)?, name, true)?;
        return vfs.overlay_upper(o, crate::vfs::overlay::CopyUp::All);
    }
    store_ino(node, Errno::EOPNOTSUPP)
}

/// The inode in the store; `pseudo` for nodes of synthetic filesystems.
fn store_ino(node: Node, pseudo: Errno) -> Result<Ino, Errno> {
    node.ino().ok_or(pseudo)
}

/// The inode whose attribute `name` is read: a store node's own, or the one behind an overlay
/// node, whose overlay keeps the attributes with its private prefix to itself (EOPNOTSUPP, as
/// 6.1's `ovl_own_xattr_get`); ENODATA for nodes of synthetic filesystems.
fn readable_ino(cx: &Cx<'_>, node: Node, name: &[u8]) -> Result<Ino, Errno> {
    match node {
        Node::Overlay(_, o) => {
            let vfs = cx.vfs()?;
            let ovl = vfs.overlay();
            if ovl.is_private_xattr(o, name) {
                return Err(Errno::EOPNOTSUPP);
            }
            Ok(ovl.node(o).real())
        }
        node => store_ino(node, Errno::ENODATA),
    }
}

/// Permission to read (`write` false) or write the extended attribute named `name`:
///
/// - `trusted.*` needs `CAP_SYS_ADMIN`; without it writes are EPERM and reads ENODATA (they are
///   invisible);
/// - `user.*` only on regular files and directories (otherwise writes are EPERM and reads
///   ENODATA); on a sticky directory only the owner may write; reads and writes need read and
///   write permission, respectively, on the file itself;
/// - `security.*` writes need `CAP_SYS_ADMIN` (the caller checks `CAP_SETFCAP` for
///   `security.capability`), reads are unrestricted; `system.*` is unrestricted.
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

/// An extended attribute changed: report inotify's IN_ATTRIB.
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

/// Writes the result back to the guest: with `size` 0 only the length is returned, and ERANGE if
/// the buffer is too small.
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
