//! 命名空间（设计文档 §6）：`clone`、`unshare` 的 `CLONE_NEWNS`、`CLONE_NEWUSER` 新建挂载
//! 命名空间、用户命名空间，`/proc/<pid>/ns/*`（`crate::pseudo::NsKind`）和 setns(2)。别的
//! 命名空间还没有：`/proc/<pid>/ns` 里它们总是开机时的那个，setns 到它们什么也不做。

use std::rc::Rc;

use super::SysResult;
use crate::cred::{Cred, MAX_USER_NS_LEVEL, UserNs, cap};
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::{Cx, Kernel};
use crate::pseudo::{NsKind, PseudoNode};
use crate::task::FsContext;
use crate::vfs::{MntNs, Node, Vfs, new_ns_id};

/// 给 `fs` 一个新的挂载命名空间，属于用户命名空间 `owner`：复制原来的那个里的所有挂载，
/// 根目录、当前目录换到复制出来的挂载上（Linux 的 `copy_mnt_ns`）。没有 rootfs 时只换编号。
pub(super) fn new_mnt_ns(vfs: Option<&Vfs>, fs: &mut FsContext, owner: Rc<UserNs>) {
    let Some(vfs) = vfs else {
        fs.mnt_ns = Rc::new(MntNs {
            id: new_ns_id(),
            owner,
        });
        return;
    };
    let (ns, map) = vfs.copy_ns(fs.mnt_ns.id, owner);
    let remap = |n: &Node| match map.get(&n.mnt()) {
        Some(&id) => n.clone().in_mount(id),
        None => n.clone(),
    };
    fs.root = remap(&fs.root);
    fs.cwd = remap(&fs.cwd);
    fs.mnt_ns = ns;
}

/// 新建用户命名空间（Linux 6.1 的 `create_user_ns`），`cred` 进到里面去：最多嵌套到第
/// 33 层（ENOSPC，老内核是 EUSERS），chroot 了的进程不行（EPERM），建立者的有效用户 ID、
/// 组 ID 在原来的命名空间里要有映射（EPERM）。`chrooted` 是根目录不是挂载命名空间的根。
pub(super) fn new_user_ns(cred: &mut Cred, chrooted: bool) -> Result<(), Errno> {
    let parent = cred.user_ns.clone();
    if parent.level > MAX_USER_NS_LEVEL {
        return Err(Errno::ENOSPC);
    }
    if chrooted {
        return Err(Errno::EPERM);
    }
    if parent.uid_map.borrow().up(cred.euid).is_none()
        || parent.gid_map.borrow().up(cred.egid).is_none()
    {
        return Err(Errno::EPERM);
    }
    let ns = UserNs::child(&parent, cred.euid);
    cred.enter_user_ns(ns);
    Ok(())
}

/// 根目录不是挂载命名空间的根（chroot 了，Linux 的 `current_chrooted`）。
pub(super) fn chrooted(cx: &Cx<'_>) -> bool {
    let Ok(vfs) = cx.vfs() else {
        return false;
    };
    vfs.ns_root(cx.mnt_ns())
        .is_some_and(|root| root != cx.root())
}

/// 编号为 `id`、还有进程在里面（或者在它下面）的用户命名空间。
fn find_user_ns(k: &Kernel, id: u64) -> Option<Rc<UserNs>> {
    k.groups.values().find_map(|g| {
        let mut ns = Some(g.borrow().cred.user_ns.clone());
        while let Some(n) = ns {
            if n.id == id {
                return Some(n);
            }
            ns = n.parent.clone();
        }
        None
    })
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
    // 每个线程的 TaskState 和 TaskRes 各持有一份引用
    let fs_shared = Rc::strong_count(&cx.t.fs) > 2;
    match kind {
        NsKind::Mnt => {
            let vfs = cx.vfs()?;
            let ns = vfs.find_ns(id).ok_or(Errno::EINVAL)?;
            // Linux 的 mntns_install：先看能力，再看 fs 有没有和别的线程、进程共用
            if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN)
                || !cred.capable_here(cap::SYS_CHROOT)
                || !cred.capable_here(cap::SYS_ADMIN)
            {
                return Err(Errno::EPERM);
            }
            if fs_shared {
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
        // Linux 的 userns_install：不能重进自己的命名空间（借此拿回能力），多线程的进程、
        // fs 共用时不行，要在目标命名空间里有 CAP_SYS_ADMIN
        NsKind::User => {
            let ns = find_user_ns(cx.kr(), id).ok_or(Errno::EINVAL)?;
            if ns.id == cred.user_ns.id || cx.t.group.borrow().threads.len() > 1 || fs_shared {
                return Err(Errno::EINVAL);
            }
            if !cred.ns_capable(&ns, cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let mut g = cx.t.group.borrow_mut();
            g.cred.enter_user_ns(ns);
            drop(g);
            cx.shared.fast_changed();
            Ok(0)
        }
        _ if cred.capable(cap::SYS_ADMIN) => Ok(0),
        _ => Err(Errno::EPERM),
    }
}
