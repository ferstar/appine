//! 命名空间（设计文档 §6）：`clone`、`unshare` 新建挂载、用户、pid（`crate::pidns`）、UTS、
//! IPC、网络、cgroup 命名空间（`crate::nsproxy`），`/proc/<pid>/ns/*`
//! （`crate::pseudo::NsKind`），命名空间文件上的 ioctl，经由命名空间文件或者 pidfd 的
//! setns(2)。打开着的、bind 挂载出去的命名空间文件让命名空间活着（[`NsRef`]）。time 命名空间
//! 只有开机时的那个，setns 到它什么也不做。

use std::rc::Rc;

use super::SysResult;
use crate::cred::{Cred, MAX_USER_NS_LEVEL, UserNs, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, Pidfd};
use crate::kernel::{Cx, Kernel};
use crate::nsproxy::{CgroupNs, INIT_TIME_NS, IpcNs, NetNs, NsRef, UtsNs};
use crate::pidns::{MAX_PID_NS_LEVEL, PidNs};
use crate::pseudo::{NsKind, PseudoNode};
use crate::task::FsContext;
use crate::vfs::{MntNs, MountId, Node, Vfs, new_ns_id};

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

/// 新的 cgroup 命名空间，属于用户命名空间 `owner`（根是调用者所在的 cgroup，也就是唯一的
/// 那个 "/"）。
pub(super) fn new_cgroup_ns(owner: Rc<UserNs>) -> Rc<crate::nsproxy::CgroupNs> {
    Rc::new(crate::nsproxy::CgroupNs {
        id: new_ns_id(),
        owner,
    })
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

/// 还有线程在里面的 UTS、IPC、网络、cgroup 命名空间里 `f` 挑出来的那个。
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

/// 编号为 `id` 的 `kind` 种命名空间（打开命名空间文件时）：还有线程在里面（或者在它下面）
/// 的，或者打开着的、bind 挂载出去的命名空间文件留着的。
pub(crate) fn find(k: &Kernel, vfs: Option<&Vfs>, kind: NsKind, id: u64) -> Option<NsRef> {
    let live = match kind {
        NsKind::Mnt => vfs.and_then(|v| v.find_ns(id)).map(NsRef::Mnt),
        NsKind::User => find_user_ns(k, id).map(NsRef::User),
        NsKind::Pid | NsKind::PidForChildren => find_pid_ns(k, id).map(NsRef::Pid),
        NsKind::Uts => find_nsproxy(k, |p| (p.uts.id == id).then(|| NsRef::Uts(p.uts.clone()))),
        NsKind::Ipc => find_nsproxy(k, |p| (p.ipc.id == id).then(|| NsRef::Ipc(p.ipc.clone()))),
        NsKind::Net => find_nsproxy(k, |p| (p.net.id == id).then(|| NsRef::Net(p.net.clone()))),
        NsKind::Cgroup => find_nsproxy(k, |p| {
            (p.cgroup.id == id).then(|| NsRef::Cgroup(p.cgroup.clone()))
        }),
        NsKind::Time | NsKind::TimeForChildren => (id == INIT_TIME_NS).then_some(NsRef::Time),
    };
    live.or_else(|| vfs.and_then(|v| v.pinned_ns(id)))
        .or_else(|| {
            // 打开着的命名空间文件
            k.tasks.values().find_map(|t| {
                let res = t.res.borrow();
                let fds = res.fds.borrow();
                fds.open_fds()
                    .into_iter()
                    .filter_map(|fd| fds.get(fd).ok())
                    .find_map(|f| match &f.borrow().kind {
                        FileKind::Proc { ns: Some(r), .. } if r.id() == id => Some(r.clone()),
                        _ => None,
                    })
            })
        })
}

/// setns(2)（Linux 6.1）：`fd` 是打开的命名空间文件（`/proc/<pid>/ns/<种类>`，或者 bind
/// 挂载出去的），`nstype` 是 0 或者与它相同的 `CLONE_NEW*`；或者是 pidfd，`nstype` 是要换成
/// 那个进程的哪几种命名空间（不能是 0），一次换过去。打开着的命名空间文件让命名空间活着，里面
/// 的进程都退出了也能进去。
pub(super) fn setns(cx: &mut Cx<'_>, fd: i32, nstype: i32) -> SysResult {
    let file = cx.fds().get(fd)?;
    let nstype = nstype as u32 as u64;
    let (flags, set) = match &file.borrow().kind {
        FileKind::Proc {
            node: PseudoNode::NsFile(kind, _),
            ns: Some(ns),
            ..
        } => {
            if nstype != 0 && nstype != kind.clone_flag() {
                return Err(Errno::EINVAL);
            }
            (kind.clone_flag(), NsSet::one(ns.clone()))
        }
        FileKind::Pidfd(p) => {
            if nstype == 0 || nstype & !super::proc::CLONE_NEW_MASK != 0 {
                return Err(Errno::EINVAL);
            }
            (nstype, NsSet::of(cx, p, nstype)?)
        }
        _ => return Err(Errno::EINVAL),
    };
    let root = validate(cx, &set, flags)?;
    commit(cx, set, root);
    Ok(0)
}

/// setns 要换成的命名空间（Linux 的 `struct nsset`）：命名空间文件只有它那一种，pidfd 是那个
/// 进程的、要求的那几种。
#[derive(Default)]
struct NsSet {
    user: Option<Rc<UserNs>>,
    mnt: Option<Rc<MntNs>>,
    uts: Option<Rc<UtsNs>>,
    ipc: Option<Rc<IpcNs>>,
    pid: Option<Rc<PidNs>>,
    cgroup: Option<Rc<CgroupNs>>,
    net: Option<Rc<NetNs>>,
    time: bool,
}

impl NsSet {
    fn one(ns: NsRef) -> NsSet {
        let mut set = NsSet::default();
        match ns {
            NsRef::Mnt(n) => set.mnt = Some(n),
            NsRef::User(n) => set.user = Some(n),
            NsRef::Pid(n) => set.pid = Some(n),
            NsRef::Uts(n) => set.uts = Some(n),
            NsRef::Ipc(n) => set.ipc = Some(n),
            NsRef::Net(n) => set.net = Some(n),
            NsRef::Cgroup(n) => set.cgroup = Some(n),
            NsRef::Time => set.time = true,
        }
        set
    }

    /// pidfd 指向的进程的 `flags` 里那几种命名空间（Linux 的 `validate_nsset` 开头取的
    /// 快照）：pid 命名空间是它自己所在的，不是给子进程用的。进程已经被回收、主线程已经退出
    /// 时 ESRCH，不能像 ptrace 那样读它（`PTRACE_MODE_READ_REALCREDS`）时 EPERM。
    fn of(cx: &Cx<'_>, p: &Pidfd, flags: u64) -> Result<NsSet, Errno> {
        let k = cx.kr();
        let group = p.group(k).ok_or(Errno::ESRCH)?;
        let task = k.tasks.get(&p.tgid).cloned().ok_or(Errno::ESRCH)?;
        let g = group.borrow();
        if !super::proc::may_ptrace(cx, &g) {
            return Err(Errno::EPERM);
        }
        let want = |kind: NsKind| flags & kind.clone_flag() != 0;
        let ns = task.ns.borrow();
        Ok(NsSet {
            user: want(NsKind::User).then(|| g.cred.user_ns.clone()),
            mnt: want(NsKind::Mnt).then(|| task.res.borrow().fs.borrow().mnt_ns.clone()),
            uts: want(NsKind::Uts).then(|| ns.uts.clone()),
            ipc: want(NsKind::Ipc).then(|| ns.ipc.clone()),
            pid: want(NsKind::Pid).then(|| g.pid_ns.clone()),
            cgroup: want(NsKind::Cgroup).then(|| ns.cgroup.clone()),
            net: want(NsKind::Net).then(|| ns.net.clone()),
            time: want(NsKind::Time),
        })
    }
}

/// 按 Linux 的顺序（用户、挂载、UTS、IPC、pid、cgroup、网络、time）检查能不能进 `set` 里的
/// 每一种（各种命名空间的 `install`），一种不行就都不换。`flags` 是要换的那几种
/// `CLONE_NEW*`。返回挂载命名空间的根目录。
///
/// 一起进了用户命名空间时，后面几种要求的"自己的用户命名空间里的能力"按进去之后的那个算
/// （Linux 的 `nsset->cred`），能力仍看原来的凭据：建立那个用户命名空间的用户不用
/// `CAP_SYS_ADMIN` 也能一次进它和它的别的命名空间。
fn validate(cx: &Cx<'_>, set: &NsSet, flags: u64) -> Result<Option<Node>, Errno> {
    let cred = cx.cred();
    // 每个线程的 TaskState 和 TaskRes 各持有一份引用
    let fs_shared = Rc::strong_count(&cx.t.fs) > 2;
    let threaded = cx.t.group.borrow().threads.len() > 1;
    let here = set.user.clone().unwrap_or_else(|| cred.user_ns.clone());
    // Linux 的 utsns_install 这些：在它的属主和自己的用户命名空间里都要有 CAP_SYS_ADMIN
    let may_enter = |owner: &UserNs| {
        if cred.ns_capable(owner, cap::SYS_ADMIN) && cred.ns_capable(&here, cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    };
    // Linux 的 userns_install：不能重进自己的命名空间（借此拿回能力），多线程的进程、fs
    // 共用时不行，要在目标命名空间里有 CAP_SYS_ADMIN
    if let Some(ns) = &set.user {
        if ns.id == cred.user_ns.id || threaded || fs_shared {
            return Err(Errno::EINVAL);
        }
        if !cred.ns_capable(ns, cap::SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
    }
    // Linux 的 mntns_install：先看能力，再看 fs 有没有和别的线程、进程共用。只换挂载命名
    // 空间时看的是自己的 fs，一起换别的时 Linux 看的是临时复制的那一份（prepare_nsset），
    // 总是只有自己在用
    let mut root = None;
    if let Some(ns) = &set.mnt {
        let vfs = cx.vfs()?;
        if !cred.ns_capable(&ns.owner, cap::SYS_ADMIN)
            || !cred.ns_capable(&here, cap::SYS_CHROOT)
            || !cred.ns_capable(&here, cap::SYS_ADMIN)
        {
            return Err(Errno::EPERM);
        }
        if fs_shared && flags == NsKind::Mnt.clone_flag() {
            return Err(Errno::EINVAL);
        }
        root = Some(vfs.ns_root(ns.id).ok_or(Errno::EINVAL)?);
    }
    if let Some(ns) = &set.uts {
        may_enter(&ns.owner)?;
    }
    if let Some(ns) = &set.ipc {
        may_enter(&ns.owner)?;
    }
    // Linux 的 pidns_install：只能是自己所在的或者它的后代
    if let Some(ns) = &set.pid {
        may_enter(&ns.owner)?;
        if !cx.pid_ns().is_ancestor_of(ns) {
            return Err(Errno::EINVAL);
        }
    }
    if let Some(ns) = &set.cgroup {
        may_enter(&ns.owner)?;
    }
    if let Some(ns) = &set.net {
        may_enter(&ns.owner)?;
    }
    // Linux 的 timens_install：多线程的进程不行
    if set.time {
        if threaded {
            return Err(Errno::EUSERS);
        }
        may_enter(&UserNs::init())?;
    }
    Ok(root)
}

/// 换成 `set` 里的命名空间（Linux 的 `commit_nsset`），`root` 是挂载命名空间的根目录：
/// 根目录、当前目录换到那里；换 IPC 命名空间之前执行并丢掉 SEM_UNDO 的撤销值（`exit_sem`）；
/// pid 命名空间是之后建的子进程所在的（1 号进程已经退出的建不了，ENOMEM）。time 命名空间
/// 只有开机时的那个，什么也不用换。
fn commit(cx: &mut Cx<'_>, set: NsSet, root: Option<Node>) {
    if let Some(ns) = set.user {
        cx.t.group.borrow_mut().cred.enter_user_ns(ns);
        cx.shared.fast_changed();
    }
    if let (Some(ns), Some(root)) = (set.mnt, root) {
        let mut fs = cx.t.fs.borrow_mut();
        fs.root = root.clone();
        fs.cwd = root;
        fs.mnt_ns = ns;
        drop(fs);
        cx.shared.fast_changed();
    }
    if set.ipc.is_some() {
        let tgid = cx.tgid();
        cx.ipc_mut().exit(tgid);
    }
    {
        let mut p = cx.task.ns.borrow_mut();
        if let Some(ns) = set.uts {
            p.uts = ns;
        }
        if let Some(ns) = set.cgroup {
            p.cgroup = ns;
        }
        if let Some(ns) = set.net {
            p.net = ns;
        }
    }
    if let Some(ns) = set.ipc {
        cx.task.ns.borrow_mut().ipc = ns;
        cx.k().collect_ipc();
    }
    if let Some(ns) = set.pid {
        *cx.task.pid_ns_children.borrow_mut() = Some(ns);
    }
}

const NS_GET_USERNS: u32 = 0xb701;
const NS_GET_PARENT: u32 = 0xb702;
const NS_GET_NSTYPE: u32 = 0xb703;
const NS_GET_OWNER_UID: u32 = 0xb704;

/// 命名空间文件上的 ioctl（Linux 6.1 的 `ns_ioctl`）：`NS_GET_USERNS`、`NS_GET_PARENT` 打开
/// 属主、上一层的命名空间，得到新的 fd（带 `FD_CLOEXEC`）；`NS_GET_NSTYPE` 是它的
/// `CLONE_NEW*`；`NS_GET_OWNER_UID` 是用户命名空间的建立者在调用者的用户命名空间里的用户 ID。
/// 别的是 ENOTTY（6.9 起才有的 `NS_GET_MNTNS_ID` 这些也是：appine 报告的内核是 6.1）。
/// `mnt` 是文件经由的挂载，`kind` 是它的种类。
pub(super) fn ioctl(
    cx: &mut Cx<'_>,
    mnt: MountId,
    kind: NsKind,
    ns: &NsRef,
    request: u32,
    arg: u64,
) -> SysResult {
    let related = match request {
        NS_GET_NSTYPE => return Ok(kind.clone_flag()),
        NS_GET_OWNER_UID => {
            let NsRef::User(user) = ns else {
                return Err(Errno::EINVAL);
            };
            let uid = cx.cred().uid_out(user.owner);
            return super::put(cx, arg, uid.to_le_bytes()).map(|()| 0);
        }
        // 用户命名空间的上一层就是它的属主
        NS_GET_USERNS | NS_GET_PARENT if matches!(ns, NsRef::User(_)) => {
            (NsKind::User, NsRef::User(visible_owner(cx, ns)?))
        }
        NS_GET_USERNS => (NsKind::User, NsRef::User(visible_owner(cx, ns)?)),
        // 上一层要是调用者所在的 pid 命名空间或者它下面的（Linux 的 pidns_get_parent）
        NS_GET_PARENT => match ns {
            NsRef::Pid(p) => {
                let parent = p.parent.clone().ok_or(Errno::EPERM)?;
                if !cx.pid_ns().is_ancestor_of(&parent) {
                    return Err(Errno::EPERM);
                }
                (NsKind::Pid, NsRef::Pid(parent))
            }
            _ => return Err(Errno::EINVAL),
        },
        _ => return Err(Errno::ENOTTY),
    };
    let (kind, related) = related;
    let id = related.id();
    let name = format!("{}:[{id}]", kind.type_name());
    // 与 Linux 的 open_related_ns 一样只读打开（没有 open 加上的 O_LARGEFILE）
    let file = crate::fd::OpenFile::with_path(
        FileKind::Proc {
            mnt,
            node: PseudoNode::NsFile(kind, id),
            data: Vec::new(),
            mm: None,
            ns: Some(related),
        },
        crate::fd::oflags::O_RDONLY,
        Some(Rc::from(name.into_bytes())),
    );
    cx.fds_mut().install(file, true, 0).map(|fd| fd as u64)
}

/// 命名空间 `ns` 的属主，要是调用者所在的用户命名空间或者它下面的（Linux 的
/// `ns_get_owner`），否则（包括开机时的用户命名空间没有属主）EPERM。
fn visible_owner(cx: &Cx<'_>, ns: &NsRef) -> Result<Rc<UserNs>, Errno> {
    let owner = ns.owner().ok_or(Errno::EPERM)?;
    if !cx.cred().user_ns.is_ancestor_of(&owner) {
        return Err(Errno::EPERM);
    }
    Ok(owner)
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
