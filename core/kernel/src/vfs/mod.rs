//! The virtual filesystem (the kernel/vfs map).
//!
//! The root filesystem is the inode store, and the device nodes in `/dev` live in the store too
//! ([`dev`]). procfs, devpts, sysfs and cgroup2 are synthetic filesystems: their content comes
//! from kernel state and is provided by the kernel layer through [`Pseudo`]. At boot they are
//! mounted on `/proc`, `/dev/pts`, `/sys` and `/sys/fs/cgroup`, and later mount(2) can mount
//! them on other directories; directories and files both in the store and in synthetic
//! filesystems can be bind-mounted ([`Mount`]). A tmpfs is a tree in the store that does not hang
//! under the root directory ([`Vfs::new_tmpfs`]), deleted when its last mount goes. Nodes carry
//! the mount they are in (Linux's `struct path`); `..` and getcwd go back up through mount
//! points, and mount flags (read-only, noexec and so on) follow the mount reached through. Path
//! resolution runs over the whole tree and follows at most 40 levels of symlinks; every store
//! directory passed needs search permission. A process's root can be replaced by chroot and
//! pivot_root: absolute paths start from it, and `..` never goes past it.

pub mod browse;
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

/// Maximum number of symlinks followed, the same as Linux's MAXSYMLINKS.
pub(crate) const MAX_SYMLINKS: u32 = 40;
/// Maximum length of a path component (`NAME_MAX`).
const NAME_MAX: usize = 255;

/// A node in the filesystem tree plus the mount it was reached through (Linux's `struct path`):
/// one directory mounted (bound) in two places is two nodes, and `..` from each goes back to its
/// own mount point.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    /// An inode in the inode store.
    Store(MountId, Ino),
    /// A node in a synthetic filesystem.
    Pseudo(MountId, PseudoNode),
}

impl Node {
    /// The root directory at boot.
    pub const ROOT: Node = Node::Store(ROOT_MOUNT, ROOT);

    /// The mount reached through.
    pub(crate) fn mnt(&self) -> MountId {
        match self {
            Node::Store(id, _) | Node::Pseudo(id, _) => *id,
        }
    }

    /// The store inode; None for nodes in synthetic filesystems.
    pub(crate) fn ino(&self) -> Option<Ino> {
        match self {
            Node::Store(_, ino) => Some(*ino),
            Node::Pseudo(..) => None,
        }
    }

    /// The node of the same file reached through mount `id`.
    pub(crate) fn in_mount(self, id: MountId) -> Node {
        match self {
            Node::Store(_, ino) => Node::Store(id, ino),
            Node::Pseudo(_, p) => Node::Pseudo(id, p),
        }
    }
}

/// A directory and one name in it.
pub(crate) type Dentry = (Node, Vec<u8>);

/// Mount id (the first column of mountinfo, `mnt_id` in fdinfo), numbered across all mount
/// namespaces together.
pub(crate) type MountId = u32;

/// Device number of the mqueue filesystem of the boot IPC namespace (0:19).
pub(crate) const MQUEUE_DEV: u64 = 0x13;
/// Id of the boot mount namespace: the inode number in `/proc/<pid>/ns/mnt`, the same as is
/// common on Linux.
pub(crate) const INIT_MNT_NS: u64 = 4_026_531_841;

/// A mount namespace (Linux's `struct mnt_namespace`): only an id; each mount records which
/// namespace it belongs to ([`Mount::ns`]). A process's `FsContext` holds it; after the last
/// reference goes, the next cleanup ([`Vfs::collect`]) detaches all its mounts.
#[derive(Debug)]
pub(crate) struct MntNs {
    pub id: u64,
    /// Owner: the user namespace it was created in (mounting needs `CAP_SYS_ADMIN` there,
    /// Linux's `may_mount`).
    pub owner: std::rc::Rc<crate::cred::UserNs>,
    /// Anonymous: the namespace of a tree copied by open_tree(`OPEN_TREE_CLONE`) and not yet
    /// attached; no process is in it, and it is held by an open file ([`Vfs::anon_ns`]).
    pub anon: bool,
}

/// Allocates a new namespace id (Linux's `proc_alloc_inum`, shared by all kinds of namespace).
pub(crate) fn new_ns_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(4_026_532_160);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
/// The root filesystem (the inode store).
pub(crate) const ROOT_MOUNT: MountId = 1;
/// The procfs mounted on `/proc` at boot.
pub(crate) const PROC_MOUNT: MountId = 2;
/// The devpts mounted on `/dev/pts` at boot.
pub(crate) const PTS_MOUNT: MountId = 3;
/// The sysfs mounted on `/sys` at boot.
pub(crate) const SYS_MOUNT: MountId = 4;
/// `mnt_id` in fdinfo of open files that are in no filesystem (pipes, sockets and so on).
pub(crate) const ANON_MOUNT: MountId = 5;
/// The cgroup2 mounted on `/sys/fs/cgroup` at boot.
pub(crate) const CGROUP_MOUNT: MountId = 6;
/// The tmpfs mounted on `/dev` at boot (Linux's devtmpfs, the tmpfs container runtimes mount).
pub(crate) const DEV_MOUNT: MountId = 7;
/// The tmpfs mounted on `/dev/shm` at boot.
pub(crate) const SHM_MOUNT: MountId = 8;
/// The mqueue mounted on `/dev/mqueue` at boot.
pub(crate) const MQUEUE_MOUNT: MountId = 9;
/// The tmpfs mounted on `/run` at boot.
pub(crate) const RUN_MOUNT: MountId = 10;

/// Kinds of filesystem: the inode store and several synthetic filesystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Fs {
    /// The inode store (the root filesystem).
    Store,
    /// tmpfs: a tree in the store that does not hang under the root directory
    /// ([`Vfs::new_tmpfs`]).
    Tmpfs,
    /// procfs.
    Proc,
    /// devpts.
    Pts,
    /// sysfs.
    Sys,
    /// cgroup2 (`crate::cgroup`).
    Cgroup,
    /// nsfs: the filesystem namespace files (what following `/proc/<pid>/ns/*` gives) are in. It
    /// cannot be mounted; only namespace files bind-mounted elsewhere are in the mount table.
    Nsfs,
    /// mqueue: the POSIX message queues (`crate::syscall::mqueue`) of the IPC namespace of the
    /// process that mounts it. One instance per namespace (Linux's `get_tree_keyed`); the mount
    /// keeps that namespace alive.
    Mqueue,
}

impl Fs {
    /// Filesystem types mount(2) can newly mount (the same names as in `/proc/filesystems`).
    pub(crate) fn from_type(name: &[u8]) -> Option<Fs> {
        match name {
            b"proc" => Some(Fs::Proc),
            b"devpts" => Some(Fs::Pts),
            b"sysfs" => Some(Fs::Sys),
            b"cgroup2" => Some(Fs::Cgroup),
            b"tmpfs" => Some(Fs::Tmpfs),
            b"mqueue" => Some(Fs::Mqueue),
            _ => None,
        }
    }

    /// Name of the filesystem type.
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            Fs::Store => "ext4",
            Fs::Tmpfs => "tmpfs",
            Fs::Proc => "proc",
            Fs::Pts => "devpts",
            Fs::Sys => "sysfs",
            Fs::Cgroup => "cgroup2",
            Fs::Nsfs => "nsfs",
            Fs::Mqueue => "mqueue",
        }
    }

    /// Device number of the instance mounted at boot (stat's `st_dev`, the third column of
    /// mountinfo).
    pub(crate) const fn dev(self) -> u64 {
        match self {
            Fs::Store => ROOT_DEV,
            // No boot instance; allocated separately on each mount
            Fs::Tmpfs => 0,
            Fs::Proc => 0x17,
            Fs::Pts => 0x18,
            Fs::Sys => 0x16,
            Fs::Cgroup => 0x1a,
            // As on Linux
            Fs::Nsfs => 0x4,
            // The boot IPC namespace's; other namespaces' are allocated separately
            // ([`Vfs::mq_dev`])
            Fs::Mqueue => MQUEUE_DEV,
        }
    }

    /// Every mount is a new instance (procfs since Linux 5.8, devpts since 4.7): another one can
    /// be mounted at the same place. sysfs and cgroup2 have only one instance per namespace, and
    /// mounting again on the root of the same mount is EBUSY.
    pub(crate) const fn new_instance(self) -> bool {
        matches!(self, Fs::Proc | Fs::Pts | Fs::Tmpfs)
    }

    /// Newly mounted instances have their own device numbers: procfs, tmpfs, and devpts (each
    /// instance has its own set of terminals, and the device number is the instance number,
    /// [`crate::tty::PtsOptions`]).
    pub(crate) const fn own_dev(self) -> bool {
        matches!(self, Fs::Proc | Fs::Tmpfs | Fs::Pts)
    }

    /// Superblock options (the part after `rw` in the last column of mountinfo).
    pub(crate) const fn options(self) -> &'static str {
        match self {
            Fs::Cgroup => ",nsdelegate,memory_recursiveprot",
            // Per instance for tmpfs and devpts ([`Vfs::sb_options`])
            Fs::Store | Fs::Tmpfs | Fs::Pts | Fs::Proc | Fs::Sys | Fs::Nsfs | Fs::Mqueue => "",
        }
    }

    /// The filesystem's root directory (mount id 0, replaced by the new mount's when mounted).
    /// tmpfs's root is a directory created at mount time ([`Vfs::new_tmpfs`]) and nsfs cannot
    /// be mounted; neither uses this.
    pub(crate) fn root(self) -> Node {
        Node::Pseudo(
            0,
            match self {
                Fs::Store | Fs::Tmpfs => return Node::Store(0, ROOT),
                Fs::Proc | Fs::Nsfs => PseudoNode::ProcRoot,
                // Replaced by the new instance's when mounted
                Fs::Pts => PseudoNode::PtsRoot(crate::tty::BOOT_PTS),
                Fs::Sys => PseudoNode::SysRoot,
                // Replaced by the root of the caller's cgroup namespace when mounted
                Fs::Cgroup => PseudoNode::Cgroup(crate::cgroup::ROOT, b""),
                // Replaced by the caller's IPC namespace's when mounted
                Fs::Mqueue => PseudoNode::MqRoot(crate::nsproxy::INIT_IPC_NS),
            },
        )
    }
}

