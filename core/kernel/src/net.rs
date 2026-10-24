//! socket（设计文档 §10）。
//!
//! - TCP、UDP 透传：guest 的 socket 持有一个宿主的非阻塞 socket，地址和选项按字节原样
//!   传递。guest 的 `SOCK_RAW + IPPROTO_ICMP` 换成宿主的 ping socket。
//! - DNS 存根：guest 的 `/etc/resolv.conf` 指向 `127.0.0.53`。发往 `127.0.0.53:53` 的
//!   UDP 数据报不经过宿主网络，而是交给宿主的解析器（[`appine_hal::net::dns_query`]），
//!   回答放进这个 socket 自己的队列，像是从 `127.0.0.53:53` 收到的。
//! - `NETLINK_ROUTE`：在进程内回答接口和地址的查询（getifaddrs、`ip addr`）。
//! - AF_UNIX：完全在进程内模拟（[`crate::unix`]）。

use std::collections::VecDeque;
use std::time::Duration;

use appine_hal::net::Interface;

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
    pub ty: i32,
    /// 用 connect 连到了 DNS 存根（宿主 socket 本身没有连接）。
    pub stub_peer: bool,
    /// DNS 存根的回答，等 guest 读走。
    pub answers: VecDeque<Vec<u8>>,
    /// `SO_RCVTIMEO`、`SO_SNDTIMEO`：阻塞由我们模拟，所以超时也由我们处理。
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
}

impl InetSocket {
    pub fn new(fd: i32, family: i32, ty: i32) -> InetSocket {
        InetSocket {
            fd,
            family,
            ty,
            stub_peer: false,
            answers: VecDeque::new(),
            rcvtimeo: None,
            sndtimeo: None,
        }
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        appine_hal::net::close(self.fd);
    }
}

/// 进程内模拟的 netlink socket。只有 `NETLINK_ROUTE` 回答查询，其他协议的 socket
/// 什么也收不到（比如 mdev 监听的 uevent）。
pub(crate) struct NetlinkSocket {
    pub protocol: i32,
    /// `nl_pid`：bind 时分配。
    pub pid: u32,
    /// 等 guest 读走的回答，每一项是一个数据报。
    pub queue: VecDeque<Vec<u8>>,
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
const RTM_GETROUTE: u16 = 26;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
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

fn align4(b: &mut Vec<u8>) {
    b.resize(b.len().next_multiple_of(4), 0);
}

fn attr(b: &mut Vec<u8>, ty: u16, data: &[u8]) {
    b.extend_from_slice(&((4 + data.len()) as u16).to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(data);
    align4(b);
}

/// 一条 netlink 消息：消息头加上内容。
fn message(ty: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
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

fn link_message(i: &Interface) -> Vec<u8> {
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

impl NetlinkSocket {
    pub fn new(protocol: i32) -> NetlinkSocket {
        NetlinkSocket {
            protocol,
            pid: 0,
            queue: VecDeque::new(),
        }
    }

    /// 处理 guest 发来的一个数据报（可能含多条请求），把回答放进队列。
    pub fn handle(&mut self, data: &[u8], interfaces: impl Fn() -> Vec<Interface>) {
        if self.protocol != NETLINK_ROUTE {
            return;
        }
        let mut off = 0;
        while off + 16 <= data.len() {
            let h = &data[off..];
            let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
            if len < 16 || off + len > data.len() {
                break;
            }
            let ty = u16::from_le_bytes([h[4], h[5]]);
            let flags = u16::from_le_bytes([h[6], h[7]]);
            let seq = u32::from_le_bytes(h[8..12].try_into().unwrap());
            let family = h.get(16).copied().unwrap_or(0);
            let reply = |ty, flags, payload: &[u8]| message(ty, flags, seq, self.pid, payload);
            let mut out = Vec::new();
            match ty {
                RTM_GETLINK | RTM_GETADDR | RTM_GETROUTE if flags & NLM_F_DUMP != 0 => {
                    let list = interfaces();
                    for i in &list {
                        if ty == RTM_GETLINK {
                            out.extend(reply(RTM_NEWLINK, NLM_F_MULTI, &link_message(i)));
                        } else if ty == RTM_GETADDR {
                            for m in addr_messages(i, family) {
                                out.extend(reply(RTM_NEWADDR, NLM_F_MULTI, &m));
                            }
                        }
                    }
                    // 路由表没有模拟：只回答一个空的结果
                    out.extend(reply(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes()));
                }
                _ if flags & NLM_F_REQUEST != 0 => {
                    // 不支持的请求：回答 EOPNOTSUPP；只要确认时回答 0
                    let err: i32 = if flags & NLM_F_ACK != 0 && ty < 16 {
                        0
                    } else {
                        -95
                    };
                    let mut p = err.to_le_bytes().to_vec();
                    p.extend_from_slice(&h[..16]);
                    out.extend(reply(NLMSG_ERROR, 0, &p));
                }
                _ => {}
            }
            if !out.is_empty() {
                self.queue.push_back(out);
            }
            off += len.next_multiple_of(4);
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
        s.handle(&req, || vec![lo()]);
        let links = parse(&s.queue.pop_front().unwrap());
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].0, RTM_NEWLINK);
        assert!(links[0].1.windows(3).any(|w| w == b"lo\0"));
        assert_eq!(links[1].0, NLMSG_DONE);
        // 只要 IPv4 地址
        let addrs = parse(&s.queue.pop_front().unwrap());
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].1[..2], [AF_INET as u8, 8]);
        assert!(addrs[0].1.windows(4).any(|w| w == [127, 0, 0, 1]));
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
