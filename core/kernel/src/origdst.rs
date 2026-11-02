//! `SO_ORIGINAL_DST`（docs/rounds/180-original-dst.md）：透明代理（iptables 的 REDIRECT 配上
//! `sslocal --protocol redir`、redsocks、clash 的 redir-port 这类）在接受的连接上问它原来要连
//! 哪里。Linux 由连接跟踪回答（`nf_conntrack_proto.c` 的 `getorigdst`、`ipv6_getorigdst`）。
//!
//! appine 没有连接跟踪：nat 在 connect 时由 socket 层做（[`crate::nft`]，
//! `syscall::net::route`），宿主上的连接连的就是 nat 之后的地址。所以 connect 时把被 nat 换过
//! 目的地址的 TCP 连接按宿主上的四元组记在这里（[`Table::register`]），接受连接的一端在 accept
//! 时按同一个四元组领走自己的命名空间里看到的原目的地址（[`Table::claim`]），之后记在 socket
//! 上（[`crate::net::Track::orig_dst`]）。客户端在 accept 之前就关掉了也照样领得到（Linux 的
//! 连接跟踪条目也还在）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// 宿主上的一端：IPv6 地址（IPv4 的写成 IPv4 映射的，两种 socket 连起来也对得上）和端口。
pub(crate) type End = ([u8; 16], u16);

/// 宿主的 sockaddr 换成 [`End`]；不是 IPv4、IPv6 的是 None。
pub(crate) fn end_of(addr: &[u8]) -> Option<End> {
    let family = u16::from_le_bytes([*addr.first()?, *addr.get(1)?]);
    let port = u16::from_be_bytes([*addr.get(2)?, *addr.get(3)?]);
    let mut ip = [0u8; 16];
    match family {
        AF_INET => {
            ip[10..12].copy_from_slice(&[0xff, 0xff]);
            ip[12..].copy_from_slice(addr.get(4..8)?);
        }
        AF_INET6 => ip.copy_from_slice(addr.get(8..24)?),
        _ => return None,
    }
    Some((ip, port))
}

/// 还没被领走的记录最多留这么久（Linux 连接跟踪里 SYN_SENT、TIME_WAIT 的超时都是 120 秒），
/// 最多这么多条（满了丢最早的）。连到 appine 外面的监听（手机上别的 App）的永远没人领。
const KEEP: Duration = Duration::from_secs(120);
const MAX: usize = 4096;

/// 被 nat 的 TCP 连接，按 (客户端在宿主上的本端, 宿主上连的目的地) 记。
#[derive(Default)]
pub(crate) struct Table {
    entries: HashMap<(End, End), Entry>,
    /// 上次清掉过期记录的时刻：最多每秒一次。
    pruned: Option<Instant>,
}

struct Entry {
    /// 各个网络命名空间（[`crate::nsproxy::NetNs::id`]）里看到的原目的地址（guest 的
    /// sockaddr）：发起连接的命名空间里是 OUTPUT nat 之前的，进入的别的命名空间里是那里的
    /// PREROUTING nat 之前的。
    origs: Vec<(u64, Vec<u8>)>,
    born: Instant,
}

impl Table {
    /// 没有记录：connect、accept 不必再取宿主上的地址。
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// connect 发出了连接（宿主上 `key` 这个四元组）：`origs` 是被 nat 换过目的地址的命名空间
    /// 和那里看到的原目的地址，空的是没有被 nat，这时去掉同一个四元组以前留下的记录。
    pub(crate) fn register(&mut self, key: (End, End), origs: Vec<(u64, Vec<u8>)>) {
        if origs.is_empty() {
            if !self.entries.is_empty() {
                self.entries.remove(&key);
            }
            return;
        }
        let now = Instant::now();
        if self
            .pruned
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
        {
            self.entries
                .retain(|_, e| now.duration_since(e.born) < KEEP);
            self.pruned = Some(now);
        }
        if self.entries.len() >= MAX
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.born)
                .map(|(k, _)| *k)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, Entry { origs, born: now });
    }

    /// accept 出来的连接（宿主上的对端、本端是 `key`）在命名空间 `ns` 里：领走那里看到的原
    /// 目的地址。整条记录都去掉（一个连接只有一个接受的一端）。
    pub(crate) fn claim(&mut self, key: (End, End), ns: u64) -> Option<Vec<u8>> {
        if self.entries.is_empty() {
            return None;
        }
        let e = self.entries.remove(&key)?;
        if Instant::now().duration_since(e.born) >= KEEP {
            return None;
        }
        e.origs.into_iter().find(|(n, _)| *n == ns).map(|(_, a)| a)
    }
}