/// Mount flags: the same values as mount(2)'s `MS_*`.
pub(crate) mod ms {
    pub const RDONLY: u64 = 1;
    pub const NOSUID: u64 = 2;
    pub const NODEV: u64 = 4;
    pub const NOEXEC: u64 = 8;
    pub const NOSYMFOLLOW: u64 = 0x100;
    pub const NOATIME: u64 = 0x400;
    pub const NODIRATIME: u64 = 0x800;
    pub const RELATIME: u64 = 1 << 21;
    /// Those kept on the mount (the ones matching Linux's `MNT_*`).
    pub const PER_MOUNT: u64 =
        RDONLY | NOSUID | NODEV | NOEXEC | NOSYMFOLLOW | NOATIME | NODIRATIME | RELATIME;
}

/// Mount flags of the root filesystem: `rw,relatime`.
pub(crate) const ROOT_FLAGS: u64 = ms::RELATIME;
/// Device number the root filesystem (the inode store) reports (8:1).
pub(crate) const ROOT_DEV: u64 = 0x801;

/// A mount (Linux's `struct mount`): `root` in filesystem `fs` appears at `point`.
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub id: MountId,
    /// The mount namespace it belongs to ([`MntNs::id`]).
    pub ns: u64,
    pub fs: Fs,
    /// Root of the part the mount sees (a node reached through this mount): the filesystem's
    /// root directory, or for a bind mount the directory or file that was bound.
    pub root: Node,
    /// Mount point: a directory in another mount (a file when a file is bind-mounted). The root
    /// of the mount tree (the store at boot, the one swapped in after pivot_root) and detached
    /// mounts have none; on Linux the former is mounted on the invisible rootfs.
    pub point: Option<Node>,
    /// When the mount point is a file in the store, its directory and name: a file can have
    /// several links and no unique parent directory, so the path is computed from this one,
    /// recorded at mount time.
    pub at: Option<Dentry>,
    /// When a file in the store is bind-mounted, the directory and name it was found at (the
    /// fourth column of mountinfo is computed from it).
    pub root_at: Option<Dentry>,
    /// Flags from [`ms`].
    pub flags: u64,
    /// Device number of the instance ([`Fs::own_dev`]).
    pub dev: u64,
    /// mount(2)'s source (the tenth column of mountinfo).
    pub source: Vec<u8>,
    /// Marked by `umount2(MNT_EXPIRE)`: detached the next time.
    pub expiring: bool,
    /// A namespace file bind-mounted elsewhere ([`Fs::Nsfs`]): it keeps that namespace alive.
    pub pin: Option<crate::nsproxy::NsRef>,
}

impl Mount {
    /// The mount the mount point is in (the second column of mountinfo); the root of the mount
    /// tree and detached mounts have none.
    pub(crate) fn parent(&self) -> Option<MountId> {
        self.point.as_ref().map(Node::mnt)
    }
}

/// Directory structure of a synthetic filesystem (implemented by the kernel layer, with content
/// from kernel state).
pub(crate) trait Pseudo {
    /// Parent directory; None for the root.
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode>;
    /// The node's name in its parent directory. `mnt` is the mount the node is in: pids in
    /// procfs follow the pid namespace of the mount's instance.
    fn name(&self, mnt: MountId, node: &PseudoNode) -> Vec<u8>;
    /// Looks up in a directory.
    fn lookup(&self, mnt: MountId, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno>;
    /// File type bits (the `S_IFMT` part).
    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno>;
    /// Contents of a symlink.
    fn readlink(&self, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno>;
    /// The node a "magic link" (`/proc/<pid>/fd/<n>` and the like) leads to directly when
    /// followed, without resolving its contents; None when it is not a magic link, an error
    /// (EACCES) when following is not allowed. `mnt` is the mount the link is in.
    fn follow(&self, mnt: MountId, node: &PseudoNode) -> Option<Result<Node, Errno>>;
    /// Through which name in which directory the open file a magic link points at was opened (a
    /// file that is not a directory has no unique parent directory; when it is a mount point,
    /// mountinfo's path is computed from this).
    fn follow_dentry(&self, _node: &PseudoNode) -> Option<Dentry> {
        None
    }
    /// Whether this is a Linux magic link (`nd_jump_link`): openat2's `RESOLVE_NO_MAGICLINKS`
    /// and scoped resolution do not follow them. Includes those [`Pseudo::follow`] jumps to
    /// directly, and also those resolved here by content that Linux jumps to directly
    /// (`/proc/<pid>/exe`).
    fn magic(&self, _node: &PseudoNode) -> bool {
        false
    }
    /// Whether `fs.protected_symlinks` is on ([`Vfs::may_follow_link`]).
    fn protected_symlinks(&self) -> bool {
        false
    }
}

/// The `resolve` of openat2's `struct open_how` (Linux's `RESOLVE_*`, matching path
/// resolution's `LOOKUP_NO_XDEV` and the like).
pub(crate) mod resolve {
    /// No crossing mounts (bind mounts included): going in or out through mount points, `..`
    /// past a mount's root, symlinks to absolute paths that jump to another mount, and magic
    /// links are all EXDEV.
    pub const NO_XDEV: u64 = 0x01;
    /// Do not follow magic links (ELOOP).
    pub const NO_MAGICLINKS: u64 = 0x02;
    /// Do not follow any symlink (ELOOP).
    pub const NO_SYMLINKS: u64 = 0x04;
    /// Do not leave the start: absolute paths, symlinks to absolute paths, `..` at the start and
    /// magic links are all EXDEV.
    pub const BENEATH: u64 = 0x08;
    /// Treat the start as the root directory: absolute paths and symlinks to absolute paths
    /// count from the start, and `..` at the start stays put; magic links are EXDEV.
    pub const IN_ROOT: u64 = 0x10;
    /// Resolve only from the cache (without blocking).
    pub const CACHED: u64 = 0x20;
    pub const ALL: u64 = 0x3f;
    /// Scoped (`LOOKUP_IS_SCOPED`): the root is the start.
    pub const SCOPED: u64 = BENEATH | IN_ROOT;
}

/// A virtual filesystem instance.
pub struct Vfs {
    store: Store,
    /// The mount table: the boot ones and those mount(2) mounted later, in mount order.
    mounts: RefCell<Mounts>,
    /// Regenerated whenever the mount table changes.
    cached: RefCell<Cached>,
    /// Recently executed program files ([`Vfs::executable`]).
    executables: crate::loader::ExeCache,
    /// The boot mount namespace.
    init_ns: std::rc::Rc<MntNs>,
    /// Mount namespaces created later: once all references to one are gone, cleanup detaches
    /// its mounts.
    namespaces: RefCell<Vec<std::rc::Weak<MntNs>>>,
}

struct Mounts {
    /// Attached mounts, in mount order (the mountinfo order); the first is the root of the mount
    /// tree, and it is not moved even when that is no longer true after pivot_root.
    list: Vec<Mount>,
    /// Detached mounts (`MNT_DETACH`) that still have a process's root, current directory or
    /// open files in them: their nodes keep working, and the mount's root has no parent
    /// ([`Vfs::collect`] removes the unused ones).
    detached: Vec<Mount>,
    /// The id of the next mount. Ids of detached mounts are never reused: files still open and
    /// current directories after the detach keep the old id, which must not count toward a new
    /// mount.
    next: MountId,
    /// Device number of the next new instance (from 0:48, avoiding [`Fs::dev`] and those pipes,
    /// sockets and so on use).
    next_dev: u64,
    /// tmpfs instances, by device number (bind mounts share their source's device number).
    tmpfs: std::collections::HashMap<u64, Tmpfs>,
    /// The pid namespace (the mounting process's) of each procfs instance mounted by mount(2),
    /// by device number; the boot one and those not recorded use the boot pid namespace.
    proc_ns: std::collections::HashMap<u64, std::rc::Rc<crate::pidns::PidNs>>,
    /// Device numbers of each IPC namespace's mqueue instance ([`Vfs::mq_dev`]), by namespace
    /// id.
    mq_dev: std::collections::HashMap<u64, u64>,
    /// Options of devpts instances mounted by mount(2), by device number (the boot one is not
    /// here).
    devpts: std::collections::HashMap<u64, crate::tty::PtsOptions>,
}

impl Mounts {
    /// A boot mount: root `root` of filesystem `fs` (device number `dev`) mounted on `point`;
    /// not mounted when there is no mount point.
    fn boot(
        &mut self,
        id: MountId,
        (fs, root, dev): (Fs, Node, u64),
        point: Option<Node>,
        flags: u64,
        source: &[u8],
    ) {
        let Some(point) = point else {
            crate::debug::log(format_args!("no mount point for {fs:?}"));
            return;
        };
        self.list.push(Mount {
            id,
            ns: INIT_MNT_NS,
            fs,
            root,
            point: Some(point),
            at: None,
            root_at: None,
            flags,
            dev,
            source: source.to_vec(),
            expiring: false,
            pin: None,
        });
    }

    /// A tmpfs mounted at boot (the same as [`Vfs::new_tmpfs`], except that the mount id is
    /// fixed); returns its root directory.
    fn boot_tmpfs(
        &mut self,
        store: &Store,
        id: MountId,
        point: Node,
        flags: u64,
        source: &[u8],
        params: TmpfsParams,
    ) -> Result<Ino, Errno> {
        let root = store.create_detached_dir(params.perm, params.uid, params.gid)?;
        let dev = self.next_dev;
        self.next_dev += 1;
        let fs = (Fs::Tmpfs, Node::Store(id, root), dev);
        self.boot(id, fs, Some(point), flags, source);
        // Usage is counted when needed ([`dev::populate`] created nodes in it)
        let used = None;
        self.tmpfs.insert(dev, Tmpfs { root, params, used });
        Ok(root)
    }

    /// Mount points in the store; no-BKL resolution hands back to normal resolution when it
    /// meets one. When the boot root filesystem is not the root of the mount tree (after
    /// pivot_root), its root directory counts too: `..` has to leave through its mount point.
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

    /// The boot root filesystem is still attached and not nosymfollow: no-BKL resolution walks
    /// only inside it.
    fn fast_usable(&self) -> bool {
        self.list
            .iter()
            .any(|m| m.id == ROOT_MOUNT && m.flags & ms::NOSYMFOLLOW == 0)
    }
}

/// tmpfs accounts usage in pages; the page size (guest pages).
pub(crate) const TMPFS_PAGE: u64 = 4096;

/// tmpfs superblock parameters (these fields of Linux's `shmem_sb_info`): set by mount(2)'s
/// options at mount time; a remount can change the size, the inode count and inode32/64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmpfsParams {
    /// Permissions, owner and group of the root directory (used only at mount time).
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    /// Maximum pages and inodes; 0 is unlimited (`size=0`, `nr_inodes=0`).
    pub blocks: u64,
    pub inodes: u64,
    /// `inode64` (the default) or `inode32`.
    pub inode64: bool,
}

impl TmpfsParams {
    /// A tmpfs mounted at boot: the root directory has permissions `perm` and belongs to root,
    /// and the size and inode count are the defaults.
    fn boot(perm: u32) -> TmpfsParams {
        TmpfsParams {
            perm,
            uid: 0,
            gid: 0,
            blocks: Self::default_limit(),
            inodes: Self::default_limit(),
            inode64: true,
        }
    }

