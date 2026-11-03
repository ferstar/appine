//! socket（kernel/net 的 map）。
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
use std::rc::Rc;
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
    /// 协议号：guest 建 socket 时给的，给的是 0 时按类型取默认的 TCP、UDP。
    pub proto: i32,
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
    /// TCP 发起的连接已经数进了 ActiveOpens、还没有数过它失败（[`crate::traffic::Protocols`]）。
    pub opening: bool,
    /// 建在新的网络命名空间里：地址要换算（[`crate::netns`]）；None 是宿主的网络。
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// 新的网络命名空间里：宿主 socket 已经绑定了（guest 绑的，或者 connect、sendto、
    /// listen 之前替它绑的）。
    pub bound: bool,
    /// 新的网络命名空间里：本端是通配地址、还没有连接（getsockname 报告通配地址）。
    pub any: bool,
    /// 新的网络命名空间里：绑在自己的虚拟地址上（宿主上与通配地址一样是 127.a.b.1，
    /// getsockname 报告它，[`crate::netns`]）。
    pub vaddr: Option<[u8; 4]>,
    /// connect 给的对端（guest 的地址）：getpeername 报告它。
    pub guest_peer: Option<Vec<u8>>,
    /// 新的网络命名空间里宿主 socket 绑在通配地址上（数据报 socket 显式绑通配地址、端口
    /// 为 0 的，第一次发往宿主网络时由宿主自动绑定的）：发得到外面。发往映射的目的地时
    /// 用 `IP_PKTINFO` 把源地址指定成这个命名空间的回环地址（[`crate::netns`]）。
    pub wide: bool,
    /// `wide` 的数据报 socket 连到了映射的目的地（宿主上的地址）：宿主 socket 不连接（连了
    /// 本端就定成宿主选的 127.0.0.1，回到这个命名空间的回环地址的数据报对不上），不带地址的
    /// 发送发往它，收到的只要来自它的（Linux 已连接的 UDP）。
    pub vconn: Option<Vec<u8>>,
    /// nftables 的 nat、过滤要记下的（[`Track`]），用到时才分配。
    pub track: Option<Box<Track>>,
    /// 开机时的命名空间里监听着的、PREROUTING 有 DNAT 的端口：从宿主外面连进来的由转发线程
    /// 转给目的地（[`crate::forward`]）。
    pub fwd: Option<Box<crate::forward::Handle>>,
    /// guest 调用过 listen（MPTCP 的宿主 socket 上取不到 `SO_ACCEPTCONN`）。
    pub listening: bool,
    /// 新的网络命名空间里的监听登记在命名空间里的编号（[`crate::nsproxy::Listener`]），
    /// 关掉时去掉。
    pub listener: Option<std::num::NonZeroU64>,
    /// UDP：宿主上绑在 127.a.b.254 上（发给自己的虚拟地址的），端口登记在命名空间的
    /// [`crate::nsproxy::Loopback::self_udp`] 里，关掉时去掉。
    pub self_port: Option<u16>,
    /// 新的网络命名空间里的 `IPV6_V6ONLY`：宿主 socket 要用 IPv4 映射的地址，不设它。
    pub v6only: bool,
    /// guest 建的是 raw ICMP socket：收到的报文经由汇集点，改成 raw socket 上的样子
    /// （[`crate::rawicmp`]）。
    pub icmp: Option<crate::rawicmp::RawIcmp>,
    /// UDP：guest 设过 `IP_TTL`、`IPV6_UNICAST_HOPS`（traceroute 的探测包），自己打开了
    /// `IP_RECVERR`、`IPV6_RECVERR`，以及看着它的错误的汇集点（[`crate::rawicmp::before_io`]）。
    pub ttl_set: bool,
    pub guest_recverr: bool,
    pub udp_tap: Option<Rc<crate::cell::RefCell<crate::rawicmp::Tap>>>,
    /// 汇集点替这个 socket 取走了宿主上的 ICMP 错误，按 Linux 的规矩要报告给 guest 的那个
    /// （errno，0 是没有）：下一次收发、`SO_ERROR` 取走它（Linux 的 `sk_err`）。
    pub icmp_err: std::cell::Cell<i32>,
    /// 绑的是映射了的低端口（[`crate::portmap`]）：租约，以及 (guest 端口, 宿主端口)。
    /// accept 出来的连接只有后者（本端是监听的那个端口）。
    pub lease: Option<crate::portmap::Lease>,
    pub local_map: Option<(u16, u16)>,
    /// 连到了本机上映射了的低端口：(guest 端口, 宿主端口)。
    pub peer_map: Option<(u16, u16)>,
}

impl InetSocket {
    pub fn new(fd: i32, family: i32, ty: i32) -> InetSocket {
        InetSocket {
            fd,
            family,
            raw: ty == SOCK_RAW,
            ty,
            proto: match ty {
                SOCK_STREAM => IPPROTO_TCP,
                SOCK_DGRAM => IPPROTO_UDP,
                _ => 0,
            },
            peer: None,
            stub_peer: false,
            answers: VecDeque::new(),
            answered: 0,
            rcvtimeo: None,
            sndtimeo: None,
            route: None,
            opening: false,
            net: None,
            bound: false,
            any: false,
            vaddr: None,
            guest_peer: None,
            wide: false,
            vconn: None,
            track: None,
            fwd: None,
            listening: false,
            listener: None,
            self_port: None,
            v6only: false,
            icmp: None,
            ttl_set: false,
            guest_recverr: false,
            udp_tap: None,
            icmp_err: std::cell::Cell::new(0),
            lease: None,
            local_map: None,
            peer_map: None,
        }
    }
}

impl InetSocket {
    /// ICMPv6 的 socket（ping socket，或者模拟 raw socket 的那个）：在新的网络命名空间里
    /// 用不了 IPv4 映射的地址（[`crate::netns::to_host`]）。
    pub fn icmp6(&self) -> bool {
        self.family == AF_INET6 && self.proto == IPPROTO_ICMPV6
    }
}

/// 一个 socket 上 Linux 由连接跟踪记着的东西（[`crate::nft`]，docs/rounds/175-filter.md）。
#[derive(Debug, Default)]
pub(crate) struct Track {
    /// nat 换过目的地址的：(换成的, guest 给的)，都是 guest 的 sockaddr。从前者收到的数据报
    /// 报告成来自后者。
    pub nat_back: Vec<(Vec<u8>, Vec<u8>)>,
    /// UDP：收到过数据报的对方（guest 的 sockaddr）。回给它们的是 ESTABLISHED 的，过滤链里
    /// `ct state established` 的规则认它们。
    pub seen: Vec<Vec<u8>>,
    /// 连接了的 UDP：连的目的地被过滤链丢弃了。不带地址的发送不发出去：0 是悄悄丢掉，别的
    /// 是错误码（OUTPUT 上的丢弃是 EPERM）。
    pub blocked: Option<i32>,
    /// accept 出来的 TCP 连接：发起的一端被 nat 换过目的地址时，这个命名空间里看到的原目的
    /// 地址（guest 的 sockaddr，`SO_ORIGINAL_DST` 报告它，[`crate::origdst`]）。
    pub orig_dst: Option<Vec<u8>>,
    /// `SO_MARK`：只记在这里，不设到宿主 socket 上（App 在宿主上没有这个权限，Android 的 netd
    /// 也用宿主上的标记选网络）。本机发出的包在 nftables 里带着它（[`crate::nft::Packet::mark`]）。
    pub mark: u32,
}

