//! 进程凭据（credentials(7)、capabilities(7)）：用户和组 ID、附加组、能力集，以及
//! 文件访问的权限检查（DAC）。
//!
//! Linux 的凭据属于线程，但 libc 的 setuid 系列会让每个线程都调用一遍，效果上是整个
//! 进程共用一份，所以这里把凭据放在线程组里。能力按 Linux 的规则随用户 ID 变化：
//! 有效用户 ID 变成非 0 时清空有效集，三个用户 ID 都变成非 0 时清空允许集，文件系统
//! 相关的能力跟着 fsuid 走；capset 可以去掉其中一部分，`PR_CAPBSET_DROP` 可以缩小边界
//! 集。execve 按 Linux 6.1 的 `cap_bprm_creds_from_file` 变换能力，算上程序文件的能力
//! （`security.capability`，[`FileCaps`]）和环境（ambient）能力；securebits 的几个位和
//! 它们的锁与 Linux 相同。v3 的文件能力只有 rootid 是 0 的起作用。
//!
//! 用户命名空间（[`UserNs`]）：凭据里的 ID 总是内核 ID（开机时的命名空间里的），对外按
//! 调用者的命名空间的 uid_map、gid_map 换算，映射不到的显示成 65534。能力只在自己的命名
//! 空间和它的后代里有效（[`Cred::ns_capable`]）：[`Cred::capable`] 是 Linux 的
//! `capable()`，只看开机时的命名空间；文件的 DAC 检查用 [`Cred::capable_wrt`]（Linux 的
//! `capable_wrt_inode_uidgid`），setuid 系列、capset 这些看自己的命名空间
//! （[`Cred::capable_here`]）。

use std::cell::Cell;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::vfs::store::{Stat, mode};

/// 能力编号（`linux/capability.h`）。
pub(crate) mod cap {
    pub const CHOWN: u32 = 0;
    pub const DAC_OVERRIDE: u32 = 1;
    pub const DAC_READ_SEARCH: u32 = 2;
    pub const FOWNER: u32 = 3;
    pub const FSETID: u32 = 4;
    pub const KILL: u32 = 5;
    pub const SETGID: u32 = 6;
    pub const SETUID: u32 = 7;
    pub const SETPCAP: u32 = 8;
    pub const LINUX_IMMUTABLE: u32 = 9;
    pub const NET_BIND_SERVICE: u32 = 10;
    pub const NET_ADMIN: u32 = 12;
    pub const NET_RAW: u32 = 13;
    pub const IPC_LOCK: u32 = 14;
    pub const IPC_OWNER: u32 = 15;
    pub const SYS_CHROOT: u32 = 18;
    pub const SYS_PTRACE: u32 = 19;
    pub const SYS_ADMIN: u32 = 21;
    pub const SYS_NICE: u32 = 23;
    pub const SYS_RESOURCE: u32 = 24;
    pub const SYS_TIME: u32 = 25;
    pub const SYS_TTY_CONFIG: u32 = 26;
    pub const MKNOD: u32 = 27;
    pub const LEASE: u32 = 28;
    pub const SETFCAP: u32 = 31;
    pub const MAC_OVERRIDE: u32 = 32;
    pub const SYSLOG: u32 = 34;
    pub const BPF: u32 = 39;
    pub const CHECKPOINT_RESTORE: u32 = 40;
    /// 最大的能力编号。
    pub const LAST: u32 = CHECKPOINT_RESTORE;
}

/// 全部能力（也是默认的边界集）。
pub(crate) const CAP_FULL: u64 = (1 << (cap::LAST + 1)) - 1;

/// securebits（`linux/securebits.h`）：每个位后面是锁住它的位。
pub(crate) mod secbit {
    /// execve 时不按 root 的规则给能力。
    pub const NOROOT: u32 = 1 << 0;
    /// 用户 ID 变化时不调整能力。
    pub const NO_SETUID_FIXUP: u32 = 1 << 2;
    /// 用户 ID 全部变成非 0 时保留允许集（`PR_SET_KEEPCAPS` 设置的就是它），execve 时清除。
    pub const KEEP_CAPS: u32 = 1 << 4;
    pub const KEEP_CAPS_LOCKED: u32 = 1 << 5;
    /// 不能提升环境能力。
    pub const NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
    pub const ALL_BITS: u32 = 0x55;
    pub const ALL_LOCKS: u32 = 0xaa;
}

/// 跟着 fsuid 走的文件系统能力（`CAP_FS_MASK`）。
const CAP_FS_MASK: u64 = 1 << cap::CHOWN
    | 1 << cap::DAC_OVERRIDE
    | 1 << cap::DAC_READ_SEARCH
    | 1 << cap::FOWNER
    | 1 << cap::FSETID
    | 1 << cap::LINUX_IMMUTABLE
    | 1 << cap::MKNOD
    | 1 << cap::MAC_OVERRIDE;

/// 权限检查要求的访问方式。
pub(crate) const MAY_EXEC: u32 = 1;
pub(crate) const MAY_WRITE: u32 = 2;
pub(crate) const MAY_READ: u32 = 4;

/// setgroups 最多接受的附加组个数（`NGROUPS_MAX`）。
pub(crate) const NGROUPS_MAX: usize = 65536;

/// 开机时的用户命名空间的编号（`/proc/<pid>/ns/user`）。
pub(crate) const INIT_USER_NS: u64 = 4_026_531_837;
/// 映射不到的 ID 对外显示成它（`/proc/sys/kernel/overflowuid`、`overflowgid`）。
pub(crate) const OVERFLOW_ID: u32 = 65534;
/// 层数超过它的用户命名空间里不能再建（Linux 的 32，最深是第 33 层）。
pub(crate) const MAX_USER_NS_LEVEL: u32 = 32;
/// 一张 ID 映射最多的段数（Linux 的 `UID_GID_MAP_MAX_EXTENTS`）。
const MAX_EXTENTS: usize = 340;

/// 一张 ID 映射（uid_map、gid_map）：每段是 (命名空间里的起点, 内核 ID 的起点, 个数)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IdMap(Vec<(u32, u32, u32)>);

impl IdMap {
    /// 开机时的命名空间：所有 ID 映射到自己（Linux 的 `0 0 4294967295`）。
    fn identity() -> IdMap {
        IdMap(vec![(0, 0, u32::MAX)])
    }

    /// 命名空间里的 `id` 对应的内核 ID（`make_kuid`）。
    pub fn down(&self, id: u32) -> Option<u32> {
        self.0
            .iter()
            .find(|&&(first, _, count)| id >= first && id - first < count)
            .map(|&(first, lower, _)| lower + (id - first))
    }

    /// 内核 ID `k` 在命名空间里的 ID（`from_kuid`）。
    pub fn up(&self, k: u32) -> Option<u32> {
        self.0
            .iter()
            .find(|&&(_, lower, count)| k >= lower && k - lower < count)
            .map(|&(first, lower, _)| first + (k - lower))
    }

    /// 各段。
    pub fn extents(&self) -> &[(u32, u32, u32)] {
        &self.0
    }

