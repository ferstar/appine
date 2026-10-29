//! 虚拟文件系统（设计文档 §8）。
//!
//! 根文件系统是 inode 仓库，`/dev` 里的设备节点也存在仓库里（[`dev`]）。procfs、
//! devpts、sysfs 和 cgroup2 是合成文件系统：它们的内容来自内核状态，由内核层经由
//! [`Pseudo`] 提供。开机时挂在 `/proc`、`/dev/pts`、`/sys`、`/sys/fs/cgroup` 上，之后
//! 可以用 mount(2) 挂到别的目录上，也可以 bind 挂载它们里面的目录（[`Mount`]）；仓库里的
//! 目录不能 bind，也没有 tmpfs。合成节点带着它所在的挂载，`..` 和 getcwd 经由挂载点回到
//! 上一层。路径解析在整棵树里进行，符号链接最多跟随 40 层；经过的每个仓库目录都要有搜索
//! 权限。进程的根目录可以被 chroot 换掉：绝对路径从它开始，`..` 不会越过它。

pub mod dev;
pub mod import;
pub mod store;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC};
use crate::errno::Errno;
pub(crate) use crate::pseudo::PseudoNode;
use store::{Ino, ROOT, Stat, Store, mode};

/// 跟随符号链接的最大次数，与 Linux 的 MAXSYMLINKS 一致。
pub(crate) const MAX_SYMLINKS: u32 = 40;
/// 路径组件的最大长度（`NAME_MAX`）。
const NAME_MAX: usize = 255;

/// 文件系统树里的一个节点。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    /// inode 仓库里的 inode。
    Store(Ino),
    /// 合成文件系统里的节点和经由的挂载（Linux 的 `struct path` 里的 vfsmount）：同一个
    /// 节点挂在两处时是两个节点，`..` 各自回到自己的挂载点。
    Pseudo(MountId, PseudoNode),
}

impl Node {
    /// 根目录。
    pub const ROOT: Node = Node::Store(ROOT);
}

/// 挂载的编号（mountinfo 的第一列、fdinfo 的 `mnt_id`）。
pub(crate) type MountId = u32;
/// 根文件系统（inode 仓库）。
pub(crate) const ROOT_MOUNT: MountId = 1;
/// 开机时挂在 `/proc` 上的 procfs。
pub(crate) const PROC_MOUNT: MountId = 2;
/// 开机时挂在 `/dev/pts` 上的 devpts。
pub(crate) const PTS_MOUNT: MountId = 3;
/// 开机时挂在 `/sys` 上的 sysfs。
pub(crate) const SYS_MOUNT: MountId = 4;
/// 不在任何文件系统里的打开文件（管道、socket 等）在 fdinfo 里的 `mnt_id`。
pub(crate) const ANON_MOUNT: MountId = 5;
/// 开机时挂在 `/sys/fs/cgroup` 上的 cgroup2。
pub(crate) const CGROUP_MOUNT: MountId = 6;

/// 合成文件系统的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Fs {
    /// procfs。
    Proc,
    /// devpts。
    Pts,
    /// sysfs。
    Sys,
    /// 只读的 cgroup2。
    Cgroup,
}

impl Fs {
    /// mount(2) 的文件系统类型（与 `/proc/filesystems` 里的名字相同）。
    pub(crate) fn from_type(name: &[u8]) -> Option<Fs> {
        match name {
            b"proc" => Some(Fs::Proc),
            b"devpts" => Some(Fs::Pts),
            b"sysfs" => Some(Fs::Sys),
            b"cgroup2" => Some(Fs::Cgroup),
            _ => None,
        }
    }

    /// 文件系统类型的名字。
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            Fs::Proc => "proc",
            Fs::Pts => "devpts",
            Fs::Sys => "sysfs",
            Fs::Cgroup => "cgroup2",
        }
    }

    /// 开机时挂上的实例的设备号（stat 的 `st_dev`、mountinfo 的第三列）。
    pub(crate) const fn dev(self) -> u64 {
        match self {
            Fs::Proc => 0x4,
            Fs::Pts => 0x18,
            Fs::Sys => 0x16,
            Fs::Cgroup => 0x1a,
        }
    }

    /// 每次挂载是一个新的实例（Linux 5.8 起的 procfs、4.7 起的 devpts）：同一处可以再挂
    /// 一个。sysfs、cgroup2 在同一个命名空间里只有一个实例，同一个挂载的根上再挂是 EBUSY。
    pub(crate) const fn new_instance(self) -> bool {
        matches!(self, Fs::Proc | Fs::Pts)
    }

    /// 新挂上的有自己的设备号：只有 procfs。devpts 的新实例在 Linux 上有自己的一套终端
    /// （编号各自从 0 开始）和设备号，这里各个挂载看到的仍是同一套终端，设备号也相同：
    /// 从设备的 fstat 报告的是它，ttyname 要拿它比对 `/dev/pts/<n>` 的 `st_dev`。
    pub(crate) const fn own_dev(self) -> bool {
        matches!(self, Fs::Proc)
    }

    /// 超级块的选项（mountinfo 最后一列 `rw` 之后的部分）。
    pub(crate) const fn options(self) -> &'static str {
        match self {
            Fs::Pts => ",gid=5,mode=620,ptmxmode=000",
            Fs::Cgroup => ",nsdelegate,memory_recursiveprot",
            Fs::Proc | Fs::Sys => "",
        }
    }
}

/// 挂载的标志：与 mount(2) 的 `MS_*` 取值相同。
pub(crate) mod ms {
    pub const RDONLY: u64 = 1;
    pub const NOSUID: u64 = 2;
    pub const NODEV: u64 = 4;
    pub const NOEXEC: u64 = 8;
    pub const NOSYMFOLLOW: u64 = 0x100;
    pub const NOATIME: u64 = 0x400;
    pub const NODIRATIME: u64 = 0x800;
    pub const RELATIME: u64 = 1 << 21;
    /// 记在挂载上的（Linux 的 `MNT_*` 对应的那些）。
    pub const PER_MOUNT: u64 =
        RDONLY | NOSUID | NODEV | NOEXEC | NOSYMFOLLOW | NOATIME | NODIRATIME | RELATIME;
}

/// 根文件系统的挂载标志：`rw,relatime`。
pub(crate) const ROOT_FLAGS: u64 = ms::RELATIME;
/// 根文件系统（inode 仓库）对外报告的设备号（8:1）。
pub(crate) const ROOT_DEV: u64 = 0x801;

