//! mount(2)、umount2(2)、pivot_root(2)（设计文档 §8.2）。
//!
//! 合成文件系统（procfs、sysfs、devpts、cgroup2）可以挂到任意目录上；仓库里的和合成文件
//! 系统里的目录、文件都可以 bind 挂载（`MS_REC` 带上挂在里面的挂载），命名空间文件也可以
//! （nsfs，挂载留住那个命名空间），挂载的标志、重新
//! 挂载、`MS_MOVE`、卸下（`MNT_DETACH`、`MNT_EXPIRE`）、pivot_root 和各种错误按 Linux 6.1
//! 的 `path_mount`、`path_umount`、`pivot_root`。传播类型可以设置，但不模拟：挂载之间没有
//! 传播，都相当于 private。挂载要在调用者的挂载命名空间里（Linux 的 `check_mnt`），别的
//! 命名空间里的（经由 `/proc/<pid>/root` 看到的）是 EINVAL。tmpfs 是仓库里不挂在根目录下面
//! 的一棵树（`Vfs::new_tmpfs`）。
//! 别的文件系统与没有编进内核的一样是 ENODEV。

use std::collections::HashSet;

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
    // 选项只有 tmpfs 看；Linux 复制最多一页，读不了第一个字节是 EFAULT
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
        // 挂载点上已经挂着东西时挂在最上面的那个上（Linux 的 lock_mount）：`.`、当前目录
        // 这类不经过查找的路径也一样
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
    // 挂载的标志，默认 relatime
    let mut mnt = flags & ms::PER_MOUNT & !ms::RELATIME;
    if flags & ms::NOATIME == 0 {
        mnt |= ms::RELATIME;
    }
    if flags & MS_STRICTATIME != 0 {
        mnt &= !(ms::RELATIME | ms::NOATIME);
    }
    // remount 不往上找：经由魔法链接给出的被盖住的节点不是挂载的根（EINVAL）
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
    data: &[u8],
) -> SysResult {
    let fstype = fstype.ok_or(Errno::EINVAL)?;
    let Some(fs) = Fs::from_type(&fstype) else {
        let name = String::from_utf8_lossy(&fstype);
        crate::debug::log(format_args!("mount: no filesystem type {name}"));
        return Err(Errno::ENODEV);
    };
    // cgroup2 可以不带只读挂上（crun 给容器挂 /sys/fs/cgroup 时就这样），只是里面的文件改不了
    // （[`Vfs::readonly`]）
    let vfs = cx.vfs()?;
    may_mount_on(&target)?;
    check_mnt(cx, &vfs, target.mnt())?;
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
    if fs == Fs::Tmpfs {
        let params = tmpfs_options(data, &cx.cred())?;
        let source = source.unwrap_or_default();
        vfs.new_tmpfs((target, at), flags, source, params)?;
        cx.shared.fast_changed();
        return Ok(0);
    }
    let dev = (!fs.own_dev()).then(|| fs.dev());
    let id = vfs.add_mount(
        fs,
        fs.root(),
        (target, at),
        flags,
        source.unwrap_or_default(),
        dev,
    );
    // procfs 里的 pid 按挂上它的进程所在的 pid 命名空间算
    let pid_ns = cx.pid_ns();
    if fs == Fs::Proc && pid_ns.level > 0 {
        vfs.set_proc_pid_ns(id, pid_ns);
    }
    cx.shared.fast_changed();
    Ok(0)
}

