//! mount(2)、umount2(2)、pivot_root(2)（设计文档 §8.2）。
//!
//! 合成文件系统（procfs、sysfs、devpts、cgroup2）可以挂到任意目录上；仓库里的和合成文件
//! 系统里的目录、文件都可以 bind 挂载（`MS_REC` 带上挂在里面的挂载），挂载的标志、重新
//! 挂载、`MS_MOVE`、卸下（`MNT_DETACH`、`MNT_EXPIRE`）、pivot_root 和各种错误按 Linux 6.1
//! 的 `path_mount`、`path_umount`、`pivot_root`。传播类型可以设置，但不模拟：挂载之间没有
//! 传播，都相当于 private。别的文件系统（tmpfs 等）与没有编进内核的一样是 ENODEV。

use std::collections::HashSet;

use super::fs::{AT_FDCWD, lookup_at, start_dir};
use super::{SysResult, get, read_path};
use crate::cred::cap;
use crate::errno::Errno;
use crate::kernel::Cx;
use crate::vfs::{Dentry, Fs, Mount, MountId, Node, Pseudo, PseudoNode, Vfs, ms};

const MS_REMOUNT: u64 = 0x20;
const MS_BIND: u64 = 0x1000;
const MS_MOVE: u64 = 0x2000;
const MS_REC: u64 = 0x4000;
const MS_SILENT: u64 = 0x8000;
const MS_UNBINDABLE: u64 = 1 << 17;
const MS_PRIVATE: u64 = 1 << 18;
const MS_SLAVE: u64 = 1 << 19;
const MS_SHARED: u64 = 1 << 20;
const MS_STRICTATIME: u64 = 1 << 24;
const MS_NOUSER: u64 = 1 << 31;
const MS_MGC_MSK: u64 = 0xffff_0000;
const MS_MGC_VAL: u64 = 0xc0ed_0000;
const MS_PROPAGATION: u64 = MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE;
/// 访问时间的几个标志：重新挂载时一个也没给就保持原来的。
const ATIME: u64 = ms::NOATIME | ms::NODIRATIME | ms::RELATIME;

const MNT_FORCE: u32 = 1;
const MNT_DETACH: u32 = 2;
const MNT_EXPIRE: u32 = 4;
const UMOUNT_NOFOLLOW: u32 = 8;

pub(super) fn mount(
    cx: &mut Cx<'_>,
    source: u64,
    target: u64,
    fstype: u64,
    flags: u64,
    data: u64,
) -> SysResult {
    // 与 Linux 的 ksys_mount 一样：先复制参数、解析挂载点，再看标志和权限。data 里的选项
    // （devpts 的 mode、gid，proc 的 hidepid 等）不看，只要求能读
    let fstype = optional_str(cx, fstype)?;
    let source = optional_str(cx, source)?;
    if data != 0 {
        get::<1>(cx, data)?;
    }
    let target = read_path(cx, target)?;
    let (node, at) = {
        let vfs = cx.vfs()?;
        let (node, at) = vfs.walk(
            &cx.view(&vfs),
            &cx.cred(),
            &cx.root(),
            &cx.cwd(),
            &target,
            true,
        )?;
        // 挂载点上已经挂着东西时挂在最上面的那个上（Linux 的 lock_mount）：`.`、当前目录
        // 这类不经过查找的路径也一样
        let top = vfs.mounted(node.clone());
        let at = at.filter(|_| top == node);
        (top, at)
    };
    let mut flags = flags;
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    if flags & MS_NOUSER != 0 {
        return Err(Errno::EINVAL);
    }
    if !cx.cred().capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    // 挂载的标志，默认 relatime
    let mut mnt = flags & ms::PER_MOUNT & !ms::RELATIME;
    if flags & ms::NOATIME == 0 {
        mnt |= ms::RELATIME;
    }
    if flags & MS_STRICTATIME != 0 {
        mnt &= !(ms::RELATIME | ms::NOATIME);
    }
    if flags & MS_REMOUNT != 0 {
        let keep_atime = flags & (ATIME | MS_STRICTATIME) == 0;
        return remount(cx, &node, mnt, keep_atime);
    }
    if flags & MS_BIND != 0 {
        return bind(cx, source.as_deref(), (node, at), flags & MS_REC != 0);
    }
    if flags & MS_PROPAGATION != 0 {
        return change_type(cx, &node, flags);
    }
    if flags & MS_MOVE != 0 {
        return move_mount(cx, source.as_deref(), (node, at));
    }
    new_mount(cx, fstype, source, (node, at), mnt)
}

