//! mount(2), umount2(2), pivot_root(2) (the kernel/vfs map).
//!
//! Synthetic filesystems (procfs, sysfs, devpts, cgroup2) can be mounted on any directory;
//! directories and files both in the store and in synthetic filesystems can be bind-mounted
//! (`MS_REC` brings the mounts inside along), and so can namespace files (nsfs; the mount keeps
//! that namespace alive). Mount flags, remounting, `MS_MOVE`, unmounting (`MNT_DETACH`,
//! `MNT_EXPIRE`), pivot_root and the various errors follow Linux 6.1's `path_mount`,
//! `path_umount` and `pivot_root`. Propagation types can be set but are not emulated: there is no
//! propagation between mounts, and all behave as private. Mounts must be in the caller's mount
//! namespace (Linux's `check_mnt`); those in other namespaces (seen through `/proc/<pid>/root`)
//! are EINVAL. A tmpfs is a tree in the store that does not hang under the root directory
//! (`Vfs::new_tmpfs`). mqueue lists the POSIX message queues in the IPC namespace of the process
//! that mounted it (`syscall::mqueue`).
//! Other filesystems are ENODEV, the same as ones not built into the kernel.

use std::collections::HashSet;
use std::rc::Rc;

use super::fs::{AT_FDCWD, lookup_at, start_dir};
use super::{SysResult, get, read_path};
use crate::cred::cap;
use crate::errno::Errno;
use crate::kernel::Cx;
use crate::nsproxy::NsRef;
use crate::pseudo::NsKind;
use crate::vfs::{Dentry, Fs, Mount, MountId, Node, Pseudo, PseudoNode, TmpfsParams, Vfs, ms};

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
/// The access-time flags: when a remount gives none of them, the old ones are kept.
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
    // As in Linux's ksys_mount: copy the arguments and resolve the mount point first, then look
    // at the flags and permissions
    let fstype = optional_str(cx, fstype)?;
    let source = optional_str(cx, source)?;
    // Only tmpfs and devpts look at the options in data (proc's hidepid and the like are ignored);
    // Linux copies at most one page, and EFAULT if the first byte cannot be read
    let data = if data != 0 {
        get::<1>(cx, data)?;
        cx.mm().read_cstr(data, 4096).unwrap_or_default()
    } else {
        Vec::new()
    };
    let target = read_path(cx, target)?;
    let (node, at, resolved) = {
        let vfs = cx.vfs()?;
        let (node, at) = vfs.walk(
            &cx.view(&vfs),
            &cx.cred(),
            &cx.root(),
            &cx.cwd(),
            &target,
            true,
        )?;
        // When something is already mounted on the mount point, mount on the topmost one (Linux's
        // lock_mount): the same for paths that involve no lookup, such as `.` and the cwd
        let top = vfs.mounted(node.clone());
        let at = at.filter(|_| top == node);
        (top, at, node)
    };
    let mut flags = flags;
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    if flags & MS_NOUSER != 0 {
        return Err(Errno::EINVAL);
    }
    if !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    // Mount flags, relatime by default
    let mut mnt = flags & ms::PER_MOUNT & !ms::RELATIME;
    if flags & ms::NOATIME == 0 {
        mnt |= ms::RELATIME;
    }
    if flags & MS_STRICTATIME != 0 {
        mnt &= !(ms::RELATIME | ms::NOATIME);
    }
    // remount does not climb up: a covered node reached through a magic link is not a mount root
    // (EINVAL)
    if flags & MS_REMOUNT != 0 {
        let keep_atime = flags & (ATIME | MS_STRICTATIME) == 0;
        let data = (flags & MS_BIND == 0).then_some(&data[..]);
        return remount(cx, &resolved, mnt, keep_atime, data);
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
    new_mount(cx, fstype, source, (node, at), mnt, &data)
}

/// A string argument of mount(2); None for a null pointer.
fn optional_str(cx: &Cx<'_>, addr: u64) -> Result<Option<Vec<u8>>, Errno> {
    if addr == 0 {
        return Ok(None);
    }
    read_path(cx, addr).map(Some)
}

