//! 线程所在的 UTS、IPC、网络、cgroup 命名空间（Linux 的 `struct nsproxy` 里的这几种；挂载命名空间
//! 在 `FsContext` 里，pid 命名空间见 [`crate::pidns`]）。与 Linux 一样按线程记：unshare、
//! setns 只改调用的线程，clone 出来的线程、进程沿用父线程的。
//!
//! UTS 命名空间有自己的主机名和 NIS 域名。IPC 命名空间有自己的 System V IPC 对象、POSIX
//! 消息队列和它们的限制，内容放在 [`crate::kernel::Kernel::ipcs`] 里按编号存，没有线程用它、
//! 也没有命名空间文件留着它（[`NsRef`]）之后丢掉。网络命名空间见 [`NetNs`]。

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
/// time 命名空间的编号：只有开机时的那个（`CLONE_NEWTIME` 是 EPERM）。
pub(crate) const INIT_TIME_NS: u64 = 4_026_531_834;

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

/// 一个 cgroup 命名空间。所有进程都在同一个 cgroup "/" 里（[`crate::cgroup`]），从哪个
/// 命名空间看都是根，所以只有编号和属主。
#[derive(Debug)]
pub(crate) struct CgroupNs {
    pub id: u64,
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
    /// time 命名空间只有开机时的那个。
    Time,
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
            NsRef::Time => INIT_TIME_NS,
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
            NsRef::Time => UserNs::init(),
        })
    }
}

/// 线程的 UTS、IPC、网络、cgroup 命名空间。
#[derive(Clone, Debug)]
pub(crate) struct NsProxy {
    pub uts: Rc<UtsNs>,
    pub ipc: Rc<IpcNs>,
    pub net: Rc<NetNs>,
    pub cgroup: Rc<CgroupNs>,
}

impl NsProxy {
    /// 开机时的命名空间（每个内核一份，所有开机时的进程共用）。
    pub fn init() -> NsProxy {
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
                owner: UserNs::init(),
            }),
        }
    }
}