    /// 写 uid_map、gid_map 的内容（Linux 的 `map_write` 的解析部分）：每行
    /// "命名空间里的起点 上一层的起点 个数"，个数不能是 0，不能溢出、重叠，最多 340 段。
    /// 第二列还是上一层命名空间里的 ID，由调用方换成内核 ID。
    pub fn parse(data: &[u8]) -> Result<IdMap, Errno> {
        let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
        let mut extents: Vec<(u32, u32, u32)> = Vec::new();
        for line in text.split('\n').filter(|l| !l.is_empty()) {
            let nums: Vec<u32> = line
                .split_ascii_whitespace()
                .map(|n| n.parse().map_err(|_| Errno::EINVAL))
                .collect::<Result<_, _>>()?;
            let &[first, lower, count] = nums.as_slice() else {
                return Err(Errno::EINVAL);
            };
            let fits = |start: u32| {
                count > 0 && start.checked_add(count - 1).is_some_and(|e| e != u32::MAX)
            };
            if !fits(first) || !fits(lower) {
                return Err(Errno::EINVAL);
            }
            let overlaps = |&(f, l, c): &(u32, u32, u32)| {
                first < f + c && f < first + count || lower < l + c && l < lower + count
            };
            if extents.iter().any(overlaps) || extents.len() == MAX_EXTENTS {
                return Err(Errno::EINVAL);
            }
            extents.push((first, lower, count));
        }
        if extents.is_empty() {
            return Err(Errno::EINVAL);
        }
        Ok(IdMap(extents))
    }

    /// 把每段的第二列从上一层命名空间（映射是 `parent`）的 ID 换成内核 ID；有映射不到的是
    /// EPERM。一段在上一层里可能对应几段，按上一层的段拆开。
    pub fn lower_to_kernel(&self, parent: &IdMap) -> Result<IdMap, Errno> {
        let mut out = Vec::new();
        for &(first, lower, count) in &self.0 {
            let mut done = 0;
            while done < count {
                let id = lower + done;
                let &(pf, pl, pc) = parent
                    .0
                    .iter()
                    .find(|&&(f, _, c)| id >= f && id - f < c)
                    .ok_or(Errno::EPERM)?;
                let n = (count - done).min(pc - (id - pf));
                out.push((first + done, pl + (id - pf), n));
                done += n;
            }
        }
        Ok(IdMap(out))
    }
}

/// 用户命名空间（user_namespaces(7)）。开机时的那个所有 ID 都映射到自己；新建的一开始没有
/// 映射，由 `/proc/<pid>/uid_map`、`gid_map` 写一次。
#[derive(Debug)]
pub(crate) struct UserNs {
    /// 编号（`/proc/<pid>/ns/user` 里的 inode 号）。
    pub id: u64,
    pub parent: Option<Rc<UserNs>>,
    /// 嵌套的层数，开机时的是 0。
    pub level: u32,
    /// 建立者的有效用户 ID（内核 ID）：它在上一层命名空间里对这个命名空间有全部能力。
    pub owner: u32,
    pub uid_map: RefCell<IdMap>,
    pub gid_map: RefCell<IdMap>,
    /// `/proc/<pid>/setgroups` 是 "allow"：写成 "deny" 之后这个命名空间里不能 setgroups。
    pub setgroups: Cell<bool>,
}

impl PartialEq for UserNs {
    fn eq(&self, other: &UserNs) -> bool {
        self.id == other.id
    }
}

impl Eq for UserNs {}

impl UserNs {
    /// 开机时的用户命名空间（每次新建一份，按编号比较）。
    pub fn init() -> Rc<UserNs> {
        Rc::new(UserNs {
            id: INIT_USER_NS,
            parent: None,
            level: 0,
            owner: 0,
            uid_map: RefCell::new(IdMap::identity()),
            gid_map: RefCell::new(IdMap::identity()),
            setgroups: Cell::new(true),
        })
    }

    /// 在 `parent` 下面新建的命名空间，建立者的有效用户 ID 是 `owner`。setgroups 的状态
    /// 继承上一层的。
    pub fn child(parent: &Rc<UserNs>, owner: u32) -> Rc<UserNs> {
        Rc::new(UserNs {
            id: crate::vfs::new_ns_id(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner,
            uid_map: RefCell::new(IdMap::default()),
            gid_map: RefCell::new(IdMap::default()),
            setgroups: Cell::new(parent.setgroups.get()),
        })
    }

    /// `self` 是 `other` 或者它的祖先（Linux 的 `in_userns(self, other)`）。
    pub fn is_ancestor_of(&self, other: &UserNs) -> bool {
        let mut ns = other;
        while ns.level > self.level {
            match &ns.parent {
                Some(p) => ns = p,
                None => return false,
            }
        }
        ns.id == self.id
    }
}

/// 一个进程的凭据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cred {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    /// 附加组，升序。
    pub groups: Rc<[u32]>,
    pub cap_effective: u64,
    pub cap_permitted: u64,
    pub cap_inheritable: u64,
    /// 边界集：execve 之后的允许集不会超出它。
    pub cap_bounding: u64,
    /// 环境能力：总在允许集和可继承集里，execve 不是特权程序时保留下来进入允许集和有效集。
    pub cap_ambient: u64,
    /// `PR_SET_SECUREBITS`（[`secbit`]）。
    pub securebits: u32,
    /// 所在的用户命名空间。
    pub user_ns: Rc<UserNs>,
}

impl Cred {
    /// root：所有 ID 是 0，拥有全部能力。
    pub fn root() -> Cred {
        Cred {
            ruid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
            fsgid: 0,
            groups: Rc::from([]),
            cap_effective: CAP_FULL,
            cap_permitted: CAP_FULL,
            cap_inheritable: 0,
            cap_bounding: CAP_FULL,
            cap_ambient: 0,
            securebits: 0,
            user_ns: UserNs::init(),
        }
    }

    /// 进入用户命名空间 `ns`（新建它、setns 到它）：拥有那里的全部能力，securebits 恢复默认
    /// （Linux 的 `set_cred_user_ns`）。ID 不变，没有映射的在里面显示成 65534。
    pub fn enter_user_ns(&mut self, ns: Rc<UserNs>) {
        self.securebits = 0;
        self.cap_inheritable = 0;
        self.cap_permitted = CAP_FULL;
        self.cap_effective = CAP_FULL;
        self.cap_ambient = 0;
        self.cap_bounding = CAP_FULL;
        self.user_ns = ns;
    }

    /// 内核 ID `k` 在自己的命名空间里的用户 ID，映射不到时是 65534（`from_kuid_munged`）。
    pub fn uid_out(&self, k: u32) -> u32 {
        self.user_ns.uid_map.borrow().up(k).unwrap_or(OVERFLOW_ID)
    }

    /// 内核 ID `k` 在自己的命名空间里的组 ID，映射不到时是 65534。
    pub fn gid_out(&self, k: u32) -> u32 {
        self.user_ns.gid_map.borrow().up(k).unwrap_or(OVERFLOW_ID)
    }

    /// 自己的命名空间里的用户 ID `id` 对应的内核 ID（`make_kuid`）；映射不到是 None。
    pub fn uid_in(&self, id: u32) -> Option<u32> {
        self.user_ns.uid_map.borrow().down(id)
    }