/// 一个挂载（Linux 的 `struct mount`）：合成文件系统 `fs` 里的 `root` 出现在 `point` 上。
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub id: MountId,
    pub fs: Fs,
    /// 挂载看到的部分的根：文件系统的根目录，bind 挂载时是被 bind 的那个节点。
    pub root: PseudoNode,
    /// 挂载点：仓库里的目录（bind 挂载文件时是文件），或者别的挂载里的目录。
    pub point: Node,
    /// 挂载点是仓库里的文件时，它所在的目录和名字：文件可能有几个链接，没有唯一的上级
    /// 目录，路径要按挂上时的这个算。
    pub at: Option<Dentry>,
    /// [`ms`] 里的标志。
    pub flags: u64,
    /// 实例的设备号（[`Fs::own_dev`]）。
    pub dev: u64,
    /// mount(2) 的 source（mountinfo 的第十列）。
    pub source: Vec<u8>,
    /// `umount2(MNT_EXPIRE)` 标记过：再来一次就卸下。
    pub expiring: bool,
}

/// 合成文件系统的目录结构（内核层实现，内容来自内核状态）。
pub(crate) trait Pseudo {
    /// 上级目录；根目录返回 None。
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode>;
    /// 节点在上级目录里的名字。
    fn name(&self, node: &PseudoNode) -> Vec<u8>;
    /// 在目录里查找。
    fn lookup(&self, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno>;
    /// 文件类型位（`S_IFMT` 部分）。
    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno>;
    /// 符号链接的内容。
    fn readlink(&self, node: &PseudoNode) -> Result<Vec<u8>, Errno>;
    /// "魔法链接"（`/proc/<pid>/fd/<n>` 等）跟随之后直接得到的节点，不按内容解析。
    /// `mnt` 是链接所在的挂载。
    fn follow(&self, mnt: MountId, node: &PseudoNode) -> Option<Node>;
}

/// 一个虚拟文件系统实例。
pub struct Vfs {
    store: Store,
    /// 挂载表：开机时的几个和后来 mount(2) 挂上的，按挂上的先后。
    mounts: RefCell<Mounts>,
    /// 挂载表变了就重新生成。
    cached: RefCell<Cached>,
    /// 最近执行过的程序文件（[`Vfs::executable`]）。
    executables: crate::loader::ExeCache,
}

struct Mounts {
    list: Vec<Mount>,
    /// 下一个挂载的编号。卸下的编号不再使用：卸下（`MNT_DETACH`）之后还开着的文件、
    /// 当前目录仍然带着原来的编号，不能算到新的挂载上。
    next: MountId,
    /// 下一个新实例的设备号（0:48 起，避开 [`Fs::dev`] 和管道、socket 等用的）。
    next_dev: u64,
}

/// 不持 BKL 解析路径要用的部分（`syscall::fast`）：仓库的元数据缓存和挂载点。
#[derive(Clone)]
pub(crate) struct Cached {
    meta: Arc<store::Meta>,
    mounts: Arc<[Ino]>,
}

impl Cached {
    /// 只用元数据缓存解析路径并取得元数据：相当于凭据有 `CAP_DAC_READ_SEARCH`（不检查
    /// 搜索权限）时的 [`Vfs::resolve`] 加上 [`Store::stat`]。`root`、`cwd` 是仓库里的
    /// 目录。路径进入合成文件系统，或者要用的东西不在缓存里时返回 None，调用方改为持有
    /// BKL 正常解析（顺带填进缓存）。
    pub(crate) fn stat(
        &self,
        root: Ino,
        cwd: Ino,
        path: &[u8],
        follow: bool,
    ) -> Option<Result<Stat, Errno>> {
        let r = self.meta.reader()?;
        match self.walk(&r, root, cwd, path, follow)? {
            Ok(ino) => r.stat(ino).map(Ok),
            Err(e) => Some(Err(e)),
        }
    }

    /// 与 [`Vfs::walk_in`] 一一对应。
    fn walk(
        &self,
        r: &store::Reader<'_>,
        root: Ino,
        cwd: Ino,
        path: &[u8],
        follow: bool,
    ) -> Option<Result<Ino, Errno>> {
        if path.is_empty() {
            return Some(Err(Errno::ENOENT));
        }
        let mut dir = if path[0] == b'/' { root } else { cwd };
        // 还没处理的组件，栈顶是下一个；不分配内存，放不下时交给正常的解析
        let mut pending = Pending::default();
        pending.push(path)?;
        let mut links = 0;
        while let Some(comp) = pending.pop() {
            let last = pending.is_empty();
            if comp.len() > NAME_MAX {
                return Some(Err(Errno::ENAMETOOLONG));
            }
            if r.kind(dir)? != mode::S_IFDIR {
                return Some(Err(Errno::ENOTDIR));
            }
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                if dir != root {
                    dir = r.parent(dir)?;
                }
                // 上级目录上挂着东西（当前目录在挂载点下面、后来才挂上的）
                if self.mounts.contains(&dir) {
                    return None;
                }
                continue;
            }
            let Some(node) = r.lookup(dir, comp)? else {
                return Some(Err(Errno::ENOENT));
            };
            if self.mounts.contains(&node) {
                return None;
            }
            if r.kind(node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Some(Err(Errno::ELOOP));
                }
                let target = r.readlink(node)?;
                if target.first() == Some(&b'/') {
                    dir = root;
                }
                pending.push(target)?;
                continue;
            }
            dir = node;
        }
        Some(Ok(dir))
    }
}

/// [`Cached::walk`] 还没处理的路径组件（[`components`] 的结果倒过来放）。
struct Pending<'p> {
    comps: [&'p [u8]; 64],
    len: usize,
}

impl Default for Pending<'_> {
    fn default() -> Self {
        Pending {
            comps: [&[]; 64],
            len: 0,
        }
    }
}

impl<'p> Pending<'p> {
    /// 把 `path` 的组件放到最前面；放不下时返回 None。
    fn push(&mut self, path: &'p [u8]) -> Option<()> {
        if path.ends_with(b"/") {
            self.push_one(b".")?;
        }
        for c in path.rsplit(|&b| b == b'/').filter(|c| !c.is_empty()) {
            self.push_one(c)?;
        }
        Some(())
    }