impl InetSocket {
    pub(crate) fn track_mut(&mut self) -> &mut Track {
        self.track.get_or_insert_with(Default::default)
    }

    /// `SO_MARK`（[`Track::mark`]）。
    pub(crate) fn mark(&self) -> u32 {
        self.track.as_ref().map_or(0, |t| t.mark)
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        appine_hal::net::close(self.fd);
        if let (Some(p), Some(n)) = (self.self_port, &self.net)
            && let Ok(mut lo) = n.lo.try_borrow_mut()
            && let Some(i) = lo.self_udp.iter().position(|&x| x == p)
        {
            lo.self_udp.swap_remove(i);
        }
        if let (Some(id), Some(n)) = (self.listener, &self.net)
            && let Ok(mut l) = n.listeners.try_borrow_mut()
        {
            l.retain(|x| x.id != id.get());
        }
    }
}

// ---- /proc/net ----

const SOL_SOCKET: i32 = 1;
const SO_PROTOCOL: i32 = 38;
pub(crate) const IPPROTO_ICMP: i32 = 1;
pub(crate) const IPPROTO_TCP: i32 = 6;
pub(crate) const IPPROTO_UDP: i32 = 17;
/// MPTCP 的协议号（`socket(2)` 的第三个参数，`SO_PROTOCOL` 报告它）。
pub(crate) const IPPROTO_MPTCP: i32 = 262;
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
    portmap: &crate::cell::RefCell<crate::portmap::PortMap>,
    w: &crate::netns::World,
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
                // MPTCP 的 socket 也列在这里（第一个子流是 TCP socket；Go 1.24 起监听默认用它）
                Table::Tcp => {
                    inet.ty == SOCK_STREAM && matches!(proto, Some(IPPROTO_TCP | IPPROTO_MPTCP))
                }
                Table::Udp => inet.ty == SOCK_DGRAM && proto == Some(IPPROTO_UDP),
                Table::Icmp => !inet.raw && inet.ty == SOCK_DGRAM && proto == Some(icmp),
                Table::Raw => inet.raw,
            };
        if !listed {
            continue;
        }
        // 地址换成 guest 看到的（crate::netns），映射了的低端口报告 guest 的（crate::portmap）
        let me = inet.net.clone().unwrap_or_else(|| w.all[0].clone());
        let unmap =
            |a: Vec<u8>, local: bool| crate::syscall::unmap_addr(portmap, w, inet, a, local);
        let host_peer = appine_hal::net::sockname(fd, true).ok();
        let local = appine_hal::net::sockname(fd, false).ok().map(|a| {
            let role = crate::netns::Role::Local {
                any: inet.any,
                vaddr: inet.vaddr,
                peer: host_peer.as_deref(),
            };
            crate::netns::from_host(w, &me, &unmap(a, true), role, inet.icmp6())
        });
        let peer = host_peer
            .clone()
            .map(|a| {
                let role = crate::netns::Role::Peer;
                crate::netns::from_host(w, &me, &unmap(a, false), role, inet.icmp6())
            })
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

/// `/proc/net/snmp` 里 TCP 的字段（Linux 6.1 的 `snmp4_tcp_list`）。
const SNMP_TCP: [&str; 15] = [
    "RtoAlgorithm",
    "RtoMin",
    "RtoMax",
    "MaxConn",
    "ActiveOpens",
    "PassiveOpens",
    "AttemptFails",
    "EstabResets",
    "CurrEstab",
    "InSegs",
    "OutSegs",
    "RetransSegs",
    "InErrs",
    "OutRsts",
    "InCsumErrors",
];

/// UDP、UDP-Lite 的字段（`snmp4_udp_list`）。
const SNMP_UDP: [&str; 9] = [
    "InDatagrams",
    "NoPorts",
    "InErrors",
    "OutDatagrams",
    "RcvbufErrors",
    "SndbufErrors",
    "InCsumErrors",
    "IgnoredMulti",
    "MemErrors",
];

/// IP 的字段（`snmp4_ipstats_list`），前面还有 Forwarding、DefaultTTL。
const SNMP_IP: [&str; 17] = [
    "InReceives",
    "InHdrErrors",
    "InAddrErrors",
    "ForwDatagrams",
    "InUnknownProtos",
    "InDiscards",
    "InDelivers",
    "OutRequests",
    "OutDiscards",
    "OutNoRoutes",
    "ReasmTimeout",
    "ReasmReqds",
    "ReasmOKs",
    "ReasmFails",
    "FragOKs",
    "FragFails",
    "FragCreates",
];

/// ICMP 按类型的字段和类型号（`icmpmibmap`），收、发各一组。
const SNMP_ICMP: [(&str, u8); 11] = [
    ("DestUnreachs", 3),
    ("TimeExcds", 11),
    ("ParmProbs", 12),
    ("SrcQuenchs", 4),
    ("Redirects", 5),
    ("Echos", 8),
    ("EchoReps", 0),
    ("Timestamps", 13),
    ("TimestampReps", 14),
    ("AddrMasks", 17),
    ("AddrMaskReps", 18),
];
const ICMP_ECHOREPLY: u8 = 0;
const ICMP_ECHO: u8 = 8;