    /// 自己的命名空间里的组 ID `id` 对应的内核 ID；映射不到是 None。
    pub fn gid_in(&self, id: u32) -> Option<u32> {
        self.user_ns.gid_map.borrow().down(id)
    }

    /// 自己的命名空间里的 root（用户 ID 0）的内核 ID；没有映射是 None。
    fn root_uid(&self) -> Option<u32> {
        self.uid_in(0)
    }

    /// `SECBIT_KEEP_CAPS`。
    pub fn keep_caps(&self) -> bool {
        self.securebits & secbit::KEEP_CAPS != 0
    }

    /// 在开机时的用户命名空间里有没有能力 `c`（Linux 的 `capable()`）：别的命名空间里的
    /// 进程总是没有。
    pub fn capable(&self, c: u32) -> bool {
        self.user_ns.level == 0 && self.capable_here(c)
    }

    /// 在自己的用户命名空间里有没有能力 `c`（有效集里有没有，`ns_capable(current_user_ns(), c)`）。
    pub fn capable_here(&self, c: u32) -> bool {
        self.cap_effective & (1 << c) != 0
    }

    /// 在用户命名空间 `ns` 里有没有能力 `c`（Linux 的 `cap_capable`）：`ns` 是自己的命名
    /// 空间时看有效集；是自己的命名空间的后代时，建立它（或它的祖先）的、有效用户 ID 与它的
    /// 属主相同的进程拥有全部能力；别的没有。
    pub fn ns_capable(&self, ns: &UserNs, c: u32) -> bool {
        let mut ns = ns;
        loop {
            if ns.id == self.user_ns.id {
                return self.capable_here(c);
            }
            if ns.level <= self.user_ns.level {
                return false;
            }
            let Some(parent) = &ns.parent else {
                return false;
            };
            if parent.id == self.user_ns.id && ns.owner == self.euid {
                return true;
            }
            ns = parent;
        }
    }

    /// 对属主、属组是 `st` 的文件有没有能力 `c`（Linux 的 `capable_wrt_inode_uidgid`）：
    /// 自己的命名空间里有，并且属主、属组在里面都有映射。
    pub fn capable_wrt(&self, st: &Stat, c: u32) -> bool {
        self.capable_here(c)
            && self.user_ns.uid_map.borrow().up(st.uid).is_some()
            && self.user_ns.gid_map.borrow().up(st.gid).is_some()
    }

    /// fsgid 或附加组里有没有 `gid`。
    pub fn in_group(&self, gid: u32) -> bool {
        self.fsgid == gid || self.groups.binary_search(&gid).is_ok()
    }

    /// 是文件的属主，或者有 `CAP_FOWNER`（chmod、设置时间戳等）。
    pub fn owns(&self, st: &Stat) -> bool {
        self.fsuid == st.uid || self.capable_wrt(st, cap::FOWNER)
    }

    /// 文件访问的权限检查（`generic_permission`）：`mask` 是 `MAY_*` 的组合。
    pub fn permission(&self, st: &Stat, mask: u32) -> Result<(), Errno> {
        let class = if self.fsuid == st.uid {
            st.mode >> 6
        } else if self.in_group(st.gid) {
            st.mode >> 3
        } else {
            st.mode
        };
        if mask & !class & 7 == 0 {
            return Ok(());
        }
        if st.kind() == mode::S_IFDIR {
            // 目录的权限总是可以被能力越过
            if mask & MAY_WRITE == 0 && self.capable_wrt(st, cap::DAC_READ_SEARCH)
                || self.capable_wrt(st, cap::DAC_OVERRIDE)
            {
                return Ok(());
            }
        } else {
            if mask == MAY_READ && self.capable_wrt(st, cap::DAC_READ_SEARCH) {
                return Ok(());
            }
            // 执行权限只有在至少一个 x 位存在时才能越过
            if (mask & MAY_EXEC == 0 || st.mode & 0o111 != 0)
                && self.capable_wrt(st, cap::DAC_OVERRIDE)
            {
                return Ok(());
            }
        }
        Err(Errno::EACCES)
    }

    /// 能不能越过所有目录的搜索权限检查（路径解析的快速路径）。别的用户命名空间里只能越过
    /// 属主、属组有映射的目录，不算。
    pub fn searches_all(&self) -> bool {
        self.capable(cap::DAC_READ_SEARCH) || self.capable(cap::DAC_OVERRIDE)
    }

    /// 在目录 `dir` 里删除或者改名 `victim`：要求目录可写可搜索，并且遵守粘滞位。
    pub fn may_delete(&self, dir: &Stat, victim: &Stat) -> Result<(), Errno> {
        self.permission(dir, MAY_WRITE | MAY_EXEC)?;
        if dir.mode & mode::S_ISVTX != 0
            && self.fsuid != victim.uid
            && self.fsuid != dir.uid
            && !self.capable_wrt(victim, cap::FOWNER)
        {
            return Err(Errno::EPERM);
        }
        Ok(())
    }

    /// access(2) 用的凭据：用真实用户 ID 和组 ID 检查，能力按真实用户 ID 是不是（自己的
    /// 命名空间里的）root 决定。
    pub fn for_access(&self) -> Cred {
        let mut c = self.clone();
        c.fsuid = self.ruid;
        c.fsgid = self.rgid;
        c.cap_effective = if Some(self.ruid) == self.root_uid() {
            self.cap_permitted
        } else {
            0
        };
        c
    }

    /// 新建文件的属主和属组：属组在 setgid 目录里继承目录的属组。返回 (uid, gid,
    /// 修正之后的权限位)：setgid 目录里新建的目录继承 setgid 位，不在属组里的调用者
    /// 不能新建 setgid 的可执行文件。
    pub fn new_owner(&self, dir: &Stat, perm: u32, is_dir: bool) -> (u32, u32, u32) {
        let mut perm = perm;
        let gid = if dir.mode & mode::S_ISGID != 0 {
            if is_dir {
                perm |= mode::S_ISGID;
            }
            dir.gid
        } else {
            self.fsgid
        };
        if !is_dir
            && perm & (mode::S_ISGID | 0o010) == mode::S_ISGID | 0o010
            && !self.in_group(gid)
            && !self.capable_wrt(dir, cap::FSETID)
        {
            perm &= !mode::S_ISGID;
        }
        (self.fsuid, gid, perm)
    }

    /// 用户 ID 变化之后按 Linux 的规则调整能力（`cap_emulate_setxuid`），root 是自己的命名
    /// 空间里的；`SECBIT_NO_SETUID_FIXUP` 时不调整。
    fn fix_caps(&mut self, old: &Cred) {
        if self.securebits & secbit::NO_SETUID_FIXUP != 0 {
            return;
        }
        let root = old.root_uid();
        let is_root = |id: u32| Some(id) == root;
        let had_root = is_root(old.ruid) || is_root(old.euid) || is_root(old.suid);
        let has_root = is_root(self.ruid) || is_root(self.euid) || is_root(self.suid);
        if had_root && !has_root {
            if !self.keep_caps() {
                self.cap_permitted = 0;
                self.cap_effective = 0;
            }
            // 原来的程序在 setresuid 成非 root 再 execve 之后就没有能力了，环境能力不能让它失效
            self.cap_ambient = 0;
        }
        if is_root(old.euid) && !is_root(self.euid) {
            self.cap_effective = 0;
        }
        if !is_root(old.euid) && is_root(self.euid) {
            self.cap_effective = self.cap_permitted;
        }
    }

