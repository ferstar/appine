//! 线程所在的 UTS、IPC、网络、cgroup、time 命名空间（Linux 的 `struct nsproxy` 里的这几种；挂载
//! 命名空间在 `FsContext` 里，pid 命名空间见 [`crate::pidns`]）。与 Linux 一样按线程记：
//! unshare、setns 只改调用的线程，clone 出来的线程、进程沿用父线程的。
//!
//! UTS 命名空间有自己的主机名和 NIS 域名。IPC 命名空间有自己的 System V IPC 对象、POSIX
//! 消息队列和它们的限制，内容放在 [`crate::kernel::Kernel::ipcs`] 里按编号存，没有线程用它、
//! 也没有命名空间文件留着它（[`NsRef`]）之后丢掉。网络命名空间见 [`NetNs`]。

use std::cell::Cell;
use std::rc::Rc;

use crate::cell::RefCell;
use crate::cred::UserNs;

/// 开机时的 UTS 命名空间的编号（`/proc/<pid>/ns/uts`）。
pub(crate) const INIT_UTS_NS: u64 = 4_026_531_838;
/// 开机时的 IPC 命名空间的编号。
pub(crate) const INIT_IPC_NS: u64 = 4_026_531_839;
/// 开机时的网络命名空间的编号。
pub(crate) const INIT_NET_NS: u64 = 4_026_531_840;
/// 开机时的 cgroup 命名空间的编号。
pub(crate) const INIT_CGROUP_NS: u64 = 4_026_531_835;
/// 开机时的 time 命名空间的编号。
pub(crate) const INIT_TIME_NS: u64 = 4_026_531_834;

/// 一个 time 命名空间（Linux 5.6 起的 `struct time_namespace`）：里面的进程看到的
/// CLOCK_MONOTONIC（含 RAW、COARSE）、CLOCK_BOOTTIME（含 ALARM）加上各自的偏移，绝对时刻
/// 的等待、定时器反过来减去偏移，`/proc/uptime`、sysinfo 和进程的启动时间按 boottime 的偏移。
/// unshare(`CLONE_NEWTIME`) 只建给子进程用的那个（偏移照抄原来的），之后 fork 出来的子进程
/// 才进去；setns 进来的线程两个都换。偏移经由 `/proc/<pid>/timens_offsets` 设置，有进程进来
/// 过之后不能再改。
#[derive(Debug)]
pub(crate) struct TimeNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间：在这里有 `CAP_SYS_TIME` 才能设偏移，`CAP_SYS_ADMIN` 才能
    /// setns 进来。
    pub owner: Rc<UserNs>,
    /// CLOCK_MONOTONIC、CLOCK_BOOTTIME 的偏移，各是 (秒, 纳秒)，纳秒在 [0, 10⁹)。
    pub offsets: Cell<[(i64, i64); 2]>,
    /// 有进程进来过，偏移不能再改（Linux 的 `frozen_offsets`）。开机时的总是。
    pub frozen: Cell<bool>,
}

impl TimeNs {
    /// 开机时的 time 命名空间（每个内核一份）。
    pub fn init() -> Rc<TimeNs> {
        Rc::new(TimeNs {
            id: INIT_TIME_NS,
            owner: UserNs::init(),
            offsets: Cell::new([(0, 0); 2]),
            frozen: Cell::new(true),
        })
    }

    /// 新建一个给子进程用的 time 命名空间，属于用户命名空间 `owner`，偏移照抄 `self`
    /// （Linux 的 `clone_time_ns`）。
    pub fn copy(&self, owner: Rc<UserNs>) -> Rc<TimeNs> {
        Rc::new(TimeNs {
            id: crate::vfs::new_ns_id(),
            owner,
            offsets: Cell::new(self.offsets.get()),
            frozen: Cell::new(false),
        })
    }

    /// 时钟 `clock` 的偏移（纳秒）：只有单调时钟和 boottime 有。
    pub fn offset(&self, clock: i32) -> i64 {
        let [mono, boot] = self.offsets_ns();
        match clock {
            // CLOCK_MONOTONIC、CLOCK_MONOTONIC_RAW、CLOCK_MONOTONIC_COARSE
            1 | 4 | 6 => mono,
            // CLOCK_BOOTTIME、CLOCK_BOOTTIME_ALARM
            7 | 9 => boot,
            _ => 0,
        }
    }