/// `/proc/net/snmp`（Linux 6.1 的 `snmp_seq_show`）：IPv4 的 IP、ICMP 和 TCP、UDP 的计数，
/// 每一组一行字段名、一行数值。计数取自 [`crate::traffic::Protocols`]，CurrEstab 是现在
/// ESTABLISHED 的 TCP socket 数；不转发（没有 `ip_forward`），TTL 是默认的 64，RTO 的
/// 几项与 Linux 的初值相同，内核层看不到的错误、重传、复位、分片都是 0。
pub(crate) fn proc_snmp(
    sockets: &[(crate::syscall::SocketRef, u32)],
    p: &crate::traffic::Protocols,
) -> String {
    let established = sockets
        .iter()
        .filter(|(sock, _)| {
            let b = sock.borrow();
            let Socket::Inet(inet) = &*b else {
                return false;
            };
            (inet.ty, inet.proto) == (SOCK_STREAM, IPPROTO_TCP)
                && appine_hal::net::getsockopt(inet.fd, IPPROTO_TCP, TCP_INFO, 8)
                    .is_ok_and(|b| b.first() == Some(&TCP_ESTABLISHED))
        })
        .count() as u64;
    let line = |group: &str, names: &[&str], values: &[String]| {
        format!(
            "{group}: {}\n{group}: {}\n",
            names.join(" "),
            values.join(" ")
        )
    };
    let nums = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>();
    let mut out = String::new();

    let mut ip = vec!["2".to_owned(), "64".to_owned()];
    let mut counts = [0u64; SNMP_IP.len()];
    counts[0] = p.ip_in_packets; // InReceives
    counts[6] = p.ip_in_packets; // InDelivers
    counts[7] = p.ip_out_packets; // OutRequests
    ip.extend(nums(&counts));
    let names: Vec<&str> = ["Forwarding", "DefaultTTL"]
        .into_iter()
        .chain(SNMP_IP)
        .collect();
    out += &line("Ip", &names, &ip);

    let by_type = |ty: u8, rx: bool| match (ty, rx) {
        (ICMP_ECHOREPLY, true) => p.icmp_in_echo_replies,
        (ICMP_ECHO, false) => p.icmp_out_echoes,
        _ => 0,
    };
    let mut names = vec![
        "InMsgs".to_owned(),
        "InErrors".into(),
        "InCsumErrors".into(),
    ];
    let mut icmp = vec![p.icmp_in_echo_replies, 0, 0];
    for (name, ty) in SNMP_ICMP {
        names.push(format!("In{name}"));
        icmp.push(by_type(ty, true));
    }
    names.extend(["OutMsgs".to_owned(), "OutErrors".into()]);
    icmp.extend([p.icmp_out_echoes, 0]);
    for (name, ty) in SNMP_ICMP {
        names.push(format!("Out{name}"));
        icmp.push(by_type(ty, false));
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    out += &line("Icmp", &names, &nums(&icmp));
    // 只列出不为 0 的类型：先收后发，各按类型号
    let msgs: Vec<(String, u64)> = [(ICMP_ECHOREPLY, true), (ICMP_ECHO, false)]
        .into_iter()
        .map(|(ty, rx)| {
            let dir = if rx { "In" } else { "Out" };
            (format!("{dir}Type{ty}"), by_type(ty, rx))
        })
        .filter(|(_, n)| *n != 0)
        .collect();
    if !msgs.is_empty() {
        let names: Vec<&str> = msgs.iter().map(|(n, _)| n.as_str()).collect();
        let values: Vec<u64> = msgs.iter().map(|(_, v)| *v).collect();
        out += &line("IcmpMsg", &names, &nums(&values));
    }

    let mut tcp = vec!["1".to_owned(), "200".into(), "120000".into(), "-1".into()];
    tcp.extend(nums(&[
        p.tcp_active_opens,
        p.tcp_passive_opens,
        p.tcp_attempt_fails,
        0,
        established,
        p.tcp_in_segs,
        p.tcp_out_segs,
        0,
        0,
        0,
        0,
    ]));
    out += &line("Tcp", &SNMP_TCP, &tcp);
    let mut udp = [0u64; SNMP_UDP.len()];
    udp[0] = p.udp_in_datagrams;
    udp[3] = p.udp_out_datagrams;
    out += &line("Udp", &SNMP_UDP, &nums(&udp));
    out += &line("UdpLite", &SNMP_UDP, &nums(&[0; SNMP_UDP.len()]));
    out
}

/// `/proc/net/netstat` 里 TcpExt 的字段（Linux 6.1 的 `snmp4_net_list`）。
const NETSTAT_TCP: &str = "SyncookiesSent SyncookiesRecv SyncookiesFailed EmbryonicRsts \
PruneCalled RcvPruned OfoPruned OutOfWindowIcmps LockDroppedIcmps ArpFilter TW TWRecycled \
TWKilled PAWSActive PAWSEstab DelayedACKs DelayedACKLocked DelayedACKLost ListenOverflows \
ListenDrops TCPHPHits TCPPureAcks TCPHPAcks TCPRenoRecovery TCPSackRecovery TCPSACKReneging \
TCPSACKReorder TCPRenoReorder TCPTSReorder TCPFullUndo TCPPartialUndo TCPDSACKUndo \
TCPLossUndo TCPLostRetransmit TCPRenoFailures TCPSackFailures TCPLossFailures \
TCPFastRetrans TCPSlowStartRetrans TCPTimeouts TCPLossProbes TCPLossProbeRecovery \
TCPRenoRecoveryFail TCPSackRecoveryFail TCPRcvCollapsed TCPBacklogCoalesce TCPDSACKOldSent \
TCPDSACKOfoSent TCPDSACKRecv TCPDSACKOfoRecv TCPAbortOnData TCPAbortOnClose \
TCPAbortOnMemory TCPAbortOnTimeout TCPAbortOnLinger TCPAbortFailed TCPMemoryPressures \
TCPMemoryPressuresChrono TCPSACKDiscard TCPDSACKIgnoredOld TCPDSACKIgnoredNoUndo \
TCPSpuriousRTOs TCPMD5NotFound TCPMD5Unexpected TCPMD5Failure TCPSackShifted TCPSackMerged \
TCPSackShiftFallback TCPBacklogDrop PFMemallocDrop TCPMinTTLDrop TCPDeferAcceptDrop \
IPReversePathFilter TCPTimeWaitOverflow TCPReqQFullDoCookies TCPReqQFullDrop \
TCPRetransFail TCPRcvCoalesce TCPOFOQueue TCPOFODrop TCPOFOMerge TCPChallengeACK \
TCPSYNChallenge TCPFastOpenActive TCPFastOpenActiveFail TCPFastOpenPassive \
TCPFastOpenPassiveFail TCPFastOpenListenOverflow TCPFastOpenCookieReqd \
TCPFastOpenBlackhole TCPSpuriousRtxHostQueues BusyPollRxPackets TCPAutoCorking \
TCPFromZeroWindowAdv TCPToZeroWindowAdv TCPWantZeroWindowAdv TCPSynRetrans TCPOrigDataSent \
TCPHystartTrainDetect TCPHystartTrainCwnd TCPHystartDelayDetect TCPHystartDelayCwnd \
TCPACKSkippedSynRecv TCPACKSkippedPAWS TCPACKSkippedSeq TCPACKSkippedFinWait2 \
TCPACKSkippedTimeWait TCPACKSkippedChallenge TCPWinProbe TCPKeepAlive TCPMTUPFail \
TCPMTUPSuccess TCPDelivered TCPDeliveredCE TCPAckCompressed TCPZeroWindowDrop TCPRcvQDrop \
TCPWqueueTooBig TCPFastOpenPassiveAltKey TcpTimeoutRehash TcpDuplicateDataRehash \
TCPDSACKRecvSegs TCPDSACKIgnoredDubious TCPMigrateReqSuccess TCPMigrateReqFailure";

/// IpExt 的字段（`snmp4_ipextstats_list`）。
const NETSTAT_IP: &str = "InNoRoutes InTruncatedPkts InMcastPkts OutMcastPkts InBcastPkts \
OutBcastPkts InOctets OutOctets InMcastOctets OutMcastOctets InBcastOctets OutBcastOctets \
InCsumErrors InNoECTPkts InECT1Pkts InECT0Pkts InCEPkts ReasmOverlaps";

/// MPTcpExt 的字段（`mptcp_snmp_list`）。
const NETSTAT_MPTCP: &str = "MPCapableSYNRX MPCapableSYNTX MPCapableSYNACKRX MPCapableACKRX \
MPCapableFallbackACK MPCapableFallbackSYNACK MPFallbackTokenInit MPTCPRetrans \
MPJoinNoTokenFound MPJoinSynRx MPJoinSynAckRx MPJoinSynAckHMacFailure MPJoinAckRx \
MPJoinAckHMacFailure DSSNotMatching InfiniteMapTx InfiniteMapRx DSSNoMatchTCP DataCsumErr \
OFOQueueTail OFOQueue OFOMerge NoDSSInWindow DuplicateData AddAddr EchoAdd PortAdd \
AddAddrDrop MPJoinPortSynRx MPJoinPortSynAckRx MPJoinPortAckRx MismatchPortSynRx \
MismatchPortAckRx RmAddr RmAddrDrop RmSubflow MPPrioTx MPPrioRx MPFailTx MPFailRx \
MPFastcloseTx MPFastcloseRx MPRstTx MPRstRx RcvPruned SubflowStale SubflowRecover \
SndWndShared RcvWndShared RcvWndConflictUpdate RcvWndConflict";

/// `/proc/net/netstat`（`netstat_seq_show`）：TCP、MPTCP 的扩展计数内核层都看不到，是 0；
/// IpExt 里收发的字节数是 IPv4 的载荷（Linux 的含 IP 头），收到的包都当作不带 ECN 标记。
pub(crate) fn proc_netstat(p: &crate::traffic::Protocols) -> String {
    let group = |name: &str, fields: &str, values: &dyn Fn(&str) -> u64| {
        let values: Vec<String> = fields.split(' ').map(|f| values(f).to_string()).collect();
        format!("{name}: {fields}\n{name}: {}\n", values.join(" "))
    };
    group("TcpExt", NETSTAT_TCP, &|_| 0)
        + &group("IpExt", NETSTAT_IP, &|f| match f {
            "InOctets" => p.ip_in_octets,
            "OutOctets" => p.ip_out_octets,
            "InNoECTPkts" => p.ip_in_packets,
            _ => 0,
        })
        + &group("MPTcpExt", NETSTAT_MPTCP, &|_| 0)
}

/// `/proc/net/arp`：邻居表总是空的（与 `RTM_GETNEIGH` 一样），只有表头。
pub(crate) const PROC_ARP: &str =
    "IP address       HW type     Flags       HW address            Mask     Device\n";

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
    /// 建在新的网络命名空间里时是它（回答它的 lo、配置 lo）。
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// `NETLINK_EXT_ACK`：出错的确认附上 extack 的消息（iproute2 打印成 `Error: ….`）。
    pub ext_ack: bool,
    /// `NETLINK_CAP_ACK`：出错的确认只附上请求的消息头。
    pub cap_ack: bool,
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
const RTM_NEWNSID: u16 = 88;
const RTM_GETNSID: u16 = 90;
const NETNSA_NSID: u16 = 1;
/// RTM_GETNSID 按进程找网络命名空间（[`Host::peer_net`]）。
pub(crate) const NETNSA_PID: u16 = 2;
/// RTM_GETNSID 按命名空间文件找网络命名空间。
pub(crate) const NETNSA_FD: u16 = 3;
const NETNSA_TARGET_NSID: u16 = 4;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_QDISC: u16 = 6;
const IFLA_STATS: u16 = 7;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_PROTO_DOWN: u16 = 39;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
/// 转储的一个数据报最多多大：接口多时分成几个（musl 的 if_nameindex 等用 8 KiB 的缓冲区）。
const DUMP_CHUNK: usize = 8192;
/// `IF_OPER_*`（RFC 2863 的接口状态）。
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_UP: u8 = 6;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_PROTO: u16 = 11;
/// 地址是内核加的：回环接口的 `::1`、自动配置的链路本地地址（`IFAPROT_KERNEL_*`）。
const IFAPROT_KERNEL_LO: u8 = 1;
const IFAPROT_KERNEL_LL: u8 = 3;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
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
/// NETLINK_NETFILTER 的协议号（nftables，[`crate::nft`]）。
pub const NETLINK_NETFILTER: i32 = 12;

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
    ack_ext(req, err, pid, None, false)
}

