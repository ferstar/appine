//! 网络接口的流量计数（设计文档 §10）：`/proc/net/dev`、`/sys/class/net/<接口>/statistics/`
//! 和 netlink 的 `IFLA_STATS`、`IFLA_STATS64` 都取自这里。
//!
//! Android 上 App 读不到宿主的计数（`/proc/net` 和 netlink 的 `RTM_GETLINK` 都被拦住），
//! 宿主的计数里也还有 App 自己和别的 App 的流量。guest 的 TCP、UDP、ping socket 都经过内核层
//! 收发，这里自己数：和容器里看到的是自己网络命名空间里的流量一样，只有 guest 的。
//!
//! - 只数载荷，不含协议头、ACK 和重传；流式 socket 的包数按接口的 MTU 减去 IPv4、TCP 头和
//!   时间戳选项估计，数据报一个算一个。
//! - 接口按 socket 的本端地址认：那是宿主选路的结果，第一次收发时取一次（getsockname），记在
//!   socket 上，bind、connect 时重新认。本端是通配地址（没有 connect 的 UDP、ping）时也记下，
//!   之后每一次按这一次的对端认：回环地址算回环接口，别的算默认接口。
//! - DNS 存根的查询和回答不经过宿主 socket，不算；AF_UNIX 不是网络接口上的流量。

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::net::{AF_INET, AF_INET6, InetSocket, SOCK_DGRAM, SOCK_STREAM};

/// 一个接口的计数。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct IfStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
}

/// socket 认定的接口和估计包数用的 MSS，记在 [`InetSocket`] 上。`iface` 为 None 时本端是
/// 通配地址，每一次按对端认。
#[derive(Clone, Debug)]
pub(crate) struct Route {
    pub iface: Option<Rc<str>>,
    pub mss: u32,
}

/// 地址表过了这么久再用时重新取：网络切换之后，新的 socket 认得出新的接口。
const TABLE_TTL: Duration = Duration::from_secs(5);
/// 本端地址不在表里时（网络刚变）重新取表，但两次之间至少隔这么久。
const MISS_REFRESH: Duration = Duration::from_secs(1);
/// MTU 里 IPv4、TCP 头和时间戳选项占的字节：剩下的是 MSS。
const TCP_OVERHEAD: u32 = 52;

const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;

/// 宿主接口的地址表。
struct Table {
    at: Instant,
    /// (地址, 接口名, MSS)：IPv4 是 4 字节，IPv6 是 16 字节。
    addrs: Vec<(Vec<u8>, Rc<str>, u32)>,
    /// 回环接口。
    lo: Rc<str>,
    /// 默认接口：发往公网时宿主选的本端地址所在的接口。
    default: Rc<str>,
}

impl Table {
    /// 接口 `name` 上的 MSS；它没有地址时按以太网的 MTU。
    fn mss_of(&self, name: &str) -> u32 {
        self.addrs
            .iter()
            .find(|(_, n, _)| &**n == name)
            .map_or(1500 - TCP_OVERHEAD, |(.., mss)| *mss)
    }

    fn lookup(&self, ip: &[u8]) -> Option<(Rc<str>, u32)> {
        self.addrs
            .iter()
            .find(|(a, ..)| a == ip)
            .map(|(_, n, mss)| (n.clone(), *mss))
    }
}

/// 各接口的计数，在 [`crate::kernel::Kernel`] 里，按接口名记。
#[derive(Default)]
pub(crate) struct Traffic {
    stats: BTreeMap<Rc<str>, IfStats>,
    table: Option<Table>,
}

impl Traffic {
    /// 接口 `name` 的计数。
    pub fn get(&self, name: &str) -> IfStats {
        self.stats.get(name).copied().unwrap_or_default()
    }

    /// 记下 socket `s` 收到（`rx`）或者发出的 `bytes` 字节。`peer` 是这一次的对端地址
    /// （sendto 的目标、recvfrom 的来源，sockaddr 的字节），可能为空。
    pub fn account(&mut self, s: &mut InetSocket, rx: bool, bytes: usize, peer: &[u8]) {
        if bytes == 0 && s.ty == SOCK_STREAM {
            return;
        }
        let (iface, mss) = self.route(s, peer);
        let packets = if s.ty == SOCK_STREAM {
            (bytes as u64).div_ceil(mss.max(1) as u64)
        } else {
            1
        };
        let st = self.stats.entry(iface).or_default();
        if rx {
            st.rx_bytes += bytes as u64;
            st.rx_packets += packets;
        } else {
            st.tx_bytes += bytes as u64;
            st.tx_packets += packets;
        }
    }

