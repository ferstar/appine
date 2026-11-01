//! 虚拟网卡：bridge、veth、dummy（docs/rounds/170-netdev.md）。
//!
//! 每个网络命名空间（开机时的那个也是）有一张设备表（[`Devices`]，在 [`NetNs::devs`]
//! 里）。容器引擎（dockerd、netavark）经由 netlink 建 bridge 和 veth 对、配地址和路由，这里
//! 照 Linux 6.1 记下这些状态、回答查询；不出现数据包，容器的流量在 socket 层换成宿主上的
//! 回环地址（[`crate::netns`]）。
//!
//! - 开机时的命名空间里虚拟设备叠在宿主的接口之上：编号从宿主最大的编号往后分配，名字
//!   不能与宿主的重复；宿主的接口仍然改不了。新的命名空间里与 lo 并列。
//! - veth 的两端各在自己的命名空间里，对端经由 `Weak` 找到：一端删除时另一端随之删除，
//!   命名空间没人用了（设备表随之释放）时它的 veth 的对端也删除（查表时清掉，
//!   [`Devices::prune`]）。
//! - 载波和状态照 6.1：veth 两端都启用了才有载波；bridge 有一个转发状态的端口（端口启用、
//!   有载波，STP 关着时马上进入转发）才有；dummy 总有。启用了、没有载波的 veth 是
//!   LOWERLAYERDOWN（`default_operstate`：iflink 不是自己），bridge 是 DOWN，dummy 总是
//!   UNKNOWN。
//! - 地址：IPv4 的照 `__inet_insert_ifa`（同一网段的第二个是 secondary），按地址加的路由照
//!   `fib_add_ifaddr`（本地路由总加，网段、广播路由在接口启用时加），接口停用时去掉经由它的
//!   路由（本地路由除外，`fib_sync_down_dev` 不动 scope 为 nowhere 的下一跳）。IPv6 在接口
//!   启用并有载波、没有关掉 IPv6 时配置（addrconf）：按 MAC 生成链路本地地址（EUI-64），加
//!   fe80::/64 和 ff00::/8 的路由；没有 DAD，地址马上可用。接口停用时 IPv6 的地址和路由都
//!   去掉（`keep_addr_on_down` 为 0）。
//! - 路由：按地址加的和经由 netlink 加的都按加入的先后记在命名空间里，转储时与宿主的
//!   （或者 lo 的）路由合在一起排序（[`crate::route`]）。只能经由虚拟设备。
//! - 接口的 sysctl 见 [`crate::netconf`]。

use std::rc::{Rc, Weak};

use appine_hal::net::Interface;

use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::net::{AF_INET, AF_INET6, attr};
use crate::netconf::{Conf, NetConf};
use crate::nsproxy::NetNs;
use crate::route::Entry;

pub(crate) type DevRef = Rc<RefCell<Dev>>;

const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_RUNNING: u32 = 0x40;
const IFF_NOARP: u32 = 0x80;
const IFF_MULTICAST: u32 = 0x1000;
const IFF_LOWER_UP: u32 = 0x1_0000;
/// `__dev_change_flags` 按请求改的位：DEBUG、NOTRAILERS、NOARP、DYNAMIC、MULTICAST、
/// PORTSEL、AUTOMEDIA，加上用户设的 PROMISC、ALLMULTI（gflags）。
const CHANGEABLE: u32 = 0x4 | 0x20 | 0x80 | 0x8000 | 0x1000 | 0x2000 | 0x4000 | 0x100 | 0x200;
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_LOWERLAYERDOWN: u8 = 3;
const IF_OPER_UP: u8 = 6;
const IFNAMSIZ: usize = 16;
const ETH_MIN_MTU: u32 = 68;
const ETH_MAX_MTU: u32 = 65535;
const ETH_DATA_LEN: u32 = 1500;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;

const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_LINK: u16 = 5;
const IFLA_QDISC: u16 = 6;
const IFLA_STATS: u16 = 7;
const IFLA_MASTER: u16 = 10;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_IFALIAS: u16 = 20;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_CARRIER_CHANGES: u16 = 35;
const IFLA_LINK_NETNSID: u16 = 37;
const IFLA_PROTO_DOWN: u16 = 39;
const IFLA_GSO_MAX_SEGS: u16 = 40;
const IFLA_GSO_MAX_SIZE: u16 = 41;
const IFLA_TARGET_NETNSID: u16 = 46;
const IFLA_CARRIER_UP_COUNT: u16 = 47;
const IFLA_CARRIER_DOWN_COUNT: u16 = 48;
const IFLA_MIN_MTU: u16 = 50;
const IFLA_MAX_MTU: u16 = 51;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_INFO_SLAVE_KIND: u16 = 4;
const IFLA_INFO_SLAVE_DATA: u16 = 5;
const VETH_INFO_PEER: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_RT_PRIORITY: u16 = 9;
const IFA_PROTO: u16 = 11;
const IFA_F_SECONDARY: u32 = 0x1;
const IFA_F_NODAD: u32 = 0x2;
const IFA_F_OPTIMISTIC: u32 = 0x4;
const IFA_F_HOMEADDRESS: u32 = 0x10;
const IFA_F_PERMANENT: u32 = 0x80;
const IFA_F_MANAGETEMPADDR: u32 = 0x100;
const IFA_F_NOPREFIXROUTE: u32 = 0x200;
const IFA_F_MCAUTOJOIN: u32 = 0x400;
/// 内核生成的链路本地地址（`IFAPROT_KERNEL_LL`）。
const IFAPROT_KERNEL_LL: u8 = 3;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const RT_TABLE_UNSPEC: u8 = 0;
const RT_TABLE_MAIN: u8 = 254;
const RT_TABLE_LOCAL: u8 = 255;
const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTN_MULTICAST: u8 = 5;
const RTPROT_KERNEL: u8 = 2;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RT_SCOPE_NOWHERE: u8 = 255;
/// rtm_flags：下一跳的接口没有载波（`ip route` 的 linkdown）。
const RTNH_F_LINKDOWN: u32 = 0x10;
/// IPv6 路由的度量：按地址加的、没有给出时（`IP6_RT_PRIO_ADDRCONF`、`IP6_RT_PRIO_USER`）。
const IP6_PRIO_ADDRCONF: u32 = 256;
const IP6_PRIO_USER: u32 = 1024;

/// netlink 请求失败：错误码和 extack 的消息（Linux 用 `NL_SET_ERR_MSG` 给出的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fail {
    pub errno: i32,
    pub msg: Option<&'static str>,
}

impl From<Errno> for Fail {
    fn from(e: Errno) -> Fail {
        Fail {
            errno: e.0,
            msg: None,
        }
    }
}

fn fail(e: Errno, msg: &'static str) -> Fail {
    Fail {
        errno: e.0,
        msg: Some(msg),
    }
}

/// 设备的种类（`IFLA_INFO_KIND`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bridge,
    Veth,
    Dummy,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bridge => "bridge",
            Kind::Veth => "veth",
            Kind::Dummy => "dummy",
        }
    }

    fn parse(b: &[u8]) -> Option<Kind> {
        match b {
            b"bridge" => Some(Kind::Bridge),
            b"veth" => Some(Kind::Veth),
            b"dummy" => Some(Kind::Dummy),
            _ => None,
        }
    }

    /// MTU 的上下限（`min_mtu`、`max_mtu`，0 是不限）：dummy 都是 0。
    fn mtu_range(self) -> (u32, u32) {
        match self {
            Kind::Dummy => (0, 0),
            _ => (ETH_MIN_MTU, ETH_MAX_MTU),
        }
    }

    /// 能不能移到别的命名空间（bridge 有 `NETIF_F_NETNS_LOCAL`）。
    fn movable(self) -> bool {
        self != Kind::Bridge
    }
}

/// 接口上的一个 IPv4 地址（`struct in_ifaddr`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr4 {
    pub local: [u8; 4],
    /// `IFA_ADDRESS`：点对点时是对端，别的时候与 local 相同。
    pub address: [u8; 4],
    pub prefix: u8,
    /// 全 0 是没有。
    pub broadcast: [u8; 4],
    pub label: Vec<u8>,
    pub flags: u32,
    pub scope: u8,
    pub metric: u32,
    /// `IFA_PROTO`，0 是没有。
    pub proto: u8,
}

impl Addr4 {
    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    /// 网段（按 `IFA_ADDRESS`）。
    fn subnet(&self) -> u32 {
        u32::from_be_bytes(self.address) & self.mask()
    }

    fn contains(&self, ip: [u8; 4]) -> bool {
        u32::from_be_bytes(ip) & self.mask() == self.subnet()
    }
}

/// 接口上的一个 IPv6 地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr6 {
    pub addr: [u8; 16],
    pub prefix: u8,
    pub flags: u32,
    pub proto: u8,
    pub metric: u32,
}

/// 地址的范围（`ipv6_addr_scope`）：链路本地的是 link，::1 是 host，别的是 universe。
fn scope6(a: &[u8; 16]) -> u8 {
    if a[0] == 0xfe && a[1] & 0xc0 == 0x80 {
        RT_SCOPE_LINK
    } else if *a == std::net::Ipv6Addr::LOCALHOST.octets() {
        RT_SCOPE_HOST
    } else {
        RT_SCOPE_UNIVERSE
    }
}

/// 一个虚拟设备。
#[derive(Debug)]
pub(crate) struct Dev {
    pub ns: Weak<NetNs>,
    pub index: u32,
    pub name: Vec<u8>,
    pub kind: Kind,
    /// `IFF_UP`。
    pub up: bool,
    /// 启用过（挂上了 noqueue 的 qdisc，停用之后还在）。
    pub ever_up: bool,
    /// bridge 按端口算过状态（`br_port_state_selection`：端口增删之后）。在那之前它的载波
    /// 是建出来时的"有"，状态也一直是 UNKNOWN（没有过载波的变化事件）。
    pub selected: bool,
    /// 别的标志：种类决定的（BROADCAST、MULTICAST、NOARP）和改过的（[`CHANGEABLE`]）。
    pub flags: u32,
    pub mtu: u32,
    /// bridge 的 MTU 是用户设的，不再跟着端口调整（`BROPT_MTU_SET_BY_USER`）。
    pub mtu_set: bool,
    pub mac: [u8; 6],
    /// MAC 是用户给的（`NET_ADDR_SET`）：bridge 不再取端口里最小的。
    pub mac_set: bool,
    pub txqlen: u32,
    pub group: u32,
    pub alias: Option<Vec<u8>>,
    /// veth 的另一端。
    pub peer: Weak<RefCell<Dev>>,
    /// veth 已经连上了另一端（建的时候先登记的那一端在另一端登记之前还没有）。
    pub paired: bool,
    /// 挂在哪个 bridge 上。
    pub master: Weak<RefCell<Dev>>,
    pub v4: Vec<Addr4>,
    pub v6: Vec<Addr6>,
    /// IPv6 已经配置过（启用并有载波时，addrconf）。
    pub v6_ready: bool,
    pub conf4: Conf,
    pub conf6: Conf,
    /// 载波变化的次数（`IFLA_CARRIER_CHANGES`），和上一次看到的载波。
    pub carrier_changes: u32,
    pub carrier_was: bool,
}

/// 按地址或者经由 netlink 加的一条路由（只经由虚拟设备）。
#[derive(Debug, Clone)]
pub(crate) struct Rt {
    pub family: i32,
    pub table: u8,
    pub ty: u8,
    pub proto: u8,
    pub scope: u8,
    pub tos: u8,
    pub dst: Vec<u8>,
    pub prefix: u8,
    pub prefsrc: Option<Vec<u8>>,
    pub gateway: Option<Vec<u8>>,
    pub dev: Weak<RefCell<Dev>>,
    pub metric: u32,
    /// 按哪个地址加的（IPv4 是 local，IPv6 是地址），或者 IPv6 配置时加的（空）；None 是
    /// 经由 netlink 加的。
    pub owner: Option<Vec<u8>>,
}

impl Rt {
    fn on(&self, d: &DevRef) -> bool {
        self.dev.upgrade().is_some_and(|x| Rc::ptr_eq(&x, d))
    }

    /// 同一条路由（加的时候去重）。
    fn same(&self, o: &Rt) -> bool {
        (
            self.family,
            self.table,
            self.ty,
            self.tos,
            &self.dst,
            self.prefix,
            &self.prefsrc,
            &self.gateway,
            self.metric,
        ) == (
            o.family, o.table, o.ty, o.tos, &o.dst, o.prefix, &o.prefsrc, &o.gateway, o.metric,
        ) && Weak::ptr_eq(&self.dev, &o.dev)
    }
}

/// 一个网络命名空间的虚拟设备、路由和 sysctl。
#[derive(Debug, Default)]
pub(crate) struct Devices {
    pub list: Vec<DevRef>,
    /// 上一次分配的接口编号（Linux 的 `net->ifindex`）。
    ifindex: u32,
    /// 经由虚拟设备的路由，按加入的先后。
    pub routes: Vec<Rt>,
    pub conf: NetConf,
    /// 给别的命名空间分配的编号（`IFLA_LINK_NETNSID`、`RTM_GETNSID`）。
    pub nsids: Vec<(Weak<NetNs>, i32)>,
    /// 开机时的命名空间里：虚拟地址和它在宿主上的回环地址（127.255.x.y，用到时分配，
    /// [`crate::netns`]）。
    pub hostmap: Vec<([u8; 4], [u8; 4])>,
}

impl Devices {
    /// 新建的命名空间的：lo 占了编号 1，sysctl 按 `init` 的（[`NetConf::for_new_ns`]）。
    pub fn for_new_ns(init: &NetConf) -> Devices {
        Devices {
            ifindex: 1,
            conf: NetConf::for_new_ns(init),
            ..Devices::default()
        }
    }

    /// 去掉对端已经没了的 veth（对端的命名空间释放了）和经由已经没了的设备的路由。
    fn prune(&mut self) {
        let gone: Vec<DevRef> = self
            .list
            .iter()
            .filter(|d| {
                let d = d.borrow();
                d.kind == Kind::Veth && d.paired && d.peer.strong_count() == 0
            })
            .cloned()
            .collect();
        if !gone.is_empty() {
            self.list.retain(|d| !gone.iter().any(|g| Rc::ptr_eq(g, d)));
            for g in &gone {
                for d in &self.list {
                    let mut d = d.borrow_mut();
                    if d.master.upgrade().is_some_and(|m| Rc::ptr_eq(&m, g)) {
                        d.master = Weak::new();
                    }
                }
            }
        }
        self.routes.retain(|r| r.dev.strong_count() > 0);
    }

    fn by_index(&self, index: u32) -> Option<DevRef> {
        self.list
            .iter()
            .find(|d| d.borrow().index == index)
            .cloned()
    }

    fn by_name(&self, name: &[u8]) -> Option<DevRef> {
        self.list.iter().find(|d| d.borrow().name == name).cloned()
    }

    /// 端口：挂在 `br` 上的设备。
    fn ports(&self, br: &DevRef) -> Vec<DevRef> {
        self.list
            .iter()
            .filter(|d| {
                d.borrow()
                    .master
                    .upgrade()
                    .is_some_and(|m| Rc::ptr_eq(&m, br))
            })
            .cloned()
            .collect()
    }
}

/// 命名空间 `ns` 里的设备表：先清掉对端没了的 veth。
fn devs(ns: &NetNs) -> crate::cell::RefMut<'_, Devices> {
    let mut d = ns.devs.borrow_mut();
    d.prune();
    d
}