/// 同 [`ack`]，另外按 `NETLINK_EXT_ACK` 附上 extack 的消息（`NLMSGERR_ATTR_MSG`，带
/// `NLM_F_ACK_TLVS`），`cap` 是设了 `NETLINK_CAP_ACK`。
pub(crate) fn ack_ext(req: &[u8], err: i32, pid: u32, msg: Option<&str>, cap: bool) -> Vec<u8> {
    const NLM_F_ACK_TLVS: u16 = 0x200;
    const NLMSGERR_ATTR_MSG: u16 = 1;
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let mut p = (-err).to_le_bytes().to_vec();
    let mut flags = 0;
    if err == 0 || cap {
        p.extend_from_slice(&req[..16]);
        flags |= NLM_F_CAPPED;
    } else {
        p.extend_from_slice(req);
    }
    if let Some(m) = msg {
        align4(&mut p);
        let mut s = m.as_bytes().to_vec();
        s.push(0);
        attr(&mut p, NLMSGERR_ATTR_MSG, &s);
        flags |= NLM_F_ACK_TLVS;
    }
    message(NLMSG_ERROR, flags, seq, pid, &p)
}

/// 网络命名空间里的一个接口（netlink 的转储用）：宿主的（或者新的命名空间里的 lo），或者
/// 虚拟设备（[`crate::netdev`]，带着对端所在命名空间的编号）。
pub(crate) enum Link {
    Host(Interface),
    Virt(crate::netdev::View, Option<i32>),
}

impl Link {
    fn index(&self) -> u32 {
        match self {
            Link::Host(i) => i.index,
            Link::Virt(v, _) => v.index,
        }
    }

    fn name(&self) -> &[u8] {
        match self {
            Link::Host(i) => i.name.as_bytes(),
            Link::Virt(v, _) => &v.name,
        }
    }
}

/// 接口的状态（`IFLA_OPERSTATE`）：与 `/sys/class/net/<接口>/operstate` 一样，回环接口是
/// unknown（Linux 的回环驱动不设置它），启用并且连着的是 up，别的是 down。
pub(crate) fn operstate(i: &Interface) -> u8 {
    if i.flags & IFF_LOOPBACK != 0 && i.flags & IFF_UP != 0 {
        IF_OPER_UNKNOWN
    } else if i.flags & IFF_UP != 0 && i.flags & IFF_RUNNING != 0 {
        IF_OPER_UP
    } else {
        IF_OPER_DOWN
    }
}

