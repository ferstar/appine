//! socket（设计文档 §10）。
//!
//! - TCP、UDP 透传：guest 的 socket 持有一个宿主的非阻塞 socket，地址和选项按字节原样
//!   传递。guest 的 `SOCK_RAW + IPPROTO_ICMP` 换成宿主的 ping socket。
//! - DNS 存根：guest 的 `/etc/resolv.conf` 指向 `127.0.0.53`。发往 `127.0.0.53:53` 的
//!   UDP 数据报不经过宿主网络，而是交给宿主的解析器（[`appine_hal::net::dns_query`]），
//!   回答放进这个 socket 自己的队列，像是从 `127.0.0.53:53` 收到的。
//! - `NETLINK_ROUTE`：在进程内回答接口、地址、路由和规则的查询（getifaddrs、`ip addr`、
//!   `ip route`、`ip rule`，路由见 [`crate::route`]）；邻居表总是空的。
//! - AF_UNIX：完全在进程内模拟（[`crate::unix`]）。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::Duration;

use appine_hal::net::{Interface, Route};

use crate::traffic::IfStats;

/// 地址族。
pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;
pub const AF_NETLINK: i32 = 16;

/// socket 类型（不含 `SOCK_NONBLOCK`、`SOCK_CLOEXEC`）。
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_RAW: i32 = 3;

/// 一个 guest socket。
pub(crate) enum Socket {
    Inet(InetSocket),
    Netlink(NetlinkSocket),
    Unix(crate::unix::UnixSocket),
}

/// 经由宿主 socket 实现的 TCP、UDP、ping socket。
pub(crate) struct InetSocket {
    /// 宿主 fd（非阻塞），随这个对象一起关闭。
    pub fd: i32,
    pub family: i32,
    /// 宿主 socket 的类型。
    pub ty: i32,
    /// guest 建的是 `SOCK_RAW`：raw ICMP 在宿主上是 ping socket，`SO_TYPE` 和
    /// `/proc/net/raw` 按 guest 建的报告。
    pub raw: bool,
    /// 数据报 socket 用 connect 连上的对端。ping 和 raw socket 没有对端端口，宿主的
    /// getpeername 不给出对端（Linux 的 inet_getname），`/proc/net` 要用这个。
    pub peer: Option<Vec<u8>>,
    /// 用 connect 连到了 DNS 存根（宿主 socket 本身没有连接）。
    pub stub_peer: bool,
    /// DNS 存根的回答，等 guest 读走。
    pub answers: VecDeque<Vec<u8>>,
    /// 放进 `answers` 的回答数（`EPOLLET` 每来一个都触发）。
    pub answered: u64,
    /// `SO_RCVTIMEO`、`SO_SNDTIMEO`：阻塞由我们模拟，所以超时也由我们处理。
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    /// 流量算在哪个接口上（[`crate::traffic`]），本端地址定下来之后才有。
    pub route: Option<crate::traffic::Route>,
}

impl InetSocket {
    pub fn new(fd: i32, family: i32, ty: i32) -> InetSocket {
        InetSocket {
            fd,
            family,
            raw: ty == SOCK_RAW,
            ty,
            peer: None,
            stub_peer: false,
            answers: VecDeque::new(),
            answered: 0,
            rcvtimeo: None,
            sndtimeo: None,
            route: None,
        }
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        appine_hal::net::close(self.fd);
    }
}

// ---- /proc/net ----

const SOL_SOCKET: i32 = 1;
const SO_PROTOCOL: i32 = 38;
const IPPROTO_ICMP: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;
const IPPROTO_ICMPV6: i32 = 58;
const TCP_INFO: i32 = 11;
/// `/proc/net` 的 st 列（Linux 的 TCP 状态，UDP 也借用其中两个）。
const TCP_ESTABLISHED: u8 = 1;
const TCP_CLOSE: u8 = 7;
const TCP_LISTEN: u8 = 10;

/// `/proc/net` 格式的地址：IPv4 是一个 32 位字，IPv6 是四个，按内存里的字节当成小端
/// 打印（与 arm64 上的 Linux 相同）；端口按数值。
fn proc_addr(addr: Option<&[u8]>, v6: bool, port: Option<u16>) -> String {
    let addr = addr.unwrap_or_default();
    let port = port.unwrap_or_else(|| {
        addr.get(2..4)
            .map_or(0, |p| u16::from_be_bytes([p[0], p[1]]))
    });
    let (start, words) = if v6 { (8, 4) } else { (4, 1) };
    let mut s = String::new();
    for w in 0..words {
        let at = start + w * 4;
        let v = addr
            .get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        write!(s, "{v:08X}").unwrap();
    }
    write!(s, ":{port:04X}").unwrap();
    s
}

/// `/proc/net` 下列出 socket 的表。
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Table {
    Tcp,
    Udp,
    /// ping socket（`icmp`、`icmp6`）。
    Icmp,
    /// raw socket（`raw`、`raw6`）。
    Raw,
}

