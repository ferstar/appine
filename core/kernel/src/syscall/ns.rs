//! 命名空间（设计文档 §6）：`clone`、`unshare` 的 `CLONE_NEWNS`、`CLONE_NEWUSER`、
//! `CLONE_NEWPID`、`CLONE_NEWUTS`、`CLONE_NEWIPC` 新建挂载命名空间、用户命名空间、pid
//! 命名空间（`crate::pidns`）、UTS 和 IPC 命名空间（`crate::nsproxy`），`/proc/<pid>/ns/*`
//! （`crate::pseudo::NsKind`）和 setns(2)。别的命名空间还没有：`/proc/<pid>/ns` 里它们总是
//! 开机时的那个，setns 到它们什么也不做。

use std::rc::Rc;

use super::SysResult;
use crate::cred::{Cred, MAX_USER_NS_LEVEL, UserNs, cap};
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::{Cx, Kernel};
use crate::pidns::{MAX_PID_NS_LEVEL, PidNs};
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

/// 新建 pid 命名空间（Linux 6.1 的 `copy_pid_ns`、`create_pid_namespace`），在 `parent`
/// （新建的子进程所在的那个）下面、属于用户命名空间 `owner`：`parent` 要是自己所在的
/// （unshare 了还没有进去时 EINVAL），`owner` 要是 `parent` 的属主或者它的后代（EPERM），
/// 最多嵌套 32 层（ENOSPC）。
pub(super) fn new_pid_ns(
    active: &PidNs,
    parent: &Rc<PidNs>,
    owner: Rc<UserNs>,
) -> Result<Rc<PidNs>, Errno> {
    if parent.id != active.id {
        return Err(Errno::EINVAL);
    }
    if !parent.owner.is_ancestor_of(&owner) {
        return Err(Errno::EPERM);
    }
    if parent.level + 1 > MAX_PID_NS_LEVEL {
        return Err(Errno::ENOSPC);
    }
    Ok(PidNs::child(parent, owner))
}

/// 根目录不是挂载命名空间的根（chroot 了，Linux 的 `current_chrooted`）。
pub(super) fn chrooted(cx: &Cx<'_>) -> bool {
    let Ok(vfs) = cx.vfs() else {
        return false;
    };
    vfs.ns_root(cx.mnt_ns())
        .is_some_and(|root| root != cx.root())
}

/// 编号为 `id`、还有进程在里面（或者在它下面），或者是哪个线程给子进程用的 pid 命名空间。
fn find_pid_ns(k: &Kernel, id: u64) -> Option<Rc<PidNs>> {
    let up = |ns: Rc<PidNs>| {
        let mut ns = Some(ns);
        while let Some(n) = ns {
            if n.id == id {
                return Some(n);
            }
            ns = n.parent.clone();
        }
        None
    };
    k.groups
        .values()
        .find_map(|g| up(g.borrow().pid_ns.clone()))
        .or_else(|| {
            k.tasks
                .values()
                .find_map(|t| t.pid_ns_children.borrow().clone().and_then(up))
        })
}

/// 编号为 `id`、还有线程在里面的 UTS、IPC 命名空间。
fn find_nsproxy<T>(k: &Kernel, f: impl Fn(&crate::nsproxy::NsProxy) -> Option<T>) -> Option<T> {
    k.tasks.values().find_map(|t| f(&t.ns.borrow()))
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
        // Linux 的 utsns_install、ipcns_install：在它的属主和自己的用户命名空间里都要有
        // CAP_SYS_ADMIN；换 IPC 命名空间之前执行并丢掉 SEM_UNDO 的撤销值（exit_sem）
        NsKind::Uts => {
            let ns = find_nsproxy(cx.kr(), |p| (p.uts.id == id).then(|| p.uts.clone()))
                .ok_or(Errno::EINVAL)?;
            if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN) || !cred.capable_here(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            cx.task.ns.borrow_mut().uts = ns;
            Ok(0)
        }
        // Linux 的 netns_install：同样要两处的 CAP_SYS_ADMIN
        NsKind::Net => {
            let ns = find_nsproxy(cx.kr(), |p| (p.net.id == id).then(|| p.net.clone()))
                .ok_or(Errno::EINVAL)?;
            if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN) || !cred.capable_here(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            cx.task.ns.borrow_mut().net = ns;
            Ok(0)
        }
        NsKind::Ipc => {
            let ns = find_nsproxy(cx.kr(), |p| (p.ipc.id == id).then(|| p.ipc.clone()))
                .ok_or(Errno::EINVAL)?;
            if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN) || !cred.capable_here(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            let tgid = cx.tgid();
            cx.ipc_mut().exit(tgid);
            cx.task.ns.borrow_mut().ipc = ns;
            cx.k().collect_ipc();
            Ok(0)
        }
        // Linux 的 pidns_install：在它的属主和自己的用户命名空间里都要有 CAP_SYS_ADMIN，
        // 只能是自己所在的或者它的后代；之后建的子进程在里面
        NsKind::Pid | NsKind::PidForChildren => {
            let ns = find_pid_ns(cx.kr(), id).ok_or(Errno::EINVAL)?;
            if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN) || !cred.capable_here(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            if !cx.pid_ns().is_ancestor_of(&ns) {
                return Err(Errno::EINVAL);
            }
            *cx.task.pid_ns_children.borrow_mut() = Some(ns);
            Ok(0)
        }
        _ if cred.capable(cap::SYS_ADMIN) => Ok(0),
        _ => Err(Errno::EPERM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_namespace_limits() {
        let init = PidNs::init();
        let user = UserNs::init();
        // 最多 32 层：在第 31 层里还能建，第 32 层里是 ENOSPC
        let mut ns = init.clone();
        for level in 1..=MAX_PID_NS_LEVEL {
            ns = new_pid_ns(&ns, &ns, user.clone()).unwrap();
            assert_eq!(ns.level, level);
        }
        assert_eq!(
            new_pid_ns(&ns, &ns, user.clone()).unwrap_err(),
            Errno::ENOSPC
        );
        // unshare 了、还没进去：给子进程用的和自己所在的不同时 EINVAL
        let child = new_pid_ns(&init, &init, user.clone()).unwrap();
        assert_eq!(
            new_pid_ns(&init, &child, user.clone()).unwrap_err(),
            Errno::EINVAL
        );
        // 属主要是父命名空间的属主或者它的后代
        let other = UserNs::child(&user, 1000);
        let owned = new_pid_ns(&init, &init, other.clone()).unwrap();
        assert_eq!(new_pid_ns(&owned, &owned, user).unwrap_err(), Errno::EPERM);
        assert!(new_pid_ns(&owned, &owned, other).is_ok());
    }
}