/// 一个接口的 `RTM_NEWLINK`（Linux 的 `rtnl_fill_ifinfo`，属性的顺序相同）：`ip link` 用到
/// 的和程序常读的几项，`IFLA_AF_SPEC` 这些设备参数没有。发送队列长度 `txqlen` 取自宿主，
/// 取不到时是 Linux 的默认值 1000；guest 的数据包交给宿主 socket，接口上没有自己的排队
/// 规则，qdisc 都是 noqueue。
fn link_message(i: &Interface, st: IfStats, txqlen: Option<u32>) -> Vec<u8> {
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
    attr(&mut p, IFLA_TXQLEN, &txqlen.unwrap_or(1000).to_le_bytes());
    attr(&mut p, IFLA_OPERSTATE, &[operstate(i)]);
    attr(&mut p, IFLA_LINKMODE, &[0]);
    attr(&mut p, IFLA_MTU, &i.mtu.to_le_bytes());
    attr(&mut p, IFLA_GROUP, &0u32.to_le_bytes());
    attr(&mut p, IFLA_PROMISCUITY, &0u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_TX_QUEUES, &1u32.to_le_bytes());
    attr(&mut p, IFLA_NUM_RX_QUEUES, &1u32.to_le_bytes());
    attr(
        &mut p,
        IFLA_CARRIER,
        &[u8::from(i.flags & IFF_RUNNING != 0)],
    );
    // 新的网络命名空间里还没起来的 lo 还没有 qdisc（crate::netns）
    let qdisc: &[u8] = if i.flags & (IFF_LOOPBACK | IFF_UP) == IFF_LOOPBACK {
        b"noop\0"
    } else {
        b"noqueue\0"
    };
    attr(&mut p, IFLA_QDISC, qdisc);
    attr(&mut p, IFLA_PROTO_DOWN, &[0]);
    let mac = i.mac.unwrap_or([0; 6]);
    attr(&mut p, IFLA_ADDRESS, &mac);
    let bcast = if loopback { [0; 6] } else { [0xff; 6] };
    attr(&mut p, IFLA_BROADCAST, &bcast);
    // 流量计数（crate::traffic）：struct rtnl_link_stats64 在 Linux 6.1 是 25 个 u64，
    // rtnl_link_stats 是 24 个 u32；前四项依次是收、发的包数和收、发的字节数，别的都是 0
    let head = [st.rx_packets, st.tx_packets, st.rx_bytes, st.tx_bytes];
    let field = |n: usize| head.get(n).copied().unwrap_or(0);
    let stats64: Vec<u8> = (0..25).flat_map(|n| field(n).to_le_bytes()).collect();
    attr(&mut p, IFLA_STATS64, &stats64);
    let stats: Vec<u8> = (0..24)
        .flat_map(|n| (field(n) as u32).to_le_bytes())
        .collect();
    attr(&mut p, IFLA_STATS, &stats);
    p
}

/// IPv4 地址 `addr`/`prefix` 的广播地址：接口支持广播、网段里不止两个地址时是网段最后
/// 一个地址（DHCP 和 `ip addr add ... brd +` 设的就是它），与本地表里的广播路由一致。
fn broadcast_of(i: &Interface, addr: &[u8], prefix: u8) -> Option<[u8; 4]> {
    if i.flags & IFF_BROADCAST == 0 || prefix >= 31 {
        return None;
    }
    let ip = u32::from_be_bytes(addr.try_into().ok()?);
    Some((ip | u32::MAX >> prefix).to_be_bytes())
}