/// 命名空间里不是虚拟设备的接口：开机时的是宿主的，新建的是 lo。
pub(crate) fn base(ns: &NetNs) -> Vec<Interface> {
    if ns.host_net.is_none() {
        appine_hal::net::interfaces()
    } else {
        vec![crate::netns::lo_interface(ns)]
    }
}

fn random_mac() -> [u8; 6] {
    let mut m = [0u8; 6];
    let _ = appine_hal::fill_random(&mut m);
    // 单播、本地管理的地址（eth_random_addr）
    m[0] = (m[0] & 0xfe) | 0x02;
    m
}

fn valid_mac(m: &[u8]) -> bool {
    m.len() == 6 && m[0] & 1 == 0 && m.iter().any(|&b| b != 0)
}

/// 接口名合不合法（`dev_valid_name`）。
fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() < IFNAMSIZ
        && name != b"."
        && name != b".."
        && !name
            .iter()
            .any(|&c| c == b'/' || c == b':' || c.is_ascii_whitespace())
}

fn name_in_use(ns: &NetNs, d: &Devices, name: &[u8]) -> bool {
    d.by_name(name).is_some() || base(ns).iter().any(|i| i.name.as_bytes() == name)
}

/// 选名字（`dev_get_valid_name`）：含 `%d` 的取最小的没用过的编号，别的要合法、没有用过
/// （不然 EINVAL、EEXIST）。
fn choose_name(ns: &NetNs, d: &Devices, pattern: &[u8]) -> Result<Vec<u8>, Errno> {
    if !valid_name(pattern) {
        return Err(Errno::EINVAL);
    }
    let Some(at) = pattern.windows(2).position(|w| w == b"%d") else {
        if pattern.contains(&b'%') {
            return Err(Errno::EINVAL);
        }
        if name_in_use(ns, d, pattern) {
            return Err(Errno::EEXIST);
        }
        return Ok(pattern.to_vec());
    };
    if pattern[at + 2..].contains(&b'%') || pattern[..at].contains(&b'%') {
        return Err(Errno::EINVAL);
    }
    for n in 0..32768 {
        let mut name = pattern[..at].to_vec();
        name.extend_from_slice(n.to_string().as_bytes());
        name.extend_from_slice(&pattern[at + 2..]);
        if name.len() >= IFNAMSIZ {
            return Err(Errno::EINVAL);
        }
        if !name_in_use(ns, d, &name) {
            return Ok(name);
        }
    }
    Err(Errno::ENFILE)
}

/// 分配接口编号（`dev_new_index`）：开机时的命名空间里从宿主最大的编号往后。
fn alloc_index(ns: &NetNs, d: &mut Devices) -> u32 {
    let host = base(ns);
    if ns.host_net.is_none() {
        let top = host.iter().map(|i| i.index).max().unwrap_or(0);
        d.ifindex = d.ifindex.max(top);
    }
    loop {
        d.ifindex = d.ifindex.wrapping_add(1).max(1);
        let i = d.ifindex;
        if d.by_index(i).is_none() && !host.iter().any(|h| h.index == i) {
            return i;
        }
    }
}

fn index_in_use(ns: &NetNs, d: &Devices, index: u32) -> bool {
    d.by_index(index).is_some() || base(ns).iter().any(|h| h.index == index)
}

// ---- 载波和状态 ----

/// 设备有没有载波（`netif_carrier_ok`）。`devs` 是它所在命名空间的设备表（找 bridge 的端口）。
fn carrier(me: &DevRef, devs: &Devices) -> bool {
    let d = me.borrow();
    match d.kind {
        Kind::Dummy => true,
        Kind::Veth => d.up && d.peer.upgrade().is_some_and(|p| p.borrow().up),
        Kind::Bridge if !d.selected => true,
        Kind::Bridge => {
            d.up && devs
                .ports(me)
                .iter()
                .any(|p| p.borrow().up && carrier(p, devs))
        }
    }
}

/// 端口的状态（`BR_STATE_*`）：bridge 和端口都启用、端口有载波时是转发（STP 关着），别的
/// 是 disabled。
fn port_state(port: &DevRef, devs: &Devices) -> Option<u8> {
    let br = port.borrow().master.upgrade()?;
    let on = br.borrow().up && port.borrow().up && carrier(port, devs);
    Some(if on { 3 } else { 0 })
}

/// 设备的快照（netlink 的回答、procfs、sysfs 用）。
#[derive(Debug, Clone)]
pub(crate) struct View {
    pub index: u32,
    pub name: Vec<u8>,
    pub kind: Kind,
    /// 报告的标志（`dev_get_flags`：含 RUNNING、LOWER_UP）。
    pub flags: u32,
    pub carrier: bool,
    pub operstate: u8,
    pub mtu: u32,
    pub mac: [u8; 6],
    pub txqlen: u32,
    pub group: u32,
    pub alias: Option<Vec<u8>>,
    /// `IFLA_LINK`：veth 的对端的编号（不在同一个命名空间里时另有它的命名空间）。
    pub link: Option<(u32, Option<Rc<NetNs>>)>,
    pub master: Option<u32>,
    pub port_state: Option<u8>,
    pub carrier_changes: u32,
    pub v4: Vec<Addr4>,
    pub v6: Vec<Addr6>,
    /// bridge 的编号（`bridge_id`）用的 MAC。
    pub bridge_mac: [u8; 6],
    /// 启用过（qdisc 是 noqueue）。
    pub ever_up: bool,
}

impl View {
    /// 按 `ip`、procfs 的用法看成一个接口。
    pub fn interface(&self) -> Interface {
        let mut addrs: Vec<(i32, Vec<u8>, u8)> = self
            .v4
            .iter()
            .map(|a| (AF_INET, a.local.to_vec(), a.prefix))
            .collect();
        addrs.extend(
            self.v6
                .iter()
                .map(|a| (AF_INET6, a.addr.to_vec(), a.prefix)),
        );
        Interface {
            name: String::from_utf8_lossy(&self.name).into_owned(),
            index: self.index,
            flags: self.flags,
            mtu: self.mtu,
            mac: Some(self.mac),
            addrs,
        }
    }
}

fn view(me: &DevRef, devs: &Devices, my_ns: &NetNs) -> View {
    let c = carrier(me, devs);
    let port = port_state(me, devs);
    let d = me.borrow();
    let link = match d.kind {
        Kind::Veth => Some(match d.peer.upgrade() {
            Some(p) => {
                let p = p.borrow();
                let other =
                    p.ns.upgrade()
                        .filter(|n| !std::ptr::eq(Rc::as_ptr(n), my_ns));
                (p.index, other)
            }
            None => (0, None),
        }),
        _ => None,
    };
    let iflink = link.as_ref().map_or(d.index, |l| l.0);
    let operstate = if !d.up {
        IF_OPER_DOWN
    } else if !c {
        if iflink != d.index {
            IF_OPER_LOWERLAYERDOWN
        } else {
            IF_OPER_DOWN
        }
    } else if d.kind == Kind::Dummy || (d.kind == Kind::Bridge && !d.selected) {
        IF_OPER_UNKNOWN
    } else {
        IF_OPER_UP
    };
    let mut flags = d.flags;
    if d.up {
        flags |= IFF_UP;
        if matches!(operstate, IF_OPER_UP | IF_OPER_UNKNOWN) {
            flags |= IFF_RUNNING;
        }
        if c {
            flags |= IFF_LOWER_UP;
        }
    }
    View {
        index: d.index,
        name: d.name.clone(),
        kind: d.kind,
        flags,
        carrier: c,
        operstate,
        mtu: d.mtu,
        mac: d.mac,
        txqlen: d.txqlen,
        group: d.group,
        alias: d.alias.clone(),
        link,
        master: d.master.upgrade().map(|m| m.borrow().index),
        port_state: port,
        carrier_changes: d.carrier_changes,
        v4: d.v4.clone(),
        v6: d.v6.clone(),
        bridge_mac: d.mac,
        ever_up: d.ever_up,
    }
}

/// 命名空间 `ns` 里的虚拟设备，按编号。
pub(crate) fn views(ns: &NetNs) -> Vec<View> {
    let d = devs(ns);
    let mut v: Vec<View> = d.list.iter().map(|x| view(x, &d, ns)).collect();
    v.sort_by_key(|x| x.index);
    v
}

/// 命名空间 `ns` 里虚拟设备上的 IPv4 地址和前缀长度（地址映射用，[`crate::netns`]）。
pub(crate) fn local_v4(ns: &NetNs) -> Vec<([u8; 4], u8)> {
    let d = devs(ns);
    let mut v = Vec::new();
    for x in &d.list {
        v.extend(x.borrow().v4.iter().map(|a| (a.local, a.prefix)));
    }
    v
}

/// 命名空间 `ns` 里经由虚拟设备到 IPv4 地址 `ip` 的路由（主表里的最长前缀，一样长的取度量
/// 小的）。
pub(crate) fn route_v4(ns: &NetNs, ip: [u8; 4]) -> Option<Entry> {
    entries(ns)
        .into_iter()
        .filter(|e| {
            e.family == AF_INET
                && e.table == RT_TABLE_MAIN
                && crate::route::network(&ip, e.prefix) == e.dst
        })
        .min_by_key(|e| (std::cmp::Reverse(e.prefix), e.metric))
}

/// 命名空间 `ns` 里经由虚拟设备有没有 IPv6 的路由到 `ip`。
pub(crate) fn routes_v6(ns: &NetNs, ip: &[u8; 16]) -> bool {
    entries(ns).iter().any(|e| {
        e.family == AF_INET6
            && e.table == RT_TABLE_MAIN
            && crate::route::network(ip, e.prefix) == e.dst
    })
}

/// 命名空间 `ns` 里编号为 `index` 的虚拟设备上的第一个 primary IPv4 地址。
pub(crate) fn first_v4(ns: &NetNs, index: u32) -> Option<[u8; 4]> {
    let d = devs(ns);
    let x = d.by_index(index)?;
    let x = x.borrow();
    x.v4.iter()
        .find(|a| a.flags & IFA_F_SECONDARY == 0)
        .map(|a| a.local)
}

/// 命名空间 `ns` 里所有的接口（宿主的或者 lo，加上虚拟设备），按编号。
pub(crate) fn interfaces(ns: &NetNs) -> Vec<Interface> {
    let mut all = base(ns);
    all.extend(views(ns).iter().map(View::interface));
    all.sort_by_key(|i| i.index);
    all
}

/// 虚拟设备上的路由（[`Entry`]，`host` 是 false），按加入的先后。
pub(crate) fn entries(ns: &NetNs) -> Vec<Entry> {
    let d = devs(ns);
    d.routes
        .iter()
        .filter_map(|r| {
            let dev = r.dev.upgrade()?;
            let linkdown = r.ty != RTN_LOCAL && !carrier(&dev, &d);
            let x = dev.borrow();
            Some(Entry {
                family: r.family,
                table: r.table,
                ty: r.ty,
                proto: r.proto,
                scope: r.scope,
                dst: r.dst.clone(),
                prefix: r.prefix,
                prefsrc: r.prefsrc.clone(),
                gateway: r.gateway.clone(),
                iface: String::from_utf8_lossy(&x.name).into_owned(),
                index: x.index,
                metric: r.metric,
                flags: if linkdown { RTNH_F_LINKDOWN } else { 0 },
                host: false,
            })
        })
        .collect()
}

/// 命名空间 `ns` 的两张表（宿主的或者 lo 的，加上虚拟设备上的）。`host_routes` 是宿主
/// 主表里的路由（开机时的命名空间，[`crate::route::host_routes`]）。
pub(crate) fn tables(ns: &NetNs, host_routes: Option<Vec<appine_hal::net::Route>>) -> Vec<Entry> {
    let base_ifaces = base(ns);
    let host = if ns.host_net.is_none() {
        crate::route::tables(&base_ifaces, host_routes)
    } else {
        // 新的命名空间：lo 的本地路由（没有宿主的主表）
        crate::route::tables(&base_ifaces, Some(Vec::new()))
            .into_iter()
            .map(|mut e| {
                e.host = false;
                e
            })
            .collect()
    };
    crate::route::merge(host, entries(ns))
}

// ---- 命名空间的编号 ----

/// `ns` 给 `peer` 的编号，没有就分配一个（`peernet2id_alloc`：从 0 起最小的没用的）。
pub(crate) fn nsid_alloc(ns: &NetNs, peer: &Rc<NetNs>) -> i32 {
    let mut d = ns.devs.borrow_mut();
    d.nsids.retain(|(w, _)| w.strong_count() > 0);
    if let Some((_, id)) = d
        .nsids
        .iter()
        .find(|(w, _)| Weak::ptr_eq(w, &Rc::downgrade(peer)))
    {
        return *id;
    }
    let id = (0..)
        .find(|i| !d.nsids.iter().any(|(_, x)| x == i))
        .unwrap();
    d.nsids.push((Rc::downgrade(peer), id));
    id
}

/// `ns` 给 `peer` 的编号，没有分配时是 None。
pub(crate) fn nsid(ns: &NetNs, peer: &Rc<NetNs>) -> Option<i32> {
    let d = ns.devs.borrow();
    d.nsids
        .iter()
        .find(|(w, _)| w.strong_count() > 0 && Weak::ptr_eq(w, &Rc::downgrade(peer)))
        .map(|(_, id)| *id)
}

// ---- 路由的增删（按地址加的） ----

fn add_route(d: &mut Devices, r: Rt) {
    if !d.routes.iter().any(|x| x.same(&r)) {
        d.routes.push(r);
    }
}

/// 一条按地址加的路由（`fib_magic`、addrconf 的）。
#[allow(clippy::too_many_arguments)]
fn kernel_route(
    family: i32,
    table: u8,
    ty: u8,
    scope: u8,
    dst: Vec<u8>,
    prefix: u8,
    prefsrc: Option<Vec<u8>>,
    dev: &DevRef,
    metric: u32,
    owner: Vec<u8>,
) -> Rt {
    Rt {
        family,
        table,
        ty,
        proto: RTPROT_KERNEL,
        scope,
        tos: 0,
        dst,
        prefix,
        prefsrc,
        gateway: None,
        dev: Rc::downgrade(dev),
        metric,
        owner: Some(owner),
    }
}

/// IPv4 地址 `a` 的路由（`fib_add_ifaddr`）：本地路由总加，显式的广播地址、网段和网段的
/// 广播只在接口启用时加；secondary 的首选源地址是同一网段的 primary。
fn add_ifaddr4(d: &mut Devices, dev: &DevRef, a: &Addr4) {
    let (up, prim) = {
        let x = dev.borrow();
        let prim = if a.flags & IFA_F_SECONDARY != 0 {
            x.v4.iter()
                .find(|p| {
                    p.flags & IFA_F_SECONDARY == 0 && p.prefix == a.prefix && p.contains(a.local)
                })
                .map_or(a.local, |p| p.local)
        } else {
            a.local
        };
        (x.up, prim)
    };
    let src = Some(prim.to_vec());
    let own = a.local.to_vec();
    let host = |ty, scope, dst: [u8; 4]| {
        kernel_route(
            AF_INET,
            RT_TABLE_LOCAL,
            ty,
            scope,
            dst.to_vec(),
            32,
            src.clone(),
            dev,
            0,
            own.clone(),
        )
    };
    add_route(d, host(RTN_LOCAL, RT_SCOPE_HOST, a.local));
    if !up {
        return;
    }
    if a.broadcast != [0; 4] && a.broadcast != [0xff; 4] {
        add_route(d, host(RTN_BROADCAST, RT_SCOPE_LINK, a.broadcast));
    }
    let prefix = a.subnet();
    if prefix != 0
        && a.flags & IFA_F_SECONDARY == 0
        && (prefix != u32::from_be_bytes(a.local) || a.prefix < 32)
    {
        if a.flags & IFA_F_NOPREFIXROUTE == 0 {
            add_route(
                d,
                kernel_route(
                    AF_INET,
                    RT_TABLE_MAIN,
                    RTN_UNICAST,
                    RT_SCOPE_LINK,
                    prefix.to_be_bytes().to_vec(),
                    a.prefix,
                    src.clone(),
                    dev,
                    a.metric,
                    own.clone(),
                ),
            );
        }
        if a.prefix < 31 {
            add_route(
                d,
                host(
                    RTN_BROADCAST,
                    RT_SCOPE_LINK,
                    (prefix | !a.mask()).to_be_bytes(),
                ),
            );
        }
    }
}