/// `getsockopt` 的回答：guest 的 sockaddr `addr` 写成 `sockaddr_in`（`SOL_IP`，16 字节，
/// IPv4 映射的地址取出 IPv4 的）或者 `sockaddr_in6`（`SOL_IPV6`，28 字节；流标签、
/// scope id 是 0：appine 不支持设流标签，链路本地地址到不了别的命名空间）。
pub(crate) fn encode(addr: &[u8], v6: bool) -> Option<Vec<u8>> {
    let (ip, port) = end_of(addr)?;
    let port = port.to_be_bytes();
    if v6 {
        let mut out = vec![0u8; 28];
        out[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        out[2..4].copy_from_slice(&port);
        out[8..24].copy_from_slice(&ip);
        Some(out)
    } else {
        if ip[..12] != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff] {
            return None;
        }
        let mut out = vec![0u8; 16];
        out[..2].copy_from_slice(&AF_INET.to_le_bytes());
        out[2..4].copy_from_slice(&port);
        out[4..8].copy_from_slice(&ip[12..]);
        Some(out)
    }
}

/// guest 的 sockaddr 是不是 IPv6 的连接（不是 IPv4、也不是 IPv4 映射的地址）：连接跟踪按
/// 包的协议族分开。
pub(crate) fn is_v6(addr: &[u8]) -> bool {
    end_of(addr).is_some_and(|(ip, _)| ip[..12] != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 16];
        a[..2].copy_from_slice(&AF_INET.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&ip);
        a
    }

    fn v6(ip: [u8; 16], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 28];
        a[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[8..24].copy_from_slice(&ip);
        a
    }

    fn mapped(ip: [u8; 4]) -> [u8; 16] {
        let mut m = [0u8; 16];
        m[10..12].copy_from_slice(&[0xff, 0xff]);
        m[12..].copy_from_slice(&ip);
        m
    }

    #[test]
    fn ipv4_and_mapped_ends_match() {
        assert_eq!(
            end_of(&v4([127, 0, 0, 1], 1081)),
            end_of(&v6(mapped([127, 0, 0, 1]), 1081))
        );
        assert!(!is_v6(&v6(mapped([127, 0, 0, 1]), 1)));
        let mut lo6 = [0u8; 16];
        lo6[15] = 1;
        assert!(is_v6(&v6(lo6, 1)));
        assert_eq!(end_of(&[1, 0, 0, 0]), None);
    }

    #[test]
    fn claimed_once_by_the_right_namespace() {
        let mut t = Table::default();
        let key = (
            end_of(&v4([127, 0, 0, 1], 40000)).unwrap(),
            end_of(&v4([127, 0, 0, 1], 1081)).unwrap(),
        );
        let orig = v4([127, 0, 0, 2], 2290);
        t.register(key, vec![(1, orig.clone()), (7, v4([10, 0, 0, 2], 80))]);
        // 别的四元组领不到
        let other = (key.1, key.0);
        assert_eq!(t.claim(other, 1), None);
        assert_eq!(t.claim(key, 1), Some(orig.clone()));
        assert_eq!(t.claim(key, 1), None);
        // 不是记下的命名空间：领不到，记录也去掉
        t.register(key, vec![(1, orig.clone())]);
        assert_eq!(t.claim(key, 2), None);
        assert_eq!(t.claim(key, 1), None);
        // 同一个四元组后来没被 nat 的连接盖掉旧的
        t.register(key, vec![(1, orig)]);
        t.register(key, Vec::new());
        assert_eq!(t.claim(key, 1), None);
    }

    #[test]
    fn bounded() {
        let mut t = Table::default();
        for p in 0..(MAX as u16 + 10) {
            let key = (
                end_of(&v4([127, 0, 0, 1], p)).unwrap(),
                end_of(&v4([127, 0, 0, 1], 1)).unwrap(),
            );
            t.register(key, vec![(1, v4([1, 1, 1, 1], p))]);
        }
        assert_eq!(t.entries.len(), MAX);
    }

    #[test]
    fn encodes_like_linux() {
        let a = encode(&v6(mapped([127, 0, 0, 2]), 2290), false).unwrap();
        assert_eq!(a, v4([127, 0, 0, 2], 2290));
        let mut lo6 = [0u8; 16];
        lo6[15] = 1;
        assert_eq!(encode(&v6(lo6, 80), false), None);
        assert_eq!(encode(&v6(lo6, 80), true).unwrap(), v6(lo6, 80));
    }
}