/// 一个接口上的 `RTM_NEWADDR`（Linux 的 `inet_fill_ifaddr`、`inet6_fill_ifaddr`，属性的顺序
/// 相同）。地址都当作不会过期的（`IFA_F_PERMANENT`，`ip addr` 显示 forever），创建和更新
/// 的时刻不知道，是 0。
fn addr_messages(i: &Interface, family: u8) -> Vec<Vec<u8>> {
    // struct ifa_cacheinfo：preferred、valid（全 1 是永久）、cstamp、tstamp
    let mut cacheinfo = [0xffu8; 16];
    cacheinfo[8..].fill(0);
    i.addrs
        .iter()
        .filter(|(f, ..)| family == 0 || *f as u8 == family)
        .map(|(f, addr, prefix)| {
            let link_local = *f == AF_INET6 && addr[0] == 0xfe && addr[1] & 0xc0 == 0x80;
            let scope = if i.flags & IFF_LOOPBACK != 0 {
                RT_SCOPE_HOST
            } else if link_local {
                RT_SCOPE_LINK
            } else {
                RT_SCOPE_UNIVERSE
            };
            // struct ifaddrmsg：family、prefixlen、flags、scope、index
            let mut p = vec![*f as u8, *prefix, IFA_F_PERMANENT, scope];
            p.extend_from_slice(&i.index.to_le_bytes());
            let flags = u32::from(IFA_F_PERMANENT).to_le_bytes();
            attr(&mut p, IFA_ADDRESS, addr);
            if *f == AF_INET {
                attr(&mut p, IFA_LOCAL, addr);
                if let Some(b) = broadcast_of(i, addr, *prefix) {
                    attr(&mut p, IFA_BROADCAST, &b);
                }
                let mut name = i.name.clone().into_bytes();
                name.push(0);
                attr(&mut p, IFA_LABEL, &name);
                attr(&mut p, IFA_FLAGS, &flags);
                attr(&mut p, IFA_CACHEINFO, &cacheinfo);
            } else {
                attr(&mut p, IFA_CACHEINFO, &cacheinfo);
                attr(&mut p, IFA_FLAGS, &flags);
                if i.flags & IFF_LOOPBACK != 0 {
                    attr(&mut p, IFA_PROTO, &[IFAPROT_KERNEL_LO]);
                } else if link_local {
                    attr(&mut p, IFA_PROTO, &[IFAPROT_KERNEL_LL]);
                }
            }
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
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
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

/// 开机时的网络命名空间里修改宿主接口的请求（`RTM_NEWLINK`、`RTM_SETLINK`，Linux 的
/// `rtnl_setlink`、`__rtnl_newlink`）。App 改不了宿主的接口，只接受什么也不改的：标志按
/// `rtnl_dev_combine_flags` 合起来之后，`__dev_change_flags` 改得了的那些位与现在的相同，
/// 带着的名字、MTU 也与现在的相同。dockerd 给容器的默认网络沙箱拉起本来就起着的 lo 就是
/// 这样（`loopbackUp`）。真要改什么的仍是 EOPNOTSUPP，新建接口（`NLM_F_CREATE`，veth、
/// bridge 这些）也是。`net_admin`：有没有 `CAP_NET_ADMIN`（没有时 EPERM，Linux 在解析
/// 之前就查）。None：不是这两种请求。
pub(crate) fn configure_host_link(
    ifaces: &[Interface],
    net_admin: bool,
    req: &[u8],
) -> Option<i32> {
    const RTM_SETLINK: u16 = 19;
    const NLM_F_REPLACE: u16 = 0x100;
    const NLM_F_EXCL: u16 = 0x200;
    const NLM_F_CREATE: u16 = 0x400;
    const ENODEV: i32 = 19;
    // __dev_change_flags 按请求改的位（IFF_UP、IFF_PROMISC、IFF_ALLMULTI 另外处理，也算）
    const CHANGEABLE: u32 =
        0x1 | 0x4 | 0x20 | 0x80 | 0x100 | 0x200 | 0x1000 | 0x2000 | 0x4000 | 0x8000;
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if ty != RTM_NEWLINK && ty != RTM_SETLINK {
        return None;
    }
    if !net_admin {
        return Some(EPERM);
    }
    let flags = u16::from_le_bytes([req[6], req[7]]);
    // struct ifinfomsg：地址族、类型、编号、标志、要改的标志
    let Some(m) = req.get(16..32) else {
        return Some(EINVAL);
    };
    let index = i32::from_le_bytes(m[4..8].try_into().unwrap());
    let ifi_flags = u32::from_le_bytes(m[8..12].try_into().unwrap());
    let change = u32::from_le_bytes(m[12..16].try_into().unwrap());
    let attrs = crate::route::attrs(&req[32..]);
    let name = attrs
        .iter()
        .find(|(t, _)| *t == IFLA_IFNAME)
        .map(|(_, d)| d.split(|&c| c == 0).next().unwrap_or_default());
    let newlink = ty == RTM_NEWLINK;
    if newlink && index < 0 {
        return Some(EINVAL);
    }
    // 按编号找，编号不是正数时按名字找；都没给时 RTM_SETLINK 是 EINVAL，RTM_NEWLINK 当作
    // 没找到
    if !newlink && index <= 0 && name.is_none() {
        return Some(EINVAL);
    }
    let found = ifaces.iter().find(|i| {
        if index > 0 {
            i.index as i32 == index
        } else {
            name.is_some() && name == Some(i.name.as_bytes())
        }
    });
    let Some(dev) = found else {
        // 要新建的话 appine 建不了（Linux 没有那种接口的驱动时也是 EOPNOTSUPP）
        return Some(if newlink && flags & NLM_F_CREATE != 0 {
            EOPNOTSUPP
        } else {
            ENODEV
        });
    };
    if newlink && flags & NLM_F_EXCL != 0 {
        return Some(EEXIST);
    }
    if newlink && flags & NLM_F_REPLACE != 0 {
        return Some(EOPNOTSUPP);
    }
    let same = attrs.iter().all(|&(t, d)| match t {
        IFLA_IFNAME => d.split(|&c| c == 0).next() == Some(dev.name.as_bytes()),
        IFLA_MTU => d.get(..4) == Some(&dev.mtu.to_le_bytes()[..]),
        _ => false,
    });
    // ifi_change 是 0 时整个 ifi_flags 都算要的（Linux 的"bugwards compatibility"），两个
    // 都是 0 时不改标志
    let wanted = if change != 0 {
        (ifi_flags & change) | (dev.flags & !change)
    } else {
        ifi_flags
    };
    let flags_same = (ifi_flags == 0 && change == 0) || (wanted ^ dev.flags) & CHANGEABLE == 0;
    Some(if same && flags_same { 0 } else { EOPNOTSUPP })
}

/// netlink 的回答要用到的宿主信息（测试里换成固定的）。
pub(crate) trait Host {
    /// 宿主的网络接口。
    fn interfaces(&self) -> Vec<Interface>;
    /// 接口的流量计数（[`crate::traffic`]）。
    fn stats(&self, name: &str) -> IfStats;
    /// 宿主主表里的路由，None 时按接口的地址合成（[`crate::route`]）。
    fn routes(&self) -> Option<Vec<Route>>;
    /// 接口的发送队列长度（`IFLA_TXQLEN`），取不到时是 None。
    fn tx_queue_len(&self, name: &str) -> Option<u32>;
    /// 发请求的进程的用户 ID（`ip route get` 回答里的 RTA_UID）。
    fn uid(&self) -> u32;
    /// 修改接口、地址、路由的请求（[`crate::netdev::request`]）：None 是不处理，Some 里是
    /// 结果。
    fn configure(&self, _req: &[u8]) -> Option<Result<(), crate::netdev::Fail>> {
        None
    }
    /// RTM_GETNSID 要找的网络命名空间：`attr` 是 [`NETNSA_PID`]（进程）或者 [`NETNSA_FD`]
    /// （命名空间文件），`value` 是它的值。找得到时是这个命名空间给它分配的编号（没有分配
    /// 是 None），不然是错误码。
    fn peer_net(&self, _attr: u16, _value: u32) -> Result<Option<i32>, i32> {
        Ok(None)
    }
    /// 所有的接口：宿主的（或者 lo）和虚拟设备，按编号。
    fn links(&self) -> Vec<Link> {
        self.interfaces().into_iter().map(Link::Host).collect()
    }
    /// 路由表（[`crate::route::tables`]，含虚拟设备上的）。
    fn tables(&self) -> Vec<Route2> {
        crate::route::tables(&self.interfaces(), self.routes())
    }
    /// 所有接口（含虚拟设备）的地址，`ip route get` 用。
    fn all_interfaces(&self) -> Vec<Interface> {
        self.interfaces()
    }
    /// 是开机时的命名空间：`ip route get` 可以让宿主选源地址。
    fn init_net(&self) -> bool {
        true
    }
    /// `NETLINK_NETFILTER` 上的一个数据报（[`crate::nft::handle`]），返回各个回答的数据报。
    fn netfilter(&self, _data: &[u8], _peer: &crate::nft::Peer<'_>) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

type Route2 = crate::route::Entry;

impl NetlinkSocket {
    pub fn new(protocol: i32) -> NetlinkSocket {
        NetlinkSocket {
            protocol,
            pid: 0,
            queue: VecDeque::new(),
            arrivals: 0,
            rcvtimeo: None,
            net: None,
            ext_ack: false,
            cap_ack: false,
        }
    }

    /// 处理 guest 发来的一个数据报（可能含多条请求），把回答放进队列。
    pub fn handle(&mut self, data: &[u8], host: &dyn Host) {
        // nftables 的批跨几条消息，整个数据报一起处理
        if self.protocol == NETLINK_NETFILTER {
            let peer = crate::nft::Peer {
                pid: self.pid,
                ext_ack: self.ext_ack,
                cap_ack: self.cap_ack,
                proc_pid: 0,
                comm: b"",
            };
            for d in host.netfilter(data, &peer) {
                self.queue.push_back(d);
                self.arrivals += 1;
            }
            return;
        }
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
        if let Some(r) = host.configure(req) {
            return match r {
                Err(f) => vec![ack_ext(
                    req,
                    f.errno,
                    self.pid,
                    f.msg.filter(|_| self.ext_ack),
                    self.cap_ack,
                )],
                Ok(()) if flags & NLM_F_ACK != 0 => vec![ack(req, 0, self.pid)],
                Ok(()) => Vec::new(),
            };
        }
        let one = |l: &Link| match l {
            Link::Host(i) => link_message(i, host.stats(&i.name), host.tx_queue_len(&i.name)),
            Link::Virt(v, nsid) => crate::netdev::link_message(v, *nsid),
        };
        match ty {
            // 与 Linux 一样，接口是一个数据报（太大时分成几个）；地址每个地址族一个
            // （AF_UNSPEC 时先是 IPv4），NLMSG_DONE 另是一个。按 IFLA_MASTER、IFLA_LINKINFO
            // 的种类过滤时消息带 NLM_F_DUMP_FILTERED（Linux 的 rtnl_dump_ifinfo）
            RTM_GETLINK if dump => {
                const NLM_F_DUMP_FILTERED: u16 = 0x20;
                let attrs = crate::route::attrs(req.get(32..).unwrap_or_default());
                let master = attrs
                    .iter()
                    .find(|(t, d)| *t == IFLA_MASTER && d.len() >= 4)
                    .map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
                    .filter(|&m| m != 0);
                let kind = attrs
                    .iter()
                    .find(|(t, _)| *t == IFLA_LINKINFO)
                    .and_then(|(_, d)| {
                        crate::route::attrs(d)
                            .into_iter()
                            .find(|(t, _)| *t == 1)
                            .map(|(_, k)| k.split(|&c| c == 0).next().unwrap_or_default().to_vec())
                    })
                    .filter(|k| matches!(&k[..], b"bridge" | b"veth" | b"dummy"));
                let filtered = master.is_some() || kind.is_some();
                let mflags = NLM_F_MULTI | if filtered { NLM_F_DUMP_FILTERED } else { 0 };
                let mut out: Vec<Vec<u8>> = Vec::new();
                let mut cur = Vec::new();
                for l in &host.links() {
                    let keep = match l {
                        Link::Host(_) => master.is_none() && kind.is_none(),
                        Link::Virt(v, _) => {
                            master.is_none_or(|m| v.master == Some(m))
                                && kind
                                    .as_deref()
                                    .is_none_or(|k| k == v.kind.name().as_bytes())
                        }
                    };
                    if !keep {
                        continue;
                    }
                    let m = reply(RTM_NEWLINK, mflags, &one(l));
                    if !cur.is_empty() && cur.len() + m.len() > DUMP_CHUNK {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur.extend(m);
                }
                if !cur.is_empty() {
                    out.push(cur);
                }
                out.push(done());
                out
            }
            RTM_GETADDR if dump => {
                let links = host.links();
                let mut out = Vec::new();
                for f in [AF_INET, AF_INET6] {
                    if family != 0 && family != f {
                        continue;
                    }
                    let d: Vec<u8> = links
                        .iter()
                        .flat_map(|l| match l {
                            Link::Host(i) => addr_messages(i, f as u8),
                            Link::Virt(v, _) => crate::netdev::addr_messages(v, f),
                        })
                        .flat_map(|m| reply(RTM_NEWADDR, NLM_F_MULTI, &m))
                        .collect();
                    if !d.is_empty() {
                        out.push(d);
                    }
                }
                out.push(done());
                out
            }
            // `ip link show <接口>`：按 ifi_index 或者 IFLA_IFNAME 找（Linux 的 rtnl_getlink）
            RTM_GETLINK => {
                const ENODEV: i32 = 19;
                let index = req
                    .get(20..24)
                    .map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap()));
                let name = crate::route::attrs(req.get(32..).unwrap_or_default())
                    .into_iter()
                    .find(|(t, _)| *t == IFLA_IFNAME)
                    .map(|(_, d)| d.split(|&c| c == 0).next().unwrap_or_default().to_vec());
                if index <= 0 && name.is_none() {
                    return vec![ack(req, EINVAL, self.pid)];
                }
                let found = host.links().into_iter().find(|l| {
                    if index > 0 {
                        l.index() as i32 == index
                    } else {
                        name.as_deref() == Some(l.name())
                    }
                });
                let Some(l) = found else {
                    return vec![ack(req, ENODEV, self.pid)];
                };
                let mut out = vec![reply(RTM_NEWLINK, 0, &one(&l))];
                if flags & NLM_F_ACK != 0 {
                    out.push(ack(req, 0, self.pid));
                }
                out
            }
            // 与 Linux 一样，每个地址族的路由是一个数据报，NLMSG_DONE 另是一个
            RTM_GETROUTE if dump => {
                let entries = host.tables();
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
                let ifaces = host.all_interfaces();
                let entries = host.tables();
                match crate::route::get(req, &entries, &ifaces, host.uid(), host.init_net()) {
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
            // 网络命名空间的编号（`ip netns list`）：只在转储 veth 时给对端所在的命名空间分配
            // （IFLA_LINK_NETNSID，Linux 的 peernet2id_alloc），RTM_NEWNSID 是 EOPNOTSUPP；
            // 找得到对方时回答分配了的编号，没有分配是 -1（NETNSA_NSID_NOT_ASSIGNED），按编号
            // 找的都找不到。顺序与 Linux 的 rtnl_net_getid 相同
            RTM_GETNSID => {
                let attrs = crate::route::attrs(req.get(20..).unwrap_or_default());
                let get = |t| {
                    attrs
                        .iter()
                        .find(|(a, d)| *a == t && d.len() >= 4)
                        .map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
                };
                let peer = if let Some(pid) = get(NETNSA_PID) {
                    host.peer_net(NETNSA_PID, pid)
                } else if let Some(fd) = get(NETNSA_FD) {
                    host.peer_net(NETNSA_FD, fd)
                } else if get(NETNSA_NSID).is_some() {
                    Err(ENOENT)
                } else {
                    Err(EINVAL)
                };
                let r = peer.and_then(|id| match get(NETNSA_TARGET_NSID) {
                    Some(_) => Err(EINVAL),
                    None => Ok(id),
                });
                match r {
                    Ok(id) => {
                        let mut p = vec![0u8; 4];
                        attr(&mut p, NETNSA_NSID, &id.unwrap_or(-1).to_le_bytes());
                        let mut out = vec![reply(RTM_NEWNSID, 0, &p)];
                        if flags & NLM_F_ACK != 0 {
                            out.push(ack(req, 0, self.pid));
                        }
                        out
                    }
                    Err(e) => vec![ack(req, e, self.pid)],
                }
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
        fn tx_queue_len(&self, _: &str) -> Option<u32> {
            None
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
        // 接口、NLMSG_DONE、IPv4 地址、NLMSG_DONE 各是一个数据报
        let datagrams: Vec<_> = s.queue.drain(..).map(|d| parse(&d)).collect();
        let types: Vec<Vec<u16>> = datagrams
            .iter()
            .map(|d| d.iter().map(|m| m.0).collect())
            .collect();
        assert_eq!(
            types,
            [
                vec![RTM_NEWLINK],
                vec![NLMSG_DONE],
                vec![RTM_NEWADDR],
                vec![NLMSG_DONE]
            ]
        );
        let links = &datagrams[0];
        assert!(links[0].1.windows(3).any(|w| w == b"lo\0"));
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
        assert_eq!(attrs[&IFLA_OPERSTATE], [IF_OPER_UNKNOWN]);
        assert_eq!(attrs[&IFLA_QDISC], b"noqueue\0");
        assert_eq!(attrs[&IFLA_TXQLEN], 1000u32.to_le_bytes());
        // 只要 IPv4 地址：回环接口没有广播地址，地址是永久的
        let addrs = &datagrams[2];
        assert_eq!(
            addrs[0].1[..4],
            [AF_INET as u8, 8, IFA_F_PERMANENT, RT_SCOPE_HOST]
        );
        let a = attributes(&addrs[0].1[8..]);
        assert_eq!(a[&IFA_LOCAL], [127, 0, 0, 1]);
        assert!(!a.contains_key(&IFA_BROADCAST));
        assert_eq!(a[&IFA_FLAGS], u32::from(IFA_F_PERMANENT).to_le_bytes());
        assert_eq!(a[&IFA_CACHEINFO][..8], [0xff; 8]);
        // 单条查询：按编号、按名字；查不到是 ENODEV，没有条件是 EINVAL
        let get = |index: i32, name: Option<&[u8]>| {
            let mut p = vec![0u8, 0, 0, 0];
            p.extend_from_slice(&index.to_le_bytes());
            p.extend_from_slice(&[0; 8]);
            if let Some(n) = name {
                attr(&mut p, IFLA_IFNAME, n);
            }
            message(RTM_GETLINK, NLM_F_REQUEST, 9, 0, &p)
        };
        let mut ask = |req: Vec<u8>| -> (u16, Vec<u8>) {
            s.handle(&req, &NOTHING);
            let d = parse(&s.queue.pop_front().unwrap());
            assert!(s.queue.is_empty());
            d[0].clone()
        };
        assert_eq!(ask(get(1, None)).0, RTM_NEWLINK);
        assert_eq!(ask(get(0, Some(b"lo\0"))).0, RTM_NEWLINK);
        let errno = |m: (u16, Vec<u8>)| i32::from_le_bytes(m.1[..4].try_into().unwrap());
        assert_eq!(errno(ask(get(0, Some(b"eth9\0")))), -19);
        assert_eq!(errno(ask(get(7, None))), -19);
        assert_eq!(errno(ask(get(0, None))), -22);
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

    #[test]
    fn netns_ids_are_never_assigned() {
        let mut s = NetlinkSocket::new(NETLINK_ROUTE);
        s.pid = 42;
        let mut ask = |attrs: &[(u16, &[u8])]| -> Vec<(u16, Vec<u8>)> {
            let mut p = vec![0u8; 4];
            for (t, d) in attrs {
                attr(&mut p, *t, d);
            }
            s.handle(&message(RTM_GETNSID, NLM_F_REQUEST, 5, 0, &p), &NOTHING);
            s.queue.drain(..).flat_map(|d| parse(&d)).collect()
        };
        // 找得到对方：回答 -1（没有分配编号），与 `ip netns list` 在 Linux 上看到的一样
        let r = ask(&[(NETNSA_FD, &3u32.to_le_bytes())]);
        let mut want = vec![0u8; 4];
        attr(&mut want, NETNSA_NSID, &(-1i32).to_le_bytes());
        assert_eq!(r, [(RTM_NEWNSID, want)]);
        assert_eq!(ask(&[(NETNSA_PID, &1u32.to_le_bytes())]).len(), 1);
        // 按编号找的都找不到；什么也没给是 EINVAL；要按编号换算成别的命名空间里的也是
        let errno = |r: Vec<(u16, Vec<u8>)>| i32::from_le_bytes(r[0].1[..4].try_into().unwrap());
        assert_eq!(errno(ask(&[(NETNSA_NSID, &0i32.to_le_bytes())])), -ENOENT);
        assert_eq!(errno(ask(&[])), -EINVAL);
        let target = [
            (NETNSA_FD, &3u32.to_le_bytes()[..]),
            (NETNSA_TARGET_NSID, &0i32.to_le_bytes()[..]),
        ];
        assert_eq!(errno(ask(&target)), -EINVAL);
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

    /// 开机时的命名空间里改宿主的接口：什么也不改的成功，要改的 EOPNOTSUPP，错误码的顺序
    /// 照 Linux 6.1 的 rtnl_setlink、__rtnl_newlink。
    #[test]
    fn host_links_accept_requests_that_change_nothing() {
        const SET: u16 = 19;
        const UP: u32 = 0x1;
        // 消息：头、ifinfomsg（编号、标志、要改的标志），后面是属性
        let msg = |ty: u16, flags: u16, index: i32, ifi: (u32, u32), attrs: &[(u16, &[u8])]| {
            let mut m = vec![0u8; 32];
            m[4..6].copy_from_slice(&ty.to_le_bytes());
            m[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | flags).to_le_bytes());
            m[20..24].copy_from_slice(&index.to_le_bytes());
            m[24..28].copy_from_slice(&ifi.0.to_le_bytes());
            m[28..32].copy_from_slice(&ifi.1.to_le_bytes());
            for (t, d) in attrs {
                attr(&mut m, *t, d);
            }
            let len = m.len() as u32;
            m[..4].copy_from_slice(&len.to_le_bytes());
            m
        };
        let ifaces = [lo()];
        let ask = |m: Vec<u8>| configure_host_link(&ifaces, true, &m);
        let lo_name: &[u8] = b"lo\0";
        let mtu = 65536u32.to_le_bytes();
        // 已经起着的 lo：按编号、按名字、两种消息、标志都是 0、整个 ifi_flags（ifi_change 0）
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (UP, UP), &[])), Some(0));
        assert_eq!(ask(msg(SET, 0, 1, (UP, UP), &[])), Some(0));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0, 0, (UP, UP), &[(IFLA_IFNAME, lo_name)])),
            Some(0)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (0, 0), &[])), Some(0));
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (UP | 0x48, 0), &[])), Some(0));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0x400, 1, (UP, UP), &[(IFLA_MTU, &mtu)])),
            Some(0)
        );
        // 要改的：关掉、混杂模式、别的 MTU、改名、别的属性
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 1, (0, UP), &[])), Some(EOPNOTSUPP));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0, 1, (0x100, 0x100), &[])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(
                RTM_NEWLINK,
                0,
                1,
                (0, 0),
                &[(IFLA_MTU, &1500u32.to_le_bytes())]
            )),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(SET, 0, 1, (0, 0), &[(IFLA_IFNAME, b"lo2\0")])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(
            ask(msg(SET, 0, 1, (0, 0), &[(IFLA_TXQLEN, &[0; 4])])),
            Some(EOPNOTSUPP)
        );
        // 找不到、新建、EXCL、REPLACE、编号和名字都没给
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 9, (UP, UP), &[])), Some(19));
        assert_eq!(
            ask(msg(SET, 0, 0, (UP, UP), &[(IFLA_IFNAME, b"eth9\0")])),
            Some(19)
        );
        assert_eq!(
            ask(msg(
                RTM_NEWLINK,
                0x400,
                0,
                (0, 0),
                &[(IFLA_IFNAME, b"eth9\0")]
            )),
            Some(EOPNOTSUPP)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0x200, 1, (UP, UP), &[])), Some(EEXIST));
        assert_eq!(
            ask(msg(RTM_NEWLINK, 0x100, 1, (UP, UP), &[])),
            Some(EOPNOTSUPP)
        );
        assert_eq!(ask(msg(RTM_NEWLINK, 0, 0, (UP, UP), &[])), Some(19));
        assert_eq!(ask(msg(SET, 0, 0, (UP, UP), &[])), Some(EINVAL));
        assert_eq!(ask(msg(RTM_NEWLINK, 0, -1, (UP, UP), &[])), Some(EINVAL));
        // 没有 CAP_NET_ADMIN 时先是 EPERM；别的消息不归它管
        assert_eq!(
            configure_host_link(&ifaces, false, &msg(RTM_NEWLINK, 0, 9, (0, 0), &[])),
            Some(EPERM)
        );
        assert_eq!(ask(msg(RTM_GETLINK, 0, 1, (0, 0), &[])), None);
    }
}