/// Mounts a new filesystem (Linux's `do_new_mount`).
fn new_mount(
    cx: &mut Cx<'_>,
    fstype: Option<Vec<u8>>,
    source: Option<Vec<u8>>,
    (target, at): (Node, Option<Dentry>),
    flags: u64,
    data: &[u8],
) -> SysResult {
    let fstype = fstype.ok_or(Errno::EINVAL)?;
    let Some(fs) = Fs::from_type(&fstype) else {
        let name = String::from_utf8_lossy(&fstype);
        crate::debug::log(format_args!("mount: no filesystem type {name}"));
        return Err(Errno::ENODEV);
    };
    // mqueue mounts the instance of the caller's IPC namespace, which needs CAP_SYS_ADMIN in the
    // namespace's owner (Linux's mqueue_init_fs_context replaces fc->user_ns with that owner, then
    // calls mount_capable)
    let ipc = cx.ns().ipc;
    if fs == Fs::Mqueue && !cx.cred().ns_capable(&ipc.owner, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    // cgroup2 can be mounted without read-only (crun does so when mounting /sys/fs/cgroup for a
    // container), and is then writable like the boot mount
    let vfs = cx.vfs()?;
    may_mount_on(&target)?;
    check_mnt(cx, &vfs, target.mnt())?;
    // A filesystem with a single instance cannot be mounted again on the root of its own mount
    // (Linux's do_add_mount); mqueue has one instance per IPC namespace
    let same_instance = |m: &Mount| {
        m.fs == fs
            && (fs != Fs::Mqueue
                || matches!(&m.root, Node::Pseudo(_, PseudoNode::MqRoot(n)) if *n == ipc.id))
    };
    if !fs.new_instance()
        && let Some(id) = vfs.mount_root(&target)
        && vfs.mount(id).is_some_and(|m| same_instance(&m))
    {
        return Err(Errno::EBUSY);
    }
    if !vfs.is_dir(&cx.view(&vfs), &target)? {
        return Err(Errno::ENOTDIR);
    }
    // devpts options (owner and permissions of the new instance's terminals, ptmx permissions,
    // the maximum)
    let devpts = if fs == Fs::Pts {
        let cred = cx.cred();
        Some(crate::tty::PtsOptions::parse(
            data,
            |u| cred.uid_in(u),
            |g| cred.gid_in(g),
        )?)
    } else {
        None
    };
    if fs == Fs::Tmpfs {
        let params = tmpfs_options(data, &cx.cred())?;
        let source = source.unwrap_or_default();
        vfs.new_tmpfs((target, at), flags, source, params)?;
        cx.shared.fast_changed();
        return Ok(0);
    }
    let (root, dev) = match fs {
        Fs::Mqueue => (
            Node::Pseudo(0, PseudoNode::MqRoot(ipc.id)),
            Some(vfs.mq_dev(ipc.id)),
        ),
        // The root of cgroup2 is the root of the caller's cgroup namespace (Linux's
        // cgroup_get_tree)
        Fs::Cgroup => (
            Node::Pseudo(0, PseudoNode::Cgroup(cx.ns().cgroup.root, b"")),
            Some(fs.dev()),
        ),
        _ => (fs.root(), (!fs.own_dev()).then(|| fs.dev())),
    };
    let id = vfs.add_mount(
        fs,
        root,
        (target, at),
        flags,
        source.unwrap_or_default(),
        dev,
    );
    // Every devpts mount is a new instance (since 4.7) with its own set of terminals, numbered by
    // the newly allocated device number
    if let Some(opts) = devpts {
        let dev = vfs.mount(id).map_or(0, |m| m.dev);
        vfs.set_devpts_options(dev, opts);
        vfs.update_mount(id, |m| {
            m.root = Node::Pseudo(id, PseudoNode::PtsRoot(dev));
        });
    }
    // An mqueue mount keeps that IPC namespace (and its queues) alive, just as Linux's superblock
    // references it
    if fs == Fs::Mqueue {
        vfs.update_mount(id, |m| m.pin = Some(crate::nsproxy::NsRef::Ipc(ipc)));
    }
    // Pids in procfs are counted in the pid namespace of the process that mounted it
    let pid_ns = cx.pid_ns();
    if fs == Fs::Proc && pid_ns.level > 0 {
        vfs.set_proc_pid_ns(id, pid_ns);
    }
    cx.shared.fast_changed();
    Ok(0)
}

/// The tmpfs mount options that were given (the `shmem_options` parsed by Linux 6.1's
/// `shmem_parse_one`). Unknown options are EINVAL.
#[derive(Default)]
struct TmpfsOptions {
    perm: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    /// Number of pages and of inodes (0 is unlimited).
    blocks: Option<u64>,
    inodes: Option<u64>,
    inode64: Option<bool>,
}

fn parse_tmpfs(data: &[u8]) -> Result<TmpfsOptions, Errno> {
    const PAGE: u64 = crate::vfs::TMPFS_PAGE;
    let mut o = TmpfsOptions::default();
    for opt in data.split(|&b| b == b',').filter(|o| !o.is_empty()) {
        let (key, value) = match opt.iter().position(|&b| b == b'=') {
            Some(i) => (&opt[..i], Some(&opt[i + 1..])),
            None => (opt, None),
        };
        let id = |v: &[u8]| {
            parse_num(v)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|&n| n != u32::MAX)
                .ok_or(Errno::EINVAL)
        };
        match (key, value) {
            (b"size", Some(v)) => {
                let (n, rest) = memparse(v)?;
                let bytes = match rest {
                    b"" => n,
                    b"%" => n.saturating_mul(appine_hal::memory_info().0) / 100,
                    _ => return Err(Errno::EINVAL),
                };
                o.blocks = Some(bytes.div_ceil(PAGE));
            }
            (b"nr_blocks", Some(v)) => match memparse(v)? {
                (n, b"") => o.blocks = Some(n),
                _ => return Err(Errno::EINVAL),
            },
            (b"nr_inodes", Some(v)) => match memparse(v)? {
                (n, b"") => o.inodes = Some(n),
                _ => return Err(Errno::EINVAL),
            },
            (b"mode", Some(v)) => {
                let s = std::str::from_utf8(v).map_err(|_| Errno::EINVAL)?;
                o.perm = Some(u32::from_str_radix(s, 8).map_err(|_| Errno::EINVAL)? & 0o7777);
            }
            (b"uid", Some(v)) => o.uid = Some(id(v)?),
            (b"gid", Some(v)) => o.gid = Some(id(v)?),
            (b"inode64", None) => o.inode64 = Some(true),
            (b"inode32", None) => o.inode64 = Some(false),
            (
                b"huge",
                Some(b"never" | b"always" | b"within_size" | b"advise" | b"deny" | b"force"),
            ) => {}
            (b"mpol", Some(_)) => {}
            // Generic superblock flags (`vfs_parse_sb_flag`); mount(8) turns them into MS_* flags
            // anyway
            (
                b"ro" | b"rw" | b"sync" | b"async" | b"dirsync" | b"mand" | b"nomand" | b"lazytime"
                | b"nolazytime" | b"posixacl" | b"noposixacl" | b"silent" | b"loud",
                None,
            ) => {}
            _ => {
                let name = String::from_utf8_lossy(opt);
                crate::debug::log(format_args!("tmpfs: unknown parameter {name}"));
                return Err(Errno::EINVAL);
            }
        }
    }
    Ok(o)
}