    /// socket `s` 这一次的流量算在哪个接口上，和那个接口上 TCP 的 MSS（模块文档）。
    fn route(&mut self, s: &mut InetSocket, peer: &[u8]) -> (Rc<str>, u32) {
        if s.route.is_none() {
            let local = appine_hal::net::sockname(s.fd, false)
                .ok()
                .and_then(|a| ip_of(&a));
            s.route = match local {
                Some(ip) if ip.iter().any(|&b| b != 0) => {
                    let (iface, mss) = self.iface_of(&ip);
                    Some(Route {
                        iface: Some(iface),
                        mss,
                    })
                }
                Some(_) => Some(Route {
                    iface: None,
                    mss: 0,
                }),
                // 取不到本端地址：这一次按对端认，下一次再取
                None => None,
            };
        }
        if let Some(Route {
            iface: Some(iface),
            mss,
        }) = &s.route
        {
            return (iface.clone(), *mss);
        }
        let t = self.table();
        if ip_of(peer).is_some_and(|ip| is_loopback(&ip)) {
            (t.lo.clone(), 0)
        } else {
            (t.default.clone(), 0)
        }
    }

    /// 本端地址 `ip` 所在的接口和那里的 MSS。
    fn iface_of(&mut self, ip: &[u8]) -> (Rc<str>, u32) {
        if let Some(n) = self.table().lookup(ip) {
            return n;
        }
        if self
            .table
            .as_ref()
            .is_some_and(|t| t.at.elapsed() >= MISS_REFRESH)
        {
            self.refresh();
        }
        let t = self.table();
        t.lookup(ip).unwrap_or_else(|| {
            let iface = if is_loopback(ip) { &t.lo } else { &t.default };
            (iface.clone(), t.mss_of(iface))
        })
    }

    /// 地址表，过期了先重新取。
    fn table(&mut self) -> &Table {
        if self
            .table
            .as_ref()
            .is_none_or(|t| t.at.elapsed() >= TABLE_TTL)
        {
            self.refresh();
        }
        self.table.as_ref().unwrap()
    }

    fn refresh(&mut self) {
        let list = appine_hal::net::interfaces();
        let mut addrs = Vec::new();
        for i in &list {
            let name: Rc<str> = i.name.as_str().into();
            let mss = i.mtu.saturating_sub(TCP_OVERHEAD);
            addrs.extend(
                i.addrs
                    .iter()
                    .map(|(_, a, _)| (a.clone(), name.clone(), mss)),
            );
        }
        let lo: Rc<str> = list
            .iter()
            .find(|i| i.flags & IFF_LOOPBACK != 0)
            .map_or("lo".into(), |i| i.name.as_str().into());
        let lookup = |ip: &[u8]| {
            addrs
                .iter()
                .find(|(a, ..)| a == ip)
                .map(|(_, n, _)| n.clone())
        };
        let default = [AF_INET, AF_INET6]
            .into_iter()
            .filter_map(outgoing_source)
            .find_map(|ip| lookup(&ip))
            .or_else(|| {
                list.iter()
                    .find(|i| i.flags & IFF_UP != 0 && i.flags & IFF_LOOPBACK == 0)
                    .map(|i| i.name.as_str().into())
            })
            .unwrap_or_else(|| lo.clone());
        self.table = Some(Table {
            at: Instant::now(),
            addrs,
            lo,
            default,
        });
    }
}

/// 宿主发往公网时选的本端地址：对一个公网地址 connect 一个 UDP socket，不发出数据报。
/// 没有这个地址族的路由时是 None。
fn outgoing_source(family: i32) -> Option<Vec<u8>> {
    let dst: std::net::IpAddr = if family == AF_INET {
        [8, 8, 8, 8].into()
    } else {
        [0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888].into()
    };
    let fd = appine_hal::net::socket(family, SOCK_DGRAM, 0).ok()?;
    let addr = appine_hal::net::sockaddr(std::net::SocketAddr::new(dst, 53));
    let local = appine_hal::net::connect(fd, &addr)
        .ok()
        .and_then(|()| appine_hal::net::sockname(fd, false).ok());
    appine_hal::net::close(fd);
    local.and_then(|a| ip_of(&a))
}

