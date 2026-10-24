//! 进程凭据（credentials(7)、capabilities(7)）：用户和组 ID、附加组、能力集，以及
//! 文件访问的权限检查（DAC）。
//!
//! Linux 的凭据属于线程，但 libc 的 setuid 系列会让每个线程都调用一遍，效果上是整个
//! 进程共用一份，所以这里把凭据放在线程组里。能力按 Linux 的规则随用户 ID 变化：
//! 有效用户 ID 变成非 0 时清空有效集，三个用户 ID 都变成非 0 时清空允许集，文件系统
//! 相关的能力跟着 fsuid 走；capset 可以去掉其中一部分，`PR_CAPBSET_DROP` 可以缩小边界
//! 集。没有用户命名空间、文件能力和环境（ambient）能力；securebits 只记录，起作用的只有
//! `SECBIT_KEEP_CAPS`。

use std::rc::Rc;

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
    pub const IPC_LOCK: u32 = 14;
    pub const IPC_OWNER: u32 = 15;
    pub const SYS_CHROOT: u32 = 18;
    pub const SYS_PTRACE: u32 = 19;
    pub const SYS_ADMIN: u32 = 21;
    pub const SYS_NICE: u32 = 23;
    pub const SYS_RESOURCE: u32 = 24;
    pub const MKNOD: u32 = 27;
    pub const MAC_OVERRIDE: u32 = 32;
    /// 最大的能力编号（`CAP_CHECKPOINT_RESTORE`）。
    pub const LAST: u32 = 40;
}

/// 全部能力（也是默认的边界集）。
pub(crate) const CAP_FULL: u64 = (1 << (cap::LAST + 1)) - 1;

