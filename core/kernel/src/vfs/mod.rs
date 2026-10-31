//! 虚拟文件系统（设计文档 §8）。
//!
//! 根文件系统是 inode 仓库，`/dev` 里的设备节点也存在仓库里（[`dev`]）。procfs、
//! devpts、sysfs 和 cgroup2 是合成文件系统：它们的内容来自内核状态，由内核层经由
//! [`Pseudo`] 提供。开机时挂在 `/proc`、`/dev/pts`、`/sys`、`/sys/fs/cgroup` 上，之后
//! 可以用 mount(2) 挂到别的目录上；仓库里的和合成文件系统里的目录、文件都可以 bind 挂载
//! （[`Mount`]）。tmpfs 是仓库里不挂在根目录下面的一棵树（[`Vfs::new_tmpfs`]），最后一个
//! 挂载没了时删掉。节点带着它所在的挂载（Linux 的 `struct path`），`..` 和
//! getcwd 经由挂载点回到上一层，挂载的标志（只读、noexec 等）按经由的挂载算。路径解析在
//! 整棵树里进行，符号链接最多跟随 40 层；经过的每个仓库目录都要有搜索权限。进程的根目录
//! 可以被 chroot、pivot_root 换掉：绝对路径从它开始，`..` 不会越过它。

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

/// 文件系统树里的一个节点和经由的挂载（Linux 的 `struct path`）：同一个目录挂在（bind 到）
/// 两处时是两个节点，`..` 各自回到自己的挂载点。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    /// inode 仓库里的 inode。
    Store(MountId, Ino),
    /// 合成文件系统里的节点。
    Pseudo(MountId, PseudoNode),
}

impl Node {
    /// 开机时的根目录。
    pub const ROOT: Node = Node::Store(ROOT_MOUNT, ROOT);

    /// 经由的挂载。
    pub(crate) fn mnt(&self) -> MountId {
        match self {
            Node::Store(id, _) | Node::Pseudo(id, _) => *id,
        }
    }

    /// 仓库里的 inode；合成文件系统里的节点是 None。
    pub(crate) fn ino(&self) -> Option<Ino> {
        match self {
            Node::Store(_, ino) => Some(*ino),
            Node::Pseudo(..) => None,
        }
    }

    /// 同一个文件经由挂载 `id` 时的节点。
    pub(crate) fn in_mount(self, id: MountId) -> Node {
        match self {
            Node::Store(_, ino) => Node::Store(id, ino),
            Node::Pseudo(_, p) => Node::Pseudo(id, p),
        }
    }
}

/// 挂载的编号（mountinfo 的第一列、fdinfo 的 `mnt_id`）：所有挂载命名空间统一编号。
pub(crate) type MountId = u32;

/// 开机时的挂载命名空间的编号（`/proc/<pid>/ns/mnt` 里的 inode 号，与 Linux 常见的相同）。
pub(crate) const INIT_MNT_NS: u64 = 4_026_531_841;

/// 挂载命名空间（Linux 的 `struct mnt_namespace`）：只有编号，每个挂载记着它属于哪个
/// 命名空间（[`Mount::ns`]）。进程的 `FsContext` 持有它；最后一个引用没了之后，下一次
/// 清理（[`Vfs::collect`]）时卸下它的所有挂载。
#[derive(Debug)]
pub(crate) struct MntNs {
    pub id: u64,
    /// 属主：建立它时所在的用户命名空间（挂载要在这里有 `CAP_SYS_ADMIN`，Linux 的
    /// `may_mount`）。
    pub owner: std::rc::Rc<crate::cred::UserNs>,
}

/// tmpfs 里一个 inode 占的页数：普通文件按大小，符号链接的目标有 128 字节以上时一页
/// （Linux 的 `SHORT_SYMLINK_LEN`，短的放在 inode 里），别的不占。
pub(crate) fn tmpfs_pages(st: &Stat) -> u64 {
    match st.kind() {
        mode::S_IFREG => st.size.div_ceil(TMPFS_PAGE),
        mode::S_IFLNK => u64::from(st.size >= 128),
        _ => 0,
    }
}

/// 分配新的命名空间编号（Linux 的 `proc_alloc_inum`，各种命名空间共用）。
pub(crate) fn new_ns_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(4_026_532_160);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
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

/// 文件系统的种类：inode 仓库和几种合成文件系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Fs {
    /// inode 仓库（根文件系统）。
    Store,
    /// tmpfs：仓库里不挂在根目录下面的一棵树（[`Vfs::new_tmpfs`]）。
    Tmpfs,
    /// procfs。
    Proc,
    /// devpts。
    Pts,
    /// sysfs。
    Sys,
    /// 只读的 cgroup2。
    Cgroup,
    /// nsfs：命名空间文件（`/proc/<pid>/ns/*` 跟随之后得到的）所在的文件系统。挂不上，只有
    /// bind 挂载出去的命名空间文件在挂载表里。
    Nsfs,
}

impl Fs {
    /// mount(2) 能新挂上的文件系统类型（与 `/proc/filesystems` 里的名字相同）。
    pub(crate) fn from_type(name: &[u8]) -> Option<Fs> {
        match name {
            b"proc" => Some(Fs::Proc),
            b"devpts" => Some(Fs::Pts),
            b"sysfs" => Some(Fs::Sys),
            b"cgroup2" => Some(Fs::Cgroup),
            b"tmpfs" => Some(Fs::Tmpfs),
            _ => None,
        }
    }

    /// 文件系统类型的名字。
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            Fs::Store => "ext4",
            Fs::Tmpfs => "tmpfs",
            Fs::Proc => "proc",
            Fs::Pts => "devpts",
            Fs::Sys => "sysfs",
            Fs::Cgroup => "cgroup2",
            Fs::Nsfs => "nsfs",
        }
    }

    /// 开机时挂上的实例的设备号（stat 的 `st_dev`、mountinfo 的第三列）。
    pub(crate) const fn dev(self) -> u64 {
        match self {
            Fs::Store => ROOT_DEV,
            // 没有开机时的实例，每次挂上另外分配
            Fs::Tmpfs => 0,
            Fs::Proc => 0x17,
            Fs::Pts => 0x18,
            Fs::Sys => 0x16,
            Fs::Cgroup => 0x1a,
            // 与 Linux 一样
            Fs::Nsfs => 0x4,
        }
    }

    /// 每次挂载是一个新的实例（Linux 5.8 起的 procfs、4.7 起的 devpts）：同一处可以再挂
    /// 一个。sysfs、cgroup2 在同一个命名空间里只有一个实例，同一个挂载的根上再挂是 EBUSY。
    pub(crate) const fn new_instance(self) -> bool {
        matches!(self, Fs::Proc | Fs::Pts | Fs::Tmpfs)
    }

    /// 新挂上的有自己的设备号：只有 procfs。devpts 的新实例在 Linux 上有自己的一套终端
    /// （编号各自从 0 开始）和设备号，这里各个挂载看到的仍是同一套终端，设备号也相同：
    /// 从设备的 fstat 报告的是它，ttyname 要拿它比对 `/dev/pts/<n>` 的 `st_dev`。
    pub(crate) const fn own_dev(self) -> bool {
        matches!(self, Fs::Proc | Fs::Tmpfs)
    }

    /// 超级块的选项（mountinfo 最后一列 `rw` 之后的部分）。
    pub(crate) const fn options(self) -> &'static str {
        match self {
            Fs::Pts => ",gid=5,mode=620,ptmxmode=000",
            Fs::Cgroup => ",nsdelegate,memory_recursiveprot",
            // tmpfs 的按实例（[`Vfs::sb_options`]）
            Fs::Store | Fs::Tmpfs | Fs::Proc | Fs::Sys | Fs::Nsfs => "",
        }
    }

    /// 文件系统的根目录（挂载编号是 0，挂上时换成新挂载的）。tmpfs 的根是挂上时新建的
    /// 目录（[`Vfs::new_tmpfs`]），nsfs 挂不上，都不用这个。
    pub(crate) fn root(self) -> Node {
        Node::Pseudo(
            0,
            match self {
                Fs::Store | Fs::Tmpfs => return Node::Store(0, ROOT),
                Fs::Proc | Fs::Nsfs => PseudoNode::ProcRoot,
                Fs::Pts => PseudoNode::PtsRoot,
                Fs::Sys => PseudoNode::SysRoot,
                Fs::Cgroup => PseudoNode::Cgroup(b""),
            },
        )
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

/// 一个挂载（Linux 的 `struct mount`）：文件系统 `fs` 里的 `root` 出现在 `point` 上。
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub id: MountId,
    /// 所属的挂载命名空间（[`MntNs::id`]）。
    pub ns: u64,
    pub fs: Fs,
    /// 挂载看到的部分的根（经由这个挂载的节点）：文件系统的根目录，bind 挂载时是被 bind
    /// 的那个目录或文件。
    pub root: Node,
    /// 挂载点：别的挂载里的目录（bind 挂载文件时是文件）。挂载树的根（开机时是仓库，
    /// pivot_root 之后是换上来的那个）和卸下了的挂载没有，Linux 上前者挂在看不到的
    /// rootfs 上。
    pub point: Option<Node>,
    /// 挂载点是仓库里的文件时，它所在的目录和名字：文件可能有几个链接，没有唯一的上级
    /// 目录，路径要按挂上时的这个算。
    pub at: Option<Dentry>,
    /// bind 挂载仓库里的文件时，找到它的目录和名字（mountinfo 的第四列按它算）。
    pub root_at: Option<Dentry>,
    /// [`ms`] 里的标志。
    pub flags: u64,
    /// 实例的设备号（[`Fs::own_dev`]）。
    pub dev: u64,
    /// mount(2) 的 source（mountinfo 的第十列）。
    pub source: Vec<u8>,
    /// `umount2(MNT_EXPIRE)` 标记过：再来一次就卸下。
    pub expiring: bool,
    /// bind 挂载出去的命名空间文件（[`Fs::Nsfs`]）：它让那个命名空间活着。
    pub pin: Option<crate::nsproxy::NsRef>,
}