/// The parameters of a newly mounted tmpfs: defaults for what is not given (permissions 1777,
/// owner and group the mounting process's fsuid and fsgid, size and inode count half of physical
/// memory, inode64).
fn tmpfs_options(data: &[u8], cred: &crate::cred::Cred) -> Result<TmpfsParams, Errno> {
    let o = parse_tmpfs(data)?;
    let default = TmpfsParams::default_limit();
    Ok(TmpfsParams {
        perm: o.perm.unwrap_or(0o1777),
        uid: o.uid.unwrap_or(cred.fsuid),
        gid: o.gid.unwrap_or(cred.fsgid),
        blocks: o.blocks.unwrap_or(default),
        inodes: o.inodes.unwrap_or(default),
        inode64: o.inode64.unwrap_or(true),
    })
}

/// Changes a tmpfs's superblock on remount (Linux 6.1's `shmem_reconfigure`): when a nonzero
/// `size=` or `nr_inodes=` is given, an unlimited value cannot become limited and a limit cannot
/// go below what is already used (both EINVAL); the given size and inode count (0 makes it
/// unlimited) and inode32/64 are applied as given; `mode=`, `uid=` and `gid=` stay unchanged.
fn remount_tmpfs(vfs: &Vfs, dev: u64, data: &[u8]) -> Result<(), Errno> {
    let o = parse_tmpfs(data)?;
    vfs.tmpfs_reconfigure(dev, |p, pages, inodes| {
        if let Some(b) = o.blocks.filter(|&b| b != 0)
            && (p.blocks == 0 || b < pages)
        {
            return Err(Errno::EINVAL);
        }
        if let Some(n) = o.inodes.filter(|&n| n != 0)
            && (p.inodes == 0 || n < inodes)
        {
            return Err(Errno::EINVAL);
        }
        p.blocks = o.blocks.unwrap_or(p.blocks);
        p.inodes = o.inodes.unwrap_or(p.inodes);
        p.inode64 = o.inode64.unwrap_or(p.inode64);
        Ok(())
    })
}

/// An unsigned number, with the same prefixes as C's `strtoull(…, 0)` (`0x` hexadecimal, `0`
/// octal).
fn parse_num(s: &[u8]) -> Option<u64> {
    let (n, rest) = leading_num(s)?;
    rest.is_empty().then_some(n)
}

/// The leading unsigned number and the rest.
fn leading_num(s: &[u8]) -> Option<(u64, &[u8])> {
    let (radix, digits) = match s {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, s),
    };
    let end = digits
        .iter()
        .position(|&b| !(b as char).is_digit(radix))
        .unwrap_or(digits.len());
    let n = std::str::from_utf8(&digits[..end]).ok()?;
    let n = if n.is_empty() && radix == 8 {
        0
    } else {
        u64::from_str_radix(n, radix).ok()?
    };
    Some((n, &digits[end..]))
}

/// Linux's `memparse`: the number may be followed by K, M, G, T, P or E (case-insensitive).
/// Returns the rest.
fn memparse(s: &[u8]) -> Result<(u64, &[u8]), Errno> {
    let (n, rest) = leading_num(s).ok_or(Errno::EINVAL)?;
    let shift = match rest.first().map(u8::to_ascii_uppercase) {
        Some(b'K') => 10,
        Some(b'M') => 20,
        Some(b'G') => 30,
        Some(b'T') => 40,
        Some(b'P') => 50,
        Some(b'E') => 60,
        _ => return Ok((n, rest)),
    };
    Ok((n.saturating_mul(1 << shift), &rest[1..]))
}