/// securebits 里的 `SECBIT_KEEP_CAPS`（`PR_SET_KEEPCAPS` 设置的就是它）。
pub(crate) const SECBIT_KEEP_CAPS: u32 = 1 << 4;

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
    /// `PR_SET_SECUREBITS`。`SECBIT_KEEP_CAPS`：用户 ID 全部变成非 0 时保留允许集，
    /// execve 时清除。
    pub securebits: u32,
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
            securebits: 0,
        }
    }

    /// `SECBIT_KEEP_CAPS`。
    pub fn keep_caps(&self) -> bool {
        self.securebits & SECBIT_KEEP_CAPS != 0
    }

    /// 有效集里有没有能力 `c`。
    pub fn capable(&self, c: u32) -> bool {
        self.cap_effective & (1 << c) != 0
    }

    /// fsgid 或附加组里有没有 `gid`。
    pub fn in_group(&self, gid: u32) -> bool {
        self.fsgid == gid || self.groups.binary_search(&gid).is_ok()
    }

    /// 是文件的属主，或者有 `CAP_FOWNER`（chmod、设置时间戳等）。
    pub fn owns(&self, st: &Stat) -> bool {
        self.fsuid == st.uid || self.capable(cap::FOWNER)
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
            if mask & MAY_WRITE == 0 && self.capable(cap::DAC_READ_SEARCH)
                || self.capable(cap::DAC_OVERRIDE)
            {
                return Ok(());
            }
        } else {
            if mask == MAY_READ && self.capable(cap::DAC_READ_SEARCH) {
                return Ok(());
            }
            // 执行权限只有在至少一个 x 位存在时才能越过
            if (mask & MAY_EXEC == 0 || st.mode & 0o111 != 0) && self.capable(cap::DAC_OVERRIDE) {
                return Ok(());
            }
        }
        Err(Errno::EACCES)
    }

    /// 能不能越过所有目录的搜索权限检查（路径解析的快速路径）。
    pub fn searches_all(&self) -> bool {
        self.capable(cap::DAC_READ_SEARCH) || self.capable(cap::DAC_OVERRIDE)
    }

    /// 在目录 `dir` 里删除或者改名 `victim`：要求目录可写可搜索，并且遵守粘滞位。
    pub fn may_delete(&self, dir: &Stat, victim: &Stat) -> Result<(), Errno> {
        self.permission(dir, MAY_WRITE | MAY_EXEC)?;
        if dir.mode & mode::S_ISVTX != 0
            && self.fsuid != victim.uid
            && self.fsuid != dir.uid
            && !self.capable(cap::FOWNER)
        {
            return Err(Errno::EPERM);
        }
        Ok(())
    }

    /// access(2) 用的凭据：用真实用户 ID 和组 ID 检查，能力按真实用户 ID 是不是 root
    /// 决定。
    pub fn for_access(&self) -> Cred {
        let mut c = self.clone();
        c.fsuid = self.ruid;
        c.fsgid = self.rgid;
        c.cap_effective = if self.ruid == 0 {
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
            && !self.capable(cap::FSETID)
        {
            perm &= !mode::S_ISGID;
        }
        (self.fsuid, gid, perm)
    }

    /// 用户 ID 变化之后按 Linux 的规则调整能力（`cap_emulate_setxuid`）。
    fn fix_caps(&mut self, old: &Cred) {
        let had_root = old.ruid == 0 || old.euid == 0 || old.suid == 0;
        let has_root = self.ruid == 0 || self.euid == 0 || self.suid == 0;
        if had_root && !has_root && !self.keep_caps() {
            self.cap_permitted = 0;
            self.cap_effective = 0;
        }
        if old.euid == 0 && self.euid != 0 {
            self.cap_effective = 0;
        }
        if old.euid != 0 && self.euid == 0 {
            self.cap_effective = self.cap_permitted;
        }
    }

    /// fsuid 变化之后调整文件系统能力。
    fn fix_fs_caps(&mut self, old_fsuid: u32) {
        if old_fsuid == 0 && self.fsuid != 0 {
            self.cap_effective &= !CAP_FS_MASK;
        }
        if old_fsuid != 0 && self.fsuid == 0 {
            self.cap_effective |= self.cap_permitted & CAP_FS_MASK;
        }
    }

    /// setuid(2)。
    pub fn setuid(&mut self, uid: u32) -> Result<(), Errno> {
        let old = self.clone();
        if self.capable(cap::SETUID) {
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
            self.capable(cap::SETUID),
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
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable(cap::SETUID) {
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
                || self.capable(cap::SETUID))
        {
            self.fsuid = uid;
            self.fix_fs_caps(old);
        }
        old
    }

    /// setgid(2)。
    pub fn setgid(&mut self, gid: u32) -> Result<(), Errno> {
        if self.capable(cap::SETGID) {
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
            self.capable(cap::SETGID),
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
        if [r, e, s].into_iter().flatten().any(|id| !known(id)) && !self.capable(cap::SETGID) {
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
                || self.capable(cap::SETGID))
        {
            self.fsgid = gid;
        }
        old
    }

    /// setgroups(2)。
    pub fn setgroups(&mut self, mut groups: Vec<u32>) -> Result<(), Errno> {
        if !self.capable(cap::SETGID) {
            return Err(Errno::EPERM);
        }
        groups.sort_unstable();
        self.groups = Rc::from(groups);
        Ok(())
    }

    /// capset(2)：只能缩小允许集，有效集不能超出允许集，可继承集不能超出原来的
    /// 可继承集和允许集（有 `CAP_SETPCAP` 时是边界集）。
    pub fn capset(
        &mut self,
        effective: u64,
        permitted: u64,
        inheritable: u64,
    ) -> Result<(), Errno> {
        let inh_limit = self.cap_inheritable
            | if self.capable(cap::SETPCAP) {
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
        Ok(())
    }

    /// execve 之后的凭据：`setuid`、`setgid` 是程序文件的 setuid 属主、setgid 属组。
    /// 没有文件能力：有效用户 ID 或真实用户 ID 是 0 时允许集是边界集加可继承集，有效
    /// 用户 ID 是 0 时有效集等于允许集，否则都清空。
    pub fn exec(&mut self, setuid: Option<u32>, setgid: Option<u32>) {
        if let Some(uid) = setuid {
            self.euid = uid;
        }
        if let Some(gid) = setgid {
            self.egid = gid;
        }
        self.suid = self.euid;
        self.fsuid = self.euid;
        self.sgid = self.egid;
        self.fsgid = self.egid;
        self.securebits &= !SECBIT_KEEP_CAPS;
        self.cap_permitted = if self.euid == 0 || self.ruid == 0 {
            self.cap_bounding | self.cap_inheritable
        } else {
            0
        };
        self.cap_effective = if self.euid == 0 {
            self.cap_permitted
        } else {
            0
        };
    }

    /// 能不能给凭据为 `target` 的进程发信号（`kill_ok_by_cred`）：发送者的真实或有效
    /// 用户 ID 等于目标的真实或保存的用户 ID，或者有 `CAP_KILL`。
    pub fn may_signal(&self, target: &Cred) -> bool {
        [self.euid, self.ruid]
            .iter()
            .any(|&u| u == target.suid || u == target.ruid)
            || self.capable(cap::KILL)
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
        c.exec(Some(0), None);
        assert_eq!((c.ruid, c.euid, c.suid), (1000, 0, 0));
        assert_eq!(c.cap_effective, CAP_FULL);
        let mut c = user(1000, 100);
        c.exec(None, None);
        assert_eq!(c.cap_permitted, 0);
        let mut c = Cred::root();
        c.setreuid(None, Some(1000)).unwrap();
        c.exec(None, None);
        assert_eq!(c.cap_permitted, CAP_FULL);
        assert_eq!(c.cap_effective, 0);
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