/// IPv6 地址的路由：网段（`IFA_F_NOPREFIXROUTE` 时没有，/128 也没有）和本地路由（接口启用
/// 时）。
fn add_ifaddr6(d: &mut Devices, dev: &DevRef, a: &Addr6) {
    let up = dev.borrow().up;
    let own = a.addr.to_vec();
    if a.flags & IFA_F_NOPREFIXROUTE == 0 && a.prefix < 128 {
        add_route(
            d,
            kernel_route(
                AF_INET6,
                RT_TABLE_MAIN,
                RTN_UNICAST,
                RT_SCOPE_UNIVERSE,
                crate::route::network(&a.addr, a.prefix),
                a.prefix,
                None,
                dev,
                if a.metric != 0 {
                    a.metric
                } else {
                    IP6_PRIO_ADDRCONF
                },
                own.clone(),
            ),
        );
    }
    if up {
        add_route(
            d,
            kernel_route(
                AF_INET6,
                RT_TABLE_LOCAL,
                RTN_LOCAL,
                RT_SCOPE_UNIVERSE,
                own.clone(),
                128,
                None,
                dev,
                0,
                own,
            ),
        );
    }
}

/// 去掉经由 `dev` 的路由：`keep_local` 时留下 IPv4 的本地路由（接口停用时）。
fn drop_routes(d: &mut Devices, dev: &DevRef, family: Option<i32>, keep_local: bool) {
    d.routes.retain(|r| {
        !(r.on(dev)
            && family.is_none_or(|f| r.family == f)
            && !(keep_local && r.family == AF_INET && r.ty == RTN_LOCAL))
    });
}

/// IPv4 地址变了之后重新整理按地址加的路由：去掉 `gone` 的（含首选源地址是它的，
/// `fib_sync_down_addr`），再按剩下的地址补上（`fib_del_ifaddr` 留下共用的）。
fn redo_ifaddr4(d: &mut Devices, dev: &DevRef, gone: &[[u8; 4]]) {
    d.routes.retain(|r| {
        !(r.family == AF_INET
            && gone.iter().any(|g| {
                r.owner.as_deref() == Some(&g[..]) || r.prefsrc.as_deref() == Some(&g[..])
            }))
    });
    let addrs = dev.borrow().v4.clone();
    for a in &addrs {
        add_ifaddr4(d, dev, a);
    }
}

/// 接口的 IPv6 关着没有：自己的 `disable_ipv6` 或者 MTU 小于 1280。
fn v6_disabled(x: &Dev) -> bool {
    x.conf6.get(true, "disable_ipv6") != 0 || i64::from(x.mtu) < crate::netconf::IPV6_MIN_MTU
}

/// 按 MAC 生成的链路本地地址（EUI-64）。
fn link_local(mac: [u8; 6]) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0] = 0xfe;
    a[1] = 0x80;
    a[8] = mac[0] ^ 0x02;
    a[9] = mac[1];
    a[10] = mac[2];
    a[11] = 0xff;
    a[12] = 0xfe;
    a[13] = mac[3];
    a[14] = mac[4];
    a[15] = mac[5];
    a
}

/// 配置接口的 IPv6（addrconf 的 `addrconf_dev_config`）：ff00::/8 和 fe80::/64 的路由，
/// 生成链路本地地址（`addr_gen_mode` 为 1 时不生成），已有的地址补上路由。
fn v6_config(d: &mut Devices, dev: &DevRef) {
    let (mac, mode) = {
        let mut x = dev.borrow_mut();
        x.v6_ready = true;
        (x.mac, x.conf6.get(true, "addr_gen_mode"))
    };
    let r = |table, ty, dst: Vec<u8>, prefix| {
        kernel_route(
            AF_INET6,
            table,
            ty,
            RT_SCOPE_UNIVERSE,
            dst,
            prefix,
            None,
            dev,
            IP6_PRIO_ADDRCONF,
            Vec::new(),
        )
    };
    let mut mc = vec![0u8; 16];
    mc[0] = 0xff;
    add_route(d, r(RT_TABLE_LOCAL, RTN_MULTICAST, mc, 8));
    let mut ll = vec![0u8; 16];
    ll[..2].copy_from_slice(&[0xfe, 0x80]);
    add_route(d, r(RT_TABLE_MAIN, RTN_UNICAST, ll, 64));
    if mode != 1 {
        let a = Addr6 {
            addr: link_local(mac),
            prefix: 64,
            flags: IFA_F_PERMANENT,
            proto: IFAPROT_KERNEL_LL,
            metric: 0,
        };
        let mut x = dev.borrow_mut();
        if !x.v6.iter().any(|b| b.addr == a.addr) {
            x.v6.push(a);
        }
    }
    let addrs = dev.borrow().v6.clone();
    for a in &addrs {
        add_ifaddr6(d, dev, a);
    }
}

/// 去掉接口的 IPv6（停用、关掉 IPv6、移走时，`addrconf_ifdown`）：地址和路由都去掉。
fn v6_down(d: &mut Devices, dev: &DevRef) {
    {
        let mut x = dev.borrow_mut();
        x.v6.clear();
        x.v6_ready = false;
    }
    drop_routes(d, dev, Some(AF_INET6), false);
}

/// 状态变了之后推进：数载波的变化，启用并有载波、IPv6 没关的接口配置 IPv6。`first` 里的
/// 先处理（Linux 按事件的先后：改的那个、它的对端、它挂的 bridge）。
fn settle(ns: &NetNs, first: &[DevRef]) {
    let mut d = devs(ns);
    let mut order: Vec<DevRef> = Vec::new();
    for x in first.iter().chain(d.list.clone().iter()) {
        let mine = x
            .borrow()
            .ns
            .upgrade()
            .is_some_and(|n| std::ptr::eq(Rc::as_ptr(&n), ns));
        if mine && !order.iter().any(|o| Rc::ptr_eq(o, x)) {
            order.push(x.clone());
        }
    }
    for x in &order {
        let c = carrier(x, &d);
        let ready = {
            let mut y = x.borrow_mut();
            if c != y.carrier_was {
                y.carrier_was = c;
                y.carrier_changes += 1;
            }
            y.up && c && !y.v6_ready && !v6_disabled(&y)
        };
        if ready {
            v6_config(&mut d, x);
        }
    }
}

/// `dev` 和与它相连的：对端、挂的 bridge（[`settle`] 的顺序）。
fn related(dev: &DevRef) -> Vec<DevRef> {
    let x = dev.borrow();
    let mut v = vec![dev.clone()];
    v.extend(x.peer.upgrade());
    v.extend(x.master.upgrade());
    v
}

/// 推进 `dev` 所在和它的对端所在的命名空间。
fn settle_around(dev: &DevRef) {
    let first = related(dev);
    let mut nss: Vec<Rc<NetNs>> = Vec::new();
    for x in &first {
        if let Some(n) = x.borrow().ns.upgrade()
            && !nss.iter().any(|m| Rc::ptr_eq(m, &n))
        {
            nss.push(n);
        }
    }
    for n in &nss {
        settle(n, &first);
    }
}

// ---- 设备的修改 ----

/// 启用、停用（`__dev_open`、`__dev_close`）。
fn set_up(ns: &NetNs, dev: &DevRef, up: bool) {
    if dev.borrow().up == up {
        return;
    }
    {
        let mut x = dev.borrow_mut();
        x.up = up;
        x.ever_up |= up;
    }
    let mut d = devs(ns);
    if up {
        // NETDEV_UP：按地址重新加路由
        let addrs = dev.borrow().v4.clone();
        for a in &addrs {
            add_ifaddr4(&mut d, dev, a);
        }
    } else {
        drop_routes(&mut d, dev, Some(AF_INET), true);
        let keep = {
            let x = dev.borrow();
            x.conf6.get(true, "keep_addr_on_down") > 0
                || d.conf.all6.get(true, "keep_addr_on_down") > 0
        };
        if keep {
            dev.borrow_mut().v6_ready = false;
            drop_routes(&mut d, dev, Some(AF_INET6), false);
        } else {
            v6_down(&mut d, dev);
        }
    }
}

/// 改标志（`dev_change_flags`）：`flags` 是合起来之后要的标志。
fn change_flags(ns: &NetNs, dev: &DevRef, flags: u32) {
    {
        let mut x = dev.borrow_mut();
        x.flags = (flags & CHANGEABLE) | (x.flags & !CHANGEABLE);
    }
    set_up(ns, dev, flags & IFF_UP != 0);
}

/// 报告的标志（`dev_get_flags`）。
fn reported_flags(ns: &NetNs, dev: &DevRef) -> u32 {
    let d = devs(ns);
    view(dev, &d, ns).flags
}

/// MTU 的检查（`dev_validate_mtu`）。
fn validate_mtu(kind: Kind, mtu: u32) -> Result<(), Fail> {
    let (min, max) = kind.mtu_range();
    if (mtu as i32) < 0 || mtu < min {
        return Err(fail(Errno::EINVAL, "mtu less than device minimum"));
    }
    if max > 0 && mtu > max {
        return Err(fail(Errno::EINVAL, "mtu greater than device maximum"));
    }
    Ok(())
}

/// 改 MTU（`dev_set_mtu_ext`）：bridge 记下是用户设的，端口变了 bridge 跟着调整；IPv6 的
/// `mtu` 跟着接口的。
fn set_mtu(ns: &NetNs, dev: &DevRef, mtu: u32) -> Result<(), Fail> {
    let kind = dev.borrow().kind;
    validate_mtu(kind, mtu)?;
    {
        let mut x = dev.borrow_mut();
        x.mtu = mtu;
        x.conf6.mtu6 = None;
        if kind == Kind::Bridge {
            x.mtu_set = true;
        }
    }
    let br = dev.borrow().master.upgrade();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
    // MTU 小于 1280 时 IPv6 关掉（NETDEV_CHANGEMTU）
    if i64::from(mtu) < crate::netconf::IPV6_MIN_MTU {
        let mut d = devs(ns);
        v6_down(&mut d, dev);
    }
    Ok(())
}

/// bridge 的 MTU 和 MAC 跟着端口（`br_mtu_auto_adjust`、`br_stp_recalculate_bridge_id`）：
/// 没有由用户设过时，MTU 是端口里最小的（没有端口是 1500），MAC 是端口里最小的（没有端口
/// 时是全 0）。
fn bridge_adjust(ns: &NetNs, br: &DevRef) {
    let ports = devs(ns).ports(br);
    let mut b = br.borrow_mut();
    b.selected = true;
    if !b.mtu_set {
        b.mtu = ports
            .iter()
            .map(|p| p.borrow().mtu)
            .min()
            .unwrap_or(ETH_DATA_LEN);
    }
    if !b.mac_set {
        b.mac = ports.iter().map(|p| p.borrow().mac).min().unwrap_or([0; 6]);
    }
}

/// 改 MAC（`dev_set_mac_address_user`）：要是合法的单播地址（EADDRNOTAVAIL）。
fn set_mac(ns: &NetNs, dev: &DevRef, mac: &[u8]) -> Result<(), Fail> {
    if mac.len() < 6 {
        return Err(Errno::EINVAL.into());
    }
    if !valid_mac(&mac[..6]) {
        return Err(Errno::EADDRNOTAVAIL.into());
    }
    {
        let mut x = dev.borrow_mut();
        x.mac.copy_from_slice(&mac[..6]);
        x.mac_set = true;
        // bridge 改 MAC 时按端口重算状态（br_set_mac_address → br_stp_change_bridge_id）
        if x.kind == Kind::Bridge {
            x.selected = true;
        }
    }
    let br = dev.borrow().master.upgrade();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
    Ok(())
}

/// 改名（`dev_change_name`）：6.1 里启用着的不能改（EBUSY）。
fn rename(ns: &NetNs, dev: &DevRef, name: &[u8]) -> Result<(), Fail> {
    if dev.borrow().name == name {
        return Ok(());
    }
    if dev.borrow().up {
        return Err(Errno::EBUSY.into());
    }
    let new = {
        let d = devs(ns);
        choose_name(ns, &d, name)?
    };
    let mut x = dev.borrow_mut();
    // 地址的标签跟着改（`inetdev_changename`）
    let old = std::mem::replace(&mut x.name, new.clone());
    for a in &mut x.v4 {
        if a.label.starts_with(&old) {
            let mut l = new.clone();
            l.extend_from_slice(&a.label[old.len()..]);
            a.label = l;
        }
    }
    Ok(())
}

/// 挂到 bridge 上、摘下（`do_set_master`、`br_add_if`、`br_del_if`）。`index` 是 0 时摘下。
fn set_master(ns: &NetNs, dev: &DevRef, index: u32) -> Result<(), Fail> {
    let cur = dev.borrow().master.upgrade();
    if let Some(cur) = &cur {
        if cur.borrow().index == index {
            return Ok(());
        }
        dev.borrow_mut().master = Weak::new();
        bridge_adjust(ns, cur);
    }
    if index == 0 {
        return Ok(());
    }
    let br = devs(ns).by_index(index);
    let Some(br) = br else {
        // 宿主的接口、lo 不能当 master（没有 ndo_add_slave），不存在的是 EINVAL
        return Err(if index_in_use(ns, &devs(ns), index) {
            Errno::EOPNOTSUPP
        } else {
            Errno::EINVAL
        }
        .into());
    };
    if br.borrow().kind != Kind::Bridge {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if dev.borrow().kind == Kind::Bridge {
        return Err(fail(Errno::ELOOP, "Can not enslave a bridge to a bridge"));
    }
    dev.borrow_mut().master = Rc::downgrade(&br);
    bridge_adjust(ns, &br);
    Ok(())
}

/// 从命名空间的表里拿掉（删除、移走时）：摘下端口、去掉经由它的路由。
fn unlink(ns: &NetNs, dev: &DevRef) {
    let ports = {
        let mut d = devs(ns);
        d.list.retain(|x| !Rc::ptr_eq(x, dev));
        d.routes.retain(|r| !r.on(dev) && r.dev.strong_count() > 0);
        d.ports(dev)
    };
    for p in &ports {
        p.borrow_mut().master = Weak::new();
    }
    let br = dev.borrow().master.upgrade();
    dev.borrow_mut().master = Weak::new();
    if let Some(br) = br {
        bridge_adjust(ns, &br);
    }
}

/// 删除（`rtnl_dellink`）：veth 连同对端。
fn delete(ns: &NetNs, dev: &DevRef) {
    let peer = dev.borrow().peer.upgrade();
    unlink(ns, dev);
    dev.borrow_mut().ns = Weak::new();
    let mut touched = Vec::new();
    if let Some(p) = peer {
        if let Some(pns) = p.borrow().ns.upgrade() {
            touched.push(pns);
        }
        if let Some(pns) = touched.last() {
            unlink(pns, &p);
        }
        p.borrow_mut().ns = Weak::new();
    }
    for n in &touched {
        settle(n, &[]);
    }
    settle(ns, &[]);
}

/// 移到命名空间 `to`（`__dev_change_net_namespace`）：先停用、摘下，地址和路由都去掉，
/// sysctl 按那边的 `default` 重来；编号在那边被占了就重新分配。名字被占了时用 `pat`
/// 改名，没有给就是 EEXIST。
fn move_to(ns: &NetNs, dev: &DevRef, to: &Rc<NetNs>, pat: Option<&[u8]>) -> Result<(), Fail> {
    if !dev.borrow().kind.movable() {
        return Err(Errno::EINVAL.into());
    }
    if std::ptr::eq(ns, Rc::as_ptr(to)) {
        return Ok(());
    }
    let name = dev.borrow().name.clone();
    let new_name = {
        let d = devs(to);
        if name_in_use(to, &d, &name) {
            match pat {
                Some(p) => choose_name(to, &d, p)?,
                None => return Err(Errno::EEXIST.into()),
            }
        } else {
            name.clone()
        }
    };
    set_up(ns, dev, false);
    unlink(ns, dev);
    {
        let mut x = dev.borrow_mut();
        x.v4.clear();
        x.v6.clear();
        x.v6_ready = false;
        x.name = new_name;
    }
    {
        let mut d = devs(to);
        let index = dev.borrow().index;
        let index = if index_in_use(to, &d, index) {
            alloc_index(to, &mut d)
        } else {
            index
        };
        let (c4, c6) = (d.conf.dflt4.inherit(), d.conf.dflt6.inherit());
        {
            let mut x = dev.borrow_mut();
            x.index = index;
            x.ns = Rc::downgrade(to);
            x.conf4 = c4;
            x.conf6 = c6;
        }
        d.list.push(dev.clone());
    }
    settle_around(dev);
    settle(ns, &[]);
    Ok(())
}

// ---- netlink 的修改请求 ----

/// 找 `IFLA_NET_NS_PID`、`IFLA_NET_NS_FD` 指的网络命名空间（进程没了是 ESRCH，fd 不是
/// 网络命名空间是 EINVAL）。
pub(crate) type Resolve<'a> = &'a dyn Fn(u16, u32) -> Result<Rc<NetNs>, Errno>;

fn get<'a>(attrs: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    attrs.iter().find(|(t, _)| *t == ty).map(|(_, d)| *d)
}