/// Bind mount (Linux's `do_loopback`): the new mount sees the source directory or file, with the
/// flags and device number of the mount the source is in.
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
    if let Node::Pseudo(_, PseudoNode::NsFile(kind, ns)) = src {
        return bind_ns(cx, &vfs, kind, ns, src, (target, at));
    }
    may_mount_on(&target)?;
    let dir = vfs.is_dir(&view, &src)?;
    if dir != vfs.is_dir(&view, &target)? {
        return Err(Errno::ENOTDIR);
    }
    // Unmounted mounts and mounts in other namespaces cannot be bound (Linux's check_mnt)
    check_mnt(cx, &vfs, src.mnt())?;
    check_mnt(cx, &vfs, target.mnt())?;
    let m = vfs.mount(src.mnt()).ok_or(Errno::EINVAL)?;
    // Note the mounts inside the source before mounting: if the target is inside the source, the
    // new mount is not copied into itself
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

/// Bind-mounts a namespace file (how `ip netns add` keeps a namespace alive): the mount holds
/// that namespace ([`Mount::pin`]), lives in nsfs, and has only the rw flag. As in Linux's
/// do_loopback, a file found through a path in another mount namespace also works; a mount
/// namespace's file can only be mounted into a namespace older than it (`mnt_ns_loop`: otherwise
/// a namespace could keep itself alive through a mount).
fn bind_ns(
    cx: &mut Cx<'_>,
    vfs: &Vfs,
    kind: NsKind,
    id: u64,
    src: Node,
    (target, at): (Node, Option<Dentry>),
) -> SysResult {
    let ns = super::ns::find(cx.kr(), Some(vfs), kind, id).ok_or(Errno::ENOENT)?;
    if matches!(&ns, NsRef::Mnt(m) if cx.mnt_ns() >= m.id) {
        return Err(Errno::EINVAL);
    }
    check_mnt(cx, vfs, target.mnt())?;
    may_mount_on(&target)?;
    if vfs.is_dir(&cx.view(vfs), &target)? {
        return Err(Errno::ENOTDIR);
    }
    let fs = Fs::Nsfs;
    let id = vfs.add_mount(fs, src, (target, at), 0, b"nsfs".to_vec(), Some(fs.dev()));
    vfs.update_mount(id, |m| m.pin = Some(ns));
    cx.shared.fast_changed();
    Ok(0)
}

/// Mount points that must be ENOENT (Linux's `lock_mount`): nameless open files such as deleted
/// files and pipes reached through `/proc/<pid>/fd/<n>` or `exe`; the entries in `/proc/<pid>/fd`
/// and `fdinfo` themselves (reached through an `O_PATH` fd pointing at them; not usable as mount
/// points since Linux 6.12, LTP's mount08).
fn may_mount_on(target: &Node) -> Result<(), Errno> {
    match target {
        Node::Pseudo(
            _,
            PseudoNode::OpenFd(..)
            | PseudoNode::OpenExe(_)
            | PseudoNode::Fd(..)
            | PseudoNode::FdInfo(..),
        ) => Err(Errno::ENOENT),
        _ => Ok(()),
    }
}

/// Mounts the mounts below `root` in the mount `root` is in (those in `snapshot`) the same way
/// into mount `to` (Linux's `copy_tree`).
fn copy_submounts(vfs: &Vfs, ps: &dyn Pseudo, snapshot: &[Mount], root: &Node, to: MountId) {
    for c in snapshot.iter().filter(|c| c.parent() == Some(root.mnt())) {
        let Some(point) = &c.point else {
            continue;
        };
        // A mount on a file searches upwards from the directory it was mounted through
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
        vfs.update_mount(id, |m| m.pin = c.pin.clone());
        copy_submounts(vfs, ps, snapshot, &c.root, id);
    }
}

/// Remount: changes the mount's flags (without `MS_BIND` Linux also changes the superblock's,
/// which is shared between several mounts; here only this one mount changes). Without `MS_BIND`
/// `data` holds options: tmpfs changes its superblock's size and inode count accordingly
/// ([`remount_tmpfs`]).
fn remount(
    cx: &mut Cx<'_>,
    target: &Node,
    flags: u64,
    keep_atime: bool,
    data: Option<&[u8]>,
) -> SysResult {
    let vfs = cx.vfs()?;
    let id = vfs.mount_root(target).ok_or(Errno::EINVAL)?;
    check_mnt(cx, &vfs, id)?;
    let m = vfs.mount(id).ok_or(Errno::EINVAL)?;
    // Without MS_BIND the superblock changes too: a tmpfs's size and inode count; on error the
    // mount's flags do not change either
    if let Some(data) = data
        && m.fs == Fs::Tmpfs
    {
        remount_tmpfs(&vfs, m.dev, data)?;
    }
    let flags = if keep_atime {
        (flags & !ATIME) | (m.flags & ATIME)
    } else {
        flags
    };
    vfs.update_mount(id, |m| m.flags = flags);
    // Once the root filesystem becomes nosymfollow, resolution without the BKL is no longer
    // possible
    cx.shared.fast_changed();
    Ok(0)
}