    fn push_one(&mut self, comp: &'p [u8]) -> Option<()> {
        *self.comps.get_mut(self.len)? = comp;
        self.len += 1;
        Some(())
    }

    fn pop(&mut self) -> Option<&'p [u8]> {
        self.len = self.len.checked_sub(1)?;
        Some(self.comps[self.len])
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Vfs {
    /// 打开 `dir` 里的 inode 仓库作为根文件系统，补齐标准的设备节点，挂上 procfs、devpts、
    /// sysfs 和 cgroup2。
    /// 开机（没有别的进程在用这个仓库）时清空 Linux 上是 tmpfs 的 /dev/shm 和 /run：
    /// 共享内存、命名信号量和 pid 文件不能留到下次。
    pub fn open(dir: &Path) -> Result<Vfs, Errno> {
        let store = Store::open(dir, &[b"dev/shm", b"run"])?;
        if let Err(e) = dev::ensure_dev_nodes(&store) {
            crate::debug::log(format_args!("cannot create the device nodes: {e}"));
        }
        if let Err(e) = ensure_resolv_conf(&store) {
            crate::debug::log(format_args!("cannot create /etc/resolv.conf: {e}"));
        }
        // 与 Linux 发行版的默认挂载选项相同
        let flags = ms::NOSUID | ms::NODEV | ms::NOEXEC | ms::RELATIME;
        let boot = [
            (PROC_MOUNT, Fs::Proc, &b"proc"[..], flags, &b"proc"[..]),
            (
                PTS_MOUNT,
                Fs::Pts,
                b"dev/pts",
                flags & !ms::NODEV,
                b"devpts",
            ),
            (SYS_MOUNT, Fs::Sys, b"sys", flags, b"sysfs"),
        ];
        let mut list = Vec::new();
        for (id, fs, path, flags, source) in boot {
            let mut ino = Some(ROOT);
            for name in path.split(|&b| b == b'/') {
                ino = ino.and_then(|i| store.lookup(i, name).ok().flatten());
            }
            match ino {
                Some(ino) if store.stat(ino)?.is_dir() => list.push(Mount {
                    id,
                    fs,
                    root: crate::pseudo::root_of(fs),
                    point: Node::Store(ino),
                    at: None,
                    flags,
                    dev: fs.dev(),
                    source: source.to_vec(),
                    expiring: false,
                }),
                _ => crate::debug::log(format_args!("no mount point for {fs:?}")),
            }
        }
        // cgroup2 挂在 sysfs 的 fs/cgroup 上，只读（§8.2）
        if list.iter().any(|m| m.id == SYS_MOUNT) {
            list.push(Mount {
                id: CGROUP_MOUNT,
                fs: Fs::Cgroup,
                root: crate::pseudo::root_of(Fs::Cgroup),
                point: Node::Pseudo(SYS_MOUNT, crate::pseudo::CGROUP_POINT),
                at: None,
                flags: flags | ms::RDONLY,
                dev: Fs::Cgroup.dev(),
                source: b"cgroup".to_vec(),
                expiring: false,
            });
        }
        let mounts = Mounts {
            list,
            next: CGROUP_MOUNT + 1,
            next_dev: 0x30,
        };
        let cached = Cached {
            meta: store.meta().clone(),
            mounts: mounts.store_points(),
        };
        Ok(Vfs {
            store,
            mounts: RefCell::new(mounts),
            cached: RefCell::new(cached),
            executables: Default::default(),
        })
    }

    /// 打开仓库里的普通文件 `ino` 作为要执行的文件（[`crate::loader::ExeCache`]）。
    pub(crate) fn executable(&self, ino: Ino) -> Result<crate::loader::Executable, Errno> {
        self.executables
            .open(ino, || self.store.open_object(ino, false))
    }

    /// 丢掉记着的程序文件（宿主内存紧张时）。
    pub(crate) fn trim(&self) {
        self.executables.clear();
    }

    /// 底层的 inode 仓库。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 不持 BKL 解析路径要用的部分。挂载表变了之后（[`Shared::fast_changed`]）要重新取。
    ///
    /// [`Shared::fast_changed`]: crate::kernel::Shared::fast_changed
    pub(crate) fn cached(&self) -> Cached {
        self.cached.borrow().clone()
    }

    /// 节点上挂着东西时换成挂在上面的挂载的根；几个挂载叠在一起时一直换到最上面的
    /// （Linux 的 `follow_mount`）。
    fn mounted(&self, node: Node) -> Node {
        let mounts = self.mounts.borrow();
        let mut node = node;
        while let Some(m) = mounts.list.iter().rev().find(|m| m.point == node) {
            node = Node::Pseudo(m.id, m.root.clone());
        }
        node
    }

    /// `node` 是一个挂载的根时，那个挂载的挂载点。
    fn point_of(&self, node: &Node) -> Option<Node> {
        self.root_point(node).map(|(point, _)| point)
    }

    /// `node` 是一个挂载的根时，那个挂载的挂载点和 [`Mount::at`]。
    fn root_point(&self, node: &Node) -> Option<(Node, Option<Dentry>)> {
        let Node::Pseudo(id, p) = node else {
            return None;
        };
        let mounts = self.mounts.borrow();
        let m = mounts.list.iter().find(|m| m.id == *id)?;
        (m.root == *p).then(|| (m.point.clone(), m.at.clone()))
    }

    /// 节点的文件类型位。
    pub(crate) fn kind(&self, ps: &dyn Pseudo, node: &Node) -> Result<u32, Errno> {
        match node {
            Node::Store(ino) => self.store.kind(*ino),
            Node::Pseudo(_, p) => ps.kind(p),
        }
    }

    /// 是不是目录。
    pub(crate) fn is_dir(&self, ps: &dyn Pseudo, node: &Node) -> Result<bool, Errno> {
        Ok(self.kind(ps, node)? == mode::S_IFDIR)
    }

    /// 在目录里查找一个名字（不是 `.` 或 `..`）。
    fn lookup(&self, ps: &dyn Pseudo, dir: &Node, name: &[u8]) -> Result<Node, Errno> {
        let child = match dir {
            Node::Store(ino) => Node::Store(self.store.lookup(*ino, name)?.ok_or(Errno::ENOENT)?),
            Node::Pseudo(id, p) => Node::Pseudo(*id, ps.lookup(p, name)?),
        };
        Ok(self.mounted(child))
    }

    /// 目录的上级目录；挂载的根的上级是挂载点的上级（getdents 的 `..`）。
    pub(crate) fn parent(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        let mut dir = dir.clone();
        while let Some(point) = self.point_of(&dir) {
            dir = point;
        }
        self.parent_in_fs(ps, &dir)
    }

    /// 同一个文件系统里的上级目录。卸下了的挂载（`MNT_DETACH`）的根没有上级，是它自己。
    fn parent_in_fs(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        Ok(match dir {
            Node::Store(ino) => Node::Store(self.store.parent(*ino)?),
            Node::Pseudo(id, p) => ps.parent(p).map_or(dir.clone(), |up| Node::Pseudo(*id, up)),
        })
    }

    /// 路径里的 `..`（Linux 的 `follow_dotdot`）：在根目录上不动；在挂载的根上先回到挂载点
    /// （叠着的挂载一层层回去），再取上级，上级上挂着东西时进到挂在上面的挂载。
    fn dotdot(&self, ps: &dyn Pseudo, root: &Node, mut dir: Node) -> Result<Node, Errno> {
        loop {
            if dir == *root {
                return Ok(dir);
            }
            match self.point_of(&dir) {
                Some(point) => dir = point,
                None => break,
            }
        }
        let up = self.parent_in_fs(ps, &dir)?;
        Ok(self.mounted(up))
    }

    /// 符号链接的内容。
    pub(crate) fn readlink(&self, ps: &dyn Pseudo, node: &Node) -> Result<Vec<u8>, Errno> {
        match node {
            Node::Store(ino) => self.store.readlink(*ino),
            Node::Pseudo(_, p) => ps.readlink(p),
        }
    }

    /// 凭据为 `cred` 的进程能不能在目录 `dir` 里查找。合成文件系统的目录总是可以。
    fn may_search(&self, cred: &Cred, dir: &Node) -> Result<(), Errno> {
        match dir {
            Node::Store(ino) if !cred.searches_all() => {
                cred.permission(&self.store.stat(*ino)?, MAY_EXEC)
            }
            _ => Ok(()),
        }
    }

    /// 以凭据 `cred`、根目录 `root` 解析路径。`follow` 为 false 时不跟随最后一个组件的
    /// 符号链接（lstat、readlink 等）。
    pub(crate) fn resolve(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<Node, Errno> {
        self.walk(ps, cred, root, cwd, path, follow)
            .map(|(node, _)| node)
    }

    /// 路径指向的文件的绝对路径，符号链接全部解析（`/proc/<pid>/exe`，与 Linux 的
    /// `d_path` 相同）。经由 `/proc/<pid>/fd/<n>` 到达、不知道名字的文件报 ENOENT。
    pub(crate) fn real_path(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
    ) -> Result<Vec<u8>, Errno> {
        let (node, found) = self.walk(ps, cred, root, cwd, path, true)?;
        self.found_path(ps, root, &node, found.as_ref())
    }

    /// [`walk`](Self::walk) 找到的 `node` 的绝对路径，按最后找到它的目录和名字 `found`
    /// 算，见 [`real_path`](Self::real_path)。
    pub(crate) fn found_path(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        node: &Node,
        found: Option<&Dentry>,
    ) -> Result<Vec<u8>, Errno> {
        match found {
            Some((dir, name)) => {
                let mut p = self.path_of(ps, root, dir)?;
                if p != b"/" {
                    p.push(b'/');
                }
                p.extend_from_slice(name);
                Ok(p)
            }
            None if self.is_dir(ps, node)? => self.path_of(ps, root, node),
            None => Err(Errno::ENOENT),
        }
    }

    /// [`resolve`](Self::resolve)，另外返回最后找到结果的目录和名字；结果是经由 `.`、
    /// `..`、根目录或者合成文件系统的链接得到的时候没有。
    pub(crate) fn walk(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        self.store
            .snapshot(|_| self.walk_in(ps, cred, root, cwd, path, follow))
    }

    fn walk_in(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut dir = if path[0] == b'/' {
            root.clone()
        } else {
            cwd.clone()
        };
        let mut found = None;
        let mut pending = components(path);
        let mut links = 0;
        while let Some(comp) = pending.pop_front() {
            let last = pending.is_empty();
            if comp.len() > NAME_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            // 每个组件（包括 . 和 ..）之前的节点都必须是目录：file/. 和 file/ 是 ENOTDIR
            if !self.is_dir(ps, &dir)? {
                return Err(Errno::ENOTDIR);
            }
            self.may_search(cred, &dir)?;
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                dir = self.dotdot(ps, root, dir)?;
                found = None;
                continue;
            }
            let node = self.lookup(ps, &dir, &comp)?;
            if self.kind(ps, &node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                if let Node::Pseudo(id, p) = &node
                    && let Some(target) = ps.follow(*id, p)
                {
                    dir = target;
                    found = None;
                    continue;
                }
                let target = self.readlink(ps, &node)?;
                if target.first() == Some(&b'/') {
                    dir = root.clone();
                    found = None;
                }
                for c in components(&target).into_iter().rev() {
                    pending.push_front(c);
                }
                continue;
            }
            found = Some((std::mem::replace(&mut dir, node), comp));
        }
        Ok((dir, found))
    }

    /// 解析路径的父目录，返回父目录和最后一个组件（去掉结尾的 `/`）。
    ///
    /// 最后一个组件可能是 `.` 或 `..`，由调用方决定怎么处理；路径是 `/` 时返回 `(ROOT, ".")`。
    pub(crate) fn resolve_parent<'p>(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &'p [u8],
    ) -> Result<(Node, &'p [u8]), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        let trimmed = &path[..end];
        if trimmed.is_empty() {
            return Ok((root.clone(), b"."));
        }
        let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(i) => (
                self.resolve(ps, cred, root, cwd, &trimmed[..=i], true)?,
                &trimmed[i + 1..],
            ),
            None => (cwd.clone(), trimmed),
        };
        if name.len() > NAME_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        if !self.is_dir(ps, &dir)? {
            return Err(Errno::ENOTDIR);
        }
        Ok((dir, name))
    }

    /// 目录相对根目录 `root` 的绝对路径（getcwd、`/proc/<pid>/cwd`）。不在 `root` 下面
    /// 的目录与 Linux 一样以 `(unreachable)` 开头。
    pub(crate) fn path_of(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        dir: &Node,
    ) -> Result<Vec<u8>, Errno> {
        let mut names = Vec::new();
        let mut cur = dir.clone();
        let mut reached = true;
        loop {
            if cur == *root {
                break;
            }
            if let Some((point, at)) = self.root_point(&cur) {
                match at {
                    // 挂在文件上的：经由挂上时的目录和名字
                    Some((dir, name)) => {
                        names.push(name);
                        cur = dir;
                    }
                    None => cur = point,
                }
                continue;
            }
            match cur {
                Node::Store(ROOT) => {
                    reached = false;
                    break;
                }
                Node::Store(ino) => {
                    let parent = self.store.parent(ino)?;
                    // 已被删除的目录找不到名字，与 Linux 一样报 ENOENT
                    names.push(self.store.name_of(parent, ino)?.ok_or(Errno::ENOENT)?);
                    cur = Node::Store(parent);
                }
                Node::Pseudo(id, p) => match ps.parent(&p) {
                    Some(up) => {
                        names.push(ps.name(&p));
                        cur = Node::Pseudo(id, up);
                    }
                    // 卸下了的挂载（MNT_DETACH）的根
                    None => {
                        reached = false;
                        break;
                    }
                },
            }
        }
        let mut out = if reached {
            Vec::new()
        } else {
            b"(unreachable)".to_vec()
        };
        if names.is_empty() {
            out.push(b'/');
            return Ok(out);
        }
        for name in names.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        Ok(out)
    }

    // ---- 挂载表（mount(2)、umount2(2)、mountinfo）----

    /// 挂载表，按挂上的先后。根文件系统（[`ROOT_MOUNT`]）不在里面。
    pub(crate) fn mounts(&self) -> Vec<Mount> {
        self.mounts.borrow().list.clone()
    }

    /// 编号为 `id` 的挂载；卸下了的是 None。
    pub(crate) fn mount(&self, id: MountId) -> Option<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// `node` 是一个挂载的根时，那个挂载的编号；仓库的根目录是 [`ROOT_MOUNT`] 的根。
    pub(crate) fn mount_root(&self, node: &Node) -> Option<MountId> {
        match node {
            Node::Store(ROOT) => Some(ROOT_MOUNT),
            Node::Store(_) => None,
            Node::Pseudo(id, _) => self.point_of(node).map(|_| *id),
        }
    }

    /// 有挂载挂在 `node` 上（rmdir、rename、unlink 它是 EBUSY）。
    pub(crate) fn is_mount_point(&self, node: &Node) -> bool {
        self.mounts.borrow().list.iter().any(|m| m.point == *node)
    }

    /// `node` 的设备号（stat 的 `st_dev`）。卸下了的挂载里的节点按开机时的实例算。
    pub(crate) fn dev_of(&self, node: &Node) -> u64 {
        match node {
            Node::Store(_) => ROOT_DEV,
            Node::Pseudo(id, p) => self
                .mount(*id)
                .map_or_else(|| crate::pseudo::fs_of(p).dev(), |m| m.dev),
        }
    }

    /// `node` 所在的挂载的标志；卸下了的挂载是 None。
    pub(crate) fn mount_flags(&self, node: &Node) -> Option<u64> {
        match node {
            Node::Store(_) => Some(ROOT_FLAGS),
            Node::Pseudo(id, _) => self.mount(*id).map(|m| m.flags),
        }
    }

    /// `node` 在只读的挂载里（cgroup2 总是只读的）：改它是 EROFS。
    pub(crate) fn readonly(&self, node: &Node) -> bool {
        match node {
            Node::Store(_) => false,
            Node::Pseudo(id, p) => {
                crate::pseudo::fs_of(p) == Fs::Cgroup
                    || self.mount(*id).is_some_and(|m| m.flags & ms::RDONLY != 0)
            }
        }
    }

    /// 挂上 `fs` 里的 `root`，返回新挂载的编号。`dev` 是 None 时另外分配一个设备号
    /// （[`Fs::own_dev`]）；bind 挂载用源的。`point` 是仓库里的文件时，
    /// `at` 是找到它的目录和名字（[`Mount::at`]）。
    pub(crate) fn add_mount(
        &self,
        fs: Fs,
        root: PseudoNode,
        (point, at): (Node, Option<Dentry>),
        flags: u64,
        source: Vec<u8>,
        dev: Option<u64>,
    ) -> MountId {
        let mut mounts = self.mounts.borrow_mut();
        let id = mounts.next;
        mounts.next += 1;
        let dev = dev.unwrap_or_else(|| {
            mounts.next_dev += 1;
            mounts.next_dev - 1
        });
        let at = at.filter(|_| matches!(point, Node::Store(_)));
        mounts.list.push(Mount {
            id,
            fs,
            root,
            point,
            at,
            flags: flags & ms::PER_MOUNT,
            dev,
            source,
            expiring: false,
        });
        drop(mounts);
        self.mounts_changed();
        id
    }

    /// 直接挂在挂载 `id` 里的挂载，按挂上的先后。
    pub(crate) fn children(&self, id: MountId) -> Vec<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .filter(|m| m.parent() == id)
            .cloned()
            .collect()
    }

    /// 卸下挂载 `id` 和挂在它里面的所有挂载（Linux 的 `umount_tree`）。
    pub(crate) fn remove_mount(&self, id: MountId) {
        let mut mounts = self.mounts.borrow_mut();
        let mut gone = vec![id];
        // 挂载总是挂在比它先挂上的挂载里，按先后找一遍就找全了
        for m in &mounts.list {
            if gone.contains(&m.parent()) {
                gone.push(m.id);
            }
        }
        mounts.list.retain(|m| !gone.contains(&m.id));
        drop(mounts);
        self.mounts_changed();
    }

    /// 改挂载 `id` 的标志、MNT_EXPIRE 的标记。
    pub(crate) fn update_mount(&self, id: MountId, f: impl FnOnce(&mut Mount)) {
        if let Some(m) = self
            .mounts
            .borrow_mut()
            .list
            .iter_mut()
            .find(|m| m.id == id)
        {
            f(m);
            m.flags &= ms::PER_MOUNT;
        }
    }

    /// 挂载表变了：不持 BKL 解析路径用的挂载点跟着换（调用方另外让各线程重新取，
    /// [`Shared::fast_changed`](crate::kernel::Shared::fast_changed)）。
    fn mounts_changed(&self) {
        let points = self.mounts.borrow().store_points();
        self.cached.borrow_mut().mounts = points;
    }
}

