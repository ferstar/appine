//! 命名空间（设计文档 §6）：`clone`、`unshare` 的 `CLONE_NEWNS` 新建挂载命名空间，
//! `/proc/<pid>/ns/*`（`crate::pseudo::NsKind`）和 setns(2)。别的命名空间还没有：
//! `/proc/<pid>/ns` 里它们总是开机时的那个，setns 到它们什么也不做。

use std::rc::Rc;

use super::SysResult;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::Cx;
use crate::pseudo::{NsKind, PseudoNode};
use crate::task::FsContext;
use crate::vfs::{MntNs, Node, Vfs, new_ns_id};

/// 给 `fs` 一个新的挂载命名空间：复制原来的那个里的所有挂载，根目录、当前目录换到复制
/// 出来的挂载上（Linux 的 `copy_mnt_ns`）。没有 rootfs 时只换编号。
pub(super) fn new_mnt_ns(vfs: Option<&Vfs>, fs: &mut FsContext) {
    let Some(vfs) = vfs else {
        fs.mnt_ns = Rc::new(MntNs { id: new_ns_id() });
        return;
    };
    let (ns, map) = vfs.copy_ns(fs.mnt_ns.id);
    let remap = |n: &Node| match map.get(&n.mnt()) {
        Some(&id) => n.clone().in_mount(id),
        None => n.clone(),
    };
    fs.root = remap(&fs.root);
    fs.cwd = remap(&fs.cwd);
    fs.mnt_ns = ns;
}

/// setns(2)：`fd` 是打开的 `/proc/<pid>/ns/<种类>`，`nstype` 是 0 或者与它相同的
/// `CLONE_NEW*`。不支持 pidfd（EINVAL）。
pub(super) fn setns(cx: &mut Cx<'_>, fd: i32, nstype: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    let (kind, id) = match &file.borrow().kind {
        FileKind::Proc {
            node: PseudoNode::NsFile(kind, id),
            ..
        } => (*kind, *id),
        _ => return Err(Errno::EINVAL),
    };
    let nstype = nstype as u32 as u64;
    if nstype != 0 && nstype != kind.clone_flag() {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    match kind {
        NsKind::Mnt => {
            // Linux 的 mntns_install：先看能力，再看 fs 有没有和别的线程、进程共用
            if !cred.capable(cap::SYS_ADMIN) || !cred.capable(cap::SYS_CHROOT) {
                return Err(Errno::EPERM);
            }
            let vfs = cx.vfs()?;
            let ns = vfs.find_ns(id).ok_or(Errno::EINVAL)?;
            // 每个线程的 TaskState 和 TaskRes 各持有一份引用
            if Rc::strong_count(&cx.t.fs) > 2 {
                return Err(Errno::EINVAL);
            }
            let root = vfs.ns_root(id).ok_or(Errno::EINVAL)?;
            {
                let mut fs = cx.t.fs.borrow_mut();
                fs.root = root.clone();
                fs.cwd = root;
                fs.mnt_ns = ns;
            }
            cx.shared.fast_changed();
            Ok(0)
        }
        // 已经在这个用户命名空间里（只有开机时的那个）
        NsKind::User => Err(Errno::EINVAL),
        _ if cred.capable(cap::SYS_ADMIN) => Ok(0),
        _ => Err(Errno::EPERM),
    }
}