/// Changes the propagation type (Linux's `do_change_type`): only checks the arguments.
fn change_type(cx: &mut Cx<'_>, target: &Node, flags: u64) -> SysResult {
    cx.vfs()?.mount_root(target).ok_or(Errno::EINVAL)?;
    let kind = flags & !(MS_REC | MS_SILENT);
    if kind & !MS_PROPAGATION != 0 || !kind.is_power_of_two() {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `MS_MOVE` (Linux's `do_move_mount`): moves mount `source`, with the mounts inside it, onto
/// `target`. The source must be the root of a mount and not the root of a mount tree, and the
/// target must not be inside it.
fn move_mount(
    cx: &mut Cx<'_>,
    source: Option<&[u8]>,
    (target, at): (Node, Option<Dentry>),
) -> SysResult {
    let source = source.filter(|s| !s.is_empty()).ok_or(Errno::EINVAL)?;
    let src = lookup_at(cx, AT_FDCWD, source, true)?;
    move_tree(cx, &src, (target, at))
}

/// Moves the mount rooted at `src` onto `target` (Linux's `do_move_mount`, shared by `MS_MOVE` and
/// move_mount): `src` must be the root of a mount attached in the caller's namespace, or the root
/// of a tree copied by open_tree and not yet attached (it is attached, and the tree enters the
/// caller's namespace). The target must be in the caller's namespace, must be a directory if and
/// only if `src` is (EINVAL), and must not be inside the mount being moved (ELOOP).
fn move_tree(cx: &mut Cx<'_>, src: &Node, (target, at): (Node, Option<Dentry>)) -> SysResult {
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    // The root of a mount tree is also mounted (on the invisible rootfs); when it is moved the
    // target is always inside it, which is the ELOOP below
    let id = vfs.mount_root(src).ok_or(Errno::EINVAL)?;
    let anon = vfs
        .mount_ns(id)
        .and_then(|ns| vfs.find_ns(ns))
        .is_some_and(|ns| ns.anon)
        && vfs.mount(id).is_some_and(|m| m.point.is_none());
    if !anon {
        check_mnt(cx, &vfs, id)?;
    }
    check_mnt(cx, &vfs, target.mnt())?;
    if vfs.is_dir(&view, src)? != vfs.is_dir(&view, &target)? {
        return Err(Errno::EINVAL);
    }
    may_mount_on(&target)?;
    if anon {
        vfs.attach_tree(id, (target, at), cx.mnt_ns());
    } else {
        if vfs.is_under(&view, &target, src) {
            return Err(Errno::ELOOP);
        }
        vfs.move_mount(id, Some((target, at)));
    }
    cx.shared.fast_changed();
    Ok(0)
}

// ---- The new mount API (open_tree, move_mount, mount_setattr) ----

const OPEN_TREE_CLONE: u32 = 1;
const OPEN_TREE_CLOEXEC: u32 = crate::fd::oflags::O_CLOEXEC;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_NO_AUTOMOUNT: u32 = 0x800;
const AT_EMPTY_PATH: u32 = 0x1000;
const AT_RECURSIVE: u32 = 0x8000;

/// move_mount flags (`MOVE_MOUNT_*`).
mod move_flags {
    pub const F_SYMLINKS: u32 = 1;
    pub const F_EMPTY_PATH: u32 = 4;
    pub const T_SYMLINKS: u32 = 0x10;
    pub const T_EMPTY_PATH: u32 = 0x40;
    pub const SET_GROUP: u32 = 0x100;
    /// All that 6.1 knows (`MOVE_MOUNT__MASK`).
    pub const MASK: u32 = 0x177;
}

/// Mount attributes (`MOUNT_ATTR_*`).
mod attr {
    pub const RDONLY: u64 = 1;
    pub const NOSUID: u64 = 2;
    pub const NODEV: u64 = 4;
    pub const NOEXEC: u64 = 8;
    pub const ATIME: u64 = 0x70;
    pub const NOATIME: u64 = 0x10;
    pub const STRICTATIME: u64 = 0x20;
    pub const NODIRATIME: u64 = 0x80;
    pub const IDMAP: u64 = 0x10_0000;
    pub const NOSYMFOLLOW: u64 = 0x20_0000;
    pub const VALID: u64 =
        RDONLY | NOSUID | NODEV | NOEXEC | ATIME | NODIRATIME | IDMAP | NOSYMFOLLOW;
}

/// A path argument of the new mount API: resolves `path` from `dirfd` (with `empty` it may be
/// empty, meaning `dirfd` itself), past mount points to the topmost mount. Returns the node and
/// the dentry it was found at.
fn walk_at(
    cx: &Cx<'_>,
    (dirfd, path): (i32, u64),
    empty: bool,
    follow: bool,
) -> Result<(Node, Option<Dentry>), Errno> {
    let path = cx.mm().read_cstr(path, super::PATH_MAX - 1)?;
    let vfs = cx.vfs()?;
    let (node, at) = if path.is_empty() {
        if !empty {
            return Err(Errno::ENOENT);
        }
        let node = if dirfd == AT_FDCWD {
            cx.cwd()
        } else {
            cx.fds().get(dirfd)?.borrow().node().ok_or(Errno::EBADF)?
        };
        (node, None)
    } else {
        let start = start_dir(cx, dirfd, &path)?;
        vfs.walk(
            &cx.view(&vfs),
            &cx.cred(),
            &cx.root(),
            &start,
            &path,
            follow,
        )?
    };
    let top = vfs.mounted(node.clone());
    let at = at.filter(|_| top == node);
    Ok((top, at))
}

/// open_tree(2) (Linux 6.1's `open_tree`, `open_detached_copy`): without `OPEN_TREE_CLONE` it is
/// just an O_PATH fd; with it, it needs `CAP_SYS_ADMIN` and bind-mounts a copy of that path
/// (with the mounts below it under `AT_RECURSIVE`) in an anonymous mount namespace, mounted
/// nowhere; the fd points at its root, move_mount attaches it, and it is torn down if closed
/// without being attached. The source must be mounted in the caller's namespace (EINVAL).
pub(super) fn open_tree(cx: &mut Cx<'_>, dirfd: i32, path: u64, flags: u32) -> SysResult {
    let valid = AT_EMPTY_PATH
        | AT_NO_AUTOMOUNT
        | AT_RECURSIVE
        | AT_SYMLINK_NOFOLLOW
        | OPEN_TREE_CLONE
        | OPEN_TREE_CLOEXEC;
    if flags & !valid != 0 || flags & (AT_RECURSIVE | OPEN_TREE_CLONE) == AT_RECURSIVE {
        return Err(Errno::EINVAL);
    }
    let clone = flags & OPEN_TREE_CLONE != 0;
    if clone && !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    let (node, found) = walk_at(
        cx,
        (dirfd, path),
        flags & AT_EMPTY_PATH != 0,
        flags & AT_SYMLINK_NOFOLLOW == 0,
    )?;
    let o_path = crate::fd::oflags::O_PATH;
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    let file = if clone {
        check_mnt(cx, &vfs, node.mnt())?;
        let dir = vfs.is_dir(&view, &node)?;
        let ns = vfs.anon_ns(cx.t.fs.borrow().mnt_ns.owner.clone());
        // Note the mounts inside the source before mounting (as in bind)
        let inside = if flags & AT_RECURSIVE != 0 {
            vfs.mounts()
        } else {
            Vec::new()
        };
        let id = vfs.clone_mount(&node, found.filter(|_| !dir), ns.id)?;
        copy_submounts(&vfs, &view, &inside, &node, id);
        let file = crate::fd::OpenFile::new(crate::fd::FileKind::Path(node.in_mount(id)), o_path);
        file.borrow_mut().tree = Some(ns);
        cx.shared.fast_changed();
        file
    } else {
        let path = vfs
            .found_path(&view, &cx.root(), &node, found.as_ref())
            .ok()
            .map(std::rc::Rc::from);
        crate::fd::OpenFile::with_path(crate::fd::FileKind::Path(node), o_path, path)
    };
    let cloexec = flags & OPEN_TREE_CLOEXEC != 0;
    Ok(cx.fds_mut().install(file, cloexec, 0)? as u64)
}

/// move_mount(2) (Linux 6.1): needs `CAP_SYS_ADMIN`; source and target are each resolved from a
/// directory fd (with `F_EMPTY_PATH`/`T_EMPTY_PATH` they may be the fd itself, with
/// `F_SYMLINKS`/`T_SYMLINKS` the last symlink is followed), and the move works as `MS_MOVE`
/// ([`move_tree`]). appine has no shared mounts, so `MOVE_MOUNT_SET_GROUP` is always EINVAL (as
/// it is on Linux when the source is not shared).
pub(super) fn sys_move_mount(
    cx: &mut Cx<'_>,
    from: (i32, u64),
    to: (i32, u64),
    flags: u32,
) -> SysResult {
    use move_flags::*;
    if !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    if flags & !MASK != 0 {
        return Err(Errno::EINVAL);
    }
    let (src, _) = walk_at(cx, from, flags & F_EMPTY_PATH != 0, flags & F_SYMLINKS != 0)?;
    let target = walk_at(cx, to, flags & T_EMPTY_PATH != 0, flags & T_SYMLINKS != 0)?;
    if flags & SET_GROUP != 0 {
        return Err(Errno::EINVAL);
    }
    move_tree(cx, &src, target)
}

/// mount_setattr(2) (Linux 6.1's `mount_setattr`, `build_mount_kattr`): changes the flags of a
/// mount (with those below it under `AT_RECURSIVE`). `struct mount_attr` is at least 32 bytes,
/// and anything beyond must be 0; changing the access time requires giving the whole
/// `MOUNT_ATTR__ATIME` in attr_clr as well. Propagation types have no effect after being checked
/// (appine's mounts do not propagate to each other). Idmapped mounts (`MOUNT_ATTR_IDMAP`) are
/// EINVAL after the userns fd is checked, the same as for a filesystem that does not support
/// them.
pub(super) fn mount_setattr(
    cx: &mut Cx<'_>,
    (dirfd, path): (i32, u64),
    flags: u32,
    uattr: u64,
    size: usize,
) -> SysResult {
    if flags & !(AT_EMPTY_PATH | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        return Err(Errno::EINVAL);
    }
    if size > 4096 {
        return Err(Errno::E2BIG);
    }
    if size < 32 {
        return Err(Errno::EINVAL);
    }
    if !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    let mut raw = vec![0u8; size];
    cx.mm()
        .read(crate::uaccess::UserSlice::new(uattr, size), &mut raw)?;
    if raw[32..].iter().any(|&b| b != 0) {
        return Err(Errno::E2BIG);
    }
    let field = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (set, clr, propagation, userns_fd) = (field(0), field(1), field(2), field(3));
    if set == 0 && clr == 0 && propagation == 0 {
        return Ok(0);
    }
    if ![0, MS_UNBINDABLE, MS_PRIVATE, MS_SLAVE, MS_SHARED].contains(&propagation)
        || (set | clr) >> 32 != 0
        || (set | clr) & !attr::VALID != 0
    {
        return Err(Errno::EINVAL);
    }
    // Access time: to change it, attr_clr gives the whole MOUNT_ATTR__ATIME and attr_set one of
    // the three
    let atime = if clr & attr::ATIME != 0 {
        if clr & attr::ATIME != attr::ATIME {
            return Err(Errno::EINVAL);
        }
        match set & attr::ATIME {
            0 => Some(ms::RELATIME),
            attr::NOATIME => Some(ms::NOATIME),
            attr::STRICTATIME => Some(0),
            _ => return Err(Errno::EINVAL),
        }
    } else if set & attr::ATIME != 0 {
        return Err(Errno::EINVAL);
    } else {
        None
    };
    if (set | clr) & attr::IDMAP != 0 {
        if clr & attr::IDMAP != 0 || userns_fd > i32::MAX as u64 {
            return Err(Errno::EINVAL);
        }
        let file = cx.fds().get(userns_fd as i32).map_err(|_| Errno::EBADF)?;
        let ns = match &file.borrow().kind {
            crate::fd::FileKind::Proc {
                ns: Some(NsRef::User(ns)),
                ..
            } => ns.clone(),
            _ => return Err(Errno::EINVAL),
        };
        if Rc::ptr_eq(&ns, &crate::cred::UserNs::init()) {
            return Err(Errno::EPERM);
        }
        return Err(Errno::EINVAL);
    }
    let (node, _) = walk_at(
        cx,
        (dirfd, path),
        flags & AT_EMPTY_PATH != 0,
        flags & AT_SYMLINK_NOFOLLOW == 0,
    )?;
    let vfs = cx.vfs()?;
    let id = vfs.mount_root(&node).ok_or(Errno::EINVAL)?;
    // In the caller's namespace, or copied by open_tree and not yet attached
    let anon = vfs
        .mount_ns(id)
        .and_then(|ns| vfs.find_ns(ns))
        .is_some_and(|ns| ns.anon);
    if !anon {
        check_mnt(cx, &vfs, id)?;
    }
    let bit = |a: u64, m: u64| if set & a != 0 { m } else { 0 };
    let set_ms = bit(attr::RDONLY, ms::RDONLY)
        | bit(attr::NOSUID, ms::NOSUID)
        | bit(attr::NODEV, ms::NODEV)
        | bit(attr::NOEXEC, ms::NOEXEC)
        | bit(attr::NODIRATIME, ms::NODIRATIME)
        | bit(attr::NOSYMFOLLOW, ms::NOSYMFOLLOW);
    let cleared = |a: u64, m: u64| if clr & a != 0 { m } else { 0 };
    let clr_ms = cleared(attr::RDONLY, ms::RDONLY)
        | cleared(attr::NOSUID, ms::NOSUID)
        | cleared(attr::NODEV, ms::NODEV)
        | cleared(attr::NOEXEC, ms::NOEXEC)
        | cleared(attr::NODIRATIME, ms::NODIRATIME)
        | cleared(attr::NOSYMFOLLOW, ms::NOSYMFOLLOW);
    let targets = if flags & AT_RECURSIVE != 0 {
        vfs.subtree(id)
    } else {
        vec![id]
    };
    for t in targets {
        vfs.update_mount(t, |m| {
            m.flags = (m.flags & !clr_ms) | set_ms;
            if let Some(a) = atime {
                m.flags = (m.flags & !(ms::RELATIME | ms::NOATIME)) | a;
            }
        });
    }
    cx.shared.fast_changed();
    Ok(0)
}

/// pivot_root(2) (Linux 6.1's `pivot_root`): the mount of `new_root` takes the place of the mount
/// the caller's root directory is in, and the old root is mounted on `put_old`; processes whose
/// root directory or cwd was the old root directory switch to `new_root` (`chroot_fs_refs`).
pub(super) fn pivot_root(cx: &mut Cx<'_>, new_root: u64, put_old: u64) -> SysResult {
    if !may_mount(cx) {
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
    // When something is mounted on put_old, use the topmost one (lock_mount)
    let old = vfs.mounted(old);
    check_mnt(cx, &vfs, new.mnt())?;
    check_mnt(cx, &vfs, root.mnt())?;
    check_mnt(cx, &vfs, old.mnt())?;
    let (Some(new_mnt), Some(root_mnt)) = (vfs.mount(new.mnt()), vfs.mount(root.mnt())) else {
        return Err(Errno::EINVAL);
    };
    // Deleted directories (d_unlinked)
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
    // The root directory and new_root must both be mount roots, put_old must be below new_root,
    // and new_root must be below the root directory
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
    if !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    let vfs = cx.vfs()?;
    // Paths such as `.` and the cwd also unmount what is mounted on them (LOOKUP_MOUNTPOINT)
    let node = vfs.mounted(node);
    let id = vfs.mount_root(&node).ok_or(Errno::EINVAL)?;
    check_mnt(cx, &vfs, id)?;
    let own_root = id == cx.root().mnt();
    let detach = flags & MNT_DETACH != 0;
    if flags & MNT_EXPIRE != 0 {
        if own_root || flags & (MNT_FORCE | MNT_DETACH) != 0 {
            return Err(Errno::EINVAL);
        }
        if busy(cx, &vfs, id) {
            return Err(Errno::EBUSY);
        }
        // The first time only marks it; if nobody has used it, the next time unmounts it. Linux
        // removes the mark once somebody has used it; here it is not removed
        if !vfs.mount(id).is_some_and(|m| m.expiring) {
            vfs.update_mount(id, |m| m.expiring = true);
            return Err(Errno::EAGAIN);
        }
    }
    // The root of a mount tree cannot be unmounted; the mount the caller's root directory is in
    // can only be unmounted with MNT_DETACH (without it, Linux remounts the root filesystem
    // read-only)
    if vfs.mount(id).is_some_and(|m| m.point.is_none()) {
        return Err(if detach { Errno::EINVAL } else { Errno::EBUSY });
    }
    if !detach && (own_root || busy(cx, &vfs, id)) {
        return Err(Errno::EBUSY);
    }
    vfs.remove_mount(id);
    vfs.collect(&in_use(cx.kr()));
    cx.shared.fast_changed();
    Ok(0)
}

/// Whether mounts may be changed (Linux's `may_mount`): `CAP_SYS_ADMIN` in the owner (a user
/// namespace) of the mount namespace.
fn may_mount(cx: &Cx<'_>) -> bool {
    let owner = cx.t.fs.borrow().mnt_ns.owner.clone();
    cx.cred().ns_capable(&owner, cap::SYS_ADMIN)
}

/// Mount `id` is attached to the tree and in the caller's mount namespace (Linux's `check_mnt`);
/// otherwise EINVAL.
fn check_mnt(cx: &Cx<'_>, vfs: &Vfs, id: MountId) -> Result<(), Errno> {
    match vfs.mount_ns(id) {
        Some(ns) if ns == cx.mnt_ns() => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// Some mount namespace is no longer used (the last process using it exited): unmount its mounts
/// and drop those nobody uses.
pub(crate) fn collect_namespaces(k: &crate::kernel::Kernel, vfs: &Vfs) {
    if vfs.has_dead_ns() {
        vfs.collect(&in_use(k));
    }
}

/// Mount `id` is in use: other mounts are mounted inside it, or a process's root directory, cwd
/// or open file is in it (terminals opened through paths in devpts count too).
fn busy(cx: &Cx<'_>, vfs: &Vfs, id: MountId) -> bool {
    !vfs.children(id).is_empty() || in_use(cx.kr()).contains(&id)
}

/// The mounts that processes' root directories, cwds and open files are in.
fn in_use(k: &crate::kernel::Kernel) -> HashSet<MountId> {
    let mut used = HashSet::new();
    for task in k.tasks.values() {
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
            if let crate::fd::FileKind::Pty(end) = &file.borrow().kind
                && end.pin
            {
                used.insert(end.mnt);
            }
        }
    }
    used
}