    /// fsuid 变化之后调整文件系统能力；`SECBIT_NO_SETUID_FIXUP` 时不调整。
    fn fix_fs_caps(&mut self, old_fsuid: u32) {
        if self.securebits & secbit::NO_SETUID_FIXUP != 0 {
            return;
        }
        let root = self.root_uid();
        if Some(old_fsuid) == root && Some(self.fsuid) != root {
            self.cap_effective &= !CAP_FS_MASK;
        }
        if Some(old_fsuid) != root && Some(self.fsuid) == root {
            self.cap_effective |= self.cap_permitted & CAP_FS_MASK;
        }
    }

    /// setuid(2)。
    pub fn setuid(&mut self, uid: u32) -> Result<(), Errno> {
        let old = self.clone();
        if self.capable_here(cap::SETUID) {
            self.ruid = uid;
            self.suid = uid;
        } else if uid != self.ruid && uid != self.suid {
            return Err(Errno::EPERM);
        }
        self.euid = uid;
        self.fsuid = uid;
        self.fix_caps(&old);
        Ok(())
    }

    /// setreuid(2)：`None` 表示不修改。
    pub fn setreuid(&mut self, r: Option<u32>, e: Option<u32>) -> Result<(), Errno> {
        let old = self.clone();
        let (ruid, euid) = re_ids(
            (old.ruid, old.euid, old.suid),
            r,
            e,
            self.capable_here(cap::SETUID),
        )?;
        self.ruid = ruid;
        self.euid = euid;
        if r.is_some() || e.is_some_and(|e| e != old.ruid) {
            self.suid = euid;
        }
        self.fsuid = euid;
        self.fix_caps(&old);
        Ok(())
    }