/// sockaddr 里的 IP 地址：IPv4 是 4 字节，IPv6 是 16 字节，IPv4 映射的 IPv6 地址按 IPv4。
fn ip_of(addr: &[u8]) -> Option<Vec<u8>> {
    let family = u16::from_ne_bytes(addr.get(..2)?.try_into().ok()?) as i32;
    match family {
        AF_INET => Some(addr.get(4..8)?.to_vec()),
        AF_INET6 => {
            let ip = addr.get(8..24)?;
            Some(
                match ip.strip_prefix(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff][..]) {
                    Some(v4) => v4.to_vec(),
                    None => ip.to_vec(),
                },
            )
        }
        _ => None,
    }
}

fn is_loopback(ip: &[u8]) -> bool {
    match ip.len() {
        4 => ip[0] == 127,
        16 => ip[..15].iter().all(|&b| b == 0) && ip[15] == 1,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sockaddr(s: &str) -> Vec<u8> {
        appine_hal::net::sockaddr(s.parse().unwrap())
    }

    #[test]
    fn addresses_are_read_from_sockaddrs() {
        assert_eq!(ip_of(&sockaddr("127.0.0.1:80")), Some(vec![127, 0, 0, 1]));
        assert_eq!(
            ip_of(&sockaddr("[::ffff:10.0.0.2]:80")),
            Some(vec![10, 0, 0, 2])
        );
        let v6 = ip_of(&sockaddr("[::1]:80")).unwrap();
        assert_eq!(v6.len(), 16);
        assert!(is_loopback(&v6));
        assert!(is_loopback(&[127, 1, 2, 3]));
        assert!(!is_loopback(&[10, 0, 0, 1]));
        assert_eq!(ip_of(&[]), None);
        assert_eq!(ip_of(&[1, 0, b'/', 0]), None);
    }

    /// 回环上的 TCP 连接：两端都算在回环接口上，字节数是载荷，包数按 MSS 估计。
    #[test]
    fn loopback_connections_count_on_the_loopback_interface() {
        use std::net::{TcpListener, TcpStream};
        use std::os::fd::IntoRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut a = InetSocket::new(client.into_raw_fd(), AF_INET, SOCK_STREAM);
        let mut b = InetSocket::new(server.into_raw_fd(), AF_INET, SOCK_STREAM);
        let mut t = Traffic::default();
        t.account(&mut a, false, 100_000, &[]);
        t.account(&mut b, true, 60_000, &[]);
        t.account(&mut b, true, 40_000, &[]);
        t.account(&mut b, true, 0, &[]);
        let lo = t.table().lo.clone();
        let route = a.route.clone().unwrap();
        let mss = route.mss as u64;
        assert_eq!(route.iface.as_deref(), Some(&*lo));
        // 回环的 MTU 65536 减去协议头
        assert_eq!(mss, 65536 - 52);
        let st = t.get(&lo);
        assert_eq!((st.tx_bytes, st.rx_bytes), (100_000, 100_000));
        assert_eq!(st.tx_packets, 100_000u64.div_ceil(mss));
        assert_eq!(
            st.rx_packets,
            60_000u64.div_ceil(mss) + 40_000u64.div_ceil(mss)
        );
    }

    /// 没有 connect 的 UDP：本端是通配地址，记下这一点，每一次按对端认接口。
    #[test]
    fn unconnected_datagrams_count_by_peer() {
        let fd = appine_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        let mut s = InetSocket::new(fd, AF_INET, SOCK_DGRAM);
        let mut t = Traffic::default();
        t.account(&mut s, false, 100, &sockaddr("127.0.0.1:9"));
        t.account(&mut s, false, 0, &sockaddr("127.0.0.1:9"));
        assert!(s.route.as_ref().is_some_and(|r| r.iface.is_none()));
        let lo = t.table().lo.clone();
        let st = t.get(&lo);
        assert_eq!((st.tx_bytes, st.tx_packets), (100, 2));
    }
}