/// `/proc/net/{tcp,udp,icmp,raw}` 和对应的 IPv6 文件：`family` 的这一种 socket。地址和
/// TCP 状态从宿主 socket 查，队列长度和计时器都是 0。与 Linux 一样，ping 和 raw socket
/// 的对端端口总是 0，raw socket 的本端端口是协议号；IPv4 的 tcp、udp、icmp 每行补空格
/// 到固定宽度，raw 和 IPv6 的不补。
pub(crate) fn proc_listing(
    sockets: &[(crate::syscall::SocketRef, u32)],
    family: i32,
    table: Table,
) -> String {
    let (v6, tcp) = (family == AF_INET6, table == Table::Tcp);
    let width = match table {
        _ if v6 => 0,
        Table::Tcp => 149,
        Table::Udp | Table::Icmp => 127,
        Table::Raw => 0,
    };
    let address = if v6 {
        "local_address                         remote_address                        "
    } else {
        "local_address rem_address   "
    };
    let mut lines = vec![format!(
        "{}  sl  {address}st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode{}",
        if table == Table::Udp && !v6 { " " } else { "" },
        if tcp { "" } else { " ref pointer drops" },
    )];
    let int = |b: Result<Vec<u8>, i32>| {
        b.ok()
            .and_then(|b| Some(i32::from_le_bytes(b.get(..4)?.try_into().ok()?)))
    };
    let icmp = if v6 { IPPROTO_ICMPV6 } else { IPPROTO_ICMP };
    for (sock, uid) in sockets {
        let b = sock.borrow();
        let Socket::Inet(inet) = &*b else {
            continue;
        };
        let fd = inet.fd;
        let proto = int(appine_hal::net::getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, 4));
        let listed = inet.family == family
            && match table {
                Table::Tcp => inet.ty == SOCK_STREAM && proto == Some(IPPROTO_TCP),
                Table::Udp => inet.ty == SOCK_DGRAM && proto == Some(IPPROTO_UDP),
                Table::Icmp => !inet.raw && inet.ty == SOCK_DGRAM && proto == Some(icmp),
                Table::Raw => inet.raw,
            };
        if !listed {
            continue;
        }
        let local = appine_hal::net::sockname(fd, false).ok();
        let peer = appine_hal::net::sockname(fd, true)
            .ok()
            .or_else(|| inet.peer.clone());
        let state = if tcp {
            appine_hal::net::getsockopt(fd, IPPROTO_TCP, TCP_INFO, 8)
                .ok()
                .and_then(|b| b.first().copied())
                .unwrap_or(TCP_CLOSE)
        } else if peer.is_some() {
            TCP_ESTABLISHED
        } else {
            TCP_CLOSE
        };
        let ino = crate::pseudo::socket_ino(sock);
        let tail = if !tcp {
            "2 0000000000000000 0".to_owned()
        } else if state == TCP_LISTEN {
            "1 0000000000000000 100 0 0 10 0".to_owned()
        } else {
            "2 0000000000000000 20 4 30 10 -1".to_owned()
        };
        let (local_port, peer_port) = match table {
            Table::Raw => (Some(proto.unwrap_or(0) as u16), Some(0)),
            Table::Icmp => (None, Some(0)),
            _ => (None, None),
        };
        let sl = lines.len() - 1;
        lines.push(format!(
            "{sl:>w$}: {} {} {state:02X} 00000000:00000000 00:00000000 00000000 {uid:5} {:8} {ino} {tail}",
            proc_addr(local.as_deref(), v6, local_port),
            proc_addr(peer.as_deref(), v6, peer_port),
            0,
            w = if tcp || table == Table::Raw && !v6 { 4 } else { 5 },
        ));
    }
    lines.iter().map(|l| format!("{l:<width$}\n")).collect()
}

/// `/proc/net/sockstat`、`sockstat6`（`v6`）：guest 的 socket 各有多少（Linux 的
/// `sockstat_seq_show`）。TCP 的 inuse 数监听着、连接着（不是 CLOSE）的，alloc 数所有 TCP
/// socket（两个地址族一起，Linux 上是整个系统的）；UDP 数绑定了端口的，raw 数所有的，
/// ping socket 不显示。宿主内核里已经关掉、还在 TIME_WAIT 或者孤儿状态的连接这里看不到，
/// 占用的内存页数也不知道，都是 0。
pub(crate) fn sockstat(sockets: &[(crate::syscall::SocketRef, u32)], v6: bool) -> String {
    let int = |b: Result<Vec<u8>, i32>| {
        b.ok()
            .and_then(|b| Some(i32::from_le_bytes(b.get(..4)?.try_into().ok()?)))
    };
    let family = if v6 { AF_INET6 } else { AF_INET };
    let (mut tcp, mut alloc, mut udp, mut raw) = (0, 0, 0, 0);
    for (sock, _) in sockets {
        let b = sock.borrow();
        let Socket::Inet(inet) = &*b else {
            continue;
        };
        let proto = int(appine_hal::net::getsockopt(
            inet.fd,
            SOL_SOCKET,
            SO_PROTOCOL,
            4,
        ));
        if inet.ty == SOCK_STREAM && proto == Some(IPPROTO_TCP) {
            alloc += 1;
            let state = appine_hal::net::getsockopt(inet.fd, IPPROTO_TCP, TCP_INFO, 8)
                .ok()
                .and_then(|b| b.first().copied());
            if inet.family == family && state.is_some_and(|s| s != TCP_CLOSE) {
                tcp += 1;
            }
            continue;
        }
        if inet.family != family {
            continue;
        }
        if inet.raw {
            raw += 1;
        } else if inet.ty == SOCK_DGRAM && proto == Some(IPPROTO_UDP) {
            let bound = appine_hal::net::sockname(inet.fd, false)
                .is_ok_and(|a| a.get(2..4).is_some_and(|p| p != [0, 0]));
            if bound || inet.stub_peer {
                udp += 1;
            }
        }
    }
    if v6 {
        format!(
            "TCP6: inuse {tcp}\nUDP6: inuse {udp}\nUDPLITE6: inuse 0\nRAW6: inuse {raw}\n\
             FRAG6: inuse 0 memory 0\n"
        )
    } else {
        format!(
            "sockets: used {}\nTCP: inuse {tcp} orphan 0 tw 0 alloc {alloc} mem 0\n\
             UDP: inuse {udp} mem 0\nUDPLITE: inuse 0\nRAW: inuse {raw}\nFRAG: inuse 0 memory 0\n",
            sockets.len()
        )
    }
}