    /// Superblock options in mountinfo (like `,size=1024k,mode=755,inode64`, Linux's
    /// `shmem_show_options`): size and inode count are written only when not the defaults, the
    /// permissions only when not 1777 and owner and group only when not 0, and last comes
    /// `inode64` or `inode32`.
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

    /// Default page and inode counts: half of physical memory (Linux's
    /// `shmem_default_max_blocks` and `shmem_default_max_inodes`, which are equal without high
    /// memory).
    pub fn default_limit() -> u64 {
        appine_hal::memory_info().0 / 2 / TMPFS_PAGE
    }
}

/// A tmpfs instance (Linux's superblock): at mount time a directory not hanging under the root
/// is created in the store ([`Store::create_detached_dir`]), and when its mounts and bind mounts
/// are all gone it is deleted with everything in it. Data is stored in host files as usual and
/// takes no app memory. Usage is limited by page and inode counts as on Linux
/// ([`Vfs::tmpfs_usage`]).
pub(crate) struct Tmpfs {
    /// The root directory.
    pub root: Ino,
    pub params: TmpfsParams,
    /// Recorded usage (pages, inodes), updated on create, delete, write and truncate, and counted
    /// afresh at statfs ([`Vfs::count_tmpfs`]); None means count it the next time it is needed.
    used: Option<(u64, u64)>,
}

/// What no-BKL path resolution needs (`syscall::fast`): the store's metadata cache and the mount
/// points.
#[derive(Clone)]
pub(crate) struct Cached {
    meta: Arc<store::Meta>,
    mounts: Arc<[Ino]>,
    /// The root filesystem's mount ([`ROOT_MOUNT`]) is still attached and not nosymfollow: only
    /// threads whose root and current directory are in it can resolve without the BKL.
    pub(crate) usable: bool,
}

impl Cached {
    /// Resolves a path and gets its metadata using only the metadata cache: equivalent to
    /// [`Vfs::resolve`] with credentials that have `CAP_DAC_READ_SEARCH` (no search permission
    /// check) plus [`Store::stat`]. `root` and `cwd` are store directories. Returns None when the
    /// path enters a synthetic filesystem or something needed is not in the cache; the caller
    /// then resolves normally holding the BKL (filling the cache along the way).
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