/// tmpfs 的挂载选项里给了的那些（Linux 6.1 的 `shmem_parse_one` 解析出来的
/// `shmem_options`）。不认识的选项是 EINVAL。
#[derive(Default)]
struct TmpfsOptions {
    perm: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    /// 页数、inode 数（0 是不限）。
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
            // 通用的超级块标志（`vfs_parse_sb_flag`），mount(8) 本来就会换成 MS_* 标志
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

/// 新挂的 tmpfs 的参数：没给的用默认值（权限 1777，属主、属组是挂载的进程的 fsuid、
/// fsgid，大小和 inode 数是物理内存的一半，inode64）。
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

/// 重新挂载 tmpfs 时改它的超级块（Linux 6.1 的 `shmem_reconfigure`）：给了非 0 的
/// `size=`、`nr_inodes=` 时，原来不限的不能改成限制、不能小于已经用掉的（都是 EINVAL）；
/// 给了的大小、inode 数（0 是改成不限）和 inode32/64 照改；`mode=`、`uid=`、`gid=` 不变。
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

/// 无符号数，前缀与 C 的 `strtoull(…, 0)` 相同（`0x` 十六进制、`0` 八进制）。
fn parse_num(s: &[u8]) -> Option<u64> {
    let (n, rest) = leading_num(s)?;
    rest.is_empty().then_some(n)
}

/// 开头的无符号数和剩下的部分。
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

/// Linux 的 `memparse`：数后面可以跟 K、M、G、T、P、E（不分大小写）。返回剩下的部分。
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
    if let Node::Pseudo(_, PseudoNode::NsFile(kind, ns)) = src {
        return bind_ns(cx, &vfs, kind, ns, src, (target, at));
    }
    may_mount_on(&target)?;
    let dir = vfs.is_dir(&view, &src)?;
    if dir != vfs.is_dir(&view, &target)? {
        return Err(Errno::ENOTDIR);
    }
    // 卸下了的、别的命名空间里的不能 bind（Linux 的 check_mnt）
    check_mnt(cx, &vfs, src.mnt())?;
    check_mnt(cx, &vfs, target.mnt())?;
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

/// bind 挂载命名空间文件（`ip netns add` 这样留住命名空间）：挂载持有那个命名空间
/// （[`Mount::pin`]），在 nsfs 里，标志只有 rw。与 Linux 的 do_loopback 一样，经由别的挂载
/// 命名空间里的路径找到的也行；挂载命名空间的文件只能挂进比它旧的命名空间（`mnt_ns_loop`：
/// 不然命名空间可能经由挂载留着自己）。
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

/// 挂载点不能是 ENOENT 的那些（Linux 的 `lock_mount`）：经由 `/proc/<pid>/fd/<n>`、`exe` 到了
/// 已经删除的文件、管道这类没有名字的打开文件；`/proc/<pid>/fd`、`fdinfo` 里的项本身（经由
/// 指向它的 `O_PATH` fd 到达，Linux 6.12 起不能当挂载点，LTP 的 mount08）。
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
        vfs.update_mount(id, |m| m.pin = c.pin.clone());
        copy_submounts(vfs, ps, snapshot, &c.root, id);
    }
}

/// 重新挂载：改挂载的标志（Linux 不带 `MS_BIND` 时还改超级块的，超级块在几个挂载之间
/// 共用，这里只改这一个挂载）。不带 `MS_BIND` 时 `data` 是选项：tmpfs 照它改超级块的
/// 大小、inode 数（[`remount_tmpfs`]）。
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
    // 不带 MS_BIND 时还改超级块：tmpfs 的大小、inode 数，出错时挂载的标志也不改
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
    check_mnt(cx, &vfs, target.mnt())?;
    // 挂载树的根也挂在（看不到的 rootfs）上，挪它时目标总在它里面，是下面的 ELOOP
    let id = vfs.mount_root(&src).ok_or(Errno::EINVAL)?;
    check_mnt(cx, &vfs, id)?;
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
    // put_old 上挂着东西时挂在最上面的那个上（lock_mount）
    let old = vfs.mounted(old);
    check_mnt(cx, &vfs, new.mnt())?;
    check_mnt(cx, &vfs, root.mnt())?;
    check_mnt(cx, &vfs, old.mnt())?;
    let (Some(new_mnt), Some(root_mnt)) = (vfs.mount(new.mnt()), vfs.mount(root.mnt())) else {
        return Err(Errno::EINVAL);
    };
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
    if !may_mount(cx) {
        return Err(Errno::EPERM);
    }
    let vfs = cx.vfs()?;
    // `.`、当前目录这类路径也卸挂在上面的（LOOKUP_MOUNTPOINT）
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
    vfs.collect(&in_use(cx.kr()));
    cx.shared.fast_changed();
    Ok(0)
}

/// 能不能动挂载（Linux 的 `may_mount`）：在挂载命名空间的属主（用户命名空间）里有
/// `CAP_SYS_ADMIN`。
fn may_mount(cx: &Cx<'_>) -> bool {
    let owner = cx.t.fs.borrow().mnt_ns.owner.clone();
    cx.cred().ns_capable(&owner, cap::SYS_ADMIN)
}

/// 挂载 `id` 挂在树上、在调用者的挂载命名空间里（Linux 的 `check_mnt`），否则 EINVAL。
fn check_mnt(cx: &Cx<'_>, vfs: &Vfs, id: MountId) -> Result<(), Errno> {
    match vfs.mount_ns(id) {
        Some(ns) if ns == cx.mnt_ns() => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// 有挂载命名空间没人用了（最后一个用它的进程退出了）：卸下它的挂载，丢掉没人用的。
pub(crate) fn collect_namespaces(k: &crate::kernel::Kernel, vfs: &Vfs) {
    if vfs.has_dead_ns() {
        vfs.collect(&in_use(k));
    }
}

/// 挂载 `id` 在用：里面挂着别的挂载，或者有进程的根目录、当前目录、打开的文件在里面。
/// 从设备的打开文件不记得经由哪个 devpts 挂载，不算。
fn busy(cx: &Cx<'_>, vfs: &Vfs, id: MountId) -> bool {
    !vfs.children(id).is_empty() || in_use(cx.kr()).contains(&id)
}

/// 进程的根目录、当前目录和打开的文件所在的挂载。
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
        }
    }
    used
}