/// 合成文件系统的目录结构（内核层实现，内容来自内核状态）。
pub(crate) trait Pseudo {
    /// 上级目录；根目录返回 None。
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode>;
    /// 节点在上级目录里的名字。`mnt` 是节点所在的挂载：procfs 里的 pid 按挂载的实例的
    /// pid 命名空间算。
    fn name(&self, mnt: MountId, node: &PseudoNode) -> Vec<u8>;
    /// 在目录里查找。
    fn lookup(&self, mnt: MountId, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno>;
    /// 文件类型位（`S_IFMT` 部分）。
    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno>;
    /// 符号链接的内容。
    fn readlink(&self, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno>;
    /// "魔法链接"（`/proc/<pid>/fd/<n>` 等）跟随之后直接得到的节点，不按内容解析；不是魔法
    /// 链接时是 None，不许跟随时是错误（EACCES）。`mnt` 是链接所在的挂载。
    fn follow(&self, mnt: MountId, node: &PseudoNode) -> Option<Result<Node, Errno>>;
    /// 魔法链接指向的打开文件是经由哪个目录里的哪个名字打开的（不是目录的文件没有唯一
    /// 的上级目录，挂载点是它时 mountinfo 的路径按这个算）。
    fn follow_dentry(&self, _node: &PseudoNode) -> Option<Dentry> {
        None
    }
    /// 是不是 Linux 的魔法链接（`nd_jump_link`）：openat2 的 `RESOLVE_NO_MAGICLINKS` 和
    /// 有作用域的解析不跟随它们。包括 [`Pseudo::follow`] 直接跳过去的，也包括这里按内容
    /// 解析、Linux 上直接跳过去的（`/proc/<pid>/exe`）。
    fn magic(&self, _node: &PseudoNode) -> bool {
        false
    }
}

/// openat2 的 `struct open_how` 的 `resolve`（Linux 的 `RESOLVE_*`，对应路径解析的
/// `LOOKUP_NO_XDEV` 这些）。
pub(crate) mod resolve {
    /// 不跨挂载（含 bind 挂载）：经由挂载点进出、`..` 越过挂载的根、跳到别的挂载上的
    /// 绝对路径的符号链接、魔法链接都是 EXDEV。
    pub const NO_XDEV: u64 = 0x01;
    /// 不跟随魔法链接（ELOOP）。
    pub const NO_MAGICLINKS: u64 = 0x02;
    /// 不跟随任何符号链接（ELOOP）。
    pub const NO_SYMLINKS: u64 = 0x04;
    /// 不越出起点：绝对路径、绝对路径的符号链接、在起点上的 `..`、魔法链接都是 EXDEV。
    pub const BENEATH: u64 = 0x08;
    /// 把起点当作根目录：绝对路径和绝对路径的符号链接从起点算，在起点上的 `..` 不动；
    /// 魔法链接是 EXDEV。
    pub const IN_ROOT: u64 = 0x10;
    /// 只用缓存里的（不会阻塞的）解析。
    pub const CACHED: u64 = 0x20;
    pub const ALL: u64 = 0x3f;
    /// 有作用域（`LOOKUP_IS_SCOPED`）：根目录是起点。
    pub const SCOPED: u64 = BENEATH | IN_ROOT;
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
    /// 开机时的挂载命名空间。
    init_ns: std::rc::Rc<MntNs>,
    /// 后来建的挂载命名空间：引用都没了的，清理时卸下它的挂载。
    namespaces: RefCell<Vec<std::rc::Weak<MntNs>>>,
}

struct Mounts {
    /// 挂在树上的挂载，按挂上的先后（mountinfo 的顺序）；第一个是挂载树的根，pivot_root
    /// 之后不是了也不挪。
    list: Vec<Mount>,
    /// 卸下了、还有进程的根目录、当前目录或者打开的文件在里面的挂载（`MNT_DETACH`）：它们
    /// 的节点照常能用，挂载的根没有上级（[`Vfs::collect`] 去掉没人用的）。
    detached: Vec<Mount>,
    /// 下一个挂载的编号。卸下的编号不再使用：卸下之后还开着的文件、当前目录仍然带着原来的
    /// 编号，不能算到新的挂载上。
    next: MountId,
    /// 下一个新实例的设备号（0:48 起，避开 [`Fs::dev`] 和管道、socket 等用的）。
    next_dev: u64,
    /// tmpfs 的实例，按设备号（bind 挂载与源共用设备号）。
    tmpfs: std::collections::HashMap<u64, Tmpfs>,
    /// mount(2) 挂上的 procfs 实例的 pid 命名空间（挂载的进程所在的），按设备号；开机时
    /// 的那个和没有记的是开机时的 pid 命名空间。
    proc_ns: std::collections::HashMap<u64, std::rc::Rc<crate::pidns::PidNs>>,
}

/// tmpfs 按页记用量，页的大小（guest 的页）。
pub(crate) const TMPFS_PAGE: u64 = 4096;

/// tmpfs 的超级块参数（Linux 的 `shmem_sb_info` 里的这几项）：挂上时由 mount(2) 的选项
/// 定下，重新挂载可以改大小、inode 数和 inode32/64。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmpfsParams {
    /// 根目录的权限、属主、属组（只在挂上时用）。
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    /// 最多的页数、inode 数，0 是不限（`size=0`、`nr_inodes=0`）。
    pub blocks: u64,
    pub inodes: u64,
    /// `inode64`（默认）或者 `inode32`。
    pub inode64: bool,
}

impl TmpfsParams {
    /// 默认的页数和 inode 数：物理内存的一半（Linux 的 `shmem_default_max_blocks`、
    /// `shmem_default_max_inodes`，没有高端内存时两者相同）。
    pub fn default_limit() -> u64 {
        appine_hal::memory_info().0 / 2 / TMPFS_PAGE
    }