fn get_u32(attrs: &[(u16, &[u8])], ty: u16) -> Option<u32> {
    get(attrs, ty)
        .and_then(|d| d.get(..4))
        .map(|d| u32::from_le_bytes(d.try_into().unwrap()))
}

/// `IFLA_IFNAME`（去掉结尾的 NUL）：超过 15 字节时是策略检查的 ERANGE。
fn ifname<'a>(attrs: &[(u16, &'a [u8])]) -> Result<Option<&'a [u8]>, Fail> {
    let Some(d) = get(attrs, IFLA_IFNAME) else {
        return Ok(None);
    };
    let n = d.split(|&c| c == 0).next().unwrap_or_default();
    if n.len() >= IFNAMSIZ {
        return Err(fail(Errno::ERANGE, "Attribute failed policy validation"));
    }
    Ok(Some(n))
}

/// `IFLA_LINKINFO` 里的种类和参数。
#[derive(Default)]
struct LinkInfo<'a> {
    kind: Option<&'a [u8]>,
    data: Option<&'a [u8]>,
    slave_data: Option<&'a [u8]>,
}

fn linkinfo<'a>(attrs: &[(u16, &'a [u8])]) -> LinkInfo<'a> {
    let Some(li) = get(attrs, IFLA_LINKINFO) else {
        return LinkInfo::default();
    };
    let inner = crate::route::attrs(li);
    LinkInfo {
        kind: get(&inner, IFLA_INFO_KIND).map(|k| k.split(|&c| c == 0).next().unwrap_or_default()),
        data: get(&inner, IFLA_INFO_DATA),
        slave_data: get(&inner, IFLA_INFO_SLAVE_DATA),
    }
}

/// 请求里指定的命名空间（`rtnl_link_get_net_capable`）：要在它的属主里有 `CAP_NET_ADMIN`。
fn target_ns(
    attrs: &[(u16, &[u8])],
    cred: &Cred,
    resolve: Resolve<'_>,
) -> Result<Option<Rc<NetNs>>, Fail> {
    let ns = if let Some(pid) = get_u32(attrs, IFLA_NET_NS_PID) {
        resolve(IFLA_NET_NS_PID, pid)?
    } else if let Some(fd) = get_u32(attrs, IFLA_NET_NS_FD) {
        resolve(IFLA_NET_NS_FD, fd)?
    } else if get(attrs, IFLA_TARGET_NETNSID).is_some() {
        // 按编号指定命名空间没有做
        return Err(Errno::EOPNOTSUPP.into());
    } else {
        return Ok(None);
    };
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Err(Errno::EPERM.into());
    }
    Ok(Some(ns))
}

/// 修改接口、地址、路由的 netlink 请求（整条消息）：`RTM_NEWLINK`、`RTM_SETLINK`、
/// `RTM_DELLINK`、`RTM_NEWADDR`、`RTM_DELADDR`、`RTM_NEWROUTE`、`RTM_DELROUTE`。None：不是
/// 这几种。要在命名空间的属主里有 `CAP_NET_ADMIN`（EPERM，`netlink_net_capable`）。
pub(crate) fn request(
    ns: &Rc<NetNs>,
    cred: &Cred,
    req: &[u8],
    resolve: Resolve<'_>,
) -> Option<Result<(), Fail>> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if !matches!(
        ty,
        RTM_NEWLINK
            | RTM_DELLINK
            | RTM_SETLINK
            | RTM_NEWADDR
            | RTM_DELADDR
            | RTM_NEWROUTE
            | RTM_DELROUTE
    ) {
        return None;
    }
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Err(Errno::EPERM.into()));
    }
    Some(match ty {
        RTM_NEWLINK | RTM_SETLINK => link_request(ns, cred, req, resolve),
        RTM_DELLINK => del_request(ns, req),
        RTM_NEWADDR | RTM_DELADDR => addr_request(ns, cred, req),
        _ => route_request(ns, req),
    })
}

/// ifinfomsg 的编号、标志、要改的标志。
fn ifinfo(m: &[u8]) -> (i32, u32, u32) {
    (
        i32::from_le_bytes(m[4..8].try_into().unwrap()),
        u32::from_le_bytes(m[8..12].try_into().unwrap()),
        u32::from_le_bytes(m[12..16].try_into().unwrap()),
    )
}

/// 合起来的标志（`rtnl_dev_combine_flags`）：`change` 是 0 时整个 `flags` 都算要的。
fn combine(cur: u32, flags: u32, change: u32) -> u32 {
    if change != 0 {
        (flags & change) | (cur & !change)
    } else {
        flags
    }
}

fn link_request(ns: &Rc<NetNs>, cred: &Cred, req: &[u8], resolve: Resolve<'_>) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let m = req.get(16..32).ok_or(Errno::EINVAL)?;
    let (index, ifi_flags, change) = ifinfo(m);
    let attrs = crate::route::attrs(&req[32..]);
    let name = ifname(&attrs)?;
    let newlink = ty == RTM_NEWLINK;
    if newlink && index < 0 {
        return Err(Errno::EINVAL.into());
    }
    if !newlink && index <= 0 && name.is_none() {
        return Err(Errno::EINVAL.into());
    }
    let virt = {
        let d = devs(ns);
        if index > 0 {
            d.by_index(index as u32)
        } else {
            name.and_then(|n| d.by_name(n))
        }
    };
    if let Some(dev) = virt {
        let info = linkinfo(&attrs);
        if newlink {
            if nlflags & NLM_F_EXCL != 0 {
                return Err(Errno::EEXIST.into());
            }
            if nlflags & NLM_F_REPLACE != 0 {
                return Err(Errno::EOPNOTSUPP.into());
            }
            // 种类的参数（changelink）：只有 bridge 有，接受但不起作用
            if info.data.is_some() {
                let kind = info.kind.and_then(Kind::parse);
                if kind != Some(dev.borrow().kind) || kind != Some(Kind::Bridge) {
                    return Err(Errno::EOPNOTSUPP.into());
                }
            }
            if info.slave_data.is_some() && dev.borrow().master.strong_count() == 0 {
                return Err(Errno::EOPNOTSUPP.into());
            }
        }
        let r = setlink(
            ns, cred, &dev, index, ifi_flags, change, &attrs, name, resolve,
        );
        settle_around(&dev);
        return r;
    }
    let base_ifs = base(ns);
    let hit = base_ifs.iter().find(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name == Some(i.name.as_bytes())
        }
    });
    if let Some(i) = hit {
        if newlink && nlflags & NLM_F_EXCL != 0 {
            return Err(Errno::EEXIST.into());
        }
        return if ns.host_net.is_some() {
            lo_link(ns, cred, ifi_flags, change, &attrs, resolve)
        } else {
            let _ = i;
            match crate::net::configure_host_link(&base_ifs, true, req) {
                Some(0) | None => Ok(()),
                Some(e) => Err(Errno(e).into()),
            }
        };
    }
    if !newlink || nlflags & NLM_F_CREATE == 0 {
        return Err(Errno::ENODEV.into());
    }
    create(ns, cred, index, ifi_flags, change, &attrs, resolve)
}

/// 新的命名空间里的 lo：只能启用、停用，改 MTU；挂到 bridge 上是 EINVAL（回环接口），
/// 移到别的命名空间是 EINVAL（`NETIF_F_NETNS_LOCAL`），改名不支持。
fn lo_link(
    ns: &Rc<NetNs>,
    cred: &Cred,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    // 移到哪个命名空间（含它所在的这个）都是 EINVAL：Linux 先查 NETIF_F_NETNS_LOCAL
    if target_ns(attrs, cred, resolve)?.is_some() {
        return Err(Errno::EINVAL.into());
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        if m < ETH_MIN_MTU {
            return Err(fail(Errno::EINVAL, "mtu less than device minimum"));
        }
        ns.lo.borrow_mut().mtu = Some(m);
    }
    // 按编号找到 lo 又给了别的名字是改名，不支持
    if ifname(attrs)?.is_some_and(|n| n != b"lo") {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if ifi_flags != 0 || change != 0 {
        let cur = crate::netns::lo_interface(ns).flags;
        crate::netns::set_up(
            &mut ns.lo.borrow_mut(),
            combine(cur, ifi_flags, change) & IFF_UP != 0,
        );
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER)
        && m != 0
    {
        let target = devs(ns).by_index(m);
        return Err(match target {
            Some(t) if t.borrow().kind != Kind::Bridge => Errno::EOPNOTSUPP,
            _ => Errno::EINVAL,
        }
        .into());
    }
    Ok(())
}

/// 改虚拟设备（`do_setlink`）：依次是移到别的命名空间、MAC、MTU、组、改名（按编号找到
/// 时）、别名、标志、master、发送队列长度；前面改了的在后面出错时也留着。
#[allow(clippy::too_many_arguments)]
fn setlink(
    ns: &Rc<NetNs>,
    cred: &Cred,
    dev: &DevRef,
    index: i32,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    name: Option<&[u8]>,
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    let mut ns = ns.clone();
    if let Some(to) = target_ns(attrs, cred, resolve)? {
        let pat = name.filter(|n| !n.is_empty());
        move_to(&ns, dev, &to, pat)?;
        ns = to;
    }
    if let Some(a) = get(attrs, IFLA_ADDRESS) {
        set_mac(&ns, dev, a)?;
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        set_mtu(&ns, dev, m)?;
    }
    if let Some(g) = get_u32(attrs, IFLA_GROUP) {
        dev.borrow_mut().group = g;
    }
    if index > 0
        && let Some(n) = name
    {
        rename(&ns, dev, n)?;
    }
    if let Some(a) = get(attrs, IFLA_IFALIAS) {
        let a = a.split(|&c| c == 0).next().unwrap_or_default();
        dev.borrow_mut().alias = (!a.is_empty()).then(|| a.to_vec());
    }
    if ifi_flags != 0 || change != 0 {
        let cur = reported_flags(&ns, dev);
        change_flags(&ns, dev, combine(cur, ifi_flags, change));
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER) {
        set_master(&ns, dev, m)?;
    }
    if get(attrs, IFLA_CARRIER).is_some() && dev.borrow().kind != Kind::Dummy {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if let Some(q) = get_u32(attrs, IFLA_TXQLEN) {
        dev.borrow_mut().txqlen = q;
    }
    Ok(())
}

/// 种类的检查（veth、bridge、dummy 的 `validate`）：MAC 要 6 字节（EINVAL）、合法
/// （EADDRNOTAVAIL）。
fn validate(kind: Kind, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    if let Some(a) = get(attrs, IFLA_ADDRESS) {
        if a.len() != 6 {
            return Err(Errno::EINVAL.into());
        }
        if !valid_mac(a) {
            return Err(Errno::EADDRNOTAVAIL.into());
        }
    }
    if let Some(m) = get_u32(attrs, IFLA_MTU) {
        validate_mtu(kind, m)?;
    }
    Ok(())
}

/// 在 `ns` 里登记一个新设备（`rtnl_create_link`、`register_netdevice`）：名字按 `pat`
/// （可以含 `%d`），编号是 `index`（0 时分配），属性里的 MTU、MAC、发送队列长度、组。
fn new_dev(
    ns: &Rc<NetNs>,
    kind: Kind,
    pat: &[u8],
    index: i32,
    attrs: &[(u16, &[u8])],
) -> Result<DevRef, Fail> {
    let mut d = devs(ns);
    let name = choose_name(ns, &d, pat)?;
    let index = if index > 0 {
        if index_in_use(ns, &d, index as u32) {
            return Err(Errno::EBUSY.into());
        }
        index as u32
    } else {
        alloc_index(ns, &mut d)
    };
    let mac = get(attrs, IFLA_ADDRESS).map(|a| <[u8; 6]>::try_from(&a[..6]).unwrap());
    let flags = match kind {
        Kind::Dummy => IFF_BROADCAST | IFF_NOARP,
        _ => IFF_BROADCAST | IFF_MULTICAST,
    };
    let dev = Rc::new(RefCell::new(Dev {
        ns: Rc::downgrade(ns),
        index,
        name,
        kind,
        up: false,
        ever_up: false,
        // 带着 MAC 建的 bridge 马上按端口算一次状态（br_dev_newlink 调
        // br_stp_change_bridge_id），没有端口所以没有载波
        selected: kind == Kind::Bridge && mac.is_some(),
        flags,
        mtu: get_u32(attrs, IFLA_MTU).unwrap_or(ETH_DATA_LEN),
        mtu_set: kind == Kind::Bridge && get(attrs, IFLA_MTU).is_some(),
        mac: mac.unwrap_or_else(random_mac),
        mac_set: mac.is_some(),
        txqlen: get_u32(attrs, IFLA_TXQLEN).unwrap_or(1000),
        group: get_u32(attrs, IFLA_GROUP).unwrap_or(0),
        alias: None,
        peer: Weak::new(),
        paired: false,
        master: Weak::new(),
        v4: Vec::new(),
        v6: Vec::new(),
        v6_ready: false,
        conf4: d.conf.dflt4.inherit(),
        conf6: d.conf.dflt6.inherit(),
        carrier_changes: 0,
        carrier_was: false,
    }));
    d.list.push(dev.clone());
    Ok(dev)
}

/// 新建（`rtnl_newlink_create`）：veth 先登记对端（对端的命名空间按 `VETH_INFO_PEER` 里的，
/// 没有给就是发请求的这个），再登记自己；然后按 ifinfomsg 改标志，最后挂到 master 上
/// （失败时删掉新建的）。
fn create(
    ns: &Rc<NetNs>,
    cred: &Cred,
    index: i32,
    ifi_flags: u32,
    change: u32,
    attrs: &[(u16, &[u8])],
    resolve: Resolve<'_>,
) -> Result<(), Fail> {
    let info = linkinfo(attrs);
    let Some(kind) = info.kind.and_then(Kind::parse) else {
        return Err(fail(Errno::EOPNOTSUPP, "Unknown device type"));
    };
    validate(kind, attrs)?;
    let dest = target_ns(attrs, cred, resolve)?.unwrap_or_else(|| ns.clone());
    let default_name: &[u8] = match kind {
        Kind::Bridge => b"bridge%d",
        Kind::Veth => b"veth%d",
        Kind::Dummy => b"dummy%d",
    };
    let pat = ifname(attrs)?.unwrap_or(default_name);
    let dev = if kind == Kind::Veth {
        let peer = info
            .data
            .map(crate::route::attrs)
            .and_then(|a| get(&a, VETH_INFO_PEER));
        let (pm, pattrs) = match peer {
            Some(p) if p.len() >= 16 => (p[..16].to_vec(), crate::route::attrs(&p[16..])),
            Some(_) => return Err(Errno::EINVAL.into()),
            None => (vec![0u8; 16], Vec::new()),
        };
        validate(kind, &pattrs)?;
        let peer_ns = target_ns(&pattrs, cred, resolve)?.unwrap_or_else(|| ns.clone());
        let ppat = ifname(&pattrs)?.unwrap_or(b"veth%d");
        let (pindex, pflags, pchange) = ifinfo(&pm);
        let p = new_dev(&peer_ns, kind, ppat, pindex, &pattrs)?;
        let dev = match new_dev(&dest, kind, pat, index, attrs) {
            Ok(d) => d,
            Err(e) => {
                unlink(&peer_ns, &p);
                return Err(e);
            }
        };
        p.borrow_mut().peer = Rc::downgrade(&dev);
        p.borrow_mut().paired = true;
        dev.borrow_mut().peer = Rc::downgrade(&p);
        dev.borrow_mut().paired = true;
        if pflags != 0 || pchange != 0 {
            let cur = reported_flags(&peer_ns, &p);
            change_flags(&peer_ns, &p, combine(cur, pflags, pchange));
        }
        dev
    } else {
        new_dev(&dest, kind, pat, index, attrs)?
    };
    if ifi_flags != 0 || change != 0 {
        let cur = reported_flags(&dest, &dev);
        change_flags(&dest, &dev, combine(cur, ifi_flags, change));
    }
    if let Some(m) = get_u32(attrs, IFLA_MASTER)
        && let Err(e) = set_master(&dest, &dev, m)
    {
        delete(&dest, &dev);
        return Err(e);
    }
    settle_around(&dev);
    Ok(())
}

fn del_request(ns: &Rc<NetNs>, req: &[u8]) -> Result<(), Fail> {
    let m = req.get(16..32).ok_or(Errno::EINVAL)?;
    let (index, ..) = ifinfo(m);
    let attrs = crate::route::attrs(&req[32..]);
    let name = ifname(&attrs)?;
    if index <= 0 && name.is_none() {
        return Err(Errno::EINVAL.into());
    }
    let virt = {
        let d = devs(ns);
        if index > 0 {
            d.by_index(index as u32)
        } else {
            name.and_then(|n| d.by_name(n))
        }
    };
    if let Some(dev) = virt {
        delete(ns, &dev);
        return Ok(());
    }
    let known = base(ns).iter().any(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name == Some(i.name.as_bytes())
        }
    });
    // lo、宿主的接口没有 rtnl_link_ops，删不了
    Err(if known {
        Errno::EOPNOTSUPP
    } else {
        Errno::ENODEV
    }
    .into())
}