/// 进程内模拟的 netlink socket。只有 `NETLINK_ROUTE` 回答查询；`NETLINK_KOBJECT_UEVENT`
/// 的 socket 什么也收不到（mdev 监听的 uevent），别的协议建不了（`syscall::net::socket`）。
pub(crate) struct NetlinkSocket {
    pub protocol: i32,
    /// `nl_pid`：bind 时分配。
    pub pid: u32,
    /// 等 guest 读走的回答，每一项是一个数据报。
    pub queue: VecDeque<Vec<u8>>,
    /// 放进 `queue` 的数据报数（`EPOLLET` 每来一个都触发）。
    pub arrivals: u64,
    /// `SO_RCVTIMEO`：没有回答可读时最多等多久。
    pub rcvtimeo: Option<Duration>,
}

// ---- DNS 存根 ----

/// DNS 存根的 IPv4 地址 127.0.0.53。
const STUB_V4: [u8; 4] = [127, 0, 0, 53];

/// 是不是发往 DNS 存根（`127.0.0.53:53`，或者它的 IPv4 映射的 IPv6 地址）的地址。
pub fn is_stub(addr: &[u8]) -> bool {
    if addr.len() < 8 || addr[2..4] != 53u16.to_be_bytes() {
        return false;
    }
    match u16::from_le_bytes([addr[0], addr[1]]) as i32 {
        AF_INET => addr[4..8] == STUB_V4,
        AF_INET6 if addr.len() >= 24 => {
            addr[8..18] == [0; 10] && addr[18..20] == [0xff, 0xff] && addr[20..24] == STUB_V4
        }
        _ => false,
    }
}

/// DNS 存根的 sockaddr，按 socket 的地址族编码。
pub fn stub_addr(family: i32) -> Vec<u8> {
    use std::net::{Ipv4Addr, SocketAddr};
    let v4 = Ipv4Addr::from(STUB_V4);
    let addr = if family == AF_INET6 {
        SocketAddr::new(v4.to_ipv6_mapped().into(), 53)
    } else {
        SocketAddr::new(v4.into(), 53)
    };
    appine_hal::net::sockaddr(addr)
}

/// 解析失败时给 guest 的回答：原样带回查询，置上 QR、RA 位，RCODE 为 SERVFAIL，
/// 让解析器立即失败而不是等到超时。
pub fn servfail(query: &[u8]) -> Vec<u8> {
    let mut r = query.to_vec();
    if r.len() >= 4 {
        r[2] |= 0x80;
        r[3] = (r[3] & 0x70) | 0x80 | 2;
    }
    r
}

// ---- netlink ----

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;
const RTM_GETNEIGH: u16 = 30;
const RTM_NEWRULE: u16 = 32;
const RTM_GETRULE: u16 = 34;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_STATS: u16 = 7;
const IFLA_STATS64: u16 = 23;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IFF_LOOPBACK: u32 = 0x8;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
/// 地址不会过期（不是 DHCP、SLAAC 得到的临时地址）。
const IFA_F_PERMANENT: u8 = 0x80;
/// NETLINK_ROUTE 的协议号。
pub const NETLINK_ROUTE: i32 = 0;
/// NETLINK_KOBJECT_UEVENT 的协议号：内核发出的热插拔事件。
pub const NETLINK_KOBJECT_UEVENT: i32 = 15;
/// NETLINK_GENERIC 的协议号。
pub const NETLINK_GENERIC: i32 = 16;

fn align4(b: &mut Vec<u8>) {
    b.resize(b.len().next_multiple_of(4), 0);
}

pub(crate) fn attr(b: &mut Vec<u8>, ty: u16, data: &[u8]) {
    b.extend_from_slice(&((4 + data.len()) as u16).to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(data);
    align4(b);
}

/// 一条 netlink 消息：消息头加上内容。
pub(crate) fn message(ty: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + payload.len());
    b.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pid.to_le_bytes());
    b.extend_from_slice(payload);
    align4(&mut b);
    b
}

/// 对请求 `req`（整条消息）的确认或者错误（Linux 的 `netlink_ack`）：成功时只附上请求的
/// 消息头、带 `NLM_F_CAPPED`，出错时附上整条请求（没有设 `NETLINK_CAP_ACK`）。
fn ack(req: &[u8], err: i32, pid: u32) -> Vec<u8> {
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let mut p = (-err).to_le_bytes().to_vec();
    if err == 0 {
        p.extend_from_slice(&req[..16]);
        message(NLMSG_ERROR, NLM_F_CAPPED, seq, pid, &p)
    } else {
        p.extend_from_slice(req);
        message(NLMSG_ERROR, 0, seq, pid, &p)
    }
}