    /// mountinfo 里超级块的选项（`,size=1024k,mode=755,inode64` 这样，Linux 的
    /// `shmem_show_options`）：大小、inode 数不是默认值才写，权限不是 1777、属主属组不是 0
    /// 才写，最后是 `inode64` 或 `inode32`。
    pub fn options(&self) -> String {
        let default = Self::default_limit();
        let mut s = String::new();
        if self.blocks != default {
            s += &format!(",size={}k", self.blocks * (TMPFS_PAGE / 1024));
        }
        if self.inodes != default {
            s += &format!(",nr_inodes={}", self.inodes);
        }
        if self.perm != 0o1777 {
            s += &format!(",mode={:03o}", self.perm);
        }
        if self.uid != 0 {
            s += &format!(",uid={}", self.uid);
        }
        if self.gid != 0 {
            s += &format!(",gid={}", self.gid);
        }
        s += if self.inode64 { ",inode64" } else { ",inode32" };
        s
    }
}

/// tmpfs 的一个实例（Linux 的超级块）：挂上时在仓库里新建一个不挂在根目录下面的目录
/// （[`Store::create_detached_dir`]），挂载和 bind 挂载都没了时连同里面的一起删掉。数据
/// 照常存在宿主的文件里，不占 App 的内存。用量与 Linux 一样按页数和 inode 数限制
/// （[`Vfs::tmpfs_usage`]）。
pub(crate) struct Tmpfs {
    /// 根目录。
    pub root: Ino,
    pub params: TmpfsParams,
    /// 记下的用量（页数、inode 数），新建、删除、写、截短时跟着改，statfs 时现数一遍
    /// （[`Vfs::count_tmpfs`]）；None 时下次要用时数。
    used: Option<(u64, u64)>,
}

/// 不持 BKL 解析路径要用的部分（`syscall::fast`）：仓库的元数据缓存和挂载点。
#[derive(Clone)]
pub(crate) struct Cached {
    meta: Arc<store::Meta>,
    mounts: Arc<[Ino]>,
    /// 根文件系统的挂载（[`ROOT_MOUNT`]）还挂在树上、没有 nosymfollow：根目录、当前目录在
    /// 它里面的线程才能不持 BKL 解析。
    pub(crate) usable: bool,
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
        let mut list = vec![Mount {
            id: ROOT_MOUNT,
            ns: INIT_MNT_NS,
            fs: Fs::Store,
            root: Node::ROOT,
            point: None,
            at: None,
            root_at: None,
            flags: ROOT_FLAGS,
            dev: ROOT_DEV,
            source: b"appine".to_vec(),
            expiring: false,
            pin: None,
        }];
        for (id, fs, path, flags, source) in boot {
            let mut ino = Some(ROOT);
            for name in path.split(|&b| b == b'/') {
                ino = ino.and_then(|i| store.lookup(i, name).ok().flatten());
            }
            match ino {
                Some(ino) if store.stat(ino)?.is_dir() => list.push(Mount {
                    id,
                    ns: INIT_MNT_NS,
                    fs,
                    root: fs.root().in_mount(id),
                    point: Some(Node::Store(ROOT_MOUNT, ino)),
                    at: None,
                    root_at: None,
                    flags,
                    dev: fs.dev(),
                    source: source.to_vec(),
                    expiring: false,
                    pin: None,
                }),
                _ => crate::debug::log(format_args!("no mount point for {fs:?}")),
            }
        }
        // cgroup2 挂在 sysfs 的 fs/cgroup 上，只读（§8.2）
        if list.iter().any(|m| m.id == SYS_MOUNT) {
            list.push(Mount {
                id: CGROUP_MOUNT,
                ns: INIT_MNT_NS,
                fs: Fs::Cgroup,
                root: Fs::Cgroup.root().in_mount(CGROUP_MOUNT),
                point: Some(Node::Pseudo(SYS_MOUNT, crate::pseudo::CGROUP_POINT)),
                at: None,
                root_at: None,
                flags: flags | ms::RDONLY,
                dev: Fs::Cgroup.dev(),
                source: b"cgroup".to_vec(),
                expiring: false,
                pin: None,
            });
        }
        let mounts = Mounts {
            list,
            detached: Vec::new(),
            next: CGROUP_MOUNT + 1,
            next_dev: 0x30,
            tmpfs: Default::default(),
            proc_ns: Default::default(),
        };
        let cached = Cached {
            meta: store.meta().clone(),
            mounts: mounts.store_points(),
            usable: mounts.fast_usable(),
        };
        Ok(Vfs {
            store,
            mounts: RefCell::new(mounts),
            cached: RefCell::new(cached),
            executables: Default::default(),
            init_ns: std::rc::Rc::new(MntNs {
                id: INIT_MNT_NS,
                owner: crate::cred::UserNs::init(),
            }),
            namespaces: Default::default(),
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
    pub(crate) fn mounted(&self, node: Node) -> Node {
        let mounts = self.mounts.borrow();
        let mut node = node;
        while let Some(m) = mounts
            .list
            .iter()
            .rev()
            .find(|m| m.point.as_ref() == Some(&node))
        {
            node = m.root.clone();
        }
        node
    }

    /// 对编号为 `id` 的挂载（挂在树上的或者卸下了的）做 `f`。
    fn with_mount<T>(&self, id: MountId, f: impl FnOnce(&Mount) -> T) -> Option<T> {
        let mounts = self.mounts.borrow();
        mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .find(|m| m.id == id)
            .map(f)
    }

    /// `node` 是它所在的挂载的根（挂在树上的和卸下了的都算）。
    fn is_mount_root(&self, node: &Node) -> bool {
        self.with_mount(node.mnt(), |m| m.root == *node)
            .unwrap_or(false)
    }

    /// `node` 是一个挂载的根时，那个挂载的挂载点。
    fn point_of(&self, node: &Node) -> Option<Node> {
        self.root_point(node).map(|(point, _)| point)
    }

    /// `node` 是一个挂在树上、有挂载点的挂载的根时，挂载点和 [`Mount::at`]。
    fn root_point(&self, node: &Node) -> Option<(Node, Option<Dentry>)> {
        let mounts = self.mounts.borrow();
        let m = mounts.list.iter().find(|m| m.id == node.mnt())?;
        if m.root != *node {
            return None;
        }
        Some((m.point.clone()?, m.at.clone()))
    }

    /// 节点的文件类型位。
    pub(crate) fn kind(&self, ps: &dyn Pseudo, node: &Node) -> Result<u32, Errno> {
        match node {
            Node::Store(_, ino) => self.store.kind(*ino),
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
            Node::Store(id, ino) => {
                Node::Store(*id, self.store.lookup(*ino, name)?.ok_or(Errno::ENOENT)?)
            }
            Node::Pseudo(id, p) => Node::Pseudo(*id, ps.lookup(*id, p, name)?),
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

    /// 同一个挂载里的上级目录。挂载的根没有上级，是它自己：挂载树的根、卸下了的挂载
    /// （`MNT_DETACH`）的根，bind 挂载里看不到被 bind 的目录外面。
    fn parent_in_fs(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        if self.is_mount_root(dir) {
            return Ok(dir.clone());
        }
        Ok(match dir {
            Node::Store(id, ino) => Node::Store(*id, self.store.parent(*ino)?),
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

    /// `node` 是 `top` 或者在它下面：经由挂载点往上找（Linux 的 `is_path_reachable`）。挂在
    /// 文件上的挂载经由挂上时的目录。
    pub(crate) fn is_under(&self, ps: &dyn Pseudo, node: &Node, top: &Node) -> bool {
        let mut cur = node.clone();
        loop {
            if cur == *top {
                return true;
            }
            if let Some((point, at)) = self.root_point(&cur) {
                cur = match at {
                    Some((dir, _)) if point != *top => dir,
                    _ => point,
                };
                continue;
            }
            match self.parent_in_fs(ps, &cur) {
                Ok(up) if up != cur => cur = up,
                _ => return false,
            }
        }
    }

    /// 符号链接的内容。
    pub(crate) fn readlink(&self, ps: &dyn Pseudo, node: &Node) -> Result<Vec<u8>, Errno> {
        match node {
            Node::Store(_, ino) => self.store.readlink(*ino),
            Node::Pseudo(id, p) => ps.readlink(*id, p),
        }
    }

    /// 凭据为 `cred` 的进程能不能在目录 `dir` 里查找。合成文件系统的目录总是可以。
    fn may_search(&self, cred: &Cred, dir: &Node) -> Result<(), Errno> {
        match dir {
            Node::Store(_, ino) if !cred.searches_all() => {
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

    /// 路径指向的文件的绝对路径，符号链接全部解析（[`found_path`](Self::found_path)）。
    #[cfg(test)]
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
    /// 算，符号链接全部解析（`/proc/<pid>/exe`，与 Linux 的 `d_path` 相同）。经由
    /// `/proc/<pid>/fd/<n>` 到达、不知道名字的文件报 ENOENT。
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
        self.walk_resolve(ps, cred, root, cwd, path, follow, 0)
    }

    /// [`walk`](Self::walk)，带着 openat2 的限制（[`resolve`]）。有作用域时
    /// （`BENEATH`、`IN_ROOT`）`root` 是起点。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn walk_resolve(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
        flags: u64,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        self.store
            .snapshot(|_| self.walk_in(ps, cred, root, cwd, path, follow, flags))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_in(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
        flags: u64,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        // 开头的 / 跳到根目录（path_init 的 nd_jump_root）：BENEATH 不许；IN_ROOT 的根就是
        // 起点；NO_XDEV 只管解析途中的跳转
        let mut dir = if path[0] == b'/' {
            if flags & resolve::BENEATH != 0 {
                return Err(Errno::EXDEV);
            }
            root.clone()
        } else {
            cwd.clone()
        };
        // Linux 的 nd->root 取过了没有：绝对路径、有作用域时一开始就取，否则到第一个 `..`
        // 或者第一次跳到根目录时才取。NO_XDEV 时还没取就跳到根目录（绝对路径的符号链接）
        // 总是 EXDEV：nd_jump_root 拿当前的挂载与空的 nd->root.mnt 比
        let mut root_set = path[0] == b'/' || flags & resolve::SCOPED != 0;
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
                // 在起点上（follow_dotdot 的 in_root）：BENEATH 越不出去；越过挂载的根、
                // 回到挂着东西的目录（进到那个挂载里）是跨挂载
                if dir == *root && flags & resolve::BENEATH != 0 {
                    return Err(Errno::EXDEV);
                }
                root_set = true;
                let up = self.dotdot(ps, root, dir.clone())?;
                if flags & resolve::NO_XDEV != 0 && up.mnt() != dir.mnt() {
                    return Err(Errno::EXDEV);
                }
                dir = up;
                found = None;
                continue;
            }
            let node = self.lookup(ps, &dir, &comp)?;
            // 名字上挂着东西：进到那个挂载里（traverse_mounts）
            if flags & resolve::NO_XDEV != 0 && node.mnt() != dir.mnt() {
                return Err(Errno::EXDEV);
            }
            if self.kind(ps, &node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                // nosymfollow 的挂载里的符号链接（包括魔法链接）、RESOLVE_NO_SYMLINKS 时的
                // 任何符号链接都不跟随（Linux 的 pick_link）
                if flags & resolve::NO_SYMLINKS != 0
                    || self
                        .mount_flags(&node)
                        .is_some_and(|f| f & ms::NOSYMFOLLOW != 0)
                {
                    return Err(Errno::ELOOP);
                }
                // 魔法链接（nd_jump_link）：先查能不能看那个进程，再看限制
                let magic = matches!(&node, Node::Pseudo(_, p) if ps.magic(p));
                let jump = match &node {
                    Node::Pseudo(id, p) => ps.follow(*id, p).transpose()?,
                    Node::Store(..) => None,
                };
                if magic {
                    if flags & resolve::NO_MAGICLINKS != 0 {
                        return Err(Errno::ELOOP);
                    }
                    if let Some(target) = &jump
                        && flags & resolve::NO_XDEV != 0
                        && target.mnt() != dir.mnt()
                    {
                        return Err(Errno::EXDEV);
                    }
                    if flags & resolve::SCOPED != 0 {
                        return Err(Errno::EXDEV);
                    }
                }
                if let (Node::Pseudo(_, p), Some(target)) = (&node, jump) {
                    dir = target;
                    found = if last { ps.follow_dentry(p) } else { None };
                    continue;
                }
                let target = self.readlink(ps, &node)?;
                // 绝对路径的符号链接（nd_jump_root）：BENEATH 不许，NO_XDEV 时根目录要已经
                // 取过、与链接所在的目录在同一个挂载里
                if target.first() == Some(&b'/') {
                    if flags & resolve::BENEATH != 0
                        || (flags & resolve::NO_XDEV != 0 && (!root_set || dir.mnt() != root.mnt()))
                    {
                        return Err(Errno::EXDEV);
                    }
                    root_set = true;
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
        self.resolve_parent_with(ps, cred, root, cwd, path, 0)
    }

    /// [`resolve_parent`](Self::resolve_parent)，带着 openat2 的限制（[`resolve`]）。
    pub(crate) fn resolve_parent_with<'p>(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &'p [u8],
        flags: u64,
    ) -> Result<(Node, &'p [u8]), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        let trimmed = &path[..end];
        if trimmed.is_empty() {
            if flags & resolve::BENEATH != 0 {
                return Err(Errno::EXDEV);
            }
            return Ok((root.clone(), b"."));
        }
        let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(i) => (
                self.walk_resolve(ps, cred, root, cwd, &trimmed[..=i], true, flags)?
                    .0,
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

    /// 目录相对根目录 `root` 的绝对路径（Linux 的 `d_path`：`/proc/<pid>/cwd`、打开文件记的
    /// 路径）。不在 `root` 下面的目录与 Linux 一样不加标记，得到从仓库的根算起的路径，卸下了的
    /// 挂载里的从那个挂载的根算起；要知道到不到得了用 [`reach`](Self::reach)。
    pub(crate) fn path_of(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        dir: &Node,
    ) -> Result<Vec<u8>, Errno> {
        self.reach(ps, root, dir).map(|(path, _)| path)
    }

    /// [`path_of`](Self::path_of)，另外返回是不是从 `root` 到得了：到不了时 getcwd 以
    /// `(unreachable)` 开头，mountinfo 不列。
    pub(crate) fn reach(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        dir: &Node,
    ) -> Result<(Vec<u8>, bool), Errno> {
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
            // 挂载树的根、卸下了的挂载的根
            if self.is_mount_root(&cur) {
                reached = false;
                break;
            }
            match cur {
                // 改了名、不在挂载看到的部分里了
                Node::Store(_, ROOT) => {
                    reached = false;
                    break;
                }
                Node::Store(id, ino) => {
                    let parent = self.store.parent(ino)?;
                    // tmpfs 的根：上级是它自己
                    if parent == ino {
                        reached = false;
                        break;
                    }
                    // 已被删除的目录找不到名字，与 Linux 一样报 ENOENT
                    names.push(self.store.name_of(parent, ino)?.ok_or(Errno::ENOENT)?);
                    cur = Node::Store(id, parent);
                }
                Node::Pseudo(id, p) => match ps.parent(&p) {
                    Some(up) => {
                        names.push(ps.name(id, &p));
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
        let mut out = Vec::new();
        if names.is_empty() {
            out.push(b'/');
        }
        for name in names.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        Ok((out, reached))
    }

    // ---- 挂载表（mount(2)、umount2(2)、pivot_root(2)、mountinfo）----

    /// 挂在树上的挂载，按挂上的先后。第一个是开机时的根文件系统（[`ROOT_MOUNT`]），
    /// pivot_root 之后它可能挂在别处、卸下了。
    pub(crate) fn mounts(&self) -> Vec<Mount> {
        self.mounts.borrow().list.clone()
    }

    /// 挂载命名空间 `ns` 里挂在树上的挂载，按挂上的先后（mountinfo）。
    pub(crate) fn mounts_in(&self, ns: u64) -> Vec<Mount> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().filter(|m| m.ns == ns).cloned().collect()
    }

    /// 开机时的挂载命名空间。
    pub(crate) fn init_ns(&self) -> std::rc::Rc<MntNs> {
        self.init_ns.clone()
    }

    /// bind 挂载出去的、编号为 `id` 的命名空间文件留着的命名空间（[`Mount::pin`]，卸下了
    /// 还有人用的挂载里的也算）。
    pub(crate) fn pinned_ns(&self, id: u64) -> Option<crate::nsproxy::NsRef> {
        let mounts = self.mounts.borrow();
        mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .filter_map(|m| m.pin.as_ref())
            .find(|r| r.id() == id)
            .cloned()
    }

    /// 编号为 `id`、还有人用的挂载命名空间（进程在里面，或者有命名空间文件留着它）。
    pub(crate) fn find_ns(&self, id: u64) -> Option<std::rc::Rc<MntNs>> {
        if id == INIT_MNT_NS {
            return Some(self.init_ns.clone());
        }
        let namespaces = self.namespaces.borrow();
        namespaces
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .find(|ns| ns.id == id)
    }

    /// 复制挂载命名空间 `from` 里挂在树上的所有挂载，成为属于用户命名空间 `owner` 的新的
    /// 命名空间（Linux 的 `copy_mnt_ns`）。返回新的命名空间和旧挂载编号到新挂载编号的对照：调用方据此把进程的
    /// 根目录、当前目录换到新的挂载上。
    pub(crate) fn copy_ns(
        &self,
        from: u64,
        owner: std::rc::Rc<crate::cred::UserNs>,
    ) -> (
        std::rc::Rc<MntNs>,
        std::collections::HashMap<MountId, MountId>,
    ) {
        let ns = std::rc::Rc::new(MntNs {
            id: new_ns_id(),
            owner,
        });
        let mut mounts = self.mounts.borrow_mut();
        let mut old: Vec<Mount> = mounts
            .list
            .iter()
            .filter(|m| m.ns == from)
            .cloned()
            .collect();
        // bind 出去的挂载命名空间文件和挂在它上面的不复制（Linux 的 copy_tree 不带
        // CL_COPY_MNT_NS_FILE）：新的命名空间比它新，留着它可能成环
        let mut skip = std::collections::HashSet::new();
        while let Some(m) = old.iter().find(|m| {
            !skip.contains(&m.id)
                && (matches!(m.pin, Some(crate::nsproxy::NsRef::Mnt(_)))
                    || m.parent().is_some_and(|p| skip.contains(&p)))
        }) {
            skip.insert(m.id);
        }
        old.retain(|m| !skip.contains(&m.id));
        // 先分配编号再复制：MS_MOVE、pivot_root 之后挂载点所在的挂载可能比它后挂上
        let mut map = std::collections::HashMap::new();
        for m in &old {
            map.insert(m.id, mounts.next);
            mounts.next += 1;
        }
        let tag = |n: &Node| {
            n.clone()
                .in_mount(map.get(&n.mnt()).copied().unwrap_or(n.mnt()))
        };
        for m in old {
            let copy = Mount {
                id: map[&m.id],
                ns: ns.id,
                root: tag(&m.root),
                point: m.point.as_ref().map(tag),
                at: m.at.as_ref().map(|(d, n)| (tag(d), n.clone())),
                root_at: m.root_at.as_ref().map(|(d, n)| (tag(d), n.clone())),
                expiring: false,
                ..m
            };
            mounts.list.push(copy);
        }
        drop(mounts);
        self.namespaces
            .borrow_mut()
            .push(std::rc::Rc::downgrade(&ns));
        self.mounts_changed();
        (ns, map)
    }

    /// 挂载命名空间 `ns` 的根目录（挂载树的根的根，上面挂着东西时是最上面的那个）。
    pub(crate) fn ns_root(&self, ns: u64) -> Option<Node> {
        let root = {
            let mounts = self.mounts.borrow();
            let m = mounts
                .list
                .iter()
                .find(|m| m.ns == ns && m.point.is_none())?;
            m.root.clone()
        };
        Some(self.mounted(root))
    }

    /// 挂载 `id` 所属的命名空间；卸下了的是 None（Linux 的 `check_mnt`）。
    pub(crate) fn mount_ns(&self, id: MountId) -> Option<u64> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().find(|m| m.id == id).map(|m| m.ns)
    }

    /// 有挂载命名空间的引用都没了，挂载还没卸下：清理时要处理。
    pub(crate) fn has_dead_ns(&self) -> bool {
        self.namespaces
            .borrow()
            .iter()
            .any(|w| w.strong_count() == 0)
    }

    /// 编号为 `id`、挂在树上的挂载；卸下了的是 None。
    pub(crate) fn mount(&self, id: MountId) -> Option<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// `node` 是一个挂在树上的挂载的根时，那个挂载的编号。
    pub(crate) fn mount_root(&self, node: &Node) -> Option<MountId> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().find(|m| m.root == *node).map(|m| m.id)
    }

    /// 有挂载挂在 `node` 上。
    #[cfg(test)]
    fn is_mount_point(&self, node: &Node) -> bool {
        self.mounts
            .borrow()
            .list
            .iter()
            .any(|m| m.point.as_ref() == Some(node))
    }

    /// 仓库里的 `ino` 经由哪个挂载上面挂着东西都算（rmdir、rename、unlink 它是 EBUSY，Linux
    /// 的 `is_local_mountpoint` 看的是目录项）。
    pub(crate) fn is_store_point(&self, ino: Ino) -> bool {
        self.mounts
            .borrow()
            .list
            .iter()
            .any(|m| matches!(m.point, Some(Node::Store(_, i)) if i == ino))
    }

    /// `node` 的设备号（stat 的 `st_dev`）：所在的挂载的；不知道是哪个挂载时按开机时的
    /// 实例算。
    pub(crate) fn dev_of(&self, node: &Node) -> u64 {
        self.with_mount(node.mnt(), |m| m.dev)
            .unwrap_or_else(|| match node {
                Node::Store(..) => ROOT_DEV,
                Node::Pseudo(_, p) => crate::pseudo::fs_of(p).dev(),
            })
    }

    /// `node` 所在的文件系统的种类。
    pub(crate) fn fs_of(&self, node: &Node) -> Fs {
        self.with_mount(node.mnt(), |m| m.fs)
            .unwrap_or_else(|| match node {
                Node::Store(..) => Fs::Store,
                Node::Pseudo(_, p) => crate::pseudo::fs_of(p),
            })
    }

    /// `node` 在 tmpfs 里时，那个实例的设备号（[`Vfs::tmpfs_usage`] 等用它）。
    pub(crate) fn tmpfs_dev(&self, node: &Node) -> Option<u64> {
        (self.fs_of(node) == Fs::Tmpfs).then(|| self.dev_of(node))
    }

    /// tmpfs 实例 `dev` 的参数和用量（页数、inode 数）。用量与 Linux 的 tmpfs 一样算：根目录
    /// 和每个名字各占一个 inode（硬链接也是，Linux 的 `shmem_link` 每个链接另占一个），
    /// 普通文件按页数（与 appine 的 `st_blocks` 一样当作没有空洞），长的符号链接一页。
    /// `fresh` 时重新数一遍（statfs、重新挂载：直接改仓库的、共享映射里写的这些不经过
    /// 记账的改动也算进去），否则用记下的，没记下时才数。
    pub(crate) fn tmpfs_usage(
        &self,
        dev: u64,
        fresh: bool,
    ) -> Result<(TmpfsParams, u64, u64), Errno> {
        let (params, root, used) = {
            let mounts = self.mounts.borrow();
            let t = mounts.tmpfs.get(&dev).ok_or(Errno::EINVAL)?;
            (t.params.clone(), t.root, t.used)
        };
        let (pages, inodes) = match used {
            Some(u) if !fresh => u,
            _ => {
                let u = self.count_tmpfs(root)?;
                if let Some(t) = self.mounts.borrow_mut().tmpfs.get_mut(&dev) {
                    t.used = Some(u);
                }
                u
            }
        };
        Ok((params, pages, inodes))
    }

    /// 在 tmpfs 实例 `dev` 里再用 `pages` 页、`inodes` 个 inode 放不放得下（Linux 的
    /// `shmem_reserve_inode`、`shmem_inode_acct_block`）：放不下是 ENOSPC。放得下时由调用方
    /// 做完之后记账（[`Vfs::tmpfs_charge`]）。
    pub(crate) fn tmpfs_check(&self, dev: u64, pages: u64, inodes: u64) -> Result<(), Errno> {
        let (p, used_pages, used_inodes) = self.tmpfs_usage(dev, false)?;
        let over = |max: u64, used: u64, more: u64| max != 0 && more > 0 && used + more > max;
        if over(p.blocks, used_pages, pages) || over(p.inodes, used_inodes, inodes) {
            return Err(Errno::ENOSPC);
        }
        Ok(())
    }

    /// tmpfs 实例 `dev` 里一个文件还能用多少页：本来有 `own` 页，加上没用掉的。不限时是 None。
    pub(crate) fn tmpfs_room(&self, dev: u64, own: u64) -> Result<Option<u64>, Errno> {
        let (p, used, _) = self.tmpfs_usage(dev, false)?;
        Ok((p.blocks != 0).then(|| (p.blocks + own).saturating_sub(used)))
    }

    /// 记下 tmpfs 实例 `dev` 的用量变了（新建、删除、写、截短之后）。还没数过时什么也不做，
    /// 要用时再数。
    pub(crate) fn tmpfs_charge(&self, dev: u64, pages: i64, inodes: i64) {
        if let Some(t) = self.mounts.borrow_mut().tmpfs.get_mut(&dev)
            && let Some((p, i)) = &mut t.used
        {
            *p = p.saturating_add_signed(pages);
            *i = i.saturating_add_signed(inodes);
        }
    }

    /// 重新挂载时改 tmpfs 实例 `dev` 的参数：`f` 拿到参数和现在的用量（页数、inode 数），
    /// 出错时什么也不改。
    pub(crate) fn tmpfs_reconfigure(
        &self,
        dev: u64,
        f: impl FnOnce(&mut TmpfsParams, u64, u64) -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        let (mut params, pages, inodes) = self.tmpfs_usage(dev, true)?;
        f(&mut params, pages, inodes)?;
        if let Some(t) = self.mounts.borrow_mut().tmpfs.get_mut(&dev) {
            t.params = params;
        }
        Ok(())
    }

    /// 数一遍以 `root` 为根的 tmpfs 的用量（页数、inode 数，见 [`Vfs::tmpfs_usage`]）。
    /// 删掉之后还开着的文件不算（Linux 关掉时才释放）。
    fn count_tmpfs(&self, root: Ino) -> Result<(u64, u64), Errno> {
        let (mut pages, mut inodes) = (0, 1);
        let mut seen = std::collections::HashSet::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for e in self.store.readdir(dir)? {
                if e.ino == dir {
                    continue;
                }
                inodes += 1;
                match e.mode & mode::S_IFMT {
                    mode::S_IFDIR => dirs.push(e.ino),
                    mode::S_IFREG | mode::S_IFLNK if seen.insert(e.ino) => {
                        pages += tmpfs_pages(&self.store.stat(e.ino)?);
                    }
                    _ => {}
                }
            }
        }
        Ok((pages, inodes))
    }

    /// 挂载的超级块选项（mountinfo 最后一列 `rw` 之后的部分）。
    pub(crate) fn sb_options(&self, m: &Mount) -> String {
        match m.fs {
            Fs::Tmpfs => self
                .mounts
                .borrow()
                .tmpfs
                .get(&m.dev)
                .map_or_else(String::new, |t| t.params.options()),
            fs => fs.options().to_owned(),
        }
    }

    /// `node` 所在的挂载的标志；不知道是哪个挂载时是 None。
    pub(crate) fn mount_flags(&self, node: &Node) -> Option<u64> {
        self.with_mount(node.mnt(), |m| m.flags)
    }

    /// `node` 在只读的挂载里（cgroup2 总是只读的）：改它是 EROFS。
    pub(crate) fn readonly(&self, node: &Node) -> bool {
        matches!(node, Node::Pseudo(_, p) if crate::pseudo::fs_of(p) == Fs::Cgroup)
            || self.mount_flags(node).is_some_and(|f| f & ms::RDONLY != 0)
    }

    /// 挂载看到的部分在文件系统里的路径（mountinfo 的第四列：bind 挂载的是哪个目录、文件）。
    pub(crate) fn root_path(&self, ps: &dyn Pseudo, m: &Mount) -> Vec<u8> {
        match &m.root_at {
            Some((dir, name)) => {
                let mut p = self.fs_path(ps, dir);
                if p != b"/" {
                    p.push(b'/');
                }
                p.extend_from_slice(name);
                p
            }
            None => self.fs_path(ps, &m.root),
        }
    }

    /// 目录在它的文件系统里的路径。命名空间文件是 `net:[编号]` 这样（Linux 的 nsfs 的
    /// `d_dname`），前面没有 `/`。
    fn fs_path(&self, ps: &dyn Pseudo, node: &Node) -> Vec<u8> {
        let mut names = Vec::new();
        match node {
            Node::Pseudo(id, p @ PseudoNode::NsFile(..)) => return ps.name(*id, p),
            Node::Store(_, ino) => {
                let mut cur = *ino;
                while cur != ROOT {
                    let Ok(up) = self.store.parent(cur) else {
                        break;
                    };
                    match self.store.name_of(up, cur) {
                        Ok(Some(name)) => names.push(name),
                        _ => break,
                    }
                    cur = up;
                }
            }
            Node::Pseudo(id, p) => {
                let mut cur = p.clone();
                while let Some(up) = ps.parent(&cur) {
                    names.push(ps.name(*id, &cur));
                    cur = up;
                }
            }
        }
        if names.is_empty() {
            return b"/".to_vec();
        }
        names
            .iter()
            .rev()
            .flat_map(|n| [&b"/"[..], n].concat())
            .collect()
    }

    /// 把文件系统 `fs` 里的 `root`（[`Fs::root`] 或者经由别的挂载的节点）挂到 `point` 上，
    /// 返回新挂载的编号。`dev` 是 None 时另外分配一个设备号（[`Fs::own_dev`]）；bind 挂载
    /// 用源的。`point` 是仓库里的文件时，`at` 是找到它的目录和名字（[`Mount::at`]）。
    pub(crate) fn add_mount(
        &self,
        fs: Fs,
        root: Node,
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
        let at = at.filter(|_| matches!(point, Node::Store(..)));
        // 新挂载属于挂载点所在的挂载的命名空间
        let ns = mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .find(|m| m.id == point.mnt())
            .map_or(INIT_MNT_NS, |m| m.ns);
        mounts.list.push(Mount {
            id,
            ns,
            fs,
            root: root.in_mount(id),
            point: Some(point),
            at,
            root_at: None,
            flags: flags & ms::PER_MOUNT,
            dev,
            source,
            expiring: false,
            pin: None,
        });
        drop(mounts);
        self.mounts_changed();
        id
    }

    /// 挂载 `id` 是 procfs 的实例时，挂上它的进程所在的 pid 命名空间（开机时的是 None）。
    pub(crate) fn proc_pid_ns(&self, id: MountId) -> Option<std::rc::Rc<crate::pidns::PidNs>> {
        let mounts = self.mounts.borrow();
        let m = mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .find(|m| m.id == id)?;
        mounts.proc_ns.get(&m.dev).cloned()
    }

    /// 新的 procfs 实例 `id` 属于 pid 命名空间 `ns`。
    pub(crate) fn set_proc_pid_ns(&self, id: MountId, ns: std::rc::Rc<crate::pidns::PidNs>) {
        let mut mounts = self.mounts.borrow_mut();
        if let Some(dev) = mounts.list.iter().find(|m| m.id == id).map(|m| m.dev) {
            mounts.proc_ns.insert(dev, ns);
        }
    }

    /// 挂上一个新的 tmpfs 实例（根目录由这里新建），返回新挂载的编号。
    pub(crate) fn new_tmpfs(
        &self,
        point: (Node, Option<Dentry>),
        flags: u64,
        source: Vec<u8>,
        params: TmpfsParams,
    ) -> Result<MountId, Errno> {
        let root = self
            .store
            .create_detached_dir(params.perm, params.uid, params.gid)?;
        let id = self.add_mount(Fs::Tmpfs, Node::Store(0, root), point, flags, source, None);
        let mut mounts = self.mounts.borrow_mut();
        let dev = mounts.list.iter().find(|m| m.id == id).map_or(0, |m| m.dev);
        mounts.tmpfs.insert(
            dev,
            Tmpfs {
                root,
                params,
                // 只有根目录
                used: Some((0, 1)),
            },
        );
        Ok(id)
    }

    /// 直接挂在挂载 `id` 里的挂载，按挂上的先后。
    pub(crate) fn children(&self, id: MountId) -> Vec<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .filter(|m| m.parent() == Some(id))
            .cloned()
            .collect()
    }

    /// 卸下挂载 `id` 和挂在它里面的所有挂载（Linux 的 `umount_tree`）：它们都不再挂在
    /// 别的挂载上，各自的根没有上级。
    pub(crate) fn remove_mount(&self, id: MountId) {
        let mut mounts = self.mounts.borrow_mut();
        let mut gone = vec![id];
        let mut i = 0;
        // MS_MOVE、pivot_root 之后挂载不一定挂在比它先挂上的挂载里，一层层找
        while let Some(&parent) = gone.get(i) {
            let inside = mounts.list.iter().filter(|m| m.parent() == Some(parent));
            gone.extend(inside.map(|m| m.id).collect::<Vec<_>>());
            i += 1;
        }
        let list = std::mem::take(&mut mounts.list);
        let (out, keep): (Vec<_>, Vec<_>) = list.into_iter().partition(|m| gone.contains(&m.id));
        mounts.list = keep;
        for mut m in out {
            m.point = None;
            m.at = None;
            mounts.detached.push(m);
        }
        drop(mounts);
        self.mounts_changed();
    }

    /// 丢掉卸下了的、`in_use` 里没有的挂载（进程的根目录、当前目录和打开的文件都不在里面）。
    /// 哪个挂载都不再看得到的 tmpfs 实例连同里面的文件一起删掉。
    pub(crate) fn collect(&self, in_use: &std::collections::HashSet<MountId>) {
        // 没人用的挂载命名空间：它的挂载都卸下
        self.namespaces
            .borrow_mut()
            .retain(|w| w.strong_count() > 0);
        let live: std::collections::HashSet<u64> = self
            .namespaces
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .map(|ns| ns.id)
            .chain([INIT_MNT_NS])
            .collect();
        if self
            .mounts
            .borrow()
            .list
            .iter()
            .any(|m| !live.contains(&m.ns))
        {
            let mut mounts = self.mounts.borrow_mut();
            let list = std::mem::take(&mut mounts.list);
            let (keep, out): (Vec<_>, Vec<_>) =
                list.into_iter().partition(|m| live.contains(&m.ns));
            mounts.list = keep;
            for mut m in out {
                m.point = None;
                m.at = None;
                mounts.detached.push(m);
            }
            drop(mounts);
            self.mounts_changed();
        }
        let gone: Vec<Ino> = {
            let mut mounts = self.mounts.borrow_mut();
            mounts.detached.retain(|m| in_use.contains(&m.id));
            let live: std::collections::HashSet<u64> = mounts
                .list
                .iter()
                .chain(&mounts.detached)
                .filter(|m| m.fs == Fs::Tmpfs)
                .map(|m| m.dev)
                .collect();
            let dead: Vec<u64> = mounts
                .tmpfs
                .keys()
                .filter(|dev| !live.contains(dev))
                .copied()
                .collect();
            let procs: std::collections::HashSet<u64> = mounts
                .list
                .iter()
                .chain(&mounts.detached)
                .filter(|m| m.fs == Fs::Proc)
                .map(|m| m.dev)
                .collect();
            mounts.proc_ns.retain(|dev, _| procs.contains(dev));
            dead.iter()
                .filter_map(|dev| mounts.tmpfs.remove(dev))
                .map(|t| t.root)
                .collect()
        };
        for root in gone {
            if let Err(e) = self.store.remove_detached_dir(root) {
                crate::debug::log(format_args!("cannot remove tmpfs {root}: {e}"));
            }
        }
        // 丢掉的挂载里有 bind 出去的挂载命名空间文件：那个命名空间可能也没人用了
        if self.has_dead_ns() {
            self.collect(in_use);
        }
    }

    /// 把挂在树上的挂载 `id` 改挂到 `point` 上（`MS_MOVE`、pivot_root）；None 是挂成挂载树
    /// 的根。
    pub(crate) fn move_mount(&self, id: MountId, point: Option<(Node, Option<Dentry>)>) {
        if let Some(m) = self
            .mounts
            .borrow_mut()
            .list
            .iter_mut()
            .find(|m| m.id == id)
        {
            (m.point, m.at) = match point {
                Some((p, at)) => {
                    let at = at.filter(|_| matches!(p, Node::Store(..)));
                    (Some(p), at)
                }
                None => (None, None),
            };
        }
        self.mounts_changed();
    }

    /// 记下 bind 挂载的文件是在哪个目录里、叫什么名字找到的（[`Mount::root_at`]）。
    pub(crate) fn set_root_at(&self, id: MountId, at: Option<Dentry>) {
        self.update_mount(id, |m| m.root_at = at);
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
        self.mounts_changed();
    }

    /// 挂载表变了：不持 BKL 解析路径用的挂载点跟着换（调用方另外让各线程重新取，
    /// [`Shared::fast_changed`](crate::kernel::Shared::fast_changed)）。
    fn mounts_changed(&self) {
        let (points, usable) = {
            let mounts = self.mounts.borrow();
            (mounts.store_points(), mounts.fast_usable())
        };
        let mut cached = self.cached.borrow_mut();
        cached.mounts = points;
        cached.usable = usable;
    }
}

impl Mount {
    /// 挂载点所在的挂载（mountinfo 的第二列）；挂载树的根、卸下了的没有。
    pub(crate) fn parent(&self) -> Option<MountId> {
        self.point.as_ref().map(Node::mnt)
    }
}

impl Mounts {
    /// 仓库里的挂载点，不持 BKL 的解析遇到就交回正常的解析。开机时的根文件系统不是挂载树
    /// 的根时（pivot_root 之后），它的根目录也算：`..` 要经由它的挂载点出去。
    fn store_points(&self) -> Arc<[Ino]> {
        let top = self
            .list
            .iter()
            .any(|m| m.id == ROOT_MOUNT && m.point.is_none());
        self.list
            .iter()
            .filter_map(|m| match m.point {
                Some(Node::Store(_, ino)) => Some(ino),
                _ => None,
            })
            .chain((!top).then_some(ROOT))
            .collect()
    }

    /// 开机时的根文件系统还挂在树上、没有 nosymfollow：不持 BKL 的解析只在它里面走。
    fn fast_usable(&self) -> bool {
        self.list
            .iter()
            .any(|m| m.id == ROOT_MOUNT && m.flags & ms::NOSYMFOLLOW == 0)
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
        fn name(&self, _: MountId, node: &PseudoNode) -> Vec<u8> {
            match node {
                PseudoNode::Static(b"sys/kernel/") => b"kernel".to_vec(),
                PseudoNode::Static(_) => b"sys".to_vec(),
                _ => Vec::new(),
            }
        }
        fn lookup(&self, _: MountId, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno> {
            match (dir, name) {
                (PseudoNode::ProcRoot, b"sys") => Ok(SYS),
                (PseudoNode::Static(b"sys/"), b"kernel") => Ok(KERNEL),
                _ => Err(Errno::ENOENT),
            }
        }
        fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
            Ok(mode::S_IFDIR)
        }
        fn readlink(&self, _: MountId, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
            Err(Errno::EINVAL)
        }
        fn follow(&self, _: MountId, _: &PseudoNode) -> Option<Result<Node, Errno>> {
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
    fn openat2_resolve_flags_limit_the_walk() {
        // sample 里：/usr/bin/env -> /bin/busybox，/usr/bin/rel -> ../../bin/busybox，
        // /bindir -> bin；/proc 挂着 procfs
        let (vfs, dir) = sample("openat2");
        let ps = &Empty;
        let cred = &Cred::root();
        let root = Node::ROOT;
        let usr = vfs.resolve(ps, cred, &root, &root, b"/usr", true).unwrap();
        let ubin = vfs
            .resolve(ps, cred, &root, &root, b"/usr/bin", true)
            .unwrap();
        let busybox = vfs
            .resolve(ps, cred, &root, &root, b"/bin/busybox", true)
            .unwrap();
        // 有作用域时根目录是起点
        let scoped = |start: &Node, p: &[u8], flags| {
            vfs.walk_resolve(ps, cred, start, start, p, true, flags)
                .map(|(n, _)| n)
        };
        let plain = |start: &Node, p: &[u8], flags| {
            vfs.walk_resolve(ps, cred, &root, start, p, true, flags)
                .map(|(n, _)| n)
        };
        use resolve::*;
        // BENEATH：绝对路径、越出起点的 ..、绝对路径的符号链接、越出起点的相对链接都是 EXDEV
        assert_eq!(scoped(&usr, b"bin", BENEATH), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"bin/..", BENEATH), Ok(usr.clone()));
        assert_eq!(scoped(&usr, b"..", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"/bin", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"bin/env", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"bin/rel", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&root, b"usr/bin/rel", BENEATH), Ok(busybox.clone()));
        // IN_ROOT：起点当作根目录，.. 停在起点，绝对路径从起点算
        assert_eq!(scoped(&usr, b"..", IN_ROOT), Ok(usr.clone()));
        assert_eq!(scoped(&usr, b"/bin", IN_ROOT), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"../../bin", IN_ROOT), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"bin/env", IN_ROOT), Err(Errno::ENOENT));
        // NO_SYMLINKS：途中和最后的符号链接都是 ELOOP，不跟随最后一个时照常
        assert_eq!(
            plain(&root, b"/bindir/busybox", NO_SYMLINKS),
            Err(Errno::ELOOP)
        );
        assert_eq!(
            plain(&root, b"/usr/bin/env", NO_SYMLINKS),
            Err(Errno::ELOOP)
        );
        assert!(
            vfs.walk_resolve(ps, cred, &root, &root, b"/usr/bin/env", false, NO_SYMLINKS)
                .is_ok()
        );
        // NO_XDEV：进出挂载是 EXDEV；还没取过根目录时跳到根目录也是
        assert_eq!(plain(&root, b"/proc", NO_XDEV), Err(Errno::EXDEV));
        assert_eq!(plain(&root, b"/usr/bin/env", NO_XDEV), Ok(busybox.clone()));
        assert_eq!(plain(&ubin, b"env", NO_XDEV), Err(Errno::EXDEV));
        assert_eq!(plain(&ubin, b"../bin/env", NO_XDEV), Ok(busybox.clone()));
        let proc = plain(&root, b"/proc", 0).unwrap();
        assert_eq!(plain(&proc, b"..", NO_XDEV), Err(Errno::EXDEV));
        // 新建时的父目录也受限制
        assert_eq!(
            vfs.resolve_parent_with(ps, cred, &usr, &usr, b"/x", BENEATH),
            Err(Errno::EXDEV)
        );
        assert_eq!(
            vfs.resolve_parent_with(ps, cred, &usr, &usr, b"/x", IN_ROOT),
            Ok((usr.clone(), &b"x"[..]))
        );
        drop(vfs);
        let _ = std::fs::remove_dir_all(dir);
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
            Ok(Node::Store(_, ino)) => ino,
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
                        .resolve(
                            ps,
                            cred,
                            &Node::ROOT,
                            &Node::Store(ROOT_MOUNT, cwd),
                            p,
                            follow,
                        )
                        .and_then(|n| match n {
                            Node::Store(_, ino) => vfs.store().stat(ino),
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
            Ok(Node::Store(_, ino)) => ino,
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
            Fs::Proc.root(),
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
            vfs.reach(ps, &usr, &proc_sys).unwrap(),
            (b"/proc/sys".to_vec(), false)
        );
        assert_eq!(
            vfs.reach(ps, &usr, &kernel).unwrap(),
            (b"/bin/sys/kernel".to_vec(), true)
        );

        // bind 挂载 procfs 里的 sys/：看到的是那个目录，`..` 回到挂载点的上级
        let b = vfs.add_mount(
            Fs::Proc,
            Node::Pseudo(0, SYS),
            (bin.clone(), None),
            0,
            b"p".to_vec(),
            Some(Fs::Proc.dev()),
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
            Fs::Pts.root(),
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
            Node::Store(_, i) => *i,
            Node::Pseudo(..) => unreachable!(),
        };
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin", true), None);
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/../usr", true), None);
        let lib = vfs.store().mkdir(ino(&ubin), b"lib", 0o755, 0, 0).unwrap();
        assert_eq!(
            r(&root, &Node::Store(ROOT_MOUNT, lib), b"..").unwrap(),
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
            vfs.reach(ps, &root, &kernel).unwrap(),
            (b"/sys/kernel".to_vec(), false)
        );
        assert_eq!(
            vfs.cached().stat(ROOT, lib, b"..", true),
            Some(vfs.store().stat(ino(&ubin)))
        );
        // 卸下的编号不再使用
        assert!(vfs.add_mount(Fs::Sys, Fs::Sys.root(), (usr, None), 0, Vec::new(), None) > s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binds_store_directories() {
        let (vfs, dir) = sample("binds");
        let ps = &Empty;
        let cred = &Cred::root();
        let r = |root: &Node, cwd: &Node, p: &[u8]| vfs.resolve(ps, cred, root, cwd, p, true);
        let root = Node::ROOT;
        let usr = r(&root, &root, b"/usr").unwrap();
        let ubin = r(&root, &root, b"/usr/bin").unwrap();
        let bin = r(&root, &root, b"/bin").unwrap();
        let busybox = r(&root, &root, b"/bin/busybox").unwrap();
        let ino = |n: &Node| n.ino().unwrap();

        // /usr bind 到 /bin 上：进去是经由新挂载的 /usr，`..` 回到挂载点的上级，路径经由挂载点
        let id = vfs.add_mount(
            Fs::Store,
            usr.clone(),
            (bin.clone(), None),
            ms::RELATIME,
            b"appine".to_vec(),
            Some(ROOT_DEV),
        );
        let top = Node::Store(id, ino(&usr));
        assert_eq!(r(&root, &root, b"/bin").unwrap(), top);
        let inner = r(&root, &root, b"/bin/bin").unwrap();
        assert_eq!(inner, Node::Store(id, ino(&ubin)));
        assert_ne!(inner, ubin);
        assert_eq!(vfs.path_of(ps, &root, &inner).unwrap(), b"/bin/bin");
        assert_eq!(vfs.path_of(ps, &root, &ubin).unwrap(), b"/usr/bin");
        assert_eq!(r(&root, &inner, b"..").unwrap(), top);
        assert_eq!(r(&root, &inner, b"../..").unwrap(), root);
        assert_eq!(vfs.parent(ps, &top).unwrap(), root);
        // 经由挂载的符号链接按挂载的路径解析：/bin/bin/rel 是 ../../bin/busybox，到了挂载的
        // 根之后回到仓库的根，再进 /bin 就又是这个挂载
        assert_eq!(
            r(&root, &root, b"/bin/bin/rel"),
            Err(Errno::ENOENT),
            "/bin is now /usr, which has no busybox"
        );
        assert_eq!(vfs.mount_root(&top), Some(id));
        assert_eq!(vfs.mount_root(&inner), None);
        assert!(vfs.is_mount_point(&bin));
        assert!(!vfs.is_mount_point(&Node::Store(id, ino(&bin))));
        assert!(vfs.is_store_point(ino(&bin)));
        assert_eq!(vfs.dev_of(&inner), ROOT_DEV);
        assert_eq!(vfs.root_path(ps, &vfs.mount(id).unwrap()), b"/usr");
        assert!(vfs.is_under(ps, &inner, &root));
        assert!(vfs.is_under(ps, &inner, &top));
        assert!(!vfs.is_under(ps, &inner, &usr));
        // 根目录换成挂载里的目录：`..` 不越过它，外面的到不了
        assert_eq!(r(&inner, &inner, b"/..").unwrap(), inner);
        assert_eq!(
            vfs.reach(ps, &inner, &top).unwrap(),
            (b"/bin".to_vec(), false)
        );

        // 只读：标志按经由的挂载算
        vfs.update_mount(id, |m| m.flags |= ms::RDONLY);
        assert!(vfs.readonly(&inner));
        assert!(!vfs.readonly(&ubin));

        // 文件 bind 到文件上：路径按挂上时的目录和名字
        let (f, at) = vfs
            .walk(ps, cred, &root, &root, b"/usr/bin/env", false)
            .unwrap();
        let fb = vfs.add_mount(
            Fs::Store,
            busybox.clone(),
            (f, at),
            0,
            Vec::new(),
            Some(ROOT_DEV),
        );
        let bound = vfs
            .walk(ps, cred, &root, &root, b"/usr/bin/env", false)
            .unwrap();
        assert_eq!(bound.0, Node::Store(fb, ino(&busybox)));
        assert_eq!(vfs.path_of(ps, &root, &ubin).unwrap(), b"/usr/bin");
        assert_eq!(
            vfs.reach(ps, &root, &bound.0).unwrap(),
            (b"/usr/bin/env".to_vec(), true)
        );

        // 改挂到 /proc 上：/bin 又是原来的目录
        let proc_top = r(&root, &root, b"/proc").unwrap();
        vfs.move_mount(id, Some((proc_top.clone(), None)));
        assert_eq!(r(&root, &root, b"/bin").unwrap(), bin);
        assert_eq!(r(&root, &root, b"/proc/bin").unwrap(), inner);
        assert_eq!(vfs.path_of(ps, &root, &inner).unwrap(), b"/proc/bin");

        // 卸下：里面的节点照常能用，挂载的根没有上级、到不了根目录；没人用了就丢掉记录
        vfs.remove_mount(id);
        assert_eq!(r(&root, &root, b"/proc").unwrap(), proc_top);
        assert_eq!(r(&root, &inner, b"../../..").unwrap(), top);
        assert_eq!(
            vfs.reach(ps, &root, &inner).unwrap(),
            (b"/bin".to_vec(), false)
        );
        assert!(vfs.readonly(&inner));
        vfs.collect(&std::collections::HashSet::new());
        assert!(!vfs.readonly(&inner));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmpfs_lives_while_mounted() {
        let (vfs, dir) = sample("tmpfs");
        let ps = &Empty;
        let cred = &Cred::root();
        let r = |p: &[u8]| vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, p, true);
        let usr = r(b"/usr").unwrap();
        let bin = r(b"/bin").unwrap();
        let params = TmpfsParams {
            perm: 0o755,
            uid: 0,
            gid: 0,
            blocks: TmpfsParams::default_limit(),
            inodes: TmpfsParams::default_limit(),
            inode64: true,
        };
        let a = vfs
            .new_tmpfs((usr.clone(), None), 0, b"t".to_vec(), params)
            .unwrap();
        let top = r(b"/usr").unwrap();
        assert_eq!(top.mnt(), a);
        assert_eq!(vfs.fs_of(&top), Fs::Tmpfs);
        assert_ne!(vfs.dev_of(&top), ROOT_DEV);
        assert_eq!(vfs.sb_options(&vfs.mount(a).unwrap()), ",mode=755,inode64");
        let root = top.ino().unwrap();
        let sub = vfs.store().mkdir(root, b"sub", 0o755, 0, 0).unwrap();
        let (f, _) = vfs.store().create_file(sub, b"f", 0o644, 0, 0).unwrap();
        // tmpfs 的根上面是挂载点；里面的路径经由它
        assert_eq!(r(b"/usr/sub/..").unwrap(), top);
        assert_eq!(r(b"/usr/..").unwrap(), Node::ROOT);
        assert_eq!(
            vfs.path_of(ps, &Node::ROOT, &Node::Store(a, sub)).unwrap(),
            b"/usr/sub"
        );
        // bind 出去的子目录让它活着
        let b = vfs.add_mount(
            Fs::Tmpfs,
            Node::Store(a, sub),
            (bin, None),
            0,
            Vec::new(),
            Some(vfs.dev_of(&top)),
        );
        assert_eq!(vfs.root_path(ps, &vfs.mount(b).unwrap()), b"/sub");
        vfs.remove_mount(a);
        vfs.collect(&std::collections::HashSet::new());
        assert_eq!(vfs.store().stat(f).unwrap().nlink, 1);
        assert_eq!(r(b"/bin/f").unwrap(), Node::Store(b, f));
        // 都没了：连同里面的一起删掉
        vfs.remove_mount(b);
        vfs.collect(&std::collections::HashSet::new());
        assert_eq!(vfs.store().stat(f).unwrap().nlink, 0);
        assert_eq!(vfs.store().stat(root).unwrap().nlink, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_and_drops_mount_namespaces() {
        let (vfs, dir) = sample("mntns");
        let ps = &Empty;
        let cred = &Cred::root();
        let usr = vfs
            .resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/usr", true)
            .unwrap();
        let before = vfs.mounts_in(INIT_MNT_NS);
        // 复制出来的命名空间：挂载一一对应，编号是新的，挂载点、根目录跟着换
        let (ns, map) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let copied = vfs.mounts_in(ns.id);
        assert_eq!(copied.len(), before.len());
        assert!(copied.iter().all(|m| !map.contains_key(&m.id)));
        let root = vfs.ns_root(ns.id).unwrap();
        assert_eq!(root, Node::Store(map[&ROOT_MOUNT], ROOT));
        let proc = vfs.resolve(ps, cred, &root, &root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(map[&PROC_MOUNT], PseudoNode::ProcRoot));
        assert_eq!(vfs.mount_ns(proc.mnt()), Some(ns.id));
        // 新命名空间里挂的 tmpfs 只在那边
        let point = vfs.resolve(ps, cred, &root, &root, b"/usr", true).unwrap();
        let params = TmpfsParams {
            perm: 0o1777,
            uid: 0,
            gid: 0,
            blocks: TmpfsParams::default_limit(),
            inodes: TmpfsParams::default_limit(),
            inode64: true,
        };
        let t = vfs.new_tmpfs((point, None), 0, Vec::new(), params).unwrap();
        assert_eq!(vfs.mount_ns(t), Some(ns.id));
        let troot = vfs.mount(t).unwrap().root.ino().unwrap();
        assert_eq!(
            vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/usr", true),
            Ok(usr)
        );
        // 没人用了：清理时卸下它的挂载，tmpfs 随之删掉
        assert!(!vfs.has_dead_ns());
        drop(ns);
        assert!(vfs.has_dead_ns());
        vfs.collect(&std::collections::HashSet::new());
        assert!(!vfs.has_dead_ns());
        assert!(vfs.mount(t).is_none());
        assert_eq!(vfs.store().stat(troot).unwrap().nlink, 0);
        assert_eq!(vfs.mounts_in(INIT_MNT_NS).len(), before.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bound_namespace_files_keep_namespaces() {
        let (vfs, dir) = sample("nsfs");
        let (pin, _) = vfs.store().create_file(ROOT, b"pin", 0o444, 0, 0).unwrap();
        let point = (
            Node::Store(ROOT_MOUNT, pin),
            Some((Node::ROOT, b"pin".to_vec())),
        );
        let before = vfs.mounts_in(INIT_MNT_NS).len();
        // 新的挂载命名空间 A 的文件 bind 到开机时的命名空间里
        let (a, _) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let a_id = a.id;
        let file = Node::Pseudo(
            PROC_MOUNT,
            PseudoNode::NsFile(crate::pseudo::NsKind::Mnt, a_id),
        );
        let fs = Fs::Nsfs;
        let id = vfs.add_mount(fs, file, point, 0, b"nsfs".to_vec(), Some(fs.dev()));
        vfs.update_mount(id, |m| m.pin = Some(crate::nsproxy::NsRef::Mnt(a.clone())));
        // 复制开机时的命名空间时不带它（不然 A 经由复制出来的挂载留着自己）
        let (c, _) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let copied = vfs.mounts_in(c.id);
        assert_eq!(copied.len(), before);
        assert!(copied.iter().all(|m| m.pin.is_none()));
        // A 里没有进程了：挂载留着它
        drop(a);
        assert!(!vfs.has_dead_ns());
        assert!(vfs.find_ns(a_id).is_some());
        assert_eq!(vfs.pinned_ns(a_id).map(|r| r.id()), Some(a_id));
        // 卸下之后清理：A 连同它的挂载一起没了
        vfs.remove_mount(id);
        vfs.collect(&std::collections::HashSet::new());
        assert!(vfs.find_ns(a_id).is_none());
        assert!(vfs.mounts_in(a_id).is_empty());
        assert!(vfs.pinned_ns(a_id).is_none());
        assert_eq!(vfs.mounts_in(c.id).len(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmpfs_counts_pages_and_inodes() {
        let (vfs, dir) = sample("tmpfs-usage");
        let ps = &Empty;
        let cred = &Cred::root();
        let usr = vfs
            .resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/usr", true)
            .unwrap();
        let params = TmpfsParams {
            perm: 0o1777,
            uid: 0,
            gid: 0,
            blocks: 4,
            inodes: 5,
            inode64: true,
        };
        let id = vfs.new_tmpfs((usr, None), 0, Vec::new(), params).unwrap();
        let m = vfs.mount(id).unwrap();
        let (root, dev) = (m.root.ino().unwrap(), m.dev);
        assert_eq!(vfs.sb_options(&m), ",size=16k,nr_inodes=5,inode64");
        let usage = |fresh| {
            let (_, pages, inodes) = vfs.tmpfs_usage(dev, fresh).unwrap();
            (pages, inodes)
        };
        // 只有根目录
        assert_eq!(usage(true), (0, 1));
        // 目录、两页的文件和它的另一个链接、长的符号链接：每个名字一个 inode，页按文件算一次
        let sub = vfs.store().mkdir(root, b"sub", 0o755, 0, 0).unwrap();
        let (f, file) = vfs.store().create_file(sub, b"f", 0o644, 0, 0).unwrap();
        file.set_len(5000).unwrap();
        vfs.store().link(root, b"g", f).unwrap();
        vfs.store().symlink(root, b"s", &[b'x'; 200], 0, 0).unwrap();
        assert_eq!(usage(false), (0, 1), "直接改仓库不记账");
        assert_eq!(usage(true), (3, 5));
        assert_eq!(vfs.tmpfs_check(dev, 0, 1), Err(Errno::ENOSPC));
        assert_eq!(vfs.tmpfs_check(dev, 2, 0), Err(Errno::ENOSPC));
        assert_eq!(vfs.tmpfs_check(dev, 1, 0), Ok(()));
        // f 本来有两页，加上没用掉的一页
        assert_eq!(vfs.tmpfs_room(dev, 2), Ok(Some(3)));
        vfs.tmpfs_charge(dev, 1, 0);
        assert_eq!(usage(false), (4, 5));
        assert_eq!(vfs.tmpfs_room(dev, 2), Ok(Some(2)));
        // 现数一遍：记下的跟着改
        assert_eq!(usage(true), (3, 5));
        assert_eq!(usage(false), (3, 5));
        // 重新挂载：不能小于用量
        let shrink = |blocks| {
            vfs.tmpfs_reconfigure(dev, |p, pages, _| {
                if blocks < pages {
                    return Err(Errno::EINVAL);
                }
                p.blocks = blocks;
                Ok(())
            })
        };
        assert_eq!(shrink(2), Err(Errno::EINVAL));
        assert_eq!(shrink(3), Ok(()));
        assert_eq!(vfs.sb_options(&m), ",size=12k,nr_inodes=5,inode64");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_paths_lexically() {
        assert_eq!(normalize(b"/a/./b//../c"), b"/a/c");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(normalize(b"/tmp/x/"), b"/tmp/x");
    }
}