// ---- 地址 ----

fn addr_request(ns: &Rc<NetNs>, cred: &Cred, req: &[u8]) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let b = req.get(16..24).ok_or(Errno::EINVAL)?;
    let (family, prefix, ifa_flags, scope) = (i32::from(b[0]), b[1], b[2], b[3]);
    let index = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let attrs = crate::route::attrs(&req[24..]);
    if family != AF_INET && family != AF_INET6 {
        return Err(Errno::EOPNOTSUPP.into());
    }
    let dev = devs(ns).by_index(index);
    let Some(dev) = dev else {
        if ns.host_net.is_some() && index == crate::netns::LO_INDEX {
            return match crate::netns::configure(ns, cred, req) {
                Some(0) | None => Ok(()),
                Some(e) => Err(Errno(e).into()),
            };
        }
        // 宿主的接口改不了
        return Err(if base(ns).iter().any(|i| i.index == index) {
            Errno::EOPNOTSUPP
        } else {
            Errno::ENODEV
        }
        .into());
    };
    let new = ty == RTM_NEWADDR;
    let flags = get_u32(&attrs, IFA_FLAGS).unwrap_or(u32::from(ifa_flags));
    let r = if family == AF_INET {
        if new {
            add_addr4(ns, &dev, nlflags, prefix, flags, scope, &attrs)
        } else {
            del_addr4(ns, &dev, prefix, &attrs)
        }
    } else if new {
        add_addr6(ns, &dev, nlflags, prefix, flags, &attrs)
    } else {
        del_addr6(ns, &dev, prefix, &attrs)
    };
    settle_around(&dev);
    r
}

fn ip4(attrs: &[(u16, &[u8])], ty: u16) -> Option<[u8; 4]> {
    get(attrs, ty).and_then(|d| d.try_into().ok())
}

fn ip6(attrs: &[(u16, &[u8])], ty: u16) -> Option<[u8; 16]> {
    get(attrs, ty).and_then(|d| d.try_into().ok())
}

/// 加 IPv4 地址（`inet_rtm_newaddr`、`__inet_insert_ifa`）。
fn add_addr4(
    ns: &Rc<NetNs>,
    dev: &DevRef,
    nlflags: u16,
    prefix: u8,
    flags: u32,
    scope: u8,
    attrs: &[(u16, &[u8])],
) -> Result<(), Fail> {
    let local = ip4(attrs, IFA_LOCAL).or_else(|| ip4(attrs, IFA_ADDRESS));
    let (Some(local), true) = (local, prefix <= 32) else {
        return Err(Errno::EINVAL.into());
    };
    let mut a = Addr4 {
        local,
        address: ip4(attrs, IFA_ADDRESS).unwrap_or(local),
        prefix,
        broadcast: ip4(attrs, IFA_BROADCAST).unwrap_or([0; 4]),
        label: get(attrs, IFA_LABEL)
            .map(|l| l.split(|&c| c == 0).next().unwrap_or_default().to_vec())
            .unwrap_or_else(|| dev.borrow().name.clone()),
        // 有效期都当作永久（IFA_CACHEINFO 不看）
        flags: (flags & !IFA_F_SECONDARY) | IFA_F_PERMANENT,
        scope,
        metric: get_u32(attrs, IFA_RT_PRIORITY).unwrap_or(0),
        proto: get(attrs, IFA_PROTO)
            .and_then(|p| p.first().copied())
            .unwrap_or(0),
    };
    let _ = IFA_CACHEINFO;
    let mut x = dev.borrow_mut();
    let same =
        x.v4.iter()
            .position(|b| b.prefix == a.prefix && b.contains(a.address) && b.local == a.local);
    if let Some(i) = same {
        if nlflags & NLM_F_EXCL != 0 || nlflags & NLM_F_REPLACE == 0 {
            return Err(Errno::EEXIST.into());
        }
        let old = &mut x.v4[i];
        old.flags = (a.flags & !IFA_F_SECONDARY) | (old.flags & IFA_F_SECONDARY);
        old.metric = a.metric;
        old.proto = a.proto;
        let local = old.local;
        drop(x);
        redo_ifaddr4(&mut devs(ns), dev, &[local]);
        return Ok(());
    }
    let mut last_primary = 0;
    for (i, b) in x.v4.iter().enumerate() {
        if b.flags & IFA_F_SECONDARY == 0 && a.scope <= b.scope {
            last_primary = i + 1;
        }
        if b.prefix == a.prefix && b.contains(a.address) {
            if b.scope != a.scope {
                return Err(Errno::EINVAL.into());
            }
            a.flags |= IFA_F_SECONDARY;
        }
    }
    if a.flags & IFA_F_SECONDARY != 0 {
        x.v4.push(a.clone());
    } else {
        x.v4.insert(last_primary, a.clone());
    }
    drop(x);
    add_ifaddr4(&mut devs(ns), dev, &a);
    Ok(())
}

/// 删 IPv4 地址（`inet_rtm_deladdr`）：按给出的 local、标签、地址和前缀找第一个；删掉
/// primary 时同一网段的 secondary 也删掉（`promote_secondaries` 没做）。
fn del_addr4(ns: &Rc<NetNs>, dev: &DevRef, prefix: u8, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    let local = ip4(attrs, IFA_LOCAL);
    let address = ip4(attrs, IFA_ADDRESS);
    let label = get(attrs, IFA_LABEL).map(|l| l.split(|&c| c == 0).next().unwrap_or_default());
    let mut x = dev.borrow_mut();
    let i =
        x.v4.iter()
            .position(|a| {
                local.is_none_or(|l| a.local == l)
                    && label.is_none_or(|l| a.label == l)
                    && address.is_none_or(|ad| a.prefix == prefix && a.contains(ad))
            })
            .ok_or(Errno::EADDRNOTAVAIL)?;
    let a = x.v4.remove(i);
    let mut gone = vec![a.local];
    if a.flags & IFA_F_SECONDARY == 0 {
        x.v4.retain(|b| {
            let sec = b.flags & IFA_F_SECONDARY != 0 && b.prefix == a.prefix && a.contains(b.local);
            if sec {
                gone.push(b.local);
            }
            !sec
        });
    }
    drop(x);
    redo_ifaddr4(&mut devs(ns), dev, &gone);
    Ok(())
}

/// IPv6 地址在列表里的范围（`ipv6_addr_src_scope`）：global 14、site 5、link 2、loopback 1。
fn src_scope6(a: &[u8; 16]) -> u8 {
    if a[0] == 0xfe && a[1] & 0xc0 == 0x80 {
        2
    } else if a[0] == 0xfe && a[1] & 0xc0 == 0xc0 {
        5
    } else if *a == std::net::Ipv6Addr::LOCALHOST.octets() {
        1
    } else {
        14
    }
}

/// 加 IPv6 地址（`inet6_rtm_newaddr`、`inet6_addr_add`）：接口关了 IPv6 时 EACCES；按范围
/// 排在列表里（大的在前，同一范围新加的在前）。
fn add_addr6(
    ns: &Rc<NetNs>,
    dev: &DevRef,
    nlflags: u16,
    prefix: u8,
    flags: u32,
    attrs: &[(u16, &[u8])],
) -> Result<(), Fail> {
    let addr = ip6(attrs, IFA_LOCAL).or_else(|| ip6(attrs, IFA_ADDRESS));
    let (Some(addr), true) = (addr, prefix <= 128) else {
        return Err(Errno::EINVAL.into());
    };
    let allowed = IFA_F_NODAD
        | IFA_F_HOMEADDRESS
        | IFA_F_MANAGETEMPADDR
        | IFA_F_NOPREFIXROUTE
        | IFA_F_MCAUTOJOIN
        | IFA_F_OPTIMISTIC;
    let a = Addr6 {
        addr,
        prefix,
        flags: (flags & allowed) | IFA_F_PERMANENT,
        proto: get(attrs, IFA_PROTO)
            .and_then(|p| p.first().copied())
            .unwrap_or(0),
        metric: get_u32(attrs, IFA_RT_PRIORITY).unwrap_or(0),
    };
    let mut x = dev.borrow_mut();
    if let Some(i) = x.v6.iter().position(|b| b.addr == addr) {
        if nlflags & NLM_F_EXCL != 0 || nlflags & NLM_F_REPLACE == 0 {
            return Err(Errno::EEXIST.into());
        }
        x.v6[i].flags = a.flags;
        x.v6[i].metric = a.metric;
        return Ok(());
    }
    if v6_disabled(&x) {
        return Err(Errno::EACCES.into());
    }
    let s = src_scope6(&addr);
    let at =
        x.v6.iter()
            .position(|b| s >= src_scope6(&b.addr))
            .unwrap_or(x.v6.len());
    x.v6.insert(at, a.clone());
    drop(x);
    add_ifaddr6(&mut devs(ns), dev, &a);
    Ok(())
}

fn del_addr6(ns: &Rc<NetNs>, dev: &DevRef, prefix: u8, attrs: &[(u16, &[u8])]) -> Result<(), Fail> {
    let addr = ip6(attrs, IFA_LOCAL)
        .or_else(|| ip6(attrs, IFA_ADDRESS))
        .ok_or(Errno::EINVAL)?;
    let mut x = dev.borrow_mut();
    let i =
        x.v6.iter()
            .position(|a| a.addr == addr && a.prefix == prefix)
            .ok_or(Errno::EADDRNOTAVAIL)?;
    x.v6.remove(i);
    drop(x);
    devs(ns)
        .routes
        .retain(|r| !(r.family == AF_INET6 && r.owner.as_deref() == Some(&addr[..])));
    Ok(())
}

// ---- 路由 ----

/// 下一跳（`fib_check_nh`、`fib6_nh_init`）：经由的接口要是虚拟设备（宿主的接口、lo 是
/// EOPNOTSUPP）、启用着。有网关时按这个命名空间的路由找它所在的网段。
fn nexthop(
    ns: &Rc<NetNs>,
    family: i32,
    gw: Option<&[u8]>,
    oif: Option<u32>,
    scope: u8,
) -> Result<DevRef, Fail> {
    let v4 = family == AF_INET;
    let not_up = if v4 {
        "Device for nexthop is not up"
    } else {
        "Nexthop device is not up"
    };
    let oif = oif.filter(|&i| i != 0);
    let Some(gw) = gw else {
        let i = oif.ok_or(Errno::ENODEV)?;
        let dev = devs(ns).by_index(i);
        let Some(dev) = dev else {
            return Err(if base(ns).iter().any(|b| b.index == i) {
                Errno::EOPNOTSUPP
            } else {
                Errno::ENODEV
            }
            .into());
        };
        if !dev.borrow().up {
            return Err(fail(Errno::ENETDOWN, not_up));
        }
        return Ok(dev);
    };
    // 网关所在的网段：本地表、主表里经由 oif（给了的话）的、范围不小于 link 的最长前缀
    let want = if v4 {
        scope.saturating_add(1).max(RT_SCOPE_LINK)
    } else {
        0
    };
    let found = entries(ns)
        .into_iter()
        .filter(|e| {
            e.family == family
                && (e.table == RT_TABLE_LOCAL || e.table == RT_TABLE_MAIN)
                && e.scope >= want
                && oif.is_none_or(|o| e.index == o)
                && crate::route::network(gw, e.prefix) == e.dst
        })
        .max_by_key(|e| e.prefix);
    let Some(e) = found else {
        let host = base(ns).iter().any(|i| {
            i.addrs.iter().any(|(f, a, p)| {
                *f == family && crate::route::network(a, *p) == crate::route::network(gw, *p)
            })
        });
        if host && ns.host_net.is_none() {
            return Err(Errno::EOPNOTSUPP.into());
        }
        return Err(if v4 {
            fail(Errno::ENETUNREACH, "Nexthop has invalid gateway")
        } else {
            Errno::ENETUNREACH.into()
        });
    };
    if e.ty != RTN_UNICAST && e.ty != RTN_LOCAL {
        return Err(fail(Errno::EINVAL, "Nexthop has invalid gateway"));
    }
    let dev = devs(ns).by_index(e.index).ok_or(Errno::ENODEV)?;
    if !dev.borrow().up {
        return Err(if v4 {
            Errno::ENETDOWN.into()
        } else {
            fail(Errno::ENETDOWN, not_up)
        });
    }
    Ok(dev)
}