fn link_message(i: &Interface, st: IfStats) -> Vec<u8> {
    let loopback = i.flags & IFF_LOOPBACK != 0;
    // struct ifinfomsg：family、pad、type、index、flags、change
    let mut p = vec![0u8, 0];
    let ty = if loopback {
        ARPHRD_LOOPBACK
    } else {
        ARPHRD_ETHER
    };
    p.extend_from_slice(&ty.to_le_bytes());
    p.extend_from_slice(&(i.index as i32).to_le_bytes());
    p.extend_from_slice(&i.flags.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let mut name = i.name.clone().into_bytes();
    name.push(0);
    attr(&mut p, IFLA_IFNAME, &name);
    attr(&mut p, IFLA_MTU, &i.mtu.to_le_bytes());
    let mac = i.mac.unwrap_or([0; 6]);
    attr(&mut p, IFLA_ADDRESS, &mac);
    let bcast = if loopback { [0; 6] } else { [0xff; 6] };
    attr(&mut p, IFLA_BROADCAST, &bcast);
    // 流量计数（crate::traffic）：struct rtnl_link_stats 是 24 个 u32，rtnl_link_stats64
    // 在 Linux 6.1 是 25 个 u64；前四项依次是收、发的包数和收、发的字节数，别的都是 0
    let head = [st.rx_packets, st.tx_packets, st.rx_bytes, st.tx_bytes];
    let field = |n: usize| head.get(n).copied().unwrap_or(0);
    let stats: Vec<u8> = (0..24)
        .flat_map(|n| (field(n) as u32).to_le_bytes())
        .collect();
    attr(&mut p, IFLA_STATS, &stats);
    let stats64: Vec<u8> = (0..25).flat_map(|n| field(n).to_le_bytes()).collect();
    attr(&mut p, IFLA_STATS64, &stats64);
    p
}

fn addr_messages(i: &Interface, family: u8) -> Vec<Vec<u8>> {
    i.addrs
        .iter()
        .filter(|(f, ..)| family == 0 || *f as u8 == family)
        .map(|(f, addr, prefix)| {
            let scope = if i.flags & IFF_LOOPBACK != 0 {
                RT_SCOPE_HOST
            } else if *f == AF_INET6 && addr[0] == 0xfe && addr[1] & 0xc0 == 0x80 {
                RT_SCOPE_LINK
            } else {
                RT_SCOPE_UNIVERSE
            };
            // struct ifaddrmsg：family、prefixlen、flags、scope、index
            let mut p = vec![*f as u8, *prefix, IFA_F_PERMANENT, scope];
            p.extend_from_slice(&i.index.to_le_bytes());
            attr(&mut p, IFA_ADDRESS, addr);
            if *f == AF_INET {
                attr(&mut p, IFA_LOCAL, addr);
            }
            let mut name = i.name.clone().into_bytes();
            name.push(0);
            attr(&mut p, IFA_LABEL, &name);
            p
        })
        .collect()
}

// ---- NETLINK_GENERIC ----
//
// 只有 Linux 总会有的 nlctrl 一个族（GENL_ID_CTRL）。nl80211、ethtool、wireguard、TASKSTATS
// 这些子系统都没有，查它们得到 ENOENT，与没有编进它们的 Linux 一样：iw、ethtool、wg、iotop
// 照它报错或者退回别的做法（原来 socket 就是 EPROTONOSUPPORT，有的程序不预期它）。回答的
// 内容、错误码和错误消息里附上的请求与 Linux 6.8 逐字节相同（差分测试）。

const GENL_ID_CTRL: u16 = 0x10;
/// 小于它的消息类型是 netlink 自己的控制消息（`NLMSG_NOOP` 等）。
const NLMSG_MIN_TYPE: u16 = 0x10;
/// 确认消息只附上请求的消息头。
const NLM_F_CAPPED: u16 = 0x100;
const CTRL_CMD_NEWFAMILY: u8 = 1;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_CMD_GETPOLICY: u8 = 10;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_VERSION: u16 = 3;
const CTRL_ATTR_HDRSIZE: u16 = 4;
const CTRL_ATTR_MAXATTR: u16 = 5;
const CTRL_ATTR_OPS: u16 = 6;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_OP_ID: u16 = 1;
const CTRL_ATTR_OP_FLAGS: u16 = 2;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;
const GENL_CMD_CAP_DO: u32 = 2;
const GENL_CMD_CAP_DUMP: u32 = 4;
const GENL_CMD_CAP_HASPOL: u32 = 8;
const NLA_TYPE_MASK: u16 = 0x3fff;
const ENOENT: i32 = 2;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// nlctrl 的 `CTRL_CMD_NEWFAMILY`：genlmsghdr 和属性，与 Linux 6.8 的 `ctrl_fill_info` 相同
/// （6.2 起 nlctrl 的命令各有自己的策略，族的 maxattr 是 0）。
fn nlctrl_family() -> Vec<u8> {
    let mut p = vec![CTRL_CMD_NEWFAMILY, 2, 0, 0];
    attr(&mut p, CTRL_ATTR_FAMILY_NAME, b"nlctrl\0");
    attr(&mut p, CTRL_ATTR_FAMILY_ID, &GENL_ID_CTRL.to_le_bytes());
    attr(&mut p, CTRL_ATTR_VERSION, &2u32.to_le_bytes());
    attr(&mut p, CTRL_ATTR_HDRSIZE, &0u32.to_le_bytes());
    attr(&mut p, CTRL_ATTR_MAXATTR, &0u32.to_le_bytes());
    let caps = [
        (
            CTRL_CMD_GETFAMILY,
            GENL_CMD_CAP_DO | GENL_CMD_CAP_DUMP | GENL_CMD_CAP_HASPOL,
        ),
        (CTRL_CMD_GETPOLICY, GENL_CMD_CAP_DUMP | GENL_CMD_CAP_HASPOL),
    ];
    let mut ops = Vec::new();
    for (n, (cmd, flags)) in caps.into_iter().enumerate() {
        let mut op = Vec::new();
        attr(&mut op, CTRL_ATTR_OP_ID, &u32::from(cmd).to_le_bytes());
        attr(&mut op, CTRL_ATTR_OP_FLAGS, &flags.to_le_bytes());
        attr(&mut ops, n as u16 + 1, &op);
    }
    attr(&mut p, CTRL_ATTR_OPS, &ops);
    let mut group = Vec::new();
    attr(
        &mut group,
        CTRL_ATTR_MCAST_GRP_ID,
        &u32::from(GENL_ID_CTRL).to_le_bytes(),
    );
    attr(&mut group, CTRL_ATTR_MCAST_GRP_NAME, b"notify\0");
    let mut groups = Vec::new();
    attr(&mut groups, 1, &group);
    attr(&mut p, CTRL_ATTR_MCAST_GROUPS, &groups);
    p
}

/// 请求 `req`（消息头之后是 genlmsghdr 和属性）指的族是不是 nlctrl：按 `CTRL_ATTR_FAMILY_ID`
/// 或者 `CTRL_ATTR_FAMILY_NAME` 找（Linux 的 `ctrl_getfamily`），两个都没有是 EINVAL，找不到
/// 是 ENOENT。
fn names_nlctrl(req: &[u8]) -> Result<(), i32> {
    let (mut id, mut name) = (None, None);
    let mut b = req.get(20..).unwrap_or_default();
    while b.len() >= 4 {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        if len < 4 || len > b.len() {
            break;
        }
        let data = &b[4..len];
        match u16::from_le_bytes([b[2], b[3]]) & NLA_TYPE_MASK {
            CTRL_ATTR_FAMILY_ID if data.len() >= 2 => {
                id = Some(u16::from_le_bytes([data[0], data[1]]))
            }
            CTRL_ATTR_FAMILY_NAME => {
                name = Some(data.split(|&c| c == 0).next().unwrap_or_default());
            }
            _ => {}
        }
        b = &b[len.next_multiple_of(4).min(b.len())..];
    }
    match (id, name) {
        (Some(GENL_ID_CTRL), _) | (_, Some(b"nlctrl")) => Ok(()),
        (None, None) => Err(EINVAL),
        _ => Err(ENOENT),
    }
}

/// 回答 `NETLINK_GENERIC` 上的一条请求 `req`（整条消息），`pid` 是这个 socket 的 `nl_pid`。
/// 返回各个数据报：与 Linux 一样，回答和确认各是一个（`genlmsg_reply`、`netlink_ack`），
/// 程序读一次只拿到回答。
fn generic_request(req: &[u8], pid: u32) -> Vec<Vec<u8>> {
    let ty = u16::from_le_bytes([req[4], req[5]]);
    let flags = u16::from_le_bytes([req[6], req[7]]);
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let reply = |ty, flags, payload: &[u8]| message(ty, flags, seq, pid, payload);
    let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
    // Linux 的 netlink_rcv_skb：不是请求的、控制消息只在要确认时确认
    let r = if flags & NLM_F_REQUEST == 0 || ty < NLMSG_MIN_TYPE {
        Ok(None)
    } else if ty != GENL_ID_CTRL {
        Err(ENOENT)
    } else if req.len() < 20 {
        Err(EINVAL)
    } else {
        match req[16] {
            // 转储很小，与 Linux 一样 NLMSG_DONE 跟在同一个数据报里
            CTRL_CMD_GETFAMILY if dump => {
                let mut out = reply(GENL_ID_CTRL, NLM_F_MULTI, &nlctrl_family());
                out.extend(reply(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes()));
                Ok(Some(out))
            }
            CTRL_CMD_GETFAMILY => {
                names_nlctrl(req).map(|()| Some(reply(GENL_ID_CTRL, 0, &nlctrl_family())))
            }
            // 策略的转储没有做：只认得出要的族不存在
            CTRL_CMD_GETPOLICY if dump => names_nlctrl(req).and(Err(EOPNOTSUPP)),
            _ => Err(EOPNOTSUPP),
        }
    };
    match r {
        // 转储之后不另外确认（Linux 的 netlink_dump_start 让 netlink_rcv_skb 跳过确认）
        Ok(answer) => {
            let mut out: Vec<Vec<u8>> = answer.into_iter().collect();
            if flags & NLM_F_ACK != 0 && (!dump || out.is_empty()) {
                out.push(ack(req, 0, pid));
            }
            out
        }
        Err(e) => vec![ack(req, e, pid)],
    }
}

/// netlink 的回答要用到的宿主信息（测试里换成固定的）。
pub(crate) trait Host {
    /// 宿主的网络接口。
    fn interfaces(&self) -> Vec<Interface>;
    /// 接口的流量计数（[`crate::traffic`]）。
    fn stats(&self, name: &str) -> IfStats;
    /// 宿主主表里的路由，None 时按接口的地址合成（[`crate::route`]）。
    fn routes(&self) -> Option<Vec<Route>>;
    /// 发请求的进程的用户 ID（`ip route get` 回答里的 RTA_UID）。
    fn uid(&self) -> u32;
}

impl NetlinkSocket {
    pub fn new(protocol: i32) -> NetlinkSocket {
        NetlinkSocket {
            protocol,
            pid: 0,
            queue: VecDeque::new(),
            arrivals: 0,
            rcvtimeo: None,
        }
    }

    /// 处理 guest 发来的一个数据报（可能含多条请求），把回答放进队列。
    pub fn handle(&mut self, data: &[u8], host: &dyn Host) {
        if !matches!(self.protocol, NETLINK_ROUTE | NETLINK_GENERIC) {
            return;
        }
        let mut off = 0;
        while off + 16 <= data.len() {
            let h = &data[off..];
            let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
            if len < 16 || off + len > data.len() {
                break;
            }
            let answers = if self.protocol == NETLINK_GENERIC {
                generic_request(&h[..len], self.pid)
            } else {
                self.route_request(&h[..len], host)
            };
            for d in answers {
                self.queue.push_back(d);
                self.arrivals += 1;
            }
            off += len.next_multiple_of(4);
        }
    }

    /// 回答 `NETLINK_ROUTE` 上的一条请求 `req`（整条消息），返回各个数据报。
    fn route_request(&self, req: &[u8], host: &dyn Host) -> Vec<Vec<u8>> {
        let ty = u16::from_le_bytes([req[4], req[5]]);
        let flags = u16::from_le_bytes([req[6], req[7]]);
        let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
        let family = req.get(16).copied().unwrap_or(0) as i32;
        let reply = |ty, flags, payload: &[u8]| message(ty, flags, seq, self.pid, payload);
        let done = || reply(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes());
        let dump = flags & NLM_F_DUMP != 0;
        match ty {
            RTM_GETLINK | RTM_GETADDR if dump => {
                let mut out = Vec::new();
                for i in &host.interfaces() {
                    if ty == RTM_GETLINK {
                        let st = host.stats(&i.name);
                        out.extend(reply(RTM_NEWLINK, NLM_F_MULTI, &link_message(i, st)));
                    } else {
                        for m in addr_messages(i, family as u8) {
                            out.extend(reply(RTM_NEWADDR, NLM_F_MULTI, &m));
                        }
                    }
                }
                out.extend(done());
                vec![out]
            }
            // 与 Linux 一样，每个地址族的路由是一个数据报，NLMSG_DONE 另是一个
            RTM_GETROUTE if dump => {
                let entries = crate::route::tables(&host.interfaces(), host.routes());
                let mut out = Vec::new();
                for f in [AF_INET, AF_INET6] {
                    let d: Vec<u8> = entries
                        .iter()
                        .filter(|e| e.family == f && (family == 0 || family == f))
                        .flat_map(|e| {
                            reply(RTM_NEWROUTE, NLM_F_MULTI, &crate::route::route_message(e))
                        })
                        .collect();
                    if !d.is_empty() {
                        out.push(d);
                    }
                }
                out.push(done());
                out
            }
            RTM_GETROUTE => {
                let ifaces = host.interfaces();
                let entries = crate::route::tables(&ifaces, host.routes());
                match crate::route::get(req, &entries, &ifaces, host.uid()) {
                    Ok(p) => {
                        let mut out = vec![reply(RTM_NEWROUTE, 0, &p)];
                        if flags & NLM_F_ACK != 0 {
                            out.push(ack(req, 0, self.pid));
                        }
                        out
                    }
                    Err(e) => vec![ack(req, e, self.pid)],
                }
            }
            // 规则一个数据报装得下，NLMSG_DONE 另是一个
            RTM_GETRULE if dump => {
                const EAFNOSUPPORT: i32 = 97;
                let rules = crate::route::rule_messages(family);
                if rules.is_empty() {
                    return vec![ack(req, EAFNOSUPPORT, self.pid)];
                }
                let d = rules
                    .iter()
                    .flat_map(|m| reply(RTM_NEWRULE, NLM_F_MULTI, m))
                    .collect();
                vec![d, done()]
            }
            // 邻居表（ARP、NDP）：Android 上 App 看不到宿主的，总是空的
            RTM_GETNEIGH if dump => vec![done()],
            RTM_GETNEIGH => vec![ack(req, ENOENT, self.pid)],
            // 不支持的请求：EOPNOTSUPP；控制消息只在要确认时确认
            _ if flags & NLM_F_REQUEST != 0 => {
                if ty < NLMSG_MIN_TYPE {
                    if flags & NLM_F_ACK != 0 {
                        vec![ack(req, 0, self.pid)]
                    } else {
                        Vec::new()
                    }
                } else {
                    vec![ack(req, EOPNOTSUPP, self.pid)]
                }
            }
            _ => Vec::new(),
        }
    }

    /// 这个 socket 的 `sockaddr_nl`。
    pub fn sockaddr(&self) -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
        b[4..8].copy_from_slice(&self.pid.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lo() -> Interface {
        Interface {
            name: "lo".into(),
            index: 1,
            flags: 0x49,
            mtu: 65536,
            mac: None,
            addrs: vec![
                (AF_INET, vec![127, 0, 0, 1], 8),
                (AF_INET6, vec![0; 16], 128),
            ],
        }
    }

    /// 消息里的属性：类型 → 内容。
    fn attributes(mut b: &[u8]) -> std::collections::BTreeMap<u16, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        while b.len() >= 4 {
            let len = u16::from_le_bytes([b[0], b[1]]) as usize;
            let ty = u16::from_le_bytes([b[2], b[3]]);
            out.insert(ty, b[4..len].to_vec());
            b = &b[len.next_multiple_of(4).min(b.len())..];
        }
        out
    }

    /// 固定的宿主：只有 lo，`stats` 是它的流量计数，宿主给出的路由是 `routes`。
    struct Fake {
        stats: IfStats,
        routes: Option<Vec<Route>>,
    }

    impl Host for Fake {
        fn interfaces(&self) -> Vec<Interface> {
            vec![lo()]
        }
        fn stats(&self, name: &str) -> IfStats {
            if name == "lo" {
                self.stats
            } else {
                IfStats::default()
            }
        }
        fn routes(&self) -> Option<Vec<Route>> {
            self.routes.clone()
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    const NOTHING: Fake = Fake {
        stats: IfStats {
            rx_bytes: 0,
            rx_packets: 0,
            tx_bytes: 0,
            tx_packets: 0,
        },
        routes: Some(Vec::new()),
    };

    /// 按 musl 的 `__rtnetlink_enumerate` 解析回答：返回 (消息类型, 消息内容) 直到 DONE。
    fn parse(datagram: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 16 <= datagram.len() {
            let len = u32::from_le_bytes(datagram[off..off + 4].try_into().unwrap()) as usize;
            let ty = u16::from_le_bytes([datagram[off + 4], datagram[off + 5]]);
            out.push((ty, datagram[off + 16..off + len].to_vec()));
            off += len.next_multiple_of(4);
        }
        out
    }

    #[test]
    fn dumps_links_and_addresses() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE);
        s.pid = 42;
        let mut req = message(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 7, 0, &[0, 0, 0, 0]);
        req.extend(message(
            RTM_GETADDR,
            NLM_F_REQUEST | NLM_F_DUMP,
            8,
            0,
            &[AF_INET as u8, 0, 0, 0],
        ));
        let st = IfStats {
            rx_bytes: 1000,
            rx_packets: 3,
            tx_bytes: 0x1_0000_0002,
            tx_packets: 4,
        };
        s.handle(
            &req,
            &Fake {
                stats: st,
                routes: None,
            },
        );
        let links = parse(&s.queue.pop_front().unwrap());
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].0, RTM_NEWLINK);
        assert!(links[0].1.windows(3).any(|w| w == b"lo\0"));
        // IFLA_STATS 截成 32 位，IFLA_STATS64 是完整的
        let attrs = attributes(&links[0].1[16..]);
        let u32s = |n: usize| -> Vec<u32> {
            attrs[&IFLA_STATS][..n * 4]
                .chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect()
        };
        assert_eq!(attrs[&IFLA_STATS].len(), 96);
        assert_eq!(u32s(5), [3, 4, 1000, 2, 0]);
        assert_eq!(attrs[&IFLA_STATS64].len(), 200);
        assert_eq!(attrs[&IFLA_STATS64][24..32], 0x1_0000_0002u64.to_le_bytes());
        assert_eq!(links[1].0, NLMSG_DONE);
        // 只要 IPv4 地址
        let addrs = parse(&s.queue.pop_front().unwrap());
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].1[..2], [AF_INET as u8, 8]);
        assert!(addrs[0].1.windows(4).any(|w| w == [127, 0, 0, 1]));
    }

    /// 路由、规则的转储：每个地址族一个数据报，NLMSG_DONE 另是一个；邻居表是空的；不支持的
    /// 请求的错误附上整条请求，确认只附上消息头。
    #[test]
    fn dumps_routes_rules_and_neighbours() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE);
        s.pid = 42;
        let mut ask = |req: &[u8]| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(req, &NOTHING);
            s.queue.drain(..).map(|d| parse(&d)).collect()
        };
        let dump = |ty, family: i32| {
            message(
                ty,
                NLM_F_REQUEST | NLM_F_DUMP,
                7,
                0,
                &[family as u8, 0, 0, 0],
            )
        };
        // lo 上：IPv4 的本地表三条，IPv6 的 ::1 一条
        let r = ask(&dump(RTM_GETROUTE, 0));
        let counts: Vec<(usize, u16)> = r.iter().map(|d| (d.len(), d[0].0)).collect();
        assert_eq!(
            counts,
            [(3, RTM_NEWROUTE), (1, RTM_NEWROUTE), (1, NLMSG_DONE)]
        );
        let r = ask(&dump(RTM_GETROUTE, AF_INET6));
        assert_eq!(r.len(), 2);
        let r = ask(&dump(RTM_GETRULE, AF_INET));
        assert_eq!((r.len(), r[0].len(), r[1][0].0), (2, 3, NLMSG_DONE));
        let r = ask(&dump(RTM_GETNEIGH, 0));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][0].0, NLMSG_DONE);
        // 不支持的请求
        let req = message(
            RTM_NEWROUTE,
            NLM_F_REQUEST,
            9,
            0,
            &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        let r = ask(&req);
        assert_eq!(r[0][0].0, NLMSG_ERROR);
        assert_eq!(r[0][0].1[..4], (-95i32).to_le_bytes());
        assert_eq!(&r[0][0].1[4..], &req[..]);
        // 要确认的 NLMSG_NOOP
        let r = ask(&message(1, NLM_F_REQUEST | NLM_F_ACK, 9, 0, &[]));
        assert_eq!(r[0][0].1.len(), 4 + 16);
        assert_eq!(r[0][0].1[..4], 0i32.to_le_bytes());
    }

    /// 一条 NETLINK_GENERIC 请求：genlmsghdr 之后是属性。
    fn genl(ty: u16, flags: u16, cmd: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut p = vec![cmd, 1, 0, 0];
        for (t, d) in attrs {
            attr(&mut p, *t, d);
        }
        message(ty, NLM_F_REQUEST | flags, 9, 0, &p)
    }

    #[test]
    fn generic_netlink_has_only_nlctrl() {
        let mut s = NetlinkSocket::new(NETLINK_GENERIC);
        s.pid = 42;
        // 回答的各个数据报，每个解析成消息
        let mut ask = |req: &[u8]| -> Vec<Vec<(u16, Vec<u8>)>> {
            s.handle(req, &NOTHING);
            s.queue.drain(..).map(|d| parse(&d)).collect()
        };
        // 按名字查 nlctrl，要确认：回答和确认是两个数据报，确认只附上请求的消息头
        let name: &[u8] = b"nlctrl\0";
        let r = ask(&genl(
            GENL_ID_CTRL,
            NLM_F_ACK,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_NAME, name)],
        ));
        assert_eq!(r.len(), 2);
        let (ty, reply) = &r[0][0];
        assert_eq!(
            (*ty, &reply[..2]),
            (GENL_ID_CTRL, &[CTRL_CMD_NEWFAMILY, 2][..])
        );
        let a = attributes(&reply[4..]);
        assert_eq!(a[&CTRL_ATTR_FAMILY_ID], GENL_ID_CTRL.to_le_bytes());
        assert_eq!(a[&CTRL_ATTR_FAMILY_NAME], name);
        let (ty, ack) = &r[1][0];
        assert_eq!(*ty, NLMSG_ERROR);
        assert_eq!(ack.len(), 4 + 16);
        assert_eq!(ack[..4], 0i32.to_le_bytes());
        // 按编号查也一样
        let id = GENL_ID_CTRL.to_le_bytes();
        let r = ask(&genl(
            GENL_ID_CTRL,
            0,
            CTRL_CMD_GETFAMILY,
            &[(CTRL_ATTR_FAMILY_ID, &id)],
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][0].0, GENL_ID_CTRL);
        // 出错时附上整条请求
        let unknown: &[u8] = b"nl80211\0";
        let cases: [(Vec<u8>, i32); 5] = [
            (
                genl(
                    GENL_ID_CTRL,
                    0,
                    CTRL_CMD_GETFAMILY,
                    &[(CTRL_ATTR_FAMILY_NAME, unknown)],
                ),
                -2,
            ),
            (genl(0x7ff0, 0, 1, &[]), -2),
            (genl(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, &[]), -22),
            (genl(GENL_ID_CTRL, 0, 99, &[]), -95),
            (
                genl(
                    GENL_ID_CTRL,
                    NLM_F_DUMP,
                    CTRL_CMD_GETPOLICY,
                    &[(CTRL_ATTR_FAMILY_NAME, unknown)],
                ),
                -2,
            ),
        ];
        for (req, err) in cases {
            let r = ask(&req);
            assert_eq!(r.len(), 1);
            let (ty, e) = &r[0][0];
            assert_eq!(*ty, NLMSG_ERROR);
            assert_eq!(e[..4], err.to_le_bytes());
            assert_eq!(&e[4..], &req[..]);
        }
        // 转储只有 nlctrl，NLMSG_DONE 在同一个数据报里，之后不另外确认
        let r = ask(&genl(
            GENL_ID_CTRL,
            NLM_F_DUMP | NLM_F_ACK,
            CTRL_CMD_GETFAMILY,
            &[],
        ));
        assert_eq!(r.len(), 1);
        let types: Vec<u16> = r[0].iter().map(|m| m.0).collect();
        assert_eq!(types, [GENL_ID_CTRL, NLMSG_DONE]);
    }

    #[test]
    fn recognizes_the_dns_stub() {
        assert!(is_stub(&stub_addr(AF_INET)));
        assert!(is_stub(&stub_addr(AF_INET6)));
        let other = appine_hal::net::sockaddr("127.0.0.53:54".parse().unwrap());
        assert!(!is_stub(&other));
        let fail = servfail(&[0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(fail[..4], [0x12, 0x34, 0x81, 0x82]);
    }
}