    /// Corresponds one to one with [`Vfs::walk_in`].
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
        // Components not yet handled, with the next one on top of the stack; allocates no
        // memory, and leaves it to normal resolution when they do not fit
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
                // Something is mounted on the parent directory (the current directory is under
                // a mount point mounted later)
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
                // fs.protected_symlinks depends on the credentials ([`Vfs::may_follow_link`]):
                // links in sticky, world-writable directories whose owner differs from the
                // directory's are left to normal resolution
                let sticky_shared = mode::S_ISVTX | 0o002;
                if last
                    && r.stat(dir)?.mode & sticky_shared == sticky_shared
                    && r.stat(dir)?.uid != r.stat(node)?.uid
                {
                    return None;
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

/// Path components [`Cached::walk`] has not handled yet (the result of [`components`] in
/// reverse).
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
    /// Puts `path`'s components at the front; returns None when they do not fit.
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
    /// Opens the inode store in `dir` as the root filesystem, adds the standard device nodes,
    /// and mounts the boot filesystems (the same as systemd and container runtimes mount):
    /// procfs; a tmpfs on `/dev` (the standard device nodes are created in it,
    /// [`dev::populate`]) with devpts, the `/dev/shm` tmpfs and mqueue in it; sysfs and cgroup2;
    /// the `/run` tmpfs.
    /// At boot (when no other process is using this store) it also empties /dev/shm and /run in
    /// the store: no tmpfs used to be mounted on them, and the shared memory and pid files left
    /// there must not stay.
    pub fn open(dir: &Path) -> Result<Vfs, Errno> {
        let store = Store::open(dir, &[b"dev/shm", b"run"])?;
        if let Err(e) = dev::ensure_dev_nodes(&store) {
            crate::debug::log(format_args!("cannot create the device nodes: {e}"));
        }
        if let Err(e) = ensure_resolv_conf(&store) {
            crate::debug::log(format_args!("cannot create /etc/resolv.conf: {e}"));
        }
        // The same as Linux distributions' default mount options
        let flags = ms::NOSUID | ms::NODEV | ms::NOEXEC | ms::RELATIME;
        let mut mounts = Mounts {
            list: vec![Mount {
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
            }],
            detached: Vec::new(),
            next: RUN_MOUNT + 1,
            next_dev: 0x30,
            tmpfs: Default::default(),
            proc_ns: Default::default(),
            mq_dev: Default::default(),
            devpts: Default::default(),
        };
        // The directory named `name` under directory `dir` in mount `mnt`
        let subdir = |dir: Option<&Node>, name: &[u8]| -> Option<Node> {
            let &Node::Store(mnt, dir) = dir? else {
                return None;
            };
            let ino = store.lookup(dir, name).ok().flatten()?;
            store
                .stat(ino)
                .is_ok_and(|st| st.is_dir())
                .then_some(Node::Store(mnt, ino))
        };
        let root = Some(Node::ROOT);
        let pseudo = |id: MountId, fs: Fs| (fs, fs.root().in_mount(id), fs.dev());
        mounts.boot(
            PROC_MOUNT,
            pseudo(PROC_MOUNT, Fs::Proc),
            subdir(root.as_ref(), b"proc"),
            flags,
            b"proc",
        );
        // /dev: falls back to the store directory when the tmpfs cannot be created
        let dev = subdir(root.as_ref(), b"dev").map(|point| {
            let r = mounts.boot_tmpfs(
                &store,
                DEV_MOUNT,
                point.clone(),
                ms::NOSUID | ms::RELATIME,
                b"tmpfs",
                TmpfsParams::boot(0o755),
            );
            match r.and_then(|ino| dev::populate(&store, ino).map(|()| ino)) {
                Ok(ino) => Node::Store(DEV_MOUNT, ino),
                Err(e) => {
                    crate::debug::log(format_args!("cannot mount /dev: {e}"));
                    mounts.list.retain(|m| m.id != DEV_MOUNT);
                    point
                }
            }
        });
        mounts.boot(
            PTS_MOUNT,
            pseudo(PTS_MOUNT, Fs::Pts),
            subdir(dev.as_ref(), b"pts"),
            flags & !ms::NODEV,
            b"devpts",
        );
        if let Some(point) = subdir(dev.as_ref(), b"shm") {
            let params = TmpfsParams::boot(0o1777);
            let flags = ms::NOSUID | ms::NODEV | ms::RELATIME;
            if let Err(e) = mounts.boot_tmpfs(&store, SHM_MOUNT, point, flags, b"shm", params) {
                crate::debug::log(format_args!("cannot mount /dev/shm: {e}"));
            }
        }
        mounts.boot(
            MQUEUE_MOUNT,
            pseudo(MQUEUE_MOUNT, Fs::Mqueue),
            subdir(dev.as_ref(), b"mqueue"),
            flags,
            b"mqueue",
        );
        mounts.boot(
            SYS_MOUNT,
            pseudo(SYS_MOUNT, Fs::Sys),
            subdir(root.as_ref(), b"sys"),
            flags,
            b"sysfs",
        );
        // cgroup2 is mounted on sysfs's fs/cgroup and is writable as on a Linux host: dockerd and
        // runc create the containers' cgroups in it
        if mounts.list.iter().any(|m| m.id == SYS_MOUNT) {
            mounts.boot(
                CGROUP_MOUNT,
                pseudo(CGROUP_MOUNT, Fs::Cgroup),
                Some(Node::Pseudo(SYS_MOUNT, crate::pseudo::CGROUP_POINT)),
                flags,
                b"cgroup",
            );
        }
        // /run: as systemd and OpenRC mount it, 20% of physical memory and 800k inodes
        if let Some(point) = subdir(root.as_ref(), b"run") {
            let params = TmpfsParams {
                blocks: appine_hal::memory_info().0 / 5 / TMPFS_PAGE,
                inodes: 800 << 10,
                ..TmpfsParams::boot(0o755)
            };
            let flags = ms::NOSUID | ms::NODEV | ms::RELATIME;
            if let Err(e) = mounts.boot_tmpfs(&store, RUN_MOUNT, point, flags, b"tmpfs", params) {
                crate::debug::log(format_args!("cannot mount /run: {e}"));
            }
        }
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
                anon: false,
            }),
            namespaces: Default::default(),
        })
    }

    /// Opens the regular file `ino` in the store as a file to execute
    /// ([`crate::loader::ExeCache`]).
    pub(crate) fn executable(&self, ino: Ino) -> Result<crate::loader::Executable, Errno> {
        self.executables
            .open(ino, || self.store.open_object(ino, false))
    }

    /// Drops the recorded program files (when host memory is tight).
    pub(crate) fn trim(&self) {
        self.executables.clear();
    }

    /// The underlying inode store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// What no-BKL path resolution needs. Must be fetched again after the mount table changes
    /// ([`Shared::fast_changed`]).
    ///
    /// [`Shared::fast_changed`]: crate::kernel::Shared::fast_changed
    pub(crate) fn cached(&self) -> Cached {
        self.cached.borrow().clone()
    }

    // ---- Path resolution ----

    /// Resolves a path with credentials `cred` and root directory `root`. With `follow` false a
    /// symlink in the last component is not followed (lstat, readlink and so on).
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

    /// [`resolve`](Self::resolve), also returning the directory and name where the result was
    /// last found; there are none when the result came through `.`, `..`, the root directory or
    /// a link in a synthetic filesystem.
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

    /// [`walk`](Self::walk) with openat2's restrictions ([`resolve`]). With a scope (`BENEATH`,
    /// `IN_ROOT`) `root` is the start.
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
        // A leading / jumps to the root (path_init's nd_jump_root): not allowed under BENEATH;
        // under IN_ROOT the root is the start; NO_XDEV only governs jumps during resolution
        let mut dir = if path[0] == b'/' {
            if flags & resolve::BENEATH != 0 {
                return Err(Errno::EXDEV);
            }
            root.clone()
        } else {
            cwd.clone()
        };
        // Whether Linux's nd->root has been taken: absolute paths and scoped resolution take it
        // at the start, otherwise it is taken at the first `..` or the first jump to the root.
        // Under NO_XDEV a jump to the root (a symlink to an absolute path) before it is taken is
        // always EXDEV: nd_jump_root compares the current mount with the empty nd->root.mnt
        let mut root_set = path[0] == b'/' || flags & resolve::SCOPED != 0;
        let mut found = None;
        let mut pending = components(path);
        let mut links = 0;
        while let Some(comp) = pending.pop_front() {
            let last = pending.is_empty();
            if comp.len() > NAME_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            // The node before every component (including . and ..) must be a directory: file/.
            // and file/ are ENOTDIR
            if !self.is_dir(ps, &dir)? {
                return Err(Errno::ENOTDIR);
            }
            self.may_search(cred, &dir)?;
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                // At the start (follow_dotdot's in_root): BENEATH cannot get out; going past a
                // mount's root, or back to a directory with something mounted on it (entering
                // that mount), crosses mounts
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
            // Something is mounted on the name: enter that mount (traverse_mounts)
            if flags & resolve::NO_XDEV != 0 && node.mnt() != dir.mnt() {
                return Err(Errno::EXDEV);
            }
            if self.kind(ps, &node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                // fs.protected_symlinks applies only to the last component (pick_link's
                // WALK_TRAILING), before nosymfollow
                if last
                    && ps.protected_symlinks()
                    && let (Node::Store(_, link), Node::Store(_, parent)) = (&node, &dir)
                {
                    self.may_follow_link(cred, *parent, *link)?;
                }
                // Symlinks (magic links included) in a nosymfollow mount, and any symlink under
                // RESOLVE_NO_SYMLINKS, are not followed (Linux's pick_link)
                if flags & resolve::NO_SYMLINKS != 0
                    || self
                        .mount_flags(&node)
                        .is_some_and(|f| f & ms::NOSYMFOLLOW != 0)
                {
                    return Err(Errno::ELOOP);
                }
                // Magic links (nd_jump_link): first check whether that process may be viewed,
                // then the restrictions
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
                // Symlinks to absolute paths (nd_jump_root): not allowed under BENEATH; under
                // NO_XDEV the root must already have been taken and be in the same mount as the
                // link's directory
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

    /// Whether a process with credentials `cred` can look up in directory `dir`. Directories of
    /// synthetic filesystems always can.
    fn may_search(&self, cred: &Cred, dir: &Node) -> Result<(), Errno> {
        match dir {
            Node::Store(_, ino) if !cred.searches_all() => {
                cred.permission(&self.store.stat(*ino)?, MAY_EXEC)
            }
            _ => Ok(()),
        }
    }

    /// `..` in a path (Linux's `follow_dotdot`): stays put at the root directory; at a mount's
    /// root it first goes back to the mount point (one level at a time through stacked mounts),
    /// then takes the parent, and enters the mount on the parent when something is mounted there.
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

    /// Whether, with `fs.protected_symlinks` on, symlink `link` in directory `dir` may be
    /// followed (Linux's `may_follow_link`): someone else's link in a sticky, world-writable
    /// directory (/tmp) is followed only when the link's owner is the directory's owner, for root
    /// too; otherwise EACCES.
    fn may_follow_link(&self, cred: &Cred, dir: Ino, link: Ino) -> Result<(), Errno> {
        let link = self.store.stat(link)?;
        if link.uid == cred.fsuid {
            return Ok(());
        }
        let dir = self.store.stat(dir)?;
        let sticky_shared = mode::S_ISVTX | 0o002;
        if dir.mode & sticky_shared != sticky_shared || dir.uid == link.uid {
            return Ok(());
        }
        Err(Errno::EACCES)
    }

    /// Resolves the parent directory of a path, returning the parent and the last component
    /// (with trailing `/` removed).
    ///
    /// The last component may be `.` or `..`, for the caller to handle; for the path `/` it
    /// returns `(ROOT, ".")`.
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

    /// [`resolve_parent`](Self::resolve_parent) with openat2's restrictions ([`resolve`]).
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

    /// The absolute path of the file a path points at, with all symlinks resolved
    /// ([`found_path`](Self::found_path)).
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

    /// The absolute path of the `node` [`walk`](Self::walk) found, computed from the directory
    /// and name `found` where it was last found, with all symlinks resolved (`/proc/<pid>/exe`,
    /// as with Linux's `d_path`). A file reached through `/proc/<pid>/fd/<n>`, whose name is
    /// unknown, reports ENOENT.
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

    /// The absolute path of a directory relative to root `root` (Linux's `d_path`:
    /// `/proc/<pid>/cwd`, the path recorded for open files). A directory not under `root` gets
    /// no mark, as on Linux, and the path counts from the store's root; in a detached mount it
    /// counts from that mount's root. Use [`reach`](Self::reach) to know whether it is
    /// reachable.
    pub(crate) fn path_of(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        dir: &Node,
    ) -> Result<Vec<u8>, Errno> {
        self.reach(ps, root, dir).map(|(path, _)| path)
    }

    /// [`path_of`](Self::path_of), also returning whether it is reachable from `root`: when it
    /// is not, getcwd starts with `(unreachable)` and mountinfo does not list it.
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
                    // Mounted on a file: through the directory and name recorded at mount
                    // time
                    Some((dir, name)) => {
                        names.push(name);
                        cur = dir;
                    }
                    None => cur = point,
                }
                continue;
            }
            // The root of the mount tree, or the root of a detached mount
            if self.is_mount_root(&cur) {
                reached = false;
                break;
            }
            match cur {
                // Renamed and no longer in the part the mount sees
                Node::Store(_, ROOT) => {
                    reached = false;
                    break;
                }
                Node::Store(id, ino) => {
                    let parent = self.store.parent(ino)?;
                    // The root of a tmpfs: its parent is itself
                    if parent == ino {
                        reached = false;
                        break;
                    }
                    // A deleted directory has no name to find; ENOENT as on Linux
                    names.push(self.store.name_of(parent, ino)?.ok_or(Errno::ENOENT)?);
                    cur = Node::Store(id, parent);
                }
                Node::Pseudo(id, p) => match ps.parent(&p) {
                    Some(up) => {
                        names.push(ps.name(id, &p));
                        cur = Node::Pseudo(id, up);
                    }
                    // The root of a detached mount (MNT_DETACH)
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

    /// Whether it is a directory.
    pub(crate) fn is_dir(&self, ps: &dyn Pseudo, node: &Node) -> Result<bool, Errno> {
        Ok(self.kind(ps, node)? == mode::S_IFDIR)
    }

    /// The node's file type bits.
    pub(crate) fn kind(&self, ps: &dyn Pseudo, node: &Node) -> Result<u32, Errno> {
        match node {
            Node::Store(_, ino) => self.store.kind(*ino),
            Node::Pseudo(_, p) => ps.kind(p),
        }
    }

    /// Directory `dir` already has `name` (`.` and `..` always exist): before creating in it,
    /// ahead of the check whether the directory can be modified (Linux's `filename_create`).
    pub(crate) fn has_child(&self, ps: &dyn Pseudo, dir: &Node, name: &[u8]) -> bool {
        name == b"." || name == b".." || self.lookup(ps, dir, name).is_ok()
    }

    /// Looks up a name (not `.` or `..`) in a directory.
    fn lookup(&self, ps: &dyn Pseudo, dir: &Node, name: &[u8]) -> Result<Node, Errno> {
        let child = match dir {
            Node::Store(id, ino) => {
                Node::Store(*id, self.store.lookup(*ino, name)?.ok_or(Errno::ENOENT)?)
            }
            Node::Pseudo(id, p) => Node::Pseudo(*id, ps.lookup(*id, p, name)?),
        };
        Ok(self.mounted(child))
    }

    /// When something is mounted on the node, replaces it with the root of the mount on it; with
    /// several mounts stacked, keeps going up to the topmost one (Linux's `follow_mount`).
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

    /// Contents of a symlink.
    pub(crate) fn readlink(&self, ps: &dyn Pseudo, node: &Node) -> Result<Vec<u8>, Errno> {
        match node {
            Node::Store(_, ino) => self.store.readlink(*ino),
            Node::Pseudo(id, p) => ps.readlink(*id, p),
        }
    }

    /// The directory's parent; the parent of a mount's root is the mount point's parent
    /// (getdents's `..`).
    pub(crate) fn parent(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        let mut dir = dir.clone();
        while let Some(point) = self.point_of(&dir) {
            dir = point;
        }
        self.parent_in_fs(ps, &dir)
    }

    /// `node` is `top` or below it: searched upwards through mount points (Linux's
    /// `is_path_reachable`). Mounts on files go through the directory recorded at mount time.
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

    /// When `node` is the root of a mount, that mount's mount point.
    fn point_of(&self, node: &Node) -> Option<Node> {
        self.root_point(node).map(|(point, _)| point)
    }

    /// The parent directory within the same mount. A mount's root has no parent and is its own:
    /// the root of the mount tree and the root of a detached mount (`MNT_DETACH`), and inside a
    /// bind mount nothing outside the bound directory is visible.
    fn parent_in_fs(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        if self.is_mount_root(dir) {
            return Ok(dir.clone());
        }
        Ok(match dir {
            Node::Store(id, ino) => Node::Store(*id, self.store.parent(*ino)?),
            Node::Pseudo(id, p) => ps.parent(p).map_or(dir.clone(), |up| Node::Pseudo(*id, up)),
        })
    }

    /// When `node` is the root of an attached mount with a mount point, the mount point and
    /// [`Mount::at`].
    fn root_point(&self, node: &Node) -> Option<(Node, Option<Dentry>)> {
        let mounts = self.mounts.borrow();
        let m = mounts.list.iter().find(|m| m.id == node.mnt())?;
        if m.root != *node {
            return None;
        }
        Some((m.point.clone()?, m.at.clone()))
    }

    /// `node` is the root of the mount it is in (attached and detached ones both count).
    fn is_mount_root(&self, node: &Node) -> bool {
        self.with_mount(node.mnt(), |m| m.root == *node)
            .unwrap_or(false)
    }

    // ---- Mount table (mount(2), umount2(2), pivot_root(2), mountinfo) ----

    /// Attached mounts, in mount order. The first is the boot root filesystem ([`ROOT_MOUNT`]),
    /// which after pivot_root may be mounted elsewhere or detached.
    pub(crate) fn mounts(&self) -> Vec<Mount> {
        self.mounts.borrow().list.clone()
    }

    /// Attached mounts in mount namespace `ns`, in mount order (mountinfo).
    pub(crate) fn mounts_in(&self, ns: u64) -> Vec<Mount> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().filter(|m| m.ns == ns).cloned().collect()
    }

    /// The boot mount namespace.
    pub(crate) fn init_ns(&self) -> std::rc::Rc<MntNs> {
        self.init_ns.clone()
    }

    /// The namespace kept by the namespace file with id `id` bind-mounted elsewhere
    /// ([`Mount::pin`]; those in detached mounts still in use count too).
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

    /// The mount namespace with id `id` that is still in use (a process is in it, or a
    /// namespace file keeps it).
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

    /// Copies all attached mounts of mount namespace `from` into a new namespace owned by user
    /// namespace `owner` (Linux's `copy_mnt_ns`). Returns the new namespace and a map from old
    /// mount ids to new ones, which the caller uses to move processes' root and current
    /// directories onto the new mounts.
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
            anon: false,
        });
        let mut mounts = self.mounts.borrow_mut();
        let mut old: Vec<Mount> = mounts
            .list
            .iter()
            .filter(|m| m.ns == from)
            .cloned()
            .collect();
        // A mount namespace file bound elsewhere and the mounts on it are not copied (Linux's
        // copy_tree without CL_COPY_MNT_NS_FILE): the new namespace is newer than it, and
        // keeping it could form a cycle
        let mut skip = std::collections::HashSet::new();
        while let Some(m) = old.iter().find(|m| {
            !skip.contains(&m.id)
                && (matches!(m.pin, Some(crate::nsproxy::NsRef::Mnt(_)))
                    || m.parent().is_some_and(|p| skip.contains(&p)))
        }) {
            skip.insert(m.id);
        }
        old.retain(|m| !skip.contains(&m.id));
        // Allocate the ids before copying: after MS_MOVE or pivot_root, the mount a mount point
        // is in may have been mounted later than it
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

    /// A new anonymous mount namespace (Linux's `alloc_mnt_ns(.., true)`): holds a tree copied
    /// by open_tree and is held by an open file; once that is closed and nothing uses it, the
    /// next cleanup ([`Vfs::collect`]) tears down its mounts.
    pub(crate) fn anon_ns(&self, owner: std::rc::Rc<crate::cred::UserNs>) -> std::rc::Rc<MntNs> {
        let ns = std::rc::Rc::new(MntNs {
            id: new_ns_id(),
            owner,
            anon: true,
        });
        self.namespaces
            .borrow_mut()
            .push(std::rc::Rc::downgrade(&ns));
        ns
    }

    /// open_tree(`OPEN_TREE_CLONE`): binds a copy of `src` (with the filesystem, flags and
    /// device number of the mount it is in) in anonymous namespace `ns`, mounted nowhere, as
    /// that namespace's root. `found` is the directory and name where a bound file was found.
    /// Returns the new mount.
    pub(crate) fn clone_mount(
        &self,
        src: &Node,
        found: Option<Dentry>,
        ns: u64,
    ) -> Result<MountId, Errno> {
        let m = self.mount(src.mnt()).ok_or(Errno::EINVAL)?;
        let mut mounts = self.mounts.borrow_mut();
        let id = mounts.next;
        mounts.next += 1;
        mounts.list.push(Mount {
            id,
            ns,
            root: src.clone().in_mount(id),
            point: None,
            at: None,
            root_at: found,
            expiring: false,
            ..m
        });
        drop(mounts);
        self.mounts_changed();
        Ok(id)
    }

    /// move_mount attaching a tree copied by open_tree: the root is mounted on `point`, and all
    /// mounts in the tree move into namespace `to`.
    pub(crate) fn attach_tree(&self, root: MountId, (point, at): (Node, Option<Dentry>), to: u64) {
        let mut mounts = self.mounts.borrow_mut();
        let from = mounts.list.iter().find(|m| m.id == root).map(|m| m.ns);
        for m in mounts.list.iter_mut().filter(|m| Some(m.ns) == from) {
            m.ns = to;
        }
        if let Some(m) = mounts.list.iter_mut().find(|m| m.id == root) {
            m.at = at.filter(|_| matches!(point, Node::Store(..)));
            m.point = Some(point);
        }
        drop(mounts);
        self.mounts_changed();
    }

    /// Mount `id` and the mounts below it (level by level downwards, mount_setattr's
    /// `AT_RECURSIVE`).
    pub(crate) fn subtree(&self, id: MountId) -> Vec<MountId> {
        let mounts = self.mounts.borrow();
        let mut out = vec![id];
        let mut i = 0;
        while let Some(&parent) = out.get(i) {
            out.extend(
                mounts
                    .list
                    .iter()
                    .filter(|m| m.parent() == Some(parent) && m.id != parent)
                    .map(|m| m.id),
            );
            i += 1;
        }
        out
    }

    /// The root directory of mount namespace `ns` (the root of the mount tree's root, or the
    /// topmost mount on it when something is mounted there).
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

    /// The namespace mount `id` belongs to; None for detached ones (Linux's `check_mnt`).
    pub(crate) fn mount_ns(&self, id: MountId) -> Option<u64> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().find(|m| m.id == id).map(|m| m.ns)
    }

    /// The attached mount with id `id`; None for detached ones.
    pub(crate) fn mount(&self, id: MountId) -> Option<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// When `node` is the root of an attached mount, that mount's id.
    pub(crate) fn mount_root(&self, node: &Node) -> Option<MountId> {
        let mounts = self.mounts.borrow();
        mounts.list.iter().find(|m| m.root == *node).map(|m| m.id)
    }

    /// Some mount is mounted on `node`.
    #[cfg(test)]
    fn is_mount_point(&self, node: &Node) -> bool {
        self.mounts
            .borrow()
            .list
            .iter()
            .any(|m| m.point.as_ref() == Some(node))
    }

    /// A mount of namespace `ns` is on `name` (inode `ino`) in store directory `dir`: rmdir,
    /// rename or unlink of it is EBUSY (Linux's `is_local_mountpoint`, which looks only at the
    /// caller's namespace). Mounts on files tell hard links apart by the directory and name
    /// where the file was found ([`Mount::at`]).
    pub(crate) fn is_store_point(&self, (ino, dir, name): (Ino, Ino, &[u8]), ns: u64) -> bool {
        self.mounts
            .borrow()
            .list
            .iter()
            .any(|m| m.ns == ns && on_entry(m, (ino, dir, name)))
    }

    /// `name` (inode `ino`) in store directory `dir` was deleted (rmdir, unlink, overwritten by
    /// rename): all mounts on it in other namespaces are detached (Linux's `detach_mounts`).
    pub(crate) fn detach_store_point(&self, (ino, dir, name): (Ino, Ino, &[u8])) {
        let ids: Vec<MountId> = self
            .mounts
            .borrow()
            .list
            .iter()
            .filter(|m| on_entry(m, (ino, dir, name)))
            .map(|m| m.id)
            .collect();
        for id in ids {
            self.remove_mount(id);
        }
    }

    /// The device number of `node` (stat's `st_dev`): that of the mount it is in; when the mount
    /// is unknown, that of the boot instance.
    pub(crate) fn dev_of(&self, node: &Node) -> u64 {
        self.with_mount(node.mnt(), |m| m.dev)
            .unwrap_or_else(|| match node {
                Node::Store(..) => ROOT_DEV,
                Node::Pseudo(_, p) => crate::pseudo::fs_of(p).dev(),
            })
    }

    /// When `node` is in tmpfs, that instance's device number (used by [`Vfs::tmpfs_usage`] and
    /// others).
    pub(crate) fn tmpfs_dev(&self, node: &Node) -> Option<u64> {
        (self.fs_of(node) == Fs::Tmpfs).then(|| self.dev_of(node))
    }

    /// The kind of filesystem `node` is in.
    pub(crate) fn fs_of(&self, node: &Node) -> Fs {
        self.with_mount(node.mnt(), |m| m.fs)
            .unwrap_or_else(|| match node {
                Node::Store(..) => Fs::Store,
                Node::Pseudo(_, p) => crate::pseudo::fs_of(p),
            })
    }

    /// Whether `pages` more pages and `inodes` more inodes fit in tmpfs instance `dev` (Linux's
    /// `shmem_reserve_inode`, `shmem_inode_acct_block`): ENOSPC when they do not. When they fit,
    /// the caller accounts after it is done ([`Vfs::tmpfs_charge`]).
    pub(crate) fn tmpfs_check(&self, dev: u64, pages: u64, inodes: u64) -> Result<(), Errno> {
        let (p, used_pages, used_inodes) = self.tmpfs_usage(dev, false)?;
        let over = |max: u64, used: u64, more: u64| max != 0 && more > 0 && used + more > max;
        if over(p.blocks, used_pages, pages) || over(p.inodes, used_inodes, inodes) {
            return Err(Errno::ENOSPC);
        }
        Ok(())
    }

    /// How many pages a file in tmpfs instance `dev` can still use: the `own` pages it already
    /// has plus those not used up. None when unlimited.
    pub(crate) fn tmpfs_room(&self, dev: u64, own: u64) -> Result<Option<u64>, Errno> {
        let (p, used, _) = self.tmpfs_usage(dev, false)?;
        Ok((p.blocks != 0).then(|| (p.blocks + own).saturating_sub(used)))
    }

    /// Records that the usage of tmpfs instance `dev` changed (after create, delete, write,
    /// truncate). Does nothing when it has not been counted yet; it is counted when needed.
    pub(crate) fn tmpfs_charge(&self, dev: u64, pages: i64, inodes: i64) {
        if let Some(t) = self.mounts.borrow_mut().tmpfs.get_mut(&dev)
            && let Some((p, i)) = &mut t.used
        {
            *p = p.saturating_add_signed(pages);
            *i = i.saturating_add_signed(inodes);
        }
    }

    /// Changes the parameters of tmpfs instance `dev` on remount: `f` gets the parameters and
    /// the current usage (pages, inodes); on error nothing changes.
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

    /// Parameters and usage (pages, inodes) of tmpfs instance `dev`. Usage is counted as in
    /// Linux's tmpfs: the root directory and every name take one inode each (hard links too;
    /// Linux's `shmem_link` takes another for each link), regular files by page count (treated
    /// as having no holes, like Appine's `st_blocks`), long symlinks one page. With `fresh` it
    /// recounts (statfs, remount: changes that bypass the accounting, such as direct changes to
    /// the store or writes through shared mappings, are included too); otherwise it uses the
    /// recorded usage and counts only when none is recorded.
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

    /// Counts the usage of the tmpfs rooted at `root` (pages, inodes; see
    /// [`Vfs::tmpfs_usage`]). Files still open after deletion do not count (Linux frees them
    /// only at close).
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

    /// The mount's superblock options (the part after `rw` in the last column of mountinfo).
    pub(crate) fn sb_options(&self, m: &Mount) -> String {
        match m.fs {
            Fs::Pts => self.devpts_options(m.dev).show(),
            Fs::Tmpfs => self
                .mounts
                .borrow()
                .tmpfs
                .get(&m.dev)
                .map_or_else(String::new, |t| t.params.options()),
            fs => fs.options().to_owned(),
        }
    }

    /// Options of devpts instance `dev` (the boot instance, and one with no mount left, get the
    /// boot options).
    pub(crate) fn devpts_options(&self, dev: u64) -> crate::tty::PtsOptions {
        self.mounts
            .borrow()
            .devpts
            .get(&dev)
            .copied()
            .unwrap_or(crate::tty::PtsOptions::BOOT)
    }

    /// Records the options of newly mounted devpts instance `dev`.
    pub(crate) fn set_devpts_options(&self, dev: u64, opts: crate::tty::PtsOptions) {
        self.mounts.borrow_mut().devpts.insert(dev, opts);
    }

    /// Flags of the mount `node` is in; None when the mount is unknown.
    pub(crate) fn mount_flags(&self, node: &Node) -> Option<u64> {
        self.with_mount(node.mnt(), |m| m.flags)
    }

    /// `node` is in a read-only mount: changing it is EROFS.
    pub(crate) fn readonly(&self, node: &Node) -> bool {
        self.mount_flags(node).is_some_and(|f| f & ms::RDONLY != 0)
    }

    /// The path in the filesystem of the part the mount sees (the fourth column of mountinfo:
    /// which directory or file a bind mount bound).
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

    /// The directory's path in its filesystem. Namespace files look like `net:[number]` (Linux
    /// nsfs's `d_dname`), with no leading `/`.
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

    /// Mounts a new tmpfs instance (whose root directory is created here), returning the new
    /// mount's id.
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
                // Only the root directory
                used: Some((0, 1)),
            },
        );
        Ok(id)
    }

    /// Mounts `root` in filesystem `fs` ([`Fs::root`], or a node reached through another mount)
    /// on `point`, returning the new mount's id. With `dev` None a device number is allocated
    /// ([`Fs::own_dev`]); bind mounts use the source's. When `point` is a file in the store,
    /// `at` is the directory and name where it was found ([`Mount::at`]).
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
        // The new mount belongs to the namespace of the mount its mount point is in
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

    /// When mount `id` is a procfs instance, the pid namespace of the process that mounted it
    /// (None for the boot one).
    pub(crate) fn proc_pid_ns(&self, id: MountId) -> Option<std::rc::Rc<crate::pidns::PidNs>> {
        let mounts = self.mounts.borrow();
        let m = mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .find(|m| m.id == id)?;
        mounts.proc_ns.get(&m.dev).cloned()
    }

    /// Device number of the mqueue instance of IPC namespace `ns` (reported by nodes in the
    /// mqueue filesystem and by fds from mq_open): [`MQUEUE_DEV`] for the boot namespace,
    /// others allocated on first use.
    pub(crate) fn mq_dev(&self, ns: u64) -> u64 {
        if ns == crate::nsproxy::INIT_IPC_NS {
            return MQUEUE_DEV;
        }
        let mut mounts = self.mounts.borrow_mut();
        if let Some(&dev) = mounts.mq_dev.get(&ns) {
            return dev;
        }
        mounts.next_dev += 1;
        let dev = mounts.next_dev - 1;
        mounts.mq_dev.insert(ns, dev);
        dev
    }

    /// New procfs instance `id` belongs to pid namespace `ns`.
    pub(crate) fn set_proc_pid_ns(&self, id: MountId, ns: std::rc::Rc<crate::pidns::PidNs>) {
        let mut mounts = self.mounts.borrow_mut();
        if let Some(dev) = mounts.list.iter().find(|m| m.id == id).map(|m| m.dev) {
            mounts.proc_ns.insert(dev, ns);
        }
    }

    /// Mounts mounted directly in mount `id`, in mount order.
    pub(crate) fn children(&self, id: MountId) -> Vec<Mount> {
        self.mounts
            .borrow()
            .list
            .iter()
            .filter(|m| m.parent() == Some(id))
            .cloned()
            .collect()
    }

    /// Detaches mount `id` and all mounts inside it (Linux's `umount_tree`): none of them is
    /// mounted on another mount any more, and each one's root has no parent.
    pub(crate) fn remove_mount(&self, id: MountId) {
        let mut mounts = self.mounts.borrow_mut();
        let mut gone = vec![id];
        let mut i = 0;
        // After MS_MOVE or pivot_root a mount is not necessarily inside a mount mounted before
        // it; search level by level
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

    /// Drops detached mounts not in `in_use` (no process's root or current directory and no
    /// open file is in them). tmpfs instances no mount can see any more are deleted with their
    /// files.
    pub(crate) fn collect(&self, in_use: &std::collections::HashSet<MountId>) {
        // Mount namespaces nobody uses: all their mounts are detached
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
            let pts: std::collections::HashSet<u64> = mounts
                .list
                .iter()
                .chain(&mounts.detached)
                .filter(|m| m.fs == Fs::Pts)
                .map(|m| m.dev)
                .collect();
            mounts.devpts.retain(|dev, _| pts.contains(dev));
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
        // The dropped mounts included a mount namespace file bound elsewhere: that namespace may
        // now be unused too
        if self.has_dead_ns() {
            self.collect(in_use);
        }
    }

    /// Some mount namespace has no references left while its mounts are not yet detached:
    /// cleanup must handle it.
    pub(crate) fn has_dead_ns(&self) -> bool {
        self.namespaces
            .borrow()
            .iter()
            .any(|w| w.strong_count() == 0)
    }

    /// Moves attached mount `id` onto `point` (`MS_MOVE`, pivot_root); None makes it the root
    /// of the mount tree.
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

    /// Records in which directory and under which name a bind-mounted file was found
    /// ([`Mount::root_at`]).
    pub(crate) fn set_root_at(&self, id: MountId, at: Option<Dentry>) {
        self.update_mount(id, |m| m.root_at = at);
    }

    /// Changes the flags and the MNT_EXPIRE mark of mount `id`.
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

    /// Runs `f` on the mount with id `id` (attached or detached).
    fn with_mount<T>(&self, id: MountId, f: impl FnOnce(&Mount) -> T) -> Option<T> {
        let mounts = self.mounts.borrow();
        mounts
            .list
            .iter()
            .chain(&mounts.detached)
            .find(|m| m.id == id)
            .map(f)
    }

    /// The mount table changed: the mount points used by no-BKL path resolution are replaced to
    /// match (the caller separately makes each thread fetch them again,
    /// [`Shared::fast_changed`](crate::kernel::Shared::fast_changed)).
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

/// When there is no `/etc/resolv.conf`, writes one pointing at the DNS stub (the kernel/net
/// map); an existing one is left alone.
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

/// Writes a regular file in the store (for the host to prepare configuration files): missing
/// parent directories are created too (0755), and symlinks in the path are not followed. When
/// the file already exists, it is left alone if `overwrite` is false, otherwise its contents
/// are replaced (permission bits unchanged).
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

/// Reads a regular file in the store (for the host to look at configuration files, such as the
/// accounts in `/etc/passwd` listed in settings): symlinks in the path are not followed (as in
/// [`write_file`]); a missing file is `ENOENT`, and one that is not a regular file `EINVAL`.
pub fn read_file(store: &Store, path: &[u8]) -> Result<Vec<u8>, Errno> {
    use std::io::Read;
    let mut ino = ROOT;
    for c in split(path) {
        if store.kind(ino)? != mode::S_IFDIR {
            return Err(Errno::ENOTDIR);
        }
        ino = store.lookup(ino, &c)?.ok_or(Errno::ENOENT)?;
    }
    if store.kind(ino)? != mode::S_IFREG {
        return Err(Errno::EINVAL);
    }
    let mut data = Vec::new();
    store
        .open_object(ino, false)?
        .read_to_end(&mut data)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok(data)
}

/// Components for path resolution: `.` is kept, and with a trailing `/` one more `.` is added,
/// so the preceding node must be a directory and a symlink in the last component is followed
/// too (as on Linux).
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

/// Splits path components, dropping empty components and `.`.
fn split(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

/// Mount `m` is mounted on `name` (inode `ino`) in store directory `dir`.
fn on_entry(m: &Mount, (ino, dir, name): (Ino, Ino, &[u8])) -> bool {
    matches!(m.point, Some(Node::Store(_, i)) if i == ino)
        && m.at
            .as_ref()
            .is_none_or(|(d, n)| d.ino() == Some(dir) && n == name)
}

/// Pages one inode takes in tmpfs: regular files by size, symlinks one page when the target is
/// 128 bytes or more (Linux's `SHORT_SYMLINK_LEN`; short ones live in the inode), others none.
pub(crate) fn tmpfs_pages(st: &Stat) -> u64 {
    match st.kind() {
        mode::S_IFREG => st.size.div_ceil(TMPFS_PAGE),
        mode::S_IFLNK => u64::from(st.size >= 128),
        _ => 0,
    }
}

/// Normalizes an absolute path lexically: drops `.` and empty components, and steps back for
/// `..` (without following symlinks).
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

    /// The synthetic filesystems all have only an empty root directory; procfs also has the two
    /// directory levels `sys/kernel/`.
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
        // In sample: /usr/bin/env -> /bin/busybox, /usr/bin/rel -> ../../bin/busybox,
        // /bindir -> bin; procfs is mounted on /proc
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
        // With a scope the root is the start
        let scoped = |start: &Node, p: &[u8], flags| {
            vfs.walk_resolve(ps, cred, start, start, p, true, flags)
                .map(|(n, _)| n)
        };
        let plain = |start: &Node, p: &[u8], flags| {
            vfs.walk_resolve(ps, cred, &root, start, p, true, flags)
                .map(|(n, _)| n)
        };
        use resolve::*;
        // BENEATH: absolute paths, .. leaving the start, symlinks to absolute paths and relative
        // links leaving the start are all EXDEV
        assert_eq!(scoped(&usr, b"bin", BENEATH), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"bin/..", BENEATH), Ok(usr.clone()));
        assert_eq!(scoped(&usr, b"..", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"/bin", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"bin/env", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&usr, b"bin/rel", BENEATH), Err(Errno::EXDEV));
        assert_eq!(scoped(&root, b"usr/bin/rel", BENEATH), Ok(busybox.clone()));
        // IN_ROOT: the start is treated as the root, .. stops at the start, and absolute paths
        // count from the start
        assert_eq!(scoped(&usr, b"..", IN_ROOT), Ok(usr.clone()));
        assert_eq!(scoped(&usr, b"/bin", IN_ROOT), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"../../bin", IN_ROOT), Ok(ubin.clone()));
        assert_eq!(scoped(&usr, b"bin/env", IN_ROOT), Err(Errno::ENOENT));
        // NO_SYMLINKS: symlinks along the way and at the end are ELOOP; not following the last
        // one works as usual
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
        // NO_XDEV: entering or leaving mounts is EXDEV; so is jumping to the root before the
        // root has been taken
        assert_eq!(plain(&root, b"/proc", NO_XDEV), Err(Errno::EXDEV));
        assert_eq!(plain(&root, b"/usr/bin/env", NO_XDEV), Ok(busybox.clone()));
        assert_eq!(plain(&ubin, b"env", NO_XDEV), Err(Errno::EXDEV));
        assert_eq!(plain(&ubin, b"../bin/env", NO_XDEV), Ok(busybox.clone()));
        let proc = plain(&root, b"/proc", 0).unwrap();
        assert_eq!(plain(&proc, b"..", NO_XDEV), Err(Errno::EXDEV));
        // The parent directory for creation is restricted too
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
        // There must also be a directory before . and ..; a symlink target ending in / likewise
        // requires a directory
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

        // The path after resolving symlinks (/proc/<pid>/exe)
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

        // /proc is a mount point: going in reaches the synthetic filesystem's root, and `..`
        // returns to the store's root directory
        let proc = r(&root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(PROC_MOUNT, PseudoNode::ProcRoot));
        assert_eq!(r(&proc, b"..", true).unwrap(), root);
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &proc).unwrap(), b"/proc");
        // Device nodes are in the tmpfs mounted on /dev at boot, and /dev/pts is mounted inside
        // it
        assert!(matches!(
            r(&root, b"/dev/null", true).unwrap(),
            Node::Store(DEV_MOUNT, _)
        ));
        let dev = r(&root, b"/dev", true).unwrap();
        assert!(matches!(dev, Node::Store(DEV_MOUNT, _)));
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &dev).unwrap(), b"/dev");
        assert_eq!(r(&dev, b"..", true).unwrap(), root);
        let pts = r(&root, b"/dev/pts", true).unwrap();
        assert_eq!(
            pts,
            Node::Pseudo(PTS_MOUNT, PseudoNode::PtsRoot(crate::tty::BOOT_PTS))
        );
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &pts).unwrap(), b"/dev/pts");
        assert!(matches!(
            r(&root, b"/run", true).unwrap(),
            Node::Store(RUN_MOUNT, _)
        ));
        assert!(matches!(
            r(&root, b"/dev/shm/..", true).unwrap(),
            Node::Store(DEV_MOUNT, _)
        ));
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
                    // Normal resolution filled everything needed into the cache; afterwards the
                    // cache alone gives the same result
                    assert_eq!(
                        cached.stat(ROOT, cwd, p, follow),
                        Some(full),
                        "{} from {cwd}, follow {follow}",
                        String::from_utf8_lossy(p)
                    );
                }
            }
        }
        // Synthetic filesystems are left to normal resolution
        assert_eq!(cached.stat(ROOT, ROOT, b"/proc", true), None);
        assert_eq!(cached.stat(ROOT, ROOT, b"/dev/pts/0", true), None);
        // Names never resolved are also left to normal resolution
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/sh", true), None);
        // After a change the old entry is not used
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

        // Mount a procfs on /usr/bin: going in reaches its root, paths go through the mount
        // point, and `..` returns to /usr
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
        // The same directory in the boot /proc is another node, and `..` returns to /proc
        let proc_sys = r(&root, &root, b"/proc/sys").unwrap();
        assert_eq!(proc_sys, Node::Pseudo(PROC_MOUNT, SYS));
        assert_eq!(r(&root, &proc_sys, b"..").unwrap(), proc);
        // The root changed to /usr: paths are relative to it, `..` does not go past it, and
        // anything outside the root is unreachable
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

        // Bind-mount sys/ in procfs: what is seen is that directory, and `..` returns to the
        // mount point's parent
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
        // The new instance has its own device number; bind mounts use the source's
        assert_eq!(vfs.dev_of(&proc_sys), Fs::Proc.dev());
        assert_ne!(vfs.dev_of(&kernel), Fs::Proc.dev());
        assert_eq!(vfs.dev_of(&bound), Fs::Proc.dev());

        // Stacked mounts: the topmost one is visible, and `..` goes back level by level to the
        // mount point
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
            Node::Pseudo(s, PseudoNode::PtsRoot(crate::tty::BOOT_PTS))
        );
        assert_eq!(r(&root, &root, b"/usr/bin/..").unwrap(), usr);
        let ids = |list: Vec<Mount>| list.iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(vfs.children(id)), [s]);
        assert_eq!(
            ids(vfs.children(ROOT_MOUNT)),
            [PROC_MOUNT, DEV_MOUNT, SYS_MOUNT, RUN_MOUNT, id, b]
        );
        assert_eq!(
            ids(vfs.children(DEV_MOUNT)),
            [PTS_MOUNT, SHM_MOUNT, MQUEUE_MOUNT]
        );
        assert_eq!(ids(vfs.children(SYS_MOUNT)), [CGROUP_MOUNT]);

        // No-BKL resolution hands mount points to normal resolution, also when the current
        // directory's parent is a mount point
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

        // Detach id and what is stacked on it: the store directory is visible again. Nodes
        // still inside stay in the detached mount, whose root has no parent, and their paths
        // are unreachable
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
        // Ids of detached mounts are not reused
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

        // /usr bound onto /bin: going in reaches /usr through the new mount, `..` returns to
        // the mount point's parent, and paths go through the mount point
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
        // Symlinks reached through the mount resolve along the mount's paths: /bin/bin/rel is
        // ../../bin/busybox, which after reaching the mount's root goes back to the store's
        // root, and entering /bin from there is this mount again
        assert_eq!(
            r(&root, &root, b"/bin/bin/rel"),
            Err(Errno::ENOENT),
            "/bin is now /usr, which has no busybox"
        );
        assert_eq!(vfs.mount_root(&top), Some(id));
        assert_eq!(vfs.mount_root(&inner), None);
        assert!(vfs.is_mount_point(&bin));
        assert!(!vfs.is_mount_point(&Node::Store(id, ino(&bin))));
        assert!(vfs.is_store_point((ino(&bin), ino(&usr), b"bin"), INIT_MNT_NS));
        assert_eq!(vfs.dev_of(&inner), ROOT_DEV);
        assert_eq!(vfs.root_path(ps, &vfs.mount(id).unwrap()), b"/usr");
        assert!(vfs.is_under(ps, &inner, &root));
        assert!(vfs.is_under(ps, &inner, &top));
        assert!(!vfs.is_under(ps, &inner, &usr));
        // The root changed to a directory inside the mount: `..` does not go past it, and
        // nothing outside can be reached
        assert_eq!(r(&inner, &inner, b"/..").unwrap(), inner);
        assert_eq!(
            vfs.reach(ps, &inner, &top).unwrap(),
            (b"/bin".to_vec(), false)
        );

        // Read-only: flags follow the mount reached through
        vfs.update_mount(id, |m| m.flags |= ms::RDONLY);
        assert!(vfs.readonly(&inner));
        assert!(!vfs.readonly(&ubin));

        // A file bound onto a file: the path follows the directory and name at mount time
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

        // Moved onto /proc: /bin is the original directory again
        let proc_top = r(&root, &root, b"/proc").unwrap();
        vfs.move_mount(id, Some((proc_top.clone(), None)));
        assert_eq!(r(&root, &root, b"/bin").unwrap(), bin);
        assert_eq!(r(&root, &root, b"/proc/bin").unwrap(), inner);
        assert_eq!(vfs.path_of(ps, &root, &inner).unwrap(), b"/proc/bin");

        // Detached: nodes inside keep working, and the mount's root has no parent and cannot
        // reach the root directory; the record is dropped once nobody uses it
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
        // Above the tmpfs root is the mount point; paths inside go through it
        assert_eq!(r(b"/usr/sub/..").unwrap(), top);
        assert_eq!(r(b"/usr/..").unwrap(), Node::ROOT);
        assert_eq!(
            vfs.path_of(ps, &Node::ROOT, &Node::Store(a, sub)).unwrap(),
            b"/usr/sub"
        );
        // A subdirectory bound elsewhere keeps it alive
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
        // All gone: deleted together with everything inside
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
        // The copied namespace: mounts correspond one to one with new ids, and mount points and
        // root directories follow
        let (ns, map) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let copied = vfs.mounts_in(ns.id);
        assert_eq!(copied.len(), before.len());
        assert!(copied.iter().all(|m| !map.contains_key(&m.id)));
        let root = vfs.ns_root(ns.id).unwrap();
        assert_eq!(root, Node::Store(map[&ROOT_MOUNT], ROOT));
        let proc = vfs.resolve(ps, cred, &root, &root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(map[&PROC_MOUNT], PseudoNode::ProcRoot));
        assert_eq!(vfs.mount_ns(proc.mnt()), Some(ns.id));
        // A tmpfs mounted in the new namespace is only there
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
        // No longer used: cleanup detaches its mounts, and the tmpfs is deleted with them
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
        // A file of new mount namespace A bound into the boot namespace
        let (a, _) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let a_id = a.id;
        let file = Node::Pseudo(
            PROC_MOUNT,
            PseudoNode::NsFile(crate::pseudo::NsKind::Mnt, a_id),
        );
        let fs = Fs::Nsfs;
        let id = vfs.add_mount(fs, file, point, 0, b"nsfs".to_vec(), Some(fs.dev()));
        vfs.update_mount(id, |m| m.pin = Some(crate::nsproxy::NsRef::Mnt(a.clone())));
        // Copying the boot namespace leaves it out (otherwise A would keep itself alive through
        // the copied mount)
        let (c, _) = vfs.copy_ns(INIT_MNT_NS, crate::cred::UserNs::init());
        let copied = vfs.mounts_in(c.id);
        assert_eq!(copied.len(), before);
        assert!(copied.iter().all(|m| m.pin.is_none()));
        // No processes left in A: the mount keeps it
        drop(a);
        assert!(!vfs.has_dead_ns());
        assert!(vfs.find_ns(a_id).is_some());
        assert_eq!(vfs.pinned_ns(a_id).map(|r| r.id()), Some(a_id));
        // Cleanup after detaching: A is gone with its mounts
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
        // Only the root directory
        assert_eq!(usage(true), (0, 1));
        // A directory, a two-page file and another link to it, and a long symlink: one inode per
        // name, and pages counted once per file
        let sub = vfs.store().mkdir(root, b"sub", 0o755, 0, 0).unwrap();
        let (f, file) = vfs.store().create_file(sub, b"f", 0o644, 0, 0).unwrap();
        file.set_len(5000).unwrap();
        vfs.store().link(root, b"g", f).unwrap();
        vfs.store().symlink(root, b"s", &[b'x'; 200], 0, 0).unwrap();
        assert_eq!(
            usage(false),
            (0, 1),
            "changing the store directly is not accounted"
        );
        assert_eq!(usage(true), (3, 5));
        assert_eq!(vfs.tmpfs_check(dev, 0, 1), Err(Errno::ENOSPC));
        assert_eq!(vfs.tmpfs_check(dev, 2, 0), Err(Errno::ENOSPC));
        assert_eq!(vfs.tmpfs_check(dev, 1, 0), Ok(()));
        // f had two pages, plus the one page not used up
        assert_eq!(vfs.tmpfs_room(dev, 2), Ok(Some(3)));
        vfs.tmpfs_charge(dev, 1, 0);
        assert_eq!(usage(false), (4, 5));
        assert_eq!(vfs.tmpfs_room(dev, 2), Ok(Some(2)));
        // Counted afresh: the recorded usage follows
        assert_eq!(usage(true), (3, 5));
        assert_eq!(usage(false), (3, 5));
        // Remount: cannot go below the usage
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