    /// 两个偏移（纳秒）：单调时钟的、boottime 的。
    pub fn offsets_ns(&self) -> [i64; 2] {
        self.offsets
            .get()
            .map(|(sec, nsec)| sec.saturating_mul(1_000_000_000).saturating_add(nsec))
    }
}

/// 一个 UTS 命名空间。
#[derive(Debug)]
pub(crate) struct UtsNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间：在这里有 `CAP_SYS_ADMIN` 才能改名字、setns 进来。
    pub owner: Rc<UserNs>,
    /// 主机名和 NIS 域名（uname、sethostname、`/proc/sys/kernel/hostname`），最长
    /// [`crate::kernel::UTS_LEN`] 字节。
    pub hostname: RefCell<Vec<u8>>,
    pub domainname: RefCell<Vec<u8>>,
}

impl UtsNs {
    /// 复制一份名字，属于用户命名空间 `owner`（Linux 的 `copy_utsname`）。
    pub fn copy(&self, owner: Rc<UserNs>) -> Rc<UtsNs> {
        Rc::new(UtsNs {
            id: crate::vfs::new_ns_id(),
            owner,
            hostname: RefCell::new(self.hostname.borrow().clone()),
            domainname: RefCell::new(self.domainname.borrow().clone()),
        })
    }
}

/// 一个 IPC 命名空间。里面的对象在 [`crate::kernel::Kernel::ipcs`] 里。
#[derive(Debug)]
pub(crate) struct IpcNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间（权限检查用）。
    pub owner: Rc<UserNs>,
}

/// 一个 cgroup 命名空间：`/proc/<pid>/cgroup` 里的路径、在里面挂的 cgroup2 的根，相对它的根
/// cgroup（[`crate::cgroup`]）。
#[derive(Debug)]
pub(crate) struct CgroupNs {
    pub id: u64,
    /// 根：建立它的进程当时所在的 cgroup（[`crate::cgroup`] 的编号）。
    pub root: u64,
    /// 建立它时所在的用户命名空间（setns 的权限检查用）。
    pub owner: Rc<UserNs>,
}

/// 一个网络命名空间。开机时的那个就是宿主的网络（socket 直接用宿主的）。新建的里面只有
/// 回环接口 lo：socket 仍是宿主 socket，地址换成宿主上给这个命名空间的回环地址
/// 127.a.b.0/24（[`crate::netns`]），别的地址都到不了。
#[derive(Debug)]
pub(crate) struct NetNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间：在这里有 `CAP_NET_ADMIN` 才能配置 lo。
    pub owner: Rc<UserNs>,
    /// 宿主上的回环地址 127.a.b.0/24 的 a、b；开机时的命名空间是 None。
    pub host_net: Option<[u8; 2]>,
    /// lo 的状态。
    pub lo: RefCell<Loopback>,
}

/// 新的网络命名空间里 lo 的状态：一开始是关着的、没有地址（Linux 的 `loopback_net_init`）。
#[derive(Debug, Default, Clone)]
pub(crate) struct Loopback {
    /// `IFF_UP`。
    pub up: bool,
    /// IPv4 地址和前缀长度。
    pub v4: Vec<([u8; 4], u8)>,
    /// 有 ::1（lo 起来时自动加上，Linux 的 addrconf）。
    pub v6: bool,
    /// 设过的 MTU（`SIOCSIFMTU`），默认 65536。
    pub mtu: Option<u32>,
}

/// 一个命名空间（Linux 的 `struct ns_common`）。打开着的命名空间文件、bind 挂载出去的
/// 命名空间文件（nsfs）持有它：里面的进程都退出之后命名空间也还在，可以 setns 进去
/// （`ip netns add` 就是这样留住网络命名空间的）。
#[derive(Clone, Debug)]
pub(crate) enum NsRef {
    Mnt(Rc<crate::vfs::MntNs>),
    User(Rc<UserNs>),
    /// `pid` 和 `pid_for_children` 的文件都是它。
    Pid(Rc<crate::pidns::PidNs>),
    Uts(Rc<UtsNs>),
    Ipc(Rc<IpcNs>),
    Net(Rc<NetNs>),
    Cgroup(Rc<CgroupNs>),
    /// `time` 和 `time_for_children` 的文件都是它。
    Time(Rc<TimeNs>),
}