/// `RTM_NEWROUTE`、`RTM_DELROUTE`（`fib_table_insert`、`fib_table_delete`，IPv6 的
/// `ip6_route_add`、`ip6_route_del`）：只有单播路由；表的编号要不大于 255。
fn route_request(ns: &Rc<NetNs>, req: &[u8]) -> Result<(), Fail> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let nlflags = u16::from_le_bytes([req[6], req[7]]);
    let m = req.get(16..28).ok_or(Errno::EINVAL)?;
    let (family, dst_len, tos, table, proto, scope, rtype) =
        (i32::from(m[0]), m[1], m[3], m[4], m[5], m[6], m[7]);
    let attrs = crate::route::attrs(&req[28..]);
    let table = match get_u32(&attrs, RTA_TABLE) {
        Some(t) if t > 255 => return Err(Errno::EOPNOTSUPP.into()),
        Some(t) => t as u8,
        None => table,
    };
    let table = if table == RT_TABLE_UNSPEC {
        RT_TABLE_MAIN
    } else {
        table
    };
    let len = match family {
        AF_INET => 4,
        AF_INET6 => 16,
        _ => return Err(Errno::EOPNOTSUPP.into()),
    };
    let v4 = family == AF_INET;
    if usize::from(dst_len) > len * 8 {
        return Err(fail(Errno::EINVAL, "Invalid prefix length"));
    }
    let addr = |t| -> Result<Option<Vec<u8>>, Fail> {
        match get(&attrs, t) {
            Some(d) if d.len() == len => Ok(Some(d.to_vec())),
            Some(_) => Err(Errno::EINVAL.into()),
            None => Ok(None),
        }
    };
    let mut dst = addr(RTA_DST)?.unwrap_or_else(|| vec![0; len]);
    let gw = addr(RTA_GATEWAY)?;
    let prefsrc = addr(RTA_PREFSRC)?;
    let oif = get_u32(&attrs, RTA_OIF);
    let metric = get_u32(&attrs, RTA_PRIORITY).unwrap_or(0);
    let masked = crate::route::network(&dst, dst_len);
    if ty == RTM_DELROUTE {
        if !v4 {
            dst = masked;
        }
        let mut d = devs(ns);
        let i = d
            .routes
            .iter()
            .position(|r| {
                let idx = r.dev.upgrade().map_or(0, |x| x.borrow().index);
                r.family == family
                    && r.table == table
                    && r.dst == dst
                    && r.prefix == dst_len
                    && (!v4 || r.tos == tos)
                    && (rtype == 0 || r.ty == rtype)
                    && (!v4 || scope == RT_SCOPE_NOWHERE || r.scope == scope)
                    && prefsrc
                        .as_ref()
                        .is_none_or(|p| r.prefsrc.as_ref() == Some(p))
                    && (proto == 0 || r.proto == proto)
                    && (metric == 0 || r.metric == metric)
                    && oif.is_none_or(|o| o == 0 || o == idx)
                    && gw.as_ref().is_none_or(|g| r.gateway.as_ref() == Some(g))
            })
            // 开机时的命名空间里别的路由是宿主的，删不了
            .ok_or(if ns.host_net.is_none() {
                Errno::EOPNOTSUPP
            } else {
                Errno::ESRCH
            })?;
        d.routes.remove(i);
        return Ok(());
    }
    if v4 && masked != dst {
        return Err(fail(
            Errno::EINVAL,
            "Invalid prefix for given prefix length",
        ));
    }
    dst = masked;
    if rtype != RTN_UNICAST {
        return Err(Errno::EOPNOTSUPP.into());
    }
    if v4 && scope > RT_SCOPE_HOST {
        return Err(fail(Errno::EINVAL, "Invalid scope"));
    }
    let dev = nexthop(ns, family, gw.as_deref(), oif, scope)?;
    if let Some(src) = &prefsrc {
        let local = interfaces(ns)
            .iter()
            .any(|i| i.addrs.iter().any(|(f, a, _)| *f == family && a == src));
        if !local {
            return Err(fail(Errno::EINVAL, "Invalid prefsrc address"));
        }
    }
    let r = Rt {
        family,
        table,
        ty: RTN_UNICAST,
        proto,
        scope: if v4 { scope } else { RT_SCOPE_UNIVERSE },
        tos: if v4 { tos } else { 0 },
        dst,
        prefix: dst_len,
        prefsrc,
        gateway: gw,
        dev: Rc::downgrade(&dev),
        metric: if !v4 && metric == 0 {
            IP6_PRIO_USER
        } else {
            metric
        },
        owner: None,
    };
    let mut d = devs(ns);
    let same: Vec<usize> = d
        .routes
        .iter()
        .enumerate()
        .filter(|(_, x)| {
            (x.family, x.table, &x.dst, x.prefix, x.tos, x.metric)
                == (r.family, r.table, &r.dst, r.prefix, r.tos, r.metric)
        })
        .map(|(i, _)| i)
        .collect();
    if let (Some(&first), Some(&last)) = (same.first(), same.last()) {
        if nlflags & NLM_F_EXCL != 0 {
            return Err(Errno::EEXIST.into());
        }
        if nlflags & NLM_F_REPLACE != 0 {
            d.routes[first] = r;
            return Ok(());
        }
        let identical = same.iter().any(|&i| {
            let x = &d.routes[i];
            x.ty == r.ty
                && x.scope == r.scope
                && Weak::ptr_eq(&x.dev, &r.dev)
                && x.gateway == r.gateway
                && x.prefsrc == r.prefsrc
        });
        if identical {
            return Err(Errno::EEXIST.into());
        }
        if nlflags & NLM_F_APPEND != 0 {
            d.routes.insert(last + 1, r);
        } else {
            d.routes.insert(first, r);
        }
        return Ok(());
    }
    if nlflags & NLM_F_CREATE == 0 && (v4 || nlflags & NLM_F_REPLACE != 0) {
        return Err(Errno::ENOENT.into());
    }
    d.routes.push(r);
    Ok(())
}

// ---- netlink 的回答 ----

/// 一个虚拟设备的 `RTM_NEWLINK`（`rtnl_fill_ifinfo`，属性的顺序相同）。`nsid`：对端在
/// 别的命名空间里时，这个命名空间给那边的编号（`IFLA_LINK_NETNSID`）。
pub(crate) fn link_message(v: &View, nsid: Option<i32>) -> Vec<u8> {
    let mut p = vec![0u8, 0];
    p.extend_from_slice(&1u16.to_le_bytes());
    p.extend_from_slice(&(v.index as i32).to_le_bytes());
    p.extend_from_slice(&v.flags.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let mut name = v.name.clone();
    name.push(0);
    attr(&mut p, IFLA_IFNAME, &name);
    attr(&mut p, IFLA_TXQLEN, &v.txqlen.to_le_bytes());
    attr(&mut p, IFLA_OPERSTATE, &[v.operstate]);
    attr(&mut p, IFLA_LINKMODE, &[0]);
    attr(&mut p, IFLA_MTU, &v.mtu.to_le_bytes());
    let (min, max) = v.kind.mtu_range();
    attr(&mut p, IFLA_MIN_MTU, &min.to_le_bytes());
    attr(&mut p, IFLA_MAX_MTU, &max.to_le_bytes());
    attr(&mut p, IFLA_GROUP, &v.group.to_le_bytes());
    attr(
        &mut p,
        IFLA_PROMISCUITY,
        &u32::from(v.port_state.is_some()).to_le_bytes(),
    );
    attr(&mut p, IFLA_NUM_TX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_GSO_MAX_SEGS, &65535u32.to_le_bytes());
    attr(&mut p, IFLA_GSO_MAX_SIZE, &65536u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_RX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_CARRIER, &[u8::from(v.carrier)]);
    attr(
        &mut p,
        IFLA_CARRIER_CHANGES,
        &v.carrier_changes.to_le_bytes(),
    );
    attr(&mut p, IFLA_PROTO_DOWN, &[0]);
    let up = v.carrier_changes.div_ceil(2);
    attr(&mut p, IFLA_CARRIER_UP_COUNT, &up.to_le_bytes());
    attr(
        &mut p,
        IFLA_CARRIER_DOWN_COUNT,
        &(v.carrier_changes - up).to_le_bytes(),
    );
    if let Some((peer, _)) = &v.link
        && *peer != v.index
    {
        attr(&mut p, IFLA_LINK, &peer.to_le_bytes());
    }
    if let Some(m) = v.master {
        attr(&mut p, IFLA_MASTER, &m.to_le_bytes());
    }
    let qdisc: &[u8] = if v.ever_up { b"noqueue\0" } else { b"noop\0" };
    attr(&mut p, IFLA_QDISC, qdisc);
    if let Some(a) = &v.alias {
        let mut a = a.clone();
        a.push(0);
        attr(&mut p, IFLA_IFALIAS, &a);
    }
    attr(&mut p, IFLA_ADDRESS, &v.mac);
    attr(&mut p, IFLA_BROADCAST, &[0xff; 6]);
    attr(&mut p, IFLA_STATS64, &[0u8; 25 * 8]);
    attr(&mut p, IFLA_STATS, &[0u8; 24 * 4]);
    let mut li = Vec::new();
    let mut kind = v.kind.name().as_bytes().to_vec();
    kind.push(0);
    attr(&mut li, IFLA_INFO_KIND, &kind);
    if v.kind == Kind::Bridge {
        attr(&mut li, IFLA_INFO_DATA, &bridge_info(v));
    }
    if let Some(state) = v.port_state {
        attr(&mut li, IFLA_INFO_SLAVE_KIND, b"bridge\0");
        let mut sd = Vec::new();
        attr(&mut sd, 1, &[state]); // IFLA_BRPORT_STATE
        attr(&mut sd, 2, &32u16.to_le_bytes()); // PRIORITY
        attr(&mut sd, 3, &2u32.to_le_bytes()); // COST
        attr(&mut sd, 4, &[0]); // MODE（hairpin）
        attr(&mut sd, 8, &[1]); // LEARNING
        attr(&mut sd, 9, &[1]); // UNICAST_FLOOD
        attr(&mut li, IFLA_INFO_SLAVE_DATA, &sd);
    }
    attr(&mut p, IFLA_LINKINFO, &li);
    if let Some(id) = nsid {
        attr(&mut p, IFLA_LINK_NETNSID, &id.to_le_bytes());
    }
    p
}

/// bridge 的参数（`br_fill_info` 的一部分）：都是 Linux 的默认值，STP 关着。
fn bridge_info(v: &View) -> Vec<u8> {
    let mut d = Vec::new();
    let mut id = vec![0x80, 0x00];
    id.extend_from_slice(&v.bridge_mac);
    attr(&mut d, 1, &1500u32.to_le_bytes()); // IFLA_BR_FORWARD_DELAY
    attr(&mut d, 2, &200u32.to_le_bytes()); // HELLO_TIME
    attr(&mut d, 3, &2000u32.to_le_bytes()); // MAX_AGE
    attr(&mut d, 4, &30000u32.to_le_bytes()); // AGEING_TIME
    attr(&mut d, 5, &0u32.to_le_bytes()); // STP_STATE
    attr(&mut d, 6, &32768u16.to_le_bytes()); // PRIORITY
    attr(&mut d, 7, &[0]); // VLAN_FILTERING
    attr(&mut d, 9, &0u16.to_le_bytes()); // GROUP_FWD_MASK
    attr(&mut d, 11, &id); // BRIDGE_ID
    attr(&mut d, 10, &id); // ROOT_ID
    attr(&mut d, 12, &0u16.to_le_bytes()); // ROOT_PORT
    attr(&mut d, 13, &0u32.to_le_bytes()); // ROOT_PATH_COST
    attr(&mut d, 14, &[0]); // TOPOLOGY_CHANGE
    attr(&mut d, 15, &[0]); // TOPOLOGY_CHANGE_DETECTED
    attr(&mut d, 20, &[0x01, 0x80, 0xc2, 0, 0, 0]); // GROUP_ADDR
    attr(&mut d, 8, &0x8100u16.to_be_bytes()); // VLAN_PROTOCOL
    attr(&mut d, 39, &1u16.to_le_bytes()); // VLAN_DEFAULT_PVID
    d
}

/// 一个虚拟设备上某一族的 `RTM_NEWADDR`（`inet_fill_ifaddr`、`inet6_fill_ifaddr`）。
pub(crate) fn addr_messages(v: &View, family: i32) -> Vec<Vec<u8>> {
    let mut cacheinfo = [0xffu8; 16];
    cacheinfo[8..].fill(0);
    let mut out = Vec::new();
    if family == AF_INET {
        for a in &v.v4 {
            let mut p = vec![AF_INET as u8, a.prefix, a.flags as u8, a.scope];
            p.extend_from_slice(&v.index.to_le_bytes());
            attr(&mut p, IFA_ADDRESS, &a.address);
            attr(&mut p, IFA_LOCAL, &a.local);
            if a.broadcast != [0; 4] {
                attr(&mut p, IFA_BROADCAST, &a.broadcast);
            }
            let mut l = a.label.clone();
            l.push(0);
            attr(&mut p, IFA_LABEL, &l);
            if a.proto != 0 {
                attr(&mut p, IFA_PROTO, &[a.proto]);
            }
            attr(&mut p, IFA_FLAGS, &a.flags.to_le_bytes());
            if a.metric != 0 {
                attr(&mut p, IFA_RT_PRIORITY, &a.metric.to_le_bytes());
            }
            attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            out.push(p);
        }
    } else {
        for a in &v.v6 {
            let mut p = vec![AF_INET6 as u8, a.prefix, a.flags as u8, scope6(&a.addr)];
            p.extend_from_slice(&v.index.to_le_bytes());
            attr(&mut p, IFA_ADDRESS, &a.addr);
            if a.metric != 0 {
                attr(&mut p, IFA_RT_PRIORITY, &a.metric.to_le_bytes());
            }
            attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            attr(&mut p, IFA_FLAGS, &a.flags.to_le_bytes());
            if a.proto != 0 {
                attr(&mut p, IFA_PROTO, &[a.proto]);
            }
            out.push(p);
        }
    }
    out
}

// ---- ioctl ----

/// 接口 ioctl（`struct ifreq`，40 字节）里针对虚拟设备的：按名字（`SIOCGIFNAME` 按编号）
/// 找到虚拟设备时处理，None 是不归这里（lo、宿主的接口、不认识的请求）。改动要在命名空间
/// 的属主里有 `CAP_NET_ADMIN`。
pub(crate) fn ioctl(
    ns: &Rc<NetNs>,
    cred: &Cred,
    request: u32,
    ifr: &mut [u8; 40],
) -> Option<Result<(), Errno>> {
    const SIOCGIFNAME: u32 = 0x8910;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCSIFFLAGS: u32 = 0x8914;
    const SIOCGIFADDR: u32 = 0x8915;
    const SIOCGIFDSTADDR: u32 = 0x8917;
    const SIOCGIFBRDADDR: u32 = 0x8919;
    const SIOCGIFNETMASK: u32 = 0x891b;
    const SIOCGIFMETRIC: u32 = 0x891d;
    const SIOCGIFMTU: u32 = 0x8921;
    const SIOCSIFMTU: u32 = 0x8922;
    const SIOCSIFNAME: u32 = 0x8923;
    const SIOCSIFHWADDR: u32 = 0x8924;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const SIOCGIFINDEX: u32 = 0x8933;
    const SIOCGIFTXQLEN: u32 = 0x8942;
    const SIOCSIFTXQLEN: u32 = 0x8943;
    if request == SIOCGIFNAME {
        let index = i32::from_le_bytes(ifr[16..20].try_into().unwrap());
        let dev = devs(ns).by_index(u32::try_from(index).ok()?)?;
        ifr[..16].fill(0);
        let name = dev.borrow().name.clone();
        ifr[..name.len()].copy_from_slice(&name);
        return Some(Ok(()));
    }
    let known = [
        SIOCGIFFLAGS,
        SIOCSIFFLAGS,
        SIOCGIFADDR,
        SIOCGIFDSTADDR,
        SIOCGIFBRDADDR,
        SIOCGIFNETMASK,
        SIOCGIFMETRIC,
        SIOCGIFMTU,
        SIOCSIFMTU,
        SIOCSIFNAME,
        SIOCSIFHWADDR,
        SIOCGIFHWADDR,
        SIOCGIFINDEX,
        SIOCGIFTXQLEN,
        SIOCSIFTXQLEN,
    ];
    if !known.contains(&request) {
        return None;
    }
    let name = ifr[..16]
        .split(|&c| c == 0)
        .next()
        .unwrap_or_default()
        .to_vec();
    // 地址类按标签找（`eth0:1` 这种别名），别的按接口名
    let base_name = name.split(|&c| c == b':').next().unwrap_or_default();
    let dev = devs(ns).by_name(base_name)?;
    let setter = matches!(
        request,
        SIOCSIFFLAGS | SIOCSIFMTU | SIOCSIFNAME | SIOCSIFHWADDR | SIOCSIFTXQLEN
    );
    if setter && !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Err(Errno::EPERM));
    }
    let data = &mut ifr[16..];
    let i32_of = |d: &[u8]| i32::from_le_bytes(d[..4].try_into().unwrap());
    let r = match request {
        SIOCGIFFLAGS => {
            let f = reported_flags(ns, &dev) as u16;
            data[..2].copy_from_slice(&f.to_le_bytes());
            Ok(())
        }
        SIOCSIFFLAGS => {
            // ifr_flags 是 short，按符号扩展（dev_change_flags 收的是 unsigned int）
            let f = i16::from_le_bytes([data[0], data[1]]) as i32 as u32;
            change_flags(ns, &dev, f);
            Ok(())
        }
        SIOCGIFMTU => {
            data[..4].copy_from_slice(&(dev.borrow().mtu as i32).to_le_bytes());
            Ok(())
        }
        SIOCSIFMTU => set_mtu(ns, &dev, i32_of(data) as u32).map_err(|f| Errno(f.errno)),
        SIOCSIFNAME => {
            let new = data[..16]
                .split(|&c| c == 0)
                .next()
                .unwrap_or_default()
                .to_vec();
            rename(ns, &dev, &new).map_err(|f| Errno(f.errno))
        }
        SIOCGIFHWADDR => {
            data[..16].fill(0);
            data[..2].copy_from_slice(&1u16.to_le_bytes());
            data[2..8].copy_from_slice(&dev.borrow().mac);
            Ok(())
        }
        SIOCSIFHWADDR => {
            if u16::from_le_bytes([data[0], data[1]]) != 1 {
                Err(Errno::EINVAL)
            } else {
                let mac = data[2..8].to_vec();
                set_mac(ns, &dev, &mac).map_err(|f| Errno(f.errno))
            }
        }
        SIOCGIFINDEX => {
            data[..4].copy_from_slice(&(dev.borrow().index as i32).to_le_bytes());
            Ok(())
        }
        SIOCGIFTXQLEN => {
            data[..4].copy_from_slice(&(dev.borrow().txqlen as i32).to_le_bytes());
            Ok(())
        }
        SIOCSIFTXQLEN => {
            let q = i32_of(data);
            if q < 0 {
                Err(Errno::EINVAL)
            } else {
                dev.borrow_mut().txqlen = q as u32;
                Ok(())
            }
        }
        SIOCGIFMETRIC => {
            data[..4].fill(0);
            Ok(())
        }
        _ => {
            let x = dev.borrow();
            match x.v4.iter().find(|a| a.label == name) {
                None => Err(Errno::EADDRNOTAVAIL),
                Some(a) => {
                    let value = match request {
                        SIOCGIFADDR => a.local,
                        SIOCGIFDSTADDR => a.address,
                        SIOCGIFBRDADDR => a.broadcast,
                        _ => a.mask().to_be_bytes(),
                    };
                    data[..16].fill(0);
                    data[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
                    data[4..8].copy_from_slice(&value);
                    Ok(())
                }
            }
        }
    };
    if setter {
        settle_around(&dev);
    }
    Some(r)
}