/// mount(2) 的字符串参数，空指针是 None。
fn optional_str(cx: &Cx<'_>, addr: u64) -> Result<Option<Vec<u8>>, Errno> {
    if addr == 0 {
        return Ok(None);
    }
    read_path(cx, addr).map(Some)
}

/// 挂上一个新的文件系统（Linux 的 `do_new_mount`）。
fn new_mount(
    cx: &mut Cx<'_>,
    fstype: Option<Vec<u8>>,
    source: Option<Vec<u8>>,
    (target, at): (Node, Option<Dentry>),
    flags: u64,
) -> SysResult {
    let fstype = fstype.ok_or(Errno::EINVAL)?;
    let Some(fs) = Fs::from_type(&fstype) else {
        let name = String::from_utf8_lossy(&fstype);
        crate::debug::log(format_args!("mount: no filesystem type {name}"));
        return Err(Errno::ENODEV);
    };
    // cgroup2 只能只读（util-linux、busybox 的 mount 得到 EROFS 会改成只读再试）
    if fs == Fs::Cgroup && flags & ms::RDONLY == 0 {
        return Err(Errno::EROFS);
    }
    let vfs = cx.vfs()?;
    may_mount_on(&target)?;
    // 只有一个实例的文件系统不能再挂在它自己的挂载的根上（Linux 的 do_add_mount）
    if !fs.new_instance()
        && let Some(id) = vfs.mount_root(&target)
        && vfs.mount(id).is_some_and(|m| m.fs == fs)
    {
        return Err(Errno::EBUSY);
    }
    if !vfs.is_dir(&cx.view(&vfs), &target)? {
        return Err(Errno::ENOTDIR);
    }
    let dev = (!fs.own_dev()).then(|| fs.dev());
    vfs.add_mount(
        fs,
        fs.root(),
        (target, at),
        flags,
        source.unwrap_or_default(),
        dev,
    );
    cx.shared.fast_changed();
    Ok(0)
}

/// bind 挂载（Linux 的 `do_loopback`）：新挂载看到源的那个目录或文件，标志、设备号与源
/// 所在的挂载相同。
fn bind(
    cx: &mut Cx<'_>,
    source: Option<&[u8]>,
    (target, at): (Node, Option<Dentry>),
    recursive: bool,
) -> SysResult {
    let source = source.filter(|s| !s.is_empty()).ok_or(Errno::EINVAL)?;
    let start = start_dir(cx, AT_FDCWD, source)?;
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    let (src, found) = vfs.walk(&view, &cx.cred(), &cx.root(), &start, source, true)?;
    may_mount_on(&target)?;
    let dir = vfs.is_dir(&view, &src)?;
    if dir != vfs.is_dir(&view, &target)? {
        return Err(Errno::ENOTDIR);
    }
    // 卸下了的挂载里的不能 bind（Linux 的 check_mnt）
    let m = vfs.mount(src.mnt()).ok_or(Errno::EINVAL)?;
    // 挂上之前先记下源里面的挂载：目标在源里面时不会把新挂载自己也复制一遍
    let inside = if recursive { vfs.mounts() } else { Vec::new() };
    let id = vfs.add_mount(
        m.fs,
        src.clone(),
        (target, at),
        m.flags,
        m.source,
        Some(m.dev),
    );
    if !dir {
        vfs.set_root_at(id, found);
    }
    copy_submounts(&vfs, &view, &inside, &src, id);
    cx.shared.fast_changed();
    Ok(0)
}

/// 挂载点不能是 ENOENT 的那些（Linux 的 `lock_mount`）：经由 `/proc/<pid>/fd/<n>` 到了已经
/// 删除的文件、管道这类没有名字的打开文件；`/proc/<pid>/fd`、`fdinfo` 里的项本身（经由
/// 指向它的 `O_PATH` fd 到达，Linux 6.12 起不能当挂载点，LTP 的 mount08）。
fn may_mount_on(target: &Node) -> Result<(), Errno> {
    match target {
        Node::Pseudo(_, PseudoNode::OpenFd(..) | PseudoNode::Fd(..) | PseudoNode::FdInfo(..)) => {
            Err(Errno::ENOENT)
        }
        _ => Ok(()),
    }
}