impl Mount {
    /// 挂载点所在的挂载（mountinfo 的第二列）。
    pub(crate) fn parent(&self) -> MountId {
        match self.point {
            Node::Store(_) => ROOT_MOUNT,
            Node::Pseudo(id, _) => id,
        }
    }
}

impl Mounts {
    /// 仓库里的挂载点。
    fn store_points(&self) -> Arc<[Ino]> {
        self.list
            .iter()
            .filter_map(|m| match m.point {
                Node::Store(ino) => Some(ino),
                Node::Pseudo(..) => None,
            })
            .collect()
    }
}

/// 没有 `/etc/resolv.conf` 时写一个指向 DNS 存根的（设计文档 §10）；已有的保持不动。
fn ensure_resolv_conf(store: &Store) -> Result<(), Errno> {
    let Some(etc) = store.lookup(ROOT, b"etc")? else {
        return Ok(());
    };
    if store.lookup(etc, b"resolv.conf")?.is_some() {
        return Ok(());
    }
    write_file(
        store,
        b"/etc/resolv.conf",
        b"nameserver 127.0.0.53\n",
        0o644,
        false,
    )
}

/// 在仓库里写一个普通文件（宿主准备配置文件用）：父目录不存在时一并建好（0755），
/// 路径里的符号链接不跟随。文件已经存在时，`overwrite` 为 false 就保持不动，否则
/// 替换内容（权限位不变）。
pub fn write_file(
    store: &Store,
    path: &[u8],
    data: &[u8],
    mode: u32,
    overwrite: bool,
) -> Result<(), Errno> {
    use std::io::Write;
    let comps: Vec<Vec<u8>> = split(path).collect();
    let (name, dirs) = comps.split_last().ok_or(Errno::EINVAL)?;
    let mut dir = ROOT;
    for c in dirs {
        dir = match store.lookup(dir, c)? {
            Some(ino) if store.kind(ino)? == mode::S_IFDIR => ino,
            Some(_) => return Err(Errno::ENOTDIR),
            None => store.mkdir(dir, c, 0o755, 0, 0)?,
        };
    }
    let mut file = match store.lookup(dir, name)? {
        Some(_) if !overwrite => return Ok(()),
        Some(ino) if store.kind(ino)? == mode::S_IFREG => {
            store.truncate(ino, 0)?;
            store.open_object(ino, true)?
        }
        Some(_) => return Err(Errno::EEXIST),
        None => store.create_file(dir, name, mode & 0o7777, 0, 0)?.1,
    };
    file.write_all(data)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))
}