    /// setresuid(2)：`None` 表示不修改。
    pub fn setresuid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), Errno> {
        let old = self.clone();
        let known = |id: u32| id == old.ruid || id == old.euid || id == old.suid;
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable_here(cap::SETUID) {
            return Err(Errno::EPERM);
        }
        self.ruid = r.unwrap_or(old.ruid);
        self.euid = e.unwrap_or(old.euid);
        self.suid = s.unwrap_or(old.suid);
        self.fsuid = self.euid;
        self.fix_caps(&old);
        Ok(())
    }

    /// setfsuid(2)：返回原来的 fsuid；不允许的修改静默忽略。
    pub fn setfsuid(&mut self, uid: Option<u32>) -> u32 {
        let old = self.fsuid;
        if let Some(uid) = uid
            && (uid == self.ruid
                || uid == self.euid
                || uid == self.suid
                || uid == self.fsuid
                || self.capable_here(cap::SETUID))
        {
            self.fsuid = uid;
            self.fix_fs_caps(old);
        }
        old
    }

    /// setgid(2)。
    pub fn setgid(&mut self, gid: u32) -> Result<(), Errno> {
        if self.capable_here(cap::SETGID) {
            self.rgid = gid;
            self.sgid = gid;
        } else if gid != self.rgid && gid != self.sgid {
            return Err(Errno::EPERM);
        }
        self.egid = gid;
        self.fsgid = gid;
        Ok(())
    }

    /// setregid(2)：`None` 表示不修改。
    pub fn setregid(&mut self, r: Option<u32>, e: Option<u32>) -> Result<(), Errno> {
        let old_rgid = self.rgid;
        let (rgid, egid) = re_ids(
            (self.rgid, self.egid, self.sgid),
            r,
            e,
            self.capable_here(cap::SETGID),
        )?;
        self.rgid = rgid;
        self.egid = egid;
        if r.is_some() || e.is_some_and(|e| e != old_rgid) {
            self.sgid = egid;
        }
        self.fsgid = egid;
        Ok(())
    }

    /// setresgid(2)：`None` 表示不修改。
    pub fn setresgid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), Errno> {
        let known = |id: u32| id == self.rgid || id == self.egid || id == self.sgid;
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable_here(cap::SETGID) {
            return Err(Errno::EPERM);
        }
        self.rgid = r.unwrap_or(self.rgid);
        self.egid = e.unwrap_or(self.egid);
        self.sgid = s.unwrap_or(self.sgid);
        self.fsgid = self.egid;
        Ok(())
    }

    /// setfsgid(2)：返回原来的 fsgid；不允许的修改静默忽略。
    pub fn setfsgid(&mut self, gid: Option<u32>) -> u32 {
        let old = self.fsgid;
        if let Some(gid) = gid
            && (gid == self.rgid
                || gid == self.egid
                || gid == self.sgid
                || gid == self.fsgid
                || self.capable_here(cap::SETGID))
        {
            self.fsgid = gid;
        }
        old
    }

    /// setgroups(2)：用户命名空间的 gid_map 写过、setgroups 没有被禁止时才行
    /// （`userns_may_setgroups`）。
    pub fn setgroups(&mut self, mut groups: Vec<u32>) -> Result<(), Errno> {
        let ns = &self.user_ns;
        let allowed = ns.setgroups.get() && !ns.gid_map.borrow().extents().is_empty();
        if !self.capable_here(cap::SETGID) || !allowed {
            return Err(Errno::EPERM);
        }
        groups.sort_unstable();
        self.groups = Rc::from(groups);
        Ok(())
    }

    /// capset(2)：只能缩小允许集，有效集不能超出允许集，可继承集不能超出原来的
    /// 可继承集和允许集（有 `CAP_SETPCAP` 时是边界集）。环境能力随之去掉不在新的允许集和
    /// 可继承集里的。
    pub fn capset(
        &mut self,
        effective: u64,
        permitted: u64,
        inheritable: u64,
    ) -> Result<(), Errno> {
        let inh_limit = self.cap_inheritable
            | if self.capable_here(cap::SETPCAP) {
                CAP_FULL
            } else {
                self.cap_permitted
            };
        if inheritable & !inh_limit != 0
            || inheritable & !(self.cap_inheritable | self.cap_bounding) != 0
            || permitted & !self.cap_permitted != 0
            || effective & !permitted != 0
        {
            return Err(Errno::EPERM);
        }
        self.cap_effective = effective;
        self.cap_permitted = permitted;
        self.cap_inheritable = inheritable;
        self.cap_ambient &= permitted & inheritable;
        Ok(())
    }

    /// `PR_CAP_AMBIENT_RAISE`：能力要同时在允许集和可继承集里，没有
    /// `SECBIT_NO_CAP_AMBIENT_RAISE`。
    pub fn raise_ambient(&mut self, c: u32) -> Result<(), Errno> {
        let bit = 1 << c;
        if self.cap_permitted & self.cap_inheritable & bit == 0
            || self.securebits & secbit::NO_CAP_AMBIENT_RAISE != 0
        {
            return Err(Errno::EPERM);
        }
        self.cap_ambient |= bit;
        Ok(())
    }

    /// `PR_SET_SECUREBITS`（`cap_task_prctl`）：要有 `CAP_SETPCAP`；锁住的位不能改，锁不能
    /// 解开，只认识 [`secbit`] 里的位。
    pub fn set_securebits(&mut self, bits: u32) -> Result<(), Errno> {
        let old = self.securebits;
        let locked = (old & secbit::ALL_LOCKS) >> 1;
        if locked & (old ^ bits) != 0
            || old & secbit::ALL_LOCKS & !bits != 0
            || bits & !(secbit::ALL_LOCKS | secbit::ALL_BITS) != 0
            || !self.capable_here(cap::SETPCAP)
        {
            return Err(Errno::EPERM);
        }
        self.securebits = bits;
        Ok(())
    }

    /// execve 之后的凭据（Linux 的 `bprm_fill_uid`、`cap_bprm_creds_from_file`）。`setuid`、
    /// `setgid` 是程序文件的 setuid 属主、setgid 属组（`no_new_privs` 时调用方不给），
    /// `fcaps` 是它的文件能力；`no_new_privs`，或者被没有 `CAP_SYS_PTRACE` 的跟踪者跟踪着
    /// （`ptraced`）时，得不到原来没有的能力。
    ///
    /// - 允许集是 (边界集 & 文件的允许集) | (可继承集 & 文件的可继承集) | 环境能力；有效用户
    ///   ID 或真实用户 ID 是 0（没有 `SECBIT_NOROOT`）时文件的两个集合当作全 1，有效用户 ID
    ///   是 0 时有效位也置上。setuid root 的程序带着文件能力、由非 root 执行时不按 root。
    /// - 有效集在文件的有效位置上时等于允许集，否则等于环境能力。
    /// - 文件能力或者 setuid、setgid 清掉环境能力。
    ///
    /// 文件要求有效位、允许集却得不到全部时返回 EPERM（程序拿不到它要的能力就别运行）。
    /// 成功时返回是不是特权执行（`AT_SECURE`）：setuid、setgid，或者非 root 得到了环境能力
    /// 以外的能力。
    pub fn exec(
        &mut self,
        setuid: Option<u32>,
        setgid: Option<u32>,
        fcaps: Option<FileCaps>,
        no_new_privs: bool,
        ptraced: bool,
    ) -> Result<bool, Errno> {
        let old = self.clone();
        let mut new = self.clone();
        if let Some(uid) = setuid {
            new.euid = uid;
        }
        if let Some(gid) = setgid {
            new.egid = gid;
        }
        let mut effective = false;
        new.cap_permitted = 0;
        if let Some(f) = fcaps {
            effective = f.effective;
            new.cap_permitted =
                old.cap_bounding & f.permitted | old.cap_inheritable & f.inheritable;
            if effective && f.permitted & !new.cap_permitted != 0 {
                return Err(Errno::EPERM);
            }
        }
        // root（自己的命名空间里的）：文件的允许集、可继承集当作全 1（handle_privileged_root）
        let root = old.root_uid();
        let is_root = |id: u32| Some(id) == root;
        let setuid_root = !is_root(new.ruid) && is_root(new.euid);
        if old.securebits & secbit::NOROOT == 0 && !(fcaps.is_some() && setuid_root) {
            if is_root(new.euid) || is_root(new.ruid) {
                new.cap_permitted = old.cap_bounding | old.cap_inheritable;
            }
            if is_root(new.euid) {
                effective = true;
            }
        }
        // 与 Linux 的 __is_setuid 一样，比的是新的有效 ID 和原来的真实 ID
        let setid = new.euid != old.ruid || new.egid != old.rgid;
        let gained = new.cap_permitted & !old.cap_permitted != 0;
        if (setid || gained) && (no_new_privs || ptraced) {
            if !old.capable_here(cap::SETUID) || no_new_privs {
                new.euid = new.ruid;
                new.egid = new.rgid;
            }
            new.cap_permitted &= old.cap_permitted;
        }
        new.suid = new.euid;
        new.fsuid = new.euid;
        new.sgid = new.egid;
        new.fsgid = new.egid;
        if fcaps.is_some() || setid {
            new.cap_ambient = 0;
        }
        new.cap_permitted |= new.cap_ambient;
        new.cap_effective = if effective {
            new.cap_permitted
        } else {
            new.cap_ambient
        };
        new.securebits &= !secbit::KEEP_CAPS;
        let secure =
            setid || !is_root(new.ruid) && (effective || new.cap_permitted & !new.cap_ambient != 0);
        *self = new;
        Ok(secure)
    }

    /// 能不能像 ptrace 那样访问凭据为 `target` 的另一个进程（Linux 的 `__ptrace_may_access`
    /// 加上 `cap_ptrace_access_check`）：真实用户、组（`fscreds` 时用文件系统的）等于目标的
    /// 所有用户、组并且目标可以 dump，或者在目标的用户命名空间里有 `CAP_SYS_PTRACE`；然后两者
    /// 在同一个用户命名空间里、目标的允许集不超过自己的，或者有那个 `CAP_SYS_PTRACE`。
    pub fn may_ptrace(&self, target: &Cred, dumpable: bool, fscreds: bool) -> bool {
        let (uid, gid) = if fscreds {
            (self.fsuid, self.fsgid)
        } else {
            (self.ruid, self.rgid)
        };
        let same = [target.ruid, target.euid, target.suid]
            .iter()
            .all(|&u| u == uid)
            && [target.rgid, target.egid, target.sgid]
                .iter()
                .all(|&g| g == gid);
        let privileged = self.ns_capable(&target.user_ns, cap::SYS_PTRACE);
        let subset =
            self.user_ns.id == target.user_ns.id && target.cap_permitted & !self.cap_permitted == 0;
        privileged || same && dumpable && subset
    }

    /// 能不能给凭据为 `target` 的进程发信号（`kill_ok_by_cred`）：发送者的真实或有效
    /// 用户 ID 等于目标的真实或保存的用户 ID，或者在目标的用户命名空间里有 `CAP_KILL`。
    pub fn may_signal(&self, target: &Cred) -> bool {
        [self.euid, self.ruid]
            .iter()
            .any(|&u| u == target.suid || u == target.ruid)
            || self.ns_capable(&target.user_ns, cap::KILL)
    }
}

/// setreuid、setregid 的新的真实 ID 和有效 ID：没有特权时真实 ID 只能改成原来的
/// 真实或有效 ID，有效 ID 只能改成原来的三个 ID 之一。
fn re_ids(
    (real, eff, saved): (u32, u32, u32),
    r: Option<u32>,
    e: Option<u32>,
    privileged: bool,
) -> Result<(u32, u32), Errno> {
    let r_ok = r.is_none_or(|r| r == real || r == eff);
    let e_ok = e.is_none_or(|e| e == real || e == eff || e == saved);
    if !(privileged || r_ok && e_ok) {
        return Err(Errno::EPERM);
    }
    Ok((r.unwrap_or(real), e.unwrap_or(eff)))
}

/// 扩展属性里程序文件能力的名字。
pub(crate) const XATTR_NAME_CAPS: &[u8] = b"security.capability";

const VFS_CAP_REVISION_MASK: u32 = 0xff00_0000;
const VFS_CAP_REVISION_1: u32 = 0x0100_0000;
const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const VFS_CAP_FLAGS_EFFECTIVE: u32 = 1;
/// 三个版本的长度：magic_etc 之后是一组（v1）或两组 permitted、inheritable，v3 再加 rootid。
const XATTR_CAPS_SZ_1: usize = 12;
const XATTR_CAPS_SZ_2: usize = 20;
const XATTR_CAPS_SZ_3: usize = 24;