/// 把 `root` 所在的挂载里、`root` 下面的挂载（`snapshot` 里的）照样挂到挂载 `to` 里
/// （Linux 的 `copy_tree`）。
fn copy_submounts(vfs: &Vfs, ps: &dyn Pseudo, snapshot: &[Mount], root: &Node, to: MountId) {
    for c in snapshot.iter().filter(|c| c.parent() == Some(root.mnt())) {
        let Some(point) = &c.point else {
            continue;
        };
        // 挂在文件上的经由挂上时的目录往上找
        let from = c.at.as_ref().map_or(point, |(dir, _)| dir);
        if point != root && !vfs.is_under(ps, from, root) {
            continue;
        }
        let at = c.at.clone().map(|(dir, name)| (dir.in_mount(to), name));
        let id = vfs.add_mount(
            c.fs,
            c.root.clone(),
            (point.clone().in_mount(to), at),
            c.flags,
            c.source.clone(),
            Some(c.dev),
        );
        vfs.set_root_at(id, c.root_at.clone());
        copy_submounts(vfs, ps, snapshot, &c.root, id);
    }
}

/// 重新挂载：改挂载的标志（Linux 不带 `MS_BIND` 时还改超级块的，超级块在几个挂载之间
/// 共用，这里只改这一个挂载）。
fn remount(cx: &mut Cx<'_>, target: &Node, flags: u64, keep_atime: bool) -> SysResult {
    let vfs = cx.vfs()?;
    let id = vfs.mount_root(target).ok_or(Errno::EINVAL)?;
    let m = vfs.mount(id).ok_or(Errno::EINVAL)?;
    let flags = if keep_atime {
        (flags & !ATIME) | (m.flags & ATIME)
    } else {
        flags
    };
    if m.fs == Fs::Cgroup && flags & ms::RDONLY == 0 {
        return Err(Errno::EROFS);
    }
    vfs.update_mount(id, |m| m.flags = flags);
    // 根文件系统改成 nosymfollow 时不能再不持 BKL 地解析
    cx.shared.fast_changed();
    Ok(0)
}