/// 拆分路径组件，去掉空组件和 `.`。
/// 路径解析的组件：保留 `.`，以 `/` 结尾时再补一个 `.`，这样前面的节点必须是目录，
/// 最后一个组件是符号链接时也会被跟随（与 Linux 一样）。
fn components(path: &[u8]) -> VecDeque<Vec<u8>> {
    let mut comps: VecDeque<Vec<u8>> = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    if path.ends_with(b"/") {
        comps.push_back(b".".to_vec());
    }
    comps
}

fn split(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

/// 目录和其中的一个名字。
pub(crate) type Dentry = (Node, Vec<u8>);

/// 词法上规范化一个绝对路径：去掉 `.`、空组件，按 `..` 回退（不跟随符号链接）。
pub(crate) fn normalize(path: &[u8]) -> Vec<u8> {
    let mut comps: Vec<&[u8]> = Vec::new();
    for c in path.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                comps.pop();
            }
            c => comps.push(c),
        }
    }
    if comps.is_empty() {
        return b"/".to_vec();
    }
    let mut out = Vec::new();
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成文件系统都只有空的根目录，procfs 里另有 `sys/kernel/` 两层目录。
    struct Empty;

    const SYS: PseudoNode = PseudoNode::Static(b"sys/");
    const KERNEL: PseudoNode = PseudoNode::Static(b"sys/kernel/");

    impl Pseudo for Empty {
        fn parent(&self, node: &PseudoNode) -> Option<PseudoNode> {
            match node {
                PseudoNode::Static(b"sys/kernel/") => Some(SYS),
                PseudoNode::Static(_) => Some(PseudoNode::ProcRoot),
                _ => None,
            }
        }
        fn name(&self, node: &PseudoNode) -> Vec<u8> {
            match node {
                PseudoNode::Static(b"sys/kernel/") => b"kernel".to_vec(),
                PseudoNode::Static(_) => b"sys".to_vec(),
                _ => Vec::new(),
            }
        }
        fn lookup(&self, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno> {
            match (dir, name) {
                (PseudoNode::ProcRoot, b"sys") => Ok(SYS),
                (PseudoNode::Static(b"sys/"), b"kernel") => Ok(KERNEL),
                _ => Err(Errno::ENOENT),
            }
        }
        fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
            Ok(mode::S_IFDIR)
        }
        fn readlink(&self, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
            Err(Errno::EINVAL)
        }
        fn follow(&self, _: MountId, _: &PseudoNode) -> Option<Node> {
            None
        }
    }

    fn sample(name: &str) -> (Vfs, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-vfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::create(&dir).unwrap();
        let bin = s.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
        let usr = s.mkdir(ROOT, b"usr", 0o755, 0, 0).unwrap();
        let ubin = s.mkdir(usr, b"bin", 0o755, 0, 0).unwrap();
        s.mkdir(ROOT, b"proc", 0o555, 0, 0).unwrap();
        s.create_file(bin, b"busybox", 0o755, 0, 0).unwrap();
        s.symlink(ubin, b"env", b"/bin/busybox", 0, 0).unwrap();
        s.symlink(ubin, b"rel", b"../../bin/busybox", 0, 0).unwrap();
        s.symlink(ROOT, b"loop", b"loop", 0, 0).unwrap();
        s.symlink(ROOT, b"bindir", b"bin", 0, 0).unwrap();
        s.symlink(ROOT, b"slash", b"bin/busybox/", 0, 0).unwrap();
        drop(s);
        (Vfs::open(&dir).unwrap(), dir)
    }

    #[test]
    fn resolves_paths_and_symlinks() {
        let (vfs, dir) = sample("resolve");
        let ps = &Empty;
        let root_cred = Cred::root();
        let cred = &root_cred;
        let r = |cwd: &Node, p: &[u8], follow| vfs.resolve(ps, cred, &Node::ROOT, cwd, p, follow);
        let root = Node::ROOT;
        let busybox = r(&root, b"/bin/busybox", true).unwrap();
        assert_eq!(r(&root, b"/usr/bin/env", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/usr/bin/rel", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/bindir/busybox", false).unwrap(), busybox);
        assert_eq!(
            r(&root, b"//usr/./bin/../../bin/busybox", true).unwrap(),
            busybox
        );
        let usr = r(&root, b"usr", true).unwrap();
        assert_eq!(r(&usr, b"../bin/busybox", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/..", true).unwrap(), root);

        let env = r(&root, b"/usr/bin/env", false).unwrap();
        assert_eq!(vfs.kind(ps, &env).unwrap(), mode::S_IFLNK);
        assert_eq!(r(&root, b"/loop", true), Err(Errno::ELOOP));
        assert_eq!(r(&root, b"/missing", true), Err(Errno::ENOENT));
        assert_eq!(r(&root, b"/bin/busybox/x", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/bin/busybox/", true), Err(Errno::ENOTDIR));
        // . 和 .. 之前也必须是目录；符号链接的目标以 / 结尾时同样要求是目录
        assert_eq!(r(&root, b"/bin/busybox/.", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/bin/busybox/..", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/slash", true), Err(Errno::ENOTDIR));
        assert_eq!(
            vfs.kind(ps, &r(&root, b"/slash", false).unwrap()).unwrap(),
            mode::S_IFLNK
        );
        assert_eq!(
            r(&root, b"/bindir/", false).unwrap(),
            r(&root, b"/bin", true).unwrap()
        );
        assert_eq!(r(&root, b"/", true).unwrap(), root);
        assert_eq!(r(&root, b"", true), Err(Errno::ENOENT));

        let ubin = r(&root, b"/usr/bin", true).unwrap();
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &root, b"/usr/bin/new")
                .unwrap(),
            (ubin.clone(), &b"new"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &usr, b"bin//")
                .unwrap(),
            (usr.clone(), &b"bin"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &usr, b"///")
                .unwrap(),
            (root.clone(), &b"."[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &root, b"/bin/busybox/x"),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &ubin).unwrap(), b"/usr/bin");
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &root).unwrap(), b"/");

        // 解析完符号链接之后的路径（/proc/<pid>/exe）
        let real = |cwd: &Node, p: &[u8]| vfs.real_path(ps, cred, &Node::ROOT, cwd, p);
        for p in [
            &b"/usr/bin/env"[..],
            b"/usr/bin/rel",
            b"/bindir/busybox",
            b"/bin/busybox",
        ] {
            assert_eq!(real(&root, p).unwrap(), b"/bin/busybox");
        }
        assert_eq!(real(&usr, b"bin/rel").unwrap(), b"/bin/busybox");
        assert_eq!(real(&usr, b"../bindir/").unwrap(), b"/bin");
        assert_eq!(real(&ubin, b".").unwrap(), b"/usr/bin");
        assert_eq!(real(&ubin, b"..").unwrap(), b"/usr");
        assert_eq!(real(&root, b"/").unwrap(), b"/");
        assert_eq!(real(&root, b"/missing"), Err(Errno::ENOENT));

        // /proc 是挂载点：进去是合成文件系统的根，`..` 回到仓库的根目录
        let proc = r(&root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(PROC_MOUNT, PseudoNode::ProcRoot));
        assert_eq!(r(&proc, b"..", true).unwrap(), root);
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &proc).unwrap(), b"/proc");
        // 设备节点和 /dev/pts 由 Vfs::open 补齐
        let pts = r(&root, b"/dev/pts", true).unwrap();
        assert_eq!(pts, Node::Pseudo(PTS_MOUNT, PseudoNode::PtsRoot));
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &pts).unwrap(), b"/dev/pts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_from_the_cache_like_the_full_walk() {
        let (vfs, dir) = sample("cached");
        let ps = &Empty;
        let cred = &Cred::root();
        let cached = vfs.cached();
        let usr = match vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/usr", true) {
            Ok(Node::Store(ino)) => ino,
            other => panic!("{other:?}"),
        };
        let paths: [&[u8]; 20] = [
            b"/bin/busybox",
            b"/usr/bin/env",
            b"/usr/bin/rel",
            b"/bindir/busybox",
            b"//usr/./bin/../../bin/busybox",
            b"../bin/busybox",
            b"/..",
            b"/loop",
            b"/missing",
            b"/bin/missing/x",
            b"/bin/busybox/x",
            b"/bin/busybox/",
            b"/bin/busybox/.",
            b"/bin/busybox/..",
            b"/slash",
            b"/bindir/",
            b"/",
            b"bin",
            b"bin/rel",
            b"./bin/../../bindir/",
        ];
        for follow in [true, false] {
            for cwd in [ROOT, usr] {
                for p in paths {
                    let full = vfs
                        .resolve(ps, cred, &Node::ROOT, &Node::Store(cwd), p, follow)
                        .and_then(|n| match n {
                            Node::Store(ino) => vfs.store().stat(ino),
                            Node::Pseudo(..) => unreachable!(),
                        });
                    // 正常解析把要用的都填进了缓存，之后只用缓存也得到同样的结果
                    assert_eq!(
                        cached.stat(ROOT, cwd, p, follow),
                        Some(full),
                        "{} from {cwd}, follow {follow}",
                        String::from_utf8_lossy(p)
                    );
                }
            }
        }
        // 合成文件系统交给正常解析
        assert_eq!(cached.stat(ROOT, ROOT, b"/proc", true), None);
        assert_eq!(cached.stat(ROOT, ROOT, b"/dev/pts/0", true), None);
        // 没解析过的名字也交给正常解析
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/sh", true), None);
        // 改了之后不会用到旧的条目
        let bin = match vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/bin", true) {
            Ok(Node::Store(ino)) => ino,
            other => panic!("{other:?}"),
        };
        vfs.store().unlink(bin, b"busybox").unwrap();
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/busybox", true), None);
        assert_eq!(cached.stat(ROOT, usr, b"bin/env", true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mounts_on_other_directories() {
        let (vfs, dir) = sample("mounts");
        let ps = &Empty;
        let cred = &Cred::root();
        let r = |root: &Node, cwd: &Node, p: &[u8]| vfs.resolve(ps, cred, root, cwd, p, true);
        let root = Node::ROOT;
        let usr = r(&root, &root, b"/usr").unwrap();
        let ubin = r(&root, &root, b"/usr/bin").unwrap();
        let bin = r(&root, &root, b"/bin").unwrap();
        let proc = r(&root, &root, b"/proc").unwrap();

        // /usr/bin 上挂一个 procfs：进去是它的根，路径经由挂载点，`..` 回到 /usr
        let id = vfs.add_mount(
            Fs::Proc,
            PseudoNode::ProcRoot,
            (ubin.clone(), None),
            0,
            b"p".to_vec(),
            None,
        );
        let top = Node::Pseudo(id, PseudoNode::ProcRoot);
        assert_eq!(r(&root, &root, b"/usr/bin").unwrap(), top);
        let kernel = r(&root, &root, b"/usr/bin/sys/kernel").unwrap();
        assert_eq!(kernel, Node::Pseudo(id, KERNEL));
        assert_eq!(
            vfs.path_of(ps, &root, &kernel).unwrap(),
            b"/usr/bin/sys/kernel"
        );
        assert_eq!(r(&root, &kernel, b"../../..").unwrap(), usr);
        assert_eq!(vfs.parent(ps, &top).unwrap(), usr);
        // 开机时的 /proc 里同一个目录是另一个节点，`..` 回到 /proc
        let proc_sys = r(&root, &root, b"/proc/sys").unwrap();
        assert_eq!(proc_sys, Node::Pseudo(PROC_MOUNT, SYS));
        assert_eq!(r(&root, &proc_sys, b"..").unwrap(), proc);
        // 根目录换成 /usr：路径相对它，`..` 不越过它，根目录以外的不可达
        assert_eq!(vfs.path_of(ps, &usr, &kernel).unwrap(), b"/bin/sys/kernel");
        assert_eq!(r(&usr, &kernel, b"../../../../..").unwrap(), usr);
        assert_eq!(r(&usr, &usr, b"/bin/sys").unwrap(), Node::Pseudo(id, SYS));
        assert_eq!(
            vfs.path_of(ps, &usr, &proc_sys).unwrap(),
            b"(unreachable)/proc/sys"
        );

        // bind 挂载 procfs 里的 sys/：看到的是那个目录，`..` 回到挂载点的上级
        let b = vfs.add_mount(
            Fs::Proc,
            SYS,
            (bin.clone(), None),
            0,
            b"p".to_vec(),
            Some(0x4),
        );
        let bound = Node::Pseudo(b, KERNEL);
        assert_eq!(r(&root, &root, b"/bin/kernel").unwrap(), bound);
        assert_eq!(vfs.path_of(ps, &root, &bound).unwrap(), b"/bin/kernel");
        assert_eq!(r(&root, &bound, b"../..").unwrap(), root);
        assert!(vfs.is_mount_point(&bin));
        assert_eq!(vfs.mount_root(&Node::Pseudo(b, SYS)), Some(b));
        assert_eq!(vfs.mount_root(&bound), None);
        assert_eq!(vfs.mount_root(&root), Some(ROOT_MOUNT));
        assert_eq!(vfs.mount_root(&usr), None);
        // 新的实例有自己的设备号，bind 挂载用源的
        assert_eq!(vfs.dev_of(&proc_sys), Fs::Proc.dev());
        assert_ne!(vfs.dev_of(&kernel), Fs::Proc.dev());
        assert_eq!(vfs.dev_of(&bound), Fs::Proc.dev());

        // 叠在上面的挂载：最上面的那个可见，`..` 一层层回到挂载点
        let s = vfs.add_mount(
            Fs::Pts,
            PseudoNode::PtsRoot,
            (top.clone(), None),
            0,
            b"d".to_vec(),
            None,
        );
        assert_eq!(
            r(&root, &root, b"/usr/bin").unwrap(),
            Node::Pseudo(s, PseudoNode::PtsRoot)
        );
        assert_eq!(r(&root, &root, b"/usr/bin/..").unwrap(), usr);
        let ids = |list: Vec<Mount>| list.iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(vfs.children(id)), [s]);
        assert_eq!(
            ids(vfs.children(ROOT_MOUNT)),
            [PROC_MOUNT, PTS_MOUNT, SYS_MOUNT, id, b]
        );
        assert_eq!(ids(vfs.children(SYS_MOUNT)), [CGROUP_MOUNT]);

        // 不持 BKL 的解析把挂载点交给正常解析，当前目录的上级是挂载点时也是
        let cached = vfs.cached();
        let ino = |n: &Node| match n {
            Node::Store(i) => *i,
            Node::Pseudo(..) => unreachable!(),
        };
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin", true), None);
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/../usr", true), None);
        let lib = vfs.store().mkdir(ino(&ubin), b"lib", 0o755, 0, 0).unwrap();
        assert_eq!(
            r(&root, &Node::Store(lib), b"..").unwrap(),
            r(&root, &root, b"/usr/bin").unwrap()
        );
        assert_eq!(cached.stat(ROOT, lib, b"..", true), None);

        // 卸下 id 和叠在上面的：又看到仓库里的目录。还在里面的节点留在卸下了的挂载里，
        // 它的根没有上级，路径不可达
        vfs.remove_mount(id);
        assert_eq!(r(&root, &root, b"/usr/bin").unwrap(), ubin);
        assert!(vfs.mount(s).is_none());
        assert_eq!(r(&root, &kernel, b"..").unwrap(), Node::Pseudo(id, SYS));
        assert_eq!(
            r(&root, &kernel, b"../../..").unwrap(),
            Node::Pseudo(id, PseudoNode::ProcRoot)
        );
        assert_eq!(
            vfs.path_of(ps, &root, &kernel).unwrap(),
            b"(unreachable)/sys/kernel"
        );
        assert_eq!(
            vfs.cached().stat(ROOT, lib, b"..", true),
            Some(vfs.store().stat(ino(&ubin)))
        );
        // 卸下的编号不再使用
        assert!(
            vfs.add_mount(
                Fs::Sys,
                PseudoNode::SysRoot,
                (usr, None),
                0,
                Vec::new(),
                None
            ) > s
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_paths_lexically() {
        assert_eq!(normalize(b"/a/./b//../c"), b"/a/c");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(normalize(b"/tmp/x/"), b"/tmp/x");
    }
}