/// `SIOCGIFCONF` 里虚拟设备的项：每个 IPv4 地址一个 ifreq（名字是地址的标签）。
pub(crate) fn ifconf(ns: &NetNs) -> Vec<u8> {
    let mut out = Vec::new();
    for v in views(ns) {
        for a in &v.v4 {
            let mut r = [0u8; 40];
            let n = a.label.len().min(15);
            r[..n].copy_from_slice(&a.label[..n]);
            r[16..18].copy_from_slice(&(AF_INET as u16).to_le_bytes());
            r[20..24].copy_from_slice(&a.local);
            out.extend_from_slice(&r);
        }
    }
    out
}

// ---- sysctl（crate::netconf） ----

/// `/proc/sys/net/ipv{4,6}/conf` 下的一个目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConfDir {
    All,
    Default,
    /// 接口，按编号。
    Dev(u32),
}

/// `ipv4/conf`、`ipv6/conf` 下的目录：`all`、`default` 和每个接口。
pub(crate) fn conf_dirs(ns: &NetNs) -> Vec<(Vec<u8>, ConfDir)> {
    let mut v = vec![
        (b"all".to_vec(), ConfDir::All),
        (b"default".to_vec(), ConfDir::Default),
    ];
    v.extend(
        interfaces(ns)
            .into_iter()
            .map(|i| (i.name.into_bytes(), ConfDir::Dev(i.index))),
    );
    v
}

/// 目录的名字（接口没了是 None）。
pub(crate) fn conf_dir_name(ns: &NetNs, dir: ConfDir) -> Option<Vec<u8>> {
    conf_dirs(ns)
        .into_iter()
        .find(|(_, d)| *d == dir)
        .map(|(n, _)| n)
}

/// 一组配置和接口的 MTU（IPv6 的 `mtu` 用）。
fn conf_of(ns: &NetNs, v6: bool, dir: ConfDir) -> Result<(Conf, Option<u32>), Errno> {
    let mut d = devs(ns);
    let pick = |c4: &Conf, c6: &Conf| if v6 { c6.clone() } else { c4.clone() };
    Ok(match dir {
        ConfDir::All => (pick(&d.conf.all4, &d.conf.all6), None),
        ConfDir::Default => (pick(&d.conf.dflt4, &d.conf.dflt6), None),
        ConfDir::Dev(i) => {
            if let Some(x) = d.by_index(i) {
                let x = x.borrow();
                (pick(&x.conf4, &x.conf6), Some(x.mtu))
            } else {
                let b = base(ns)
                    .into_iter()
                    .find(|b| b.index == i)
                    .ok_or(Errno::ENODEV)?;
                let (c4, c6) = d.conf.other_mut(i).clone();
                (pick(&c4, &c6), Some(b.mtu))
            }
        }
    })
}

/// 读接口配置的一项。
pub(crate) fn conf_read(ns: &NetNs, v6: bool, dir: ConfDir, name: &[u8]) -> Result<String, Errno> {
    let i = crate::netconf::position(v6, name).ok_or(Errno::ENOENT)?;
    let (c, mtu) = conf_of(ns, v6, dir)?;
    if v6 && name == b"stable_secret" {
        return crate::netconf::show_secret(c.secret);
    }
    if v6 && name == b"mtu" {
        return Ok(format!(
            "{}\n",
            c.mtu6.or(mtu.map(i64::from)).unwrap_or(c.vals[i])
        ));
    }
    Ok(format!("{}\n", c.vals[i]))
}

/// 写接口配置的一项（模块文档 [`crate::netconf`] 说的那几项起作用）。
pub(crate) fn conf_write(
    ns: &NetNs,
    v6: bool,
    dir: ConfDir,
    name: &[u8],
    data: &[u8],
) -> Result<(), Errno> {
    let i = crate::netconf::position(v6, name).ok_or(Errno::ENOENT)?;
    let key = std::str::from_utf8(name).unwrap_or_default();
    if v6 && name == b"stable_secret" {
        let s = crate::netconf::parse_secret(data)?;
        return conf_apply(ns, v6, dir, |c| c.secret = Some(s));
    }
    let v = crate::netconf::parse_int(data)?;
    crate::netconf::check(v6, key, v)?;
    if v6
        && name == b"mtu"
        && let ConfDir::Dev(_) = dir
    {
        let (_, mtu) = conf_of(ns, v6, dir)?;
        if v > i64::from(mtu.unwrap_or(0)) {
            return Err(Errno::EINVAL);
        }
        return conf_apply(ns, v6, dir, |c| c.mtu6 = Some(v));
    }
    let set = |c: &mut Conf| {
        c.vals[i] = v;
        c.set[i] = true;
    };
    match dir {
        ConfDir::Dev(_) => conf_apply(ns, v6, dir, set)?,
        ConfDir::Default => {
            conf_apply(ns, v6, dir, set)?;
            // IPv4 的 default 改到还没单独设过这一项的接口（devinet_copy_dflt_conf）
            if !v6 {
                for_each_conf(ns, false, |c| {
                    if !c.set[i] {
                        c.vals[i] = v;
                    }
                });
            }
        }
        ConfDir::All => {
            conf_apply(ns, v6, dir, set)?;
            // forwarding、IPv6 的 disable_ipv6 在 all 上写时改到 default 和每个接口
            if name == b"forwarding" || (v6 && name == b"disable_ipv6") {
                conf_apply(ns, v6, ConfDir::Default, |c| c.vals[i] = v)?;
                for_each_conf(ns, v6, |c| c.vals[i] = v);
            }
        }
    }
    if v6 && name == b"disable_ipv6" {
        ipv6_switch(ns);
    }
    Ok(())
}

/// 改一组配置。
fn conf_apply(ns: &NetNs, v6: bool, dir: ConfDir, f: impl FnOnce(&mut Conf)) -> Result<(), Errno> {
    fn pick<'a>(v6: bool, c4: &'a mut Conf, c6: &'a mut Conf) -> &'a mut Conf {
        if v6 { c6 } else { c4 }
    }
    let mut d = devs(ns);
    match dir {
        ConfDir::All => {
            let NetConf { all4, all6, .. } = &mut d.conf;
            f(pick(v6, all4, all6));
        }
        ConfDir::Default => {
            let NetConf { dflt4, dflt6, .. } = &mut d.conf;
            f(pick(v6, dflt4, dflt6));
        }
        ConfDir::Dev(i) => {
            if let Some(x) = d.by_index(i) {
                let mut x = x.borrow_mut();
                let Dev { conf4, conf6, .. } = &mut *x;
                f(pick(v6, conf4, conf6));
            } else {
                if !base(ns).iter().any(|b| b.index == i) {
                    return Err(Errno::ENODEV);
                }
                let (c4, c6) = d.conf.other_mut(i);
                f(pick(v6, c4, c6));
            }
        }
    }
    Ok(())
}

/// 改每个接口的配置（虚拟设备的、lo 和宿主接口的）。
fn for_each_conf(ns: &NetNs, v6: bool, mut f: impl FnMut(&mut Conf)) {
    let mut d = devs(ns);
    for x in &d.list {
        let mut x = x.borrow_mut();
        f(if v6 { &mut x.conf6 } else { &mut x.conf4 });
    }
    for b in base(ns) {
        let (c4, c6) = d.conf.other_mut(b.index);
        f(if v6 { c6 } else { c4 });
    }
}

/// `disable_ipv6` 变了之后：关掉的接口去掉 IPv6，打开的、启用并有载波的重新配置；新的
/// 命名空间里 lo 的 ::1 跟着。
fn ipv6_switch(ns: &NetNs) {
    {
        let mut d = devs(ns);
        let list = d.list.clone();
        for x in &list {
            if v6_disabled(&x.borrow()) && x.borrow().v6_ready {
                v6_down(&mut d, x);
            } else if v6_disabled(&x.borrow()) {
                x.borrow_mut().v6.clear();
            }
        }
    }
    settle(ns, &[]);
    if ns.host_net.is_some() {
        let off = {
            let mut d = devs(ns);
            d.conf
                .other_mut(crate::netns::LO_INDEX)
                .1
                .get(true, "disable_ipv6")
                != 0
        };
        let mut lo = ns.lo.borrow_mut();
        lo.v6 = lo.up && !off;
    }
}

/// `ipv4/ip_forward`（`all/forwarding` 的别名）。
pub(crate) fn ip_forward(ns: &NetNs) -> i64 {
    ns.devs.borrow().conf.all4.get(false, "forwarding")
}