/// 改传播类型（Linux 的 `do_change_type`）：只检查参数。
fn change_type(cx: &mut Cx<'_>, target: &Node, flags: u64) -> SysResult {
    cx.vfs()?.mount_root(target).ok_or(Errno::EINVAL)?;
    let kind = flags & !(MS_REC | MS_SILENT);
    if kind & !MS_PROPAGATION != 0 || !kind.is_power_of_two() {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `MS_MOVE`（Linux 的 `do_move_mount`）：把挂载 `source` 连同挂在它里面的改挂到 `target`
/// 上。源要是一个挂载的根，不能是挂载树的根，目标不能在它里面。
fn move_mount(
    cx: &mut Cx<'_>,
    source: Option<&[u8]>,
    (target, at): (Node, Option<Dentry>),
) -> SysResult {
    let source = source.filter(|s| !s.is_empty()).ok_or(Errno::EINVAL)?;
    let src = lookup_at(cx, AT_FDCWD, source, true)?;
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    vfs.mount(target.mnt()).ok_or(Errno::EINVAL)?;
    // 挂载树的根也挂在（看不到的 rootfs）上，挪它时目标总在它里面，是下面的 ELOOP
    let id = vfs.mount_root(&src).ok_or(Errno::EINVAL)?;
    if vfs.is_dir(&view, &src)? != vfs.is_dir(&view, &target)? {
        return Err(Errno::EINVAL);
    }
    may_mount_on(&target)?;
    if vfs.is_under(&view, &target, &src) {
        return Err(Errno::ELOOP);
    }
    vfs.move_mount(id, Some((target, at)));
    cx.shared.fast_changed();
    Ok(0)
}

/// pivot_root(2)（Linux 6.1 的 `pivot_root`）：`new_root` 的挂载换到调用者的根目录所在的
/// 挂载的位置，原来的根挂到 `put_old` 上；根目录、当前目录是原来的根目录的进程都换成
/// `new_root`（`chroot_fs_refs`）。
pub(super) fn pivot_root(cx: &mut Cx<'_>, new_root: u64, put_old: u64) -> SysResult {
    if !cx.cred().capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let new = read_path(cx, new_root)?;
    let new = lookup_at(cx, AT_FDCWD, &new, true)?;
    let old = read_path(cx, put_old)?;
    let old = lookup_at(cx, AT_FDCWD, &old, true)?;
    let root = cx.root();
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    for node in [&new, &old] {
        if !vfs.is_dir(&view, node)? {
            return Err(Errno::ENOTDIR);
        }
    }
    // put_old 上挂着东西时挂在最上面的那个上（lock_mount）
    let old = vfs.mounted(old);
    let (Some(new_mnt), Some(root_mnt)) = (vfs.mount(new.mnt()), vfs.mount(root.mnt())) else {
        return Err(Errno::EINVAL);
    };
    vfs.mount(old.mnt()).ok_or(Errno::EINVAL)?;
    // 已经删除的目录（d_unlinked）
    for node in [&new, &old] {
        if let Node::Store(_, ino) = node
            && vfs.store().stat(*ino)?.nlink == 0
        {
            return Err(Errno::ENOENT);
        }
    }
    if new_mnt.id == root_mnt.id || old.mnt() == root_mnt.id {
        return Err(Errno::EBUSY);
    }
    // 根目录、new_root 都要是挂载的根，put_old 要在 new_root 下面，new_root 要在根目录下面
    if root_mnt.root != root
        || new_mnt.root != new
        || !vfs.is_under(&view, &old, &new)
        || !vfs.is_under(&view, &new, &root)
    {
        return Err(Errno::EINVAL);
    }
    vfs.move_mount(new_mnt.id, None);
    vfs.move_mount(root_mnt.id, Some((old, None)));
    vfs.move_mount(new_mnt.id, root_mnt.point.map(|point| (point, root_mnt.at)));
    for task in cx.kr().tasks.values() {
        let res = task.res.borrow();
        let mut fs = res.fs.borrow_mut();
        if fs.root == root {
            fs.root = new.clone();
        }
        if fs.cwd == root {
            fs.cwd = new.clone();
        }
    }
    cx.shared.fast_changed();
    Ok(0)
}

pub(super) fn umount2(cx: &mut Cx<'_>, target: u64, flags: u32) -> SysResult {
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, target)?;
    let node = lookup_at(cx, AT_FDCWD, &path, flags & UMOUNT_NOFOLLOW == 0)?;
    if !cx.cred().capable(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let vfs = cx.vfs()?;
    // `.`、当前目录这类路径也卸挂在上面的（LOOKUP_MOUNTPOINT）
    let node = vfs.mounted(node);
    let id = vfs.mount_root(&node).ok_or(Errno::EINVAL)?;
    let own_root = id == cx.root().mnt();
    let detach = flags & MNT_DETACH != 0;
    if flags & MNT_EXPIRE != 0 {
        if own_root || flags & (MNT_FORCE | MNT_DETACH) != 0 {
            return Err(Errno::EINVAL);
        }
        if busy(cx, &vfs, id) {
            return Err(Errno::EBUSY);
        }
        // 第一次只标记，没有人用过它的话下一次才卸下。Linux 在有人用过之后去掉标记，
        // 这里不去掉
        if !vfs.mount(id).is_some_and(|m| m.expiring) {
            vfs.update_mount(id, |m| m.expiring = true);
            return Err(Errno::EAGAIN);
        }
    }
    // 挂载树的根卸不下；调用者的根目录所在的挂载只能 MNT_DETACH（Linux 不带它时把根文件
    // 系统重新挂成只读）
    if vfs.mount(id).is_some_and(|m| m.point.is_none()) {
        return Err(if detach { Errno::EINVAL } else { Errno::EBUSY });
    }
    if !detach && (own_root || busy(cx, &vfs, id)) {
        return Err(Errno::EBUSY);
    }
    vfs.remove_mount(id);
    vfs.collect(&in_use(cx));
    cx.shared.fast_changed();
    Ok(0)
}

/// 挂载 `id` 在用：里面挂着别的挂载，或者有进程的根目录、当前目录、打开的文件在里面。
/// 从设备的打开文件不记得经由哪个 devpts 挂载，不算。
fn busy(cx: &Cx<'_>, vfs: &Vfs, id: MountId) -> bool {
    !vfs.children(id).is_empty() || in_use(cx).contains(&id)
}

/// 进程的根目录、当前目录和打开的文件所在的挂载。
fn in_use(cx: &Cx<'_>) -> HashSet<MountId> {
    let mut used = HashSet::new();
    for task in cx.kr().tasks.values() {
        let res = task.res.borrow();
        {
            let fs = res.fs.borrow();
            used.insert(fs.root.mnt());
            used.insert(fs.cwd.mnt());
        }
        let fds = res.fds.borrow();
        for file in fds.open_fds().into_iter().filter_map(|fd| fds.get(fd).ok()) {
            if let Some(node) = file.borrow().node() {
                used.insert(node.mnt());
            }
        }
    }
    used
}