impl NsRef {
    /// 编号（命名空间文件的 inode 号）。
    pub fn id(&self) -> u64 {
        match self {
            NsRef::Mnt(n) => n.id,
            NsRef::User(n) => n.id,
            NsRef::Pid(n) => n.id,
            NsRef::Uts(n) => n.id,
            NsRef::Ipc(n) => n.id,
            NsRef::Net(n) => n.id,
            NsRef::Cgroup(n) => n.id,
            NsRef::Time(n) => n.id,
        }
    }

    /// 属主（Linux 的 `ns->ops->owner`）：建立它时所在的用户命名空间，用户命名空间的是
    /// 上一层，开机时的用户命名空间没有。
    pub fn owner(&self) -> Option<Rc<UserNs>> {
        Some(match self {
            NsRef::Mnt(n) => n.owner.clone(),
            NsRef::User(n) => return n.parent.clone(),
            NsRef::Pid(n) => n.owner.clone(),
            NsRef::Uts(n) => n.owner.clone(),
            NsRef::Ipc(n) => n.owner.clone(),
            NsRef::Net(n) => n.owner.clone(),
            NsRef::Cgroup(n) => n.owner.clone(),
            NsRef::Time(n) => n.owner.clone(),
        })
    }
}

/// 线程的 UTS、IPC、网络、cgroup、time 命名空间。
#[derive(Clone, Debug)]
pub(crate) struct NsProxy {
    pub uts: Rc<UtsNs>,
    pub ipc: Rc<IpcNs>,
    pub net: Rc<NetNs>,
    pub cgroup: Rc<CgroupNs>,
    /// 所在的 time 命名空间，和之后建的子进程进的那个（unshare 了 `CLONE_NEWTIME` 时不同）。
    pub time: Rc<TimeNs>,
    pub time_for_children: Rc<TimeNs>,
}

impl NsProxy {
    /// 开机时的命名空间（每个内核一份，所有开机时的进程共用）。
    pub fn init() -> NsProxy {
        let time = TimeNs::init();
        NsProxy {
            uts: Rc::new(UtsNs {
                id: INIT_UTS_NS,
                owner: UserNs::init(),
                hostname: RefCell::new(b"localhost".to_vec()),
                domainname: RefCell::new(b"(none)".to_vec()),
            }),
            ipc: Rc::new(IpcNs {
                id: INIT_IPC_NS,
                owner: UserNs::init(),
            }),
            net: Rc::new(NetNs {
                id: INIT_NET_NS,
                owner: UserNs::init(),
                host_net: None,
                lo: RefCell::default(),
            }),
            cgroup: Rc::new(CgroupNs {
                id: INIT_CGROUP_NS,
                root: crate::cgroup::ROOT,
                owner: UserNs::init(),
            }),
            time: time.clone(),
            time_for_children: time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 偏移按时钟分：单调时钟的三种、boottime 的两种，别的没有；新建的照抄、没有冻结。
    #[test]
    fn time_namespace_offsets() {
        let init = TimeNs::init();
        assert!(init.frozen.get());
        let ns = init.copy(UserNs::init());
        assert_ne!(ns.id, init.id);
        assert!(!ns.frozen.get());
        ns.offsets.set([(86_400, 0), (-1, 500_000_000)]);
        for clock in [1, 4, 6] {
            assert_eq!(ns.offset(clock), 86_400_000_000_000);
        }
        for clock in [7, 9] {
            assert_eq!(ns.offset(clock), -500_000_000);
        }
        for clock in [0, 2, 3, 5, 8, 11] {
            assert_eq!(ns.offset(clock), 0);
        }
        assert_eq!(ns.copy(UserNs::init()).offsets.get(), ns.offsets.get());
    }
}