/// 测试用：在 `ns` 里建一个启用着的 dummy 设备，配上地址 `addr`/`prefix`，给了网关时再加
/// 一条经由它的默认路由。
#[cfg(test)]
pub(crate) fn test_dev(ns: &Rc<NetNs>, name: &str, addr: [u8; 4], prefix: u8, gw: Option<[u8; 4]>) {
    let none = |_: u16, _: u32| -> Result<Rc<NetNs>, Errno> { Err(Errno::ESRCH) };
    let root = Cred::root();
    let send = |ty: u16, flags: u16, body: Vec<u8>| {
        let req = crate::net::message(ty, flags | 1, 1, 0, &body);
        request(ns, &root, &req, &none).unwrap().unwrap();
    };
    let mut link = vec![0u8; 16];
    link[8..12].copy_from_slice(&IFF_UP.to_le_bytes());
    link[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
    let mut n = name.as_bytes().to_vec();
    n.push(0);
    attr(&mut link, IFLA_IFNAME, &n);
    let mut li = Vec::new();
    attr(&mut li, IFLA_INFO_KIND, b"dummy");
    attr(&mut link, IFLA_LINKINFO, &li);
    send(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, link);
    let index = views(ns)
        .iter()
        .find(|v| v.name == name.as_bytes())
        .unwrap()
        .index;
    let mut a = vec![AF_INET as u8, prefix, 0, 0];
    a.extend_from_slice(&index.to_le_bytes());
    attr(&mut a, IFA_LOCAL, &addr);
    send(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, a);
    if let Some(g) = gw {
        let mut r = vec![
            AF_INET as u8,
            0,
            0,
            0,
            RT_TABLE_MAIN,
            3,
            0,
            RTN_UNICAST,
            0,
            0,
            0,
            0,
        ];
        attr(&mut r, RTA_GATEWAY, &g);
        send(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::message;

    fn ns(b: u8) -> Rc<NetNs> {
        Rc::new(NetNs {
            id: u64::from(b),
            owner: crate::cred::UserNs::init(),
            host_net: Some([9, b]),
            lo: Default::default(),
            devs: RefCell::new(Devices::for_new_ns(&NetConf::default())),
            nft: Default::default(),
            listeners: Default::default(),
        })
    }

    fn nested(ty: u16, children: &[(u16, Vec<u8>)]) -> (u16, Vec<u8>) {
        let mut b = Vec::new();
        for (t, d) in children {
            attr(&mut b, *t, d);
        }
        (ty, b)
    }

    fn link(
        ty: u16,
        flags: u16,
        index: i32,
        up: Option<bool>,
        attrs: &[(u16, Vec<u8>)],
    ) -> Vec<u8> {
        let mut p = vec![0u8; 16];
        p[4..8].copy_from_slice(&index.to_le_bytes());
        if let Some(u) = up {
            p[8..12].copy_from_slice(&u32::from(u).to_le_bytes());
            p[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
        }
        for (t, d) in attrs {
            attr(&mut p, *t, d);
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn name(n: &str) -> (u16, Vec<u8>) {
        let mut v = n.as_bytes().to_vec();
        v.push(0);
        (IFLA_IFNAME, v)
    }

    fn kind(k: &str) -> (u16, Vec<u8>) {
        nested(IFLA_LINKINFO, &[(IFLA_INFO_KIND, k.as_bytes().to_vec())])
    }

    fn run(n: &Rc<NetNs>, req: &[u8], others: &[Rc<NetNs>]) -> Result<(), Fail> {
        let resolve = |_: u16, v: u32| -> Result<Rc<NetNs>, Errno> {
            others
                .iter()
                .find(|o| o.id == u64::from(v))
                .cloned()
                .ok_or(Errno::ESRCH)
        };
        request(n, &Cred::root(), req, &resolve).unwrap()
    }

    fn up(n: &Rc<NetNs>, dev: &str, on: bool) {
        run(n, &link(RTM_SETLINK, 0, 0, Some(on), &[name(dev)]), &[]).unwrap();
    }

    fn view_of(n: &NetNs, dev: &str) -> View {
        views(n)
            .into_iter()
            .find(|v| v.name == dev.as_bytes())
            .unwrap()
    }

    const CREATE: u16 = NLM_F_CREATE | NLM_F_EXCL;

    #[test]
    fn veth_peer_in_another_namespace() {
        let (a, b) = (ns(1), ns(2));
        let mut peer = vec![0u8; 16];
        let (t, d) = name("eth0");
        attr(&mut peer, t, &d);
        attr(&mut peer, IFLA_NET_NS_PID, &2u32.to_le_bytes());
        let info = nested(
            IFLA_LINKINFO,
            &[
                (IFLA_INFO_KIND, b"veth".to_vec()),
                (IFLA_INFO_DATA, {
                    let mut x = Vec::new();
                    attr(&mut x, VETH_INFO_PEER, &peer);
                    x
                }),
            ],
        );
        let req = link(RTM_NEWLINK, CREATE, 0, None, &[name("v0"), info]);
        run(&a, &req, std::slice::from_ref(&b)).unwrap();
        // 对端先登记：在 b 里是 2（lo 之后），自己在 a 里也是 2
        let (va, vb) = (view_of(&a, "v0"), view_of(&b, "eth0"));
        assert_eq!((va.index, vb.index), (2, 2));
        assert_eq!(va.link.as_ref().unwrap().0, 2);
        assert!(va.link.as_ref().unwrap().1.is_some());
        // 两端都启用了才有载波；只启用一端时，两端编号相同，是 DOWN 不是 LOWERLAYERDOWN
        up(&a, "v0", true);
        assert_eq!(view_of(&a, "v0").operstate, IF_OPER_DOWN);
        up(&b, "eth0", true);
        assert_eq!(view_of(&a, "v0").operstate, IF_OPER_UP);
        assert!(view_of(&b, "eth0").carrier);
        // IPv6 配置了：有链路本地地址
        assert_eq!(view_of(&b, "eth0").v6.len(), 1);
        assert_eq!(nsid_alloc(&a, &b), 0);
        assert_eq!(nsid(&a, &b), Some(0));
        // 同名的建不了，b 没了之后 v0 也没了（快照里拿着对端的命名空间，先丢掉）
        assert_eq!(
            run(&a, &req, std::slice::from_ref(&b)).unwrap_err().errno,
            Errno::EEXIST.0
        );
        drop((va, vb, b));
        assert!(views(&a).is_empty());
    }

    #[test]
    fn bridge_carrier_mtu_and_mac_follow_ports() {
        let n = ns(3);
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("br0"), kind("bridge")]),
            &[],
        )
        .unwrap();
        up(&n, "br0", true);
        // 还没有过端口：有载波，状态 UNKNOWN
        let v = view_of(&n, "br0");
        assert!(v.carrier && v.operstate == IF_OPER_UNKNOWN);
        let mut peer = vec![0u8; 16];
        let (t, d) = name("v1");
        attr(&mut peer, t, &d);
        attr(&mut peer, IFLA_MTU, &1400u32.to_le_bytes());
        let info = nested(
            IFLA_LINKINFO,
            &[
                (IFLA_INFO_KIND, b"veth".to_vec()),
                (IFLA_INFO_DATA, {
                    let mut x = Vec::new();
                    attr(&mut x, VETH_INFO_PEER, &peer);
                    x
                }),
            ],
        );
        let br = view_of(&n, "br0").index;
        let req = link(
            RTM_NEWLINK,
            CREATE,
            0,
            None,
            &[
                name("v0"),
                (IFLA_MTU, 1300u32.to_le_bytes().to_vec()),
                (IFLA_MASTER, br.to_le_bytes().to_vec()),
                info,
            ],
        );
        run(&n, &req, &[]).unwrap();
        let v = view_of(&n, "br0");
        assert!(!v.carrier && v.operstate == IF_OPER_DOWN);
        assert_eq!(v.mtu, 1300);
        assert_eq!(v.mac, view_of(&n, "v0").mac);
        assert_eq!(view_of(&n, "v0").port_state, Some(0));
        up(&n, "v0", true);
        up(&n, "v1", true);
        assert!(view_of(&n, "br0").carrier);
        assert_eq!(view_of(&n, "v0").port_state, Some(3));
        // bridge 不能挂到 bridge 上，不能移走
        let me = link(
            RTM_SETLINK,
            0,
            0,
            None,
            &[name("br0"), (IFLA_MASTER, br.to_le_bytes().to_vec())],
        );
        assert_eq!(
            run(&n, &me, &[]).unwrap_err(),
            fail(Errno::ELOOP, "Can not enslave a bridge to a bridge")
        );
        // 删掉 veth（连同对端）：bridge 没有端口了，MAC 是全 0、没有载波
        run(&n, &link(RTM_DELLINK, 0, 0, None, &[name("v1")]), &[]).unwrap();
        let v = view_of(&n, "br0");
        assert_eq!((v.mac, v.carrier, v.mtu), ([0; 6], false, 1500));
        assert_eq!(views(&n).len(), 1);
        // 不认识的种类、lo 删不了
        let bad = link(RTM_NEWLINK, CREATE, 0, None, &[name("x"), kind("vxlan")]);
        assert_eq!(
            run(&n, &bad, &[]).unwrap_err(),
            fail(Errno::EOPNOTSUPP, "Unknown device type")
        );
        let lo = link(RTM_DELLINK, 0, 1, None, &[]);
        assert_eq!(run(&n, &lo, &[]).unwrap_err().errno, Errno::EOPNOTSUPP.0);
    }

    fn addr(
        ty: u16,
        flags: u16,
        family: i32,
        prefix: u8,
        index: u32,
        a: &[u8],
        brd: Option<[u8; 4]>,
    ) -> Vec<u8> {
        let mut p = vec![family as u8, prefix, 0, 0];
        p.extend_from_slice(&index.to_le_bytes());
        attr(&mut p, IFA_LOCAL, a);
        attr(&mut p, IFA_ADDRESS, a);
        if let Some(b) = brd {
            attr(&mut p, IFA_BROADCAST, &b);
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn route(
        ty: u16,
        flags: u16,
        dst: [u8; 4],
        prefix: u8,
        gw: Option<[u8; 4]>,
        oif: Option<u32>,
    ) -> Vec<u8> {
        let mut p = vec![
            AF_INET as u8,
            prefix,
            0,
            0,
            RT_TABLE_MAIN,
            3,
            0,
            RTN_UNICAST,
            0,
            0,
            0,
            0,
        ];
        if ty == RTM_DELROUTE {
            p[6] = RT_SCOPE_NOWHERE;
        }
        attr(&mut p, RTA_DST, &dst);
        if let Some(g) = gw {
            attr(&mut p, RTA_GATEWAY, &g);
        }
        if let Some(o) = oif {
            attr(&mut p, RTA_OIF, &o.to_le_bytes());
        }
        message(ty, flags | 1, 1, 0, &p)
    }

    fn shown(n: &NetNs) -> Vec<String> {
        entries(n)
            .iter()
            .filter(|e| e.family == AF_INET)
            .map(|e| {
                format!(
                    "t{} n{} {:?}/{} src {:?} via {:?} {}{}",
                    e.table,
                    e.ty,
                    e.dst,
                    e.prefix,
                    e.prefsrc,
                    e.gateway,
                    e.iface,
                    if e.flags != 0 { " linkdown" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn addresses_and_routes() {
        let n = ns(4);
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("d0"), kind("dummy")]),
            &[],
        )
        .unwrap();
        let i = view_of(&n, "d0").index;
        // 停用着：只有本地路由
        run(
            &n,
            &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 1], None),
            &[],
        )
        .unwrap();
        assert_eq!(
            shown(&n),
            ["t255 n2 [10, 0, 0, 1]/32 src Some([10, 0, 0, 1]) via None d0"]
        );
        up(&n, "d0", true);
        run(
            &n,
            &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 2], None),
            &[],
        )
        .unwrap();
        assert_eq!(
            run(
                &n,
                &addr(RTM_NEWADDR, CREATE, AF_INET, 24, i, &[10, 0, 0, 2], None),
                &[]
            )
            .unwrap_err()
            .errno,
            Errno::EEXIST.0
        );
        let v = view_of(&n, "d0");
        assert_eq!(v.v4[1].flags & IFA_F_SECONDARY, IFA_F_SECONDARY);
        assert_eq!(
            shown(&n),
            [
                "t255 n2 [10, 0, 0, 1]/32 src Some([10, 0, 0, 1]) via None d0",
                "t254 n1 [10, 0, 0, 0]/24 src Some([10, 0, 0, 1]) via None d0",
                "t255 n3 [10, 0, 0, 255]/32 src Some([10, 0, 0, 1]) via None d0",
                "t255 n2 [10, 0, 0, 2]/32 src Some([10, 0, 0, 1]) via None d0",
            ]
        );
        // 网关要在网段里，接口要启用；重复的是 EEXIST，删不存在的是 ESRCH
        let gw = route(RTM_NEWROUTE, CREATE, [0; 4], 0, Some([10, 0, 0, 254]), None);
        run(&n, &gw, &[]).unwrap();
        assert_eq!(run(&n, &gw, &[]).unwrap_err().errno, Errno::EEXIST.0);
        let far = route(
            RTM_NEWROUTE,
            CREATE,
            [10, 9, 0, 0],
            16,
            Some([192, 168, 0, 1]),
            None,
        );
        assert_eq!(
            run(&n, &far, &[]).unwrap_err(),
            fail(Errno::ENETUNREACH, "Nexthop has invalid gateway")
        );
        let host_bits = route(RTM_NEWROUTE, CREATE, [10, 9, 0, 1], 16, None, Some(i));
        assert_eq!(run(&n, &host_bits, &[]).unwrap_err().errno, Errno::EINVAL.0);
        let del = route(RTM_DELROUTE, 0, [10, 8, 0, 0], 16, None, None);
        assert_eq!(run(&n, &del, &[]).unwrap_err().errno, Errno::ESRCH.0);
        // 停用：本地路由留着，别的（含经由它的默认路由）去掉；再启用只补回按地址加的
        up(&n, "d0", false);
        assert_eq!(shown(&n).len(), 2);
        let down = route(RTM_NEWROUTE, CREATE, [10, 7, 0, 0], 16, None, Some(i));
        assert_eq!(
            run(&n, &down, &[]).unwrap_err(),
            fail(Errno::ENETDOWN, "Device for nexthop is not up")
        );
        up(&n, "d0", true);
        assert_eq!(shown(&n).len(), 4);
        // 删掉 primary：同一网段的 secondary 一起删掉，路由都没了
        run(
            &n,
            &addr(RTM_DELADDR, 0, AF_INET, 24, i, &[10, 0, 0, 1], None),
            &[],
        )
        .unwrap();
        assert!(view_of(&n, "d0").v4.is_empty());
        assert!(shown(&n).is_empty());
    }

    #[test]
    fn sysctls_follow_default_all_and_disable_ipv6() {
        let n = ns(5);
        conf_write(&n, false, ConfDir::Default, b"rp_filter", b"2\n").unwrap();
        run(
            &n,
            &link(RTM_NEWLINK, CREATE, 0, None, &[name("d0"), kind("dummy")]),
            &[],
        )
        .unwrap();
        let i = view_of(&n, "d0").index;
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"rp_filter").unwrap(),
            "2\n"
        );
        // default 改到没单独设过的接口，设过的不变
        conf_write(&n, false, ConfDir::Dev(i), b"arp_notify", b"1").unwrap();
        conf_write(&n, false, ConfDir::Default, b"arp_notify", b"0").unwrap();
        conf_write(&n, false, ConfDir::Default, b"rp_filter", b"1").unwrap();
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"arp_notify").unwrap(),
            "1\n"
        );
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"rp_filter").unwrap(),
            "1\n"
        );
        // ip_forward 是 all/forwarding：改到每个接口
        conf_write(&n, false, ConfDir::All, b"forwarding", b"1").unwrap();
        assert_eq!(ip_forward(&n), 1);
        assert_eq!(
            conf_read(&n, false, ConfDir::Dev(i), b"forwarding").unwrap(),
            "1\n"
        );
        // IPv6：启用之后有链路本地地址，关掉 IPv6 时去掉，加地址是 EACCES
        up(&n, "d0", true);
        assert_eq!(view_of(&n, "d0").v6.len(), 1);
        assert_eq!(
            conf_read(&n, true, ConfDir::Dev(i), b"mtu").unwrap(),
            "1500\n"
        );
        assert_eq!(
            conf_read(&n, true, ConfDir::Dev(i), b"stable_secret"),
            Err(Errno::EIO)
        );
        conf_write(&n, true, ConfDir::Dev(i), b"disable_ipv6", b"1").unwrap();
        assert!(view_of(&n, "d0").v6.is_empty());
        let a6 = addr(RTM_NEWADDR, CREATE, AF_INET6, 64, i, &[0xfd; 16], None);
        assert_eq!(run(&n, &a6, &[]).unwrap_err().errno, Errno::EACCES.0);
        conf_write(&n, true, ConfDir::Dev(i), b"disable_ipv6", b"0").unwrap();
        assert_eq!(view_of(&n, "d0").v6.len(), 1);
        assert_eq!(
            conf_write(&n, true, ConfDir::Dev(i), b"mtu", b"9000"),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            conf_write(&n, true, ConfDir::Dev(i), b"hop_limit", b"x"),
            Err(Errno::EINVAL)
        );
        let dirs: Vec<Vec<u8>> = conf_dirs(&n).into_iter().map(|(d, _)| d).collect();
        assert_eq!(
            dirs,
            [
                b"all".to_vec(),
                b"default".to_vec(),
                b"lo".to_vec(),
                b"d0".to_vec()
            ]
        );
    }
}