/// 程序文件的能力（`security.capability` 里的 `struct vfs_cap_data`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileCaps {
    pub permitted: u64,
    pub inheritable: u64,
    /// 有效位：execve 之后有效集等于允许集。
    pub effective: bool,
}

fn le32(v: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(v[at..at + 4].try_into().unwrap())
}

impl FileCaps {
    /// 按 Linux 的 `get_vfs_caps_from_disk` 解析 execve 的程序的扩展属性：长度、版本对不上
    /// 是 EINVAL（execve 失败），v3 的 rootid 不是 0（属于别的用户命名空间）时没有能力。
    /// 超出 [`CAP_FULL`] 的位不要。
    pub fn parse(v: &[u8]) -> Result<Option<FileCaps>, Errno> {
        if v.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let magic = le32(v, 0);
        let words = match (magic & VFS_CAP_REVISION_MASK, v.len()) {
            (VFS_CAP_REVISION_1, XATTR_CAPS_SZ_1) => 1,
            (VFS_CAP_REVISION_2, XATTR_CAPS_SZ_2) => 2,
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) if le32(v, 20) == 0 => 2,
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) => return Ok(None),
            _ => return Err(Errno::EINVAL),
        };
        let mut caps = FileCaps {
            effective: magic & VFS_CAP_FLAGS_EFFECTIVE != 0,
            ..FileCaps::default()
        };
        for i in 0..words {
            caps.permitted |= u64::from(le32(v, 4 + 8 * i)) << (32 * i);
            caps.inheritable |= u64::from(le32(v, 8 + 8 * i)) << (32 * i);
        }
        caps.permitted &= CAP_FULL;
        caps.inheritable &= CAP_FULL;
        Ok(Some(caps))
    }

    /// setxattr 能不能写这个值（`cap_convert_nscap` 的 `validheader`）：只接受 v2、v3。
    pub fn valid_xattr(v: &[u8]) -> bool {
        let revision = |r| v.len() >= 4 && le32(v, 0) & VFS_CAP_REVISION_MASK == r;
        v.len() == XATTR_CAPS_SZ_2 && revision(VFS_CAP_REVISION_2)
            || v.len() == XATTR_CAPS_SZ_3 && revision(VFS_CAP_REVISION_3)
    }

    /// getxattr 读到的值（`cap_inode_getsecurity`）：rootid 为 0 的 v3 按 v2 返回，v2 和别的
    /// rootid 的 v3 原样返回，存着的不是 v2、v3（长度为 0 的值可以不经检查写进去）时
    /// EINVAL。
    pub fn xattr_for_reader(v: &[u8]) -> Result<Vec<u8>, Errno> {
        let revision = if v.len() >= 4 {
            le32(v, 0) & VFS_CAP_REVISION_MASK
        } else {
            0
        };
        match (revision, v.len()) {
            (VFS_CAP_REVISION_2, XATTR_CAPS_SZ_2) => Ok(v.to_vec()),
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) if le32(v, 20) == 0 => {
                let magic = VFS_CAP_REVISION_2 | le32(v, 0) & VFS_CAP_FLAGS_EFFECTIVE;
                let mut out = magic.to_le_bytes().to_vec();
                out.extend_from_slice(&v[4..XATTR_CAPS_SZ_2]);
                Ok(out)
            }
            (VFS_CAP_REVISION_3, XATTR_CAPS_SZ_3) => Ok(v.to_vec()),
            _ => Err(Errno::EINVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(perm: u32, uid: u32, gid: u32) -> Stat {
        Stat {
            ino: 1,
            mode: mode::S_IFREG | perm,
            nlink: 1,
            uid,
            gid,
            rdev: 0,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        }
    }

    fn user(uid: u32, gid: u32) -> Cred {
        let mut c = Cred::root();
        c.setresgid(Some(gid), Some(gid), Some(gid)).unwrap();
        c.setresuid(Some(uid), Some(uid), Some(uid)).unwrap();
        c
    }

    #[test]
    fn user_namespaces() {
        // 映射的解析：三列、个数不为 0、不重叠、不溢出
        assert_eq!(
            IdMap::parse(b"0 1000 1\n").unwrap().extents(),
            &[(0, 1000, 1)]
        );
        assert_eq!(
            IdMap::parse(b"0 0 1\n1 100000 65536").unwrap().extents(),
            &[(0, 0, 1), (1, 100_000, 65536)]
        );
        for bad in [
            &b""[..],
            b"0 0",
            b"0 0 0",
            b"0 0 1\n0 5 1",
            b"x 0 1",
            b"4294967295 0 1",
        ] {
            assert_eq!(IdMap::parse(bad), Err(Errno::EINVAL), "{bad:?}");
        }
        // 第二列按上一层的映射换成内核 ID，跨段时拆开
        let parent = IdMap(vec![(0, 100_000, 10), (10, 200_000, 10)]);
        let m = IdMap::parse(b"0 5 10")
            .unwrap()
            .lower_to_kernel(&parent)
            .unwrap();
        assert_eq!(m.extents(), &[(0, 100_005, 5), (5, 200_000, 5)]);
        assert_eq!(m.down(6), Some(200_001));
        assert_eq!(m.up(100_007), Some(2));
        assert_eq!(
            IdMap::parse(b"0 15 10").unwrap().lower_to_kernel(&parent),
            Err(Errno::EPERM)
        );

        // uid 1000 新建的命名空间：里面有全部能力，开机时的命名空间里没有；没写映射时 ID
        // 显示成 65534
        let user = Cred {
            ruid: 1000,
            euid: 1000,
            suid: 1000,
            fsuid: 1000,
            cap_effective: 0,
            cap_permitted: 0,
            ..Cred::root()
        };
        let init = user.user_ns.clone();
        let mut inside = user.clone();
        inside.enter_user_ns(UserNs::child(&init, 1000));
        let ns = inside.user_ns.clone();
        assert!(inside.capable_here(cap::SYS_ADMIN));
        assert!(!inside.capable(cap::SYS_ADMIN));
        assert!(inside.ns_capable(&ns, cap::SYS_ADMIN));
        assert!(!inside.ns_capable(&init, cap::SYS_ADMIN));
        assert_eq!(inside.uid_out(1000), OVERFLOW_ID);
        // 建立者（同一个有效用户 ID）在上一层对它有全部能力，别的用户、root 的普通能力不算
        assert!(user.ns_capable(&ns, cap::SYS_ADMIN));
        assert!(Cred::root().ns_capable(&ns, cap::SYS_ADMIN));
        let other = Cred {
            euid: 2000,
            ..user.clone()
        };
        assert!(!other.ns_capable(&ns, cap::SYS_ADMIN));
        // 写好 0 -> 1000 的映射：里面是 root；DAC 的能力只对有映射的文件有效
        *ns.uid_map.borrow_mut() = IdMap(vec![(0, 1000, 1)]);
        *ns.gid_map.borrow_mut() = IdMap(vec![(0, 0, 1)]);
        assert_eq!(inside.uid_out(1000), 0);
        assert_eq!(inside.uid_in(0), Some(1000));
        assert_eq!(inside.uid_in(1), None);
        assert!(inside.permission(&file(0o600, 1000, 0), MAY_READ).is_ok());
        assert_eq!(
            inside.permission(&file(0o600, 0, 0), MAY_READ),
            Err(Errno::EACCES)
        );
        // 换成映射不到的用户之后（setuid 的参数由 syscall 层换算）不再是 root，能力随之清掉
        let mut c = inside.clone();
        c.setresuid(None, Some(5000), None).unwrap();
        assert!(!c.capable_here(cap::SYS_ADMIN));
        c.setresuid(None, Some(1000), None).unwrap();
        assert!(c.capable_here(cap::SYS_ADMIN));
        // 执行程序：里面的 root 保留能力
        let mut e = inside.clone();
        e.exec(None, None, None, false, false).unwrap();
        assert_eq!(e.cap_effective, CAP_FULL);
    }

    #[test]
    fn permission_uses_the_owner_group_or_other_class() {
        let u = user(1000, 100);
        assert_eq!(u.cap_effective, 0);
        assert_eq!(u.cap_permitted, 0);
        // 属主只看属主位，哪怕其他位允许
        assert_eq!(
            u.permission(&file(0o077, 1000, 0), MAY_READ),
            Err(Errno::EACCES)
        );
        assert!(u.permission(&file(0o640, 0, 100), MAY_READ).is_ok());
        assert_eq!(
            u.permission(&file(0o640, 0, 100), MAY_WRITE),
            Err(Errno::EACCES)
        );
        assert!(u.permission(&file(0o604, 0, 0), MAY_READ).is_ok());
        let root = Cred::root();
        assert!(
            root.permission(&file(0o000, 1000, 100), MAY_READ | MAY_WRITE)
                .is_ok()
        );
        // root 执行文件也需要至少一个 x 位
        assert_eq!(
            root.permission(&file(0o666, 0, 0), MAY_EXEC),
            Err(Errno::EACCES)
        );
        assert!(root.permission(&file(0o100, 1000, 0), MAY_EXEC).is_ok());
        let mut dir = file(0o000, 1000, 100);
        dir.mode = mode::S_IFDIR;
        assert!(root.permission(&dir, MAY_EXEC | MAY_WRITE).is_ok());
    }

    #[test]
    fn dropping_root_follows_the_capability_rules() {
        // seteuid 到普通用户：有效集清空，允许集保留，可以再变回 root
        let mut c = Cred::root();
        c.setreuid(None, Some(1000)).unwrap();
        assert_eq!((c.ruid, c.euid, c.suid, c.fsuid), (0, 1000, 1000, 1000));
        assert!(!c.capable(cap::DAC_OVERRIDE));
        assert_eq!(c.cap_permitted, CAP_FULL);
        c.setresuid(None, Some(0), None).unwrap();
        assert!(c.capable(cap::SETUID));
        // setuid 之后三个 ID 都是普通用户，不能再回来
        c.setuid(1000).unwrap();
        assert_eq!((c.ruid, c.euid, c.suid), (1000, 1000, 1000));
        assert_eq!(c.cap_permitted, 0);
        assert_eq!(c.setuid(0), Err(Errno::EPERM));
        assert_eq!(c.setresuid(Some(0), None, None), Err(Errno::EPERM));
        // 再次调用同样的修改总是成功（libc 让每个线程都调用一遍）
        c.setuid(1000).unwrap();
        // fsuid 只能改成已有的某个 ID
        assert_eq!(c.setfsuid(Some(0)), 1000);
        assert_eq!(c.fsuid, 1000);
        assert_eq!(c.setfsuid(None), 1000);
    }

    #[test]
    fn fsuid_controls_the_filesystem_capabilities() {
        let mut c = Cred::root();
        assert_eq!(c.setfsuid(Some(1000)), 0);
        assert!(!c.capable(cap::DAC_OVERRIDE));
        assert!(c.capable(cap::KILL));
        c.setfsuid(Some(0));
        assert!(c.capable(cap::DAC_OVERRIDE));
    }

    #[test]
    fn exec_recomputes_capabilities() {
        let mut c = user(1000, 100);
        assert_eq!(c.exec(Some(0), None, None, false, false), Ok(true));
        assert_eq!((c.ruid, c.euid, c.suid), (1000, 0, 0));
        assert_eq!(c.cap_effective, CAP_FULL);
        let mut c = user(1000, 100);
        assert_eq!(c.exec(None, None, None, false, false), Ok(false));
        assert_eq!(c.cap_permitted, 0);
        let mut c = Cred::root();
        assert_eq!(c.exec(None, None, None, false, false), Ok(false));
        assert_eq!(c.cap_effective, CAP_FULL);
        let mut c = Cred::root();
        c.setreuid(None, Some(1000)).unwrap();
        assert_eq!(c.exec(None, None, None, false, false), Ok(true));
        assert_eq!(c.cap_permitted, CAP_FULL);
        assert_eq!(c.cap_effective, 0);
    }

    const NET_RAW: u64 = 1 << 13;
    const NET_BIND: u64 = 1 << cap::NET_BIND_SERVICE;

    fn caps(permitted: u64, inheritable: u64, effective: bool) -> Option<FileCaps> {
        Some(FileCaps {
            permitted,
            inheritable,
            effective,
        })
    }

    #[test]
    fn file_capabilities_grant_to_other_users() {
        // cap_net_raw+ep：允许集和有效集都有，特权执行
        let mut c = user(1000, 100);
        let r = c.exec(None, None, caps(NET_RAW, 0, true), false, false);
        assert_eq!(r, Ok(true));
        assert_eq!((c.cap_permitted, c.cap_effective), (NET_RAW, NET_RAW));
        // cap_net_raw+p：只进允许集，仍是特权执行
        let mut c = user(1000, 100);
        assert_eq!(
            c.exec(None, None, caps(NET_RAW, 0, false), false, false),
            Ok(true)
        );
        assert_eq!((c.cap_permitted, c.cap_effective), (NET_RAW, 0));
        // 文件的可继承集只给进程的可继承集里有的
        let mut c = user(1000, 100);
        c.cap_inheritable = NET_BIND;
        c.exec(None, None, caps(0, NET_BIND | NET_RAW, true), false, false)
            .unwrap();
        assert_eq!(c.cap_permitted, NET_BIND);
        // 边界集里没有、又要求有效位：EPERM，凭据不变
        let mut c = user(1000, 100);
        c.cap_bounding &= !NET_RAW;
        let before = c.clone();
        assert_eq!(
            c.exec(None, None, caps(NET_RAW, 0, true), false, false),
            Err(Errno::EPERM)
        );
        assert_eq!(c, before);
        // no_new_privs、没有特权的跟踪者：得不到新的能力
        for (nnp, traced) in [(true, false), (false, true)] {
            let mut c = user(1000, 100);
            c.exec(None, None, caps(NET_RAW, 0, true), nnp, traced)
                .unwrap();
            assert_eq!(c.cap_permitted, 0);
        }
        // root 照样是全部能力；setuid root 的程序带着文件能力、由非 root 执行时只有文件的
        let mut c = Cred::root();
        c.exec(None, None, caps(NET_RAW, 0, true), false, false)
            .unwrap();
        assert_eq!(c.cap_permitted, CAP_FULL);
        let mut c = user(1000, 100);
        c.exec(Some(0), None, caps(NET_RAW, 0, true), false, false)
            .unwrap();
        assert_eq!((c.euid, c.cap_permitted), (0, NET_RAW));
        // SECBIT_NOROOT：root 也只有文件的
        let mut c = Cred::root();
        c.set_securebits(secbit::NOROOT).unwrap();
        c.exec(None, None, None, false, false).unwrap();
        assert_eq!(c.cap_permitted, 0);
    }

    #[test]
    fn ambient_capabilities_survive_exec_of_ordinary_programs() {
        let mut c = Cred::root();
        c.set_securebits(secbit::KEEP_CAPS).unwrap();
        c.setresuid(Some(1000), Some(1000), Some(1000)).unwrap();
        c.capset(0, NET_RAW | NET_BIND, NET_RAW | NET_BIND).unwrap();
        assert_eq!(c.raise_ambient(cap::NET_ADMIN), Err(Errno::EPERM));
        c.raise_ambient(13).unwrap();
        let mut plain = c.clone();
        assert_eq!(plain.exec(None, None, None, false, false), Ok(false));
        assert_eq!(
            (plain.cap_permitted, plain.cap_effective, plain.cap_ambient),
            (NET_RAW, NET_RAW, NET_RAW)
        );
        // 文件能力、setuid 清掉环境能力
        let mut f = c.clone();
        f.exec(None, None, caps(NET_BIND, 0, true), false, false)
            .unwrap();
        assert_eq!((f.cap_permitted, f.cap_ambient), (NET_BIND, 0));
        let mut s = c.clone();
        s.exec(None, Some(50), None, false, false).unwrap();
        assert_eq!(s.cap_ambient, 0);
        // capset 去掉可继承集里的，环境能力随之去掉；用户 ID 全部变成非 0 时清空
        let mut d = c.clone();
        d.capset(0, NET_RAW | NET_BIND, NET_BIND).unwrap();
        assert_eq!(d.cap_ambient, 0);
        let mut r = Cred::root();
        r.cap_inheritable = NET_RAW;
        r.raise_ambient(13).unwrap();
        r.set_securebits(secbit::KEEP_CAPS).unwrap();
        r.setuid(1000).unwrap();
        assert_eq!((r.cap_permitted, r.cap_ambient), (CAP_FULL, 0));
        // SECBIT_NO_CAP_AMBIENT_RAISE
        let mut n = c.clone();
        n.securebits |= secbit::NO_CAP_AMBIENT_RAISE;
        assert_eq!(n.raise_ambient(13), Err(Errno::EPERM));
    }

    #[test]
    fn securebits_locks_cannot_be_undone() {
        let mut c = Cred::root();
        c.set_securebits(secbit::NOROOT | secbit::KEEP_CAPS_LOCKED)
            .unwrap();
        // 锁住的位不能改，锁不能解开，不认识的位不行
        assert_eq!(
            c.set_securebits(secbit::NOROOT | secbit::KEEP_CAPS | secbit::KEEP_CAPS_LOCKED),
            Err(Errno::EPERM)
        );
        assert_eq!(c.set_securebits(secbit::NOROOT), Err(Errno::EPERM));
        assert_eq!(
            c.set_securebits(0x100 | secbit::KEEP_CAPS_LOCKED),
            Err(Errno::EPERM)
        );
        c.set_securebits(secbit::KEEP_CAPS_LOCKED).unwrap();
        let mut u = user(1000, 100);
        assert_eq!(u.set_securebits(0), Err(Errno::EPERM));
        // SECBIT_NO_SETUID_FIXUP：变成非 root 也保留能力
        let mut f = Cred::root();
        f.set_securebits(secbit::NO_SETUID_FIXUP).unwrap();
        f.setresuid(Some(1000), Some(1000), Some(1000)).unwrap();
        assert_eq!(f.cap_effective, CAP_FULL);
    }

    #[test]
    fn file_capability_xattrs() {
        let v2 = |magic: u32, p: u32, i: u32| -> Vec<u8> {
            [magic, p, i, 1, 0]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect()
        };
        let parsed = FileCaps::parse(&v2(0x0200_0001, 1 << 13, 0))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.permitted, NET_RAW | 1 << 32);
        assert!(parsed.effective);
        // v1 只有低 32 位；长度、版本不对是 EINVAL；v3 的 rootid 不是 0 时没有能力
        let v1: Vec<u8> = [0x0100_0000u32, 1, 0]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(FileCaps::parse(&v1).unwrap().unwrap().permitted, 1);
        assert_eq!(FileCaps::parse(&v1[..8]), Err(Errno::EINVAL));
        assert_eq!(FileCaps::parse(&v2(0x0400_0000, 0, 0)), Err(Errno::EINVAL));
        let mut v3 = v2(0x0300_0001, 1, 0);
        v3.extend_from_slice(&1000u32.to_le_bytes());
        assert_eq!(FileCaps::parse(&v3), Ok(None));
        assert!(FileCaps::valid_xattr(&v3) && !FileCaps::valid_xattr(&v1));
        assert_eq!(FileCaps::xattr_for_reader(&v3).unwrap(), v3);
        // rootid 为 0 的 v3 读出来是 v2
        v3[20..].copy_from_slice(&0u32.to_le_bytes());
        let one = FileCaps {
            permitted: 1 | 1 << 32,
            inheritable: 0,
            effective: true,
        };
        assert_eq!(FileCaps::parse(&v3), Ok(Some(one)));
        assert_eq!(
            FileCaps::xattr_for_reader(&v3).unwrap(),
            v2(0x0200_0001, 1, 0)
        );
        assert_eq!(FileCaps::xattr_for_reader(&[]), Err(Errno::EINVAL));
    }

    #[test]
    fn sticky_directories_protect_other_users_files() {
        let u = user(1000, 100);
        let mut tmp = file(0o1777, 0, 0);
        tmp.mode = mode::S_IFDIR | 0o1777;
        assert!(u.may_delete(&tmp, &file(0o644, 1000, 100)).is_ok());
        assert_eq!(
            u.may_delete(&tmp, &file(0o666, 2000, 100)),
            Err(Errno::EPERM)
        );
        assert!(
            Cred::root()
                .may_delete(&tmp, &file(0o644, 2000, 100))
                .is_ok()
        );
    }

    #[test]
    fn new_files_inherit_the_group_of_setgid_directories() {
        let u = user(1000, 100);
        let mut dir = file(0o2775, 0, 50);
        dir.mode = mode::S_IFDIR | 0o2775;
        assert_eq!(u.new_owner(&dir, 0o755, true), (1000, 50, 0o2755));
        // 不在属组 50 里：新建的 setgid 可执行文件去掉 setgid 位
        assert_eq!(u.new_owner(&dir, 0o2755, false), (1000, 50, 0o755));
        assert_eq!(
            u.new_owner(&file(0o755, 0, 0), 0o644, false),
            (1000, 100, 0o644)
        );
    }
}
