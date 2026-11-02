//! 新建的网络命名空间（[`NetNs`]）：里面只有回环接口 lo。
//!
//! socket 仍是宿主 socket，地址在系统调用的边界上换算：guest 的 127.0.0.x 是宿主上给这个
//! 命名空间的 127.a.b.x，绑定通配地址时绑 127.a.b.1；AF_INET6 的 ::1、:: 是 IPv4 映射的
//! ::ffff:127.a.b.1（宿主 socket 不设 `IPV6_V6ONLY`）。别的地址绑不上（EADDRNOTAVAIL）、
//! 到不了（ENETUNREACH），lo 没起来、没有地址时 127.0.0.1 也一样。没有绑定就 connect、
//! sendto、listen 的 socket 先绑到 127.a.b.1，免得宿主替它绑到所有接口上。
//!
//! lo 的配置经由 netlink（`RTM_NEWADDR`、`RTM_DELADDR`、`RTM_NEWLINK`）和接口 ioctl 读写，
//! bubblewrap 就是这样拉起 lo 的。

use std::rc::Rc;

use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::nsproxy::{Loopback, NetNs};
use appine_hal::net::Interface;

const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_LOWER_UP: u32 = 0x10000;
/// 新的命名空间里 lo 的接口编号。
pub(crate) const LO_INDEX: u32 = 1;
const LOOPBACK6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// guest 的地址拿来做什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Use {
    /// bind：要是本地地址（不是时 EADDRNOTAVAIL），通配地址也行。
    Bind,
    /// connect、sendto：要到得了（不然 ENETUNREACH）。
    Reach,
}

impl Loopback {
    /// IPv4 回环地址 `ip` 能不能这样用：与 Linux 上看到的一样，127.0.0.0/8 总能绑定（lo
    /// 关着、没有地址时也行），要 lo 起来、地址所在的网段配在上面才到得了。
    fn has_v4(&self, ip: [u8; 4], usage: Use) -> bool {
        let covered = |&(a, len): &([u8; 4], u8)| {
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            u32::from_be_bytes(a) & mask == u32::from_be_bytes(ip) & mask
        };
        match usage {
            Use::Bind => true,
            Use::Reach => self.up && self.v4.iter().any(covered),
        }
    }
}

fn family(addr: &[u8]) -> Option<i32> {
    addr.get(..2)
        .map(|b| i32::from(u16::from_le_bytes([b[0], b[1]])))
}

fn mapped(ip: &[u8]) -> Option<[u8; 4]> {
    (ip[..10] == [0; 10] && ip[10..12] == [0xff, 0xff]).then(|| ip[12..16].try_into().unwrap())
}

/// 所有的网络命名空间（地址换算要看别的命名空间里的虚拟地址）：第一个是开机时的。
pub(crate) struct World {
    pub all: Vec<Rc<NetNs>>,
}

/// 开机时的命名空间里的虚拟地址在宿主上的回环地址段：127.255.0.0/16（新命名空间的段
/// 127.a.b.0/24 的 a 不取 255）。
pub(crate) const INIT_BLOCK: u8 = 255;

/// 新命名空间里连自己的虚拟地址时源地址绑的 127.a.b.254：换回来是这个命名空间的虚拟地址
/// （guest 的 127.0.0.254 因此当作虚拟地址报告）。
const SELF_VIA_VADDR: u8 = 254;

impl World {
    pub(crate) fn init(&self) -> &Rc<NetNs> {
        &self.all[0]
    }

    /// 宿主上的回环地址段 127.a.b.0/24 是哪个新命名空间的。
    fn by_block(&self, a: u8, b: u8) -> Option<&Rc<NetNs>> {
        self.all.iter().find(|n| n.host_net == Some([a, b]))
    }

    /// 虚拟地址 `ip` 在哪个命名空间（除了 `me`）：有好几个时取 `me` 有直连路由的网段里的。
    fn owner(&self, me: &NetNs, ip: [u8; 4]) -> Option<&Rc<NetNs>> {
        let mut found = self.all.iter().filter(|n| {
            !std::ptr::eq(n.as_ref(), me)
                && crate::netdev::local_v4(n).iter().any(|(a, _)| *a == ip)
        });
        let first = found.next()?;
        let mut rest = std::iter::once(first).chain(found);
        let direct = connected(me);
        Some(
            rest.find(|_| direct.iter().any(|&(net, p)| covers(net, p, ip)))
                .unwrap_or(first),
        )
    }
}

/// `me` 里的直连网段（虚拟设备上没有网关、不是 /32 的路由）。
fn connected(me: &NetNs) -> Vec<([u8; 4], u8)> {
    crate::netdev::entries(me)
        .into_iter()
        .filter(|e| e.family == AF_INET && e.table == 254 && e.gateway.is_none() && e.prefix < 32)
        .filter_map(|e| Some((<[u8; 4]>::try_from(&e.dst[..]).ok()?, e.prefix)))
        .collect()
}

fn covers(net: [u8; 4], prefix: u8, ip: [u8; 4]) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from_be_bytes(net) & mask == u32::from_be_bytes(ip) & mask
}

/// 开机时的命名空间里虚拟地址 `v` 在宿主上的回环地址（127.255.x.y），没有就分配一个。
pub(crate) fn init_host(init: &NetNs, v: [u8; 4]) -> [u8; 4] {
    let mut d = init.devs.borrow_mut();
    if let Some((_, h)) = d.hostmap.iter().find(|(g, _)| *g == v) {
        return *h;
    }
    let n = (1..0xffffu16)
        .find(|n| {
            let h = [127, INIT_BLOCK, (n >> 8) as u8, *n as u8];
            !d.hostmap.iter().any(|(_, x)| *x == h)
        })
        .unwrap_or(1);
    let h = [127, INIT_BLOCK, (n >> 8) as u8, n as u8];
    d.hostmap.push((v, h));
    h
}

/// 宿主上的 127.255.x.y 是开机时的命名空间里哪个虚拟地址。
fn init_guest(init: &NetNs, h: [u8; 4]) -> Option<[u8; 4]> {
    init.devs
        .borrow()
        .hostmap
        .iter()
        .find(|(_, x)| *x == h)
        .map(|(g, _)| *g)
}

/// `m` 里面向 `me` 的那个虚拟地址：在 `me` 有直连路由的网段里的，没有就是第一个。
fn facing(m: &NetNs, me: &NetNs) -> Option<[u8; 4]> {
    let mine = crate::netdev::local_v4(m);
    let direct = connected(me);
    mine.iter()
        .find(|(a, _)| direct.iter().any(|&(net, p)| covers(net, p, *a)))
        .or(mine.first())
        .map(|(a, _)| *a)
}

/// `me` 发往 `ip` 时的源地址（`inet_select_addr`）：路由的首选源地址，没有时取出口接口上
/// 的地址，再没有就是 `me` 的第一个虚拟地址。
fn source(me: &NetNs, ip: [u8; 4]) -> Option<[u8; 4]> {
    let r = crate::netdev::route_v4(me, ip);
    r.as_ref()
        .and_then(|r| r.prefsrc.as_deref())
        .and_then(|p| p.try_into().ok())
        .or_else(|| r.and_then(|r| crate::netdev::first_v4(me, r.index)))
        .or_else(|| crate::netdev::local_v4(me).first().map(|(a, _)| *a))
}

/// sockaddr 里的 IPv4 地址（IPv4 映射的 IPv6 地址取 IPv4 的部分）。
fn ipv4_of(addr: &[u8]) -> Option<[u8; 4]> {
    match family(addr) {
        Some(AF_INET) if addr.len() >= 8 => addr[4..8].try_into().ok(),
        Some(AF_INET6) if addr.len() >= 24 => mapped(&addr[8..24]),
        _ => None,
    }
}

/// 把 sockaddr 里的 IPv4 地址换成 `ip`（AF_INET6 的写成 IPv4 映射的，scope id 清零）。
fn with_ipv4(addr: &[u8], ip: [u8; 4]) -> Vec<u8> {
    let mut out = addr.to_vec();
    if family(addr) == Some(AF_INET) {
        out[4..8].copy_from_slice(&ip);
    } else {
        out[8..18].fill(0);
        out[18..20].copy_from_slice(&[0xff, 0xff]);
        out[20..24].copy_from_slice(&ip);
        if let Some(s) = out.get_mut(24..28) {
            s.fill(0);
        }
    }
    out
}

/// guest 的地址换成宿主上的，结果（[`to_host`]）。
pub(crate) struct Mapped {
    pub addr: Vec<u8>,
    /// 目的地在哪个网络命名空间（低端口映射按它的）；None 是交给宿主的网络（经由默认路由
    /// 出去的、开机时的命名空间里不是虚拟地址的）。
    pub target: Option<Rc<NetNs>>,
    /// socket 还没绑定时先绑到这个宿主地址（端口 0）：新的命名空间里是它的 127.a.b.1，开机时
    /// 的命名空间里连别的命名空间时是面向对方的虚拟地址在宿主上的那个。
    pub source: Option<Vec<u8>>,
    /// 新的命名空间里绑在虚拟地址上（宿主上是 127.a.b.1，getsockname 报告它）。
    pub vaddr: Option<[u8; 4]>,
}

impl Mapped {
    fn plain(addr: &[u8]) -> Mapped {
        Mapped {
            addr: addr.to_vec(),
            target: None,
            source: None,
            vaddr: None,
        }
    }
}

/// guest 给的地址换成宿主上的（bind、connect、sendto）。不是 IPv4、IPv6 的（AF_UNSPEC 断开）
/// 原样返回。
///
/// 新的网络命名空间（127.a.b.0/24）：
/// - 127.0.0.x 是 127.a.b.x，通配地址只在 bind 时可以，lo 没起来、没有地址时到不了
///   （[`Loopback::has_v4`]）；::1、:: 是 IPv4 映射的 127.a.b.1。`icmp6` 是 ICMPv6 的 socket：
///   它发不到 IPv4 映射的地址，::1 换成宿主真正的 ::1（echo 由宿主内核回答），通配地址也
///   绑到 ::1。
/// - 自己的虚拟设备上的地址也是 127.a.b.1（与通配地址相同，监听通配地址的服务从哪个地址都
///   连得到）。
/// - 别的命名空间的虚拟地址：要有经由虚拟设备的路由（没有是 ENETUNREACH），换成那个命名
///   空间的 127.c.d.1，开机时的命名空间里的换成它的 127.255.x.y。
/// - 不是谁的虚拟地址：经由网关的路由（默认路由）到得了的交给宿主网络，原样；直连网段里
///   没有这个地址是 EHOSTUNREACH；没有路由是 ENETUNREACH。
/// - 别的地址绑不上（EADDRNOTAVAIL）。
///
/// 开机时的命名空间：自己的虚拟地址换成 127.255.x.y；经由虚拟设备的路由到得了的别的命名
/// 空间的虚拟地址同上；别的原样。
pub(crate) fn to_host(
    w: &World,
    me: &Rc<NetNs>,
    addr: &[u8],
    usage: Use,
    icmp6: bool,
) -> Result<Mapped, Errno> {
    let Some([a, b]) = me.host_net else {
        return init_to_host(w, me, addr, usage);
    };
    let lo = me.lo.borrow();
    let fail = || match usage {
        Use::Bind => Errno::EADDRNOTAVAIL,
        Use::Reach => Errno::ENETUNREACH,
    };
    let block = |x: u8| [127, a, b, x];
    let src = || implicit_bind(me, family(addr).unwrap_or(AF_INET), icmp6);
    let mapped_to = |ip: [u8; 4], target: &Rc<NetNs>, vaddr| Mapped {
        addr: with_ipv4(addr, ip),
        target: Some(target.clone()),
        source: src(),
        vaddr,
    };
    match family(addr) {
        Some(AF_INET6) if addr.len() >= 24 && icmp6 => {
            let ip = &addr[8..24];
            if ip == LOOPBACK6 && !(lo.up && lo.v6) {
                return Err(Errno::EADDRNOTAVAIL);
            }
            if !(ip == LOOPBACK6 || ip == [0; 16] && usage == Use::Bind) {
                return Err(fail());
            }
            let mut out = addr.to_vec();
            out[8..24].copy_from_slice(&LOOPBACK6);
            if let Some(s) = out.get_mut(24..28) {
                s.fill(0);
            }
            Ok(Mapped {
                target: Some(me.clone()),
                source: src(),
                ..Mapped::plain(&out)
            })
        }
        Some(AF_INET6) if addr.len() >= 24 && mapped(&addr[8..24]).is_none() => {
            let ip: [u8; 16] = addr[8..24].try_into().unwrap();
            if ip == [0; 16] && usage == Use::Bind {
                return Ok(mapped_to(block(1), me, None));
            }
            if ip == LOOPBACK6 {
                // 没有 ::1 时连接选不出源地址
                if !(lo.up && lo.v6) {
                    return Err(Errno::EADDRNOTAVAIL);
                }
                return Ok(mapped_to(block(1), me, None));
            }
            // 别的 IPv6 地址：经由虚拟设备的路由出去的交给宿主网络
            if usage == Use::Reach && crate::netdev::routes_v6(me, &ip) {
                return Ok(Mapped::plain(addr));
            }
            Err(fail())
        }
        Some(AF_INET | AF_INET6) if ipv4_of(addr).is_some() => {
            let ip = ipv4_of(addr).unwrap();
            if ip == [0; 4] {
                return match usage {
                    Use::Bind => Ok(mapped_to(block(1), me, None)),
                    Use::Reach => Err(fail()),
                };
            }
            if ip[0] == 127 {
                return if ip[..3] == [127, 0, 0] && lo.has_v4(ip, usage) {
                    Ok(mapped_to(block(ip[3]), me, None))
                } else {
                    Err(fail())
                };
            }
            drop(lo);
            if crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip) {
                if usage == Use::Bind {
                    return Ok(mapped_to(block(1), me, Some(ip)));
                }
                // 连自己的虚拟地址：源地址绑到 127.a.b.254，两边都换回虚拟地址
                let mut m = mapped_to(block(1), me, None);
                m.source = m.source.map(|s| with_ipv4(&s, block(SELF_VIA_VADDR)));
                return Ok(m);
            }
            if usage == Use::Bind {
                return Err(fail());
            }
            let route = crate::netdev::route_v4(me, ip).ok_or_else(fail)?;
            match w.owner(me, ip) {
                Some(m) => Ok(mapped_to(host_of(w, m, ip), m, None)),
                // 交给宿主网络（经由网关出去）：不先绑回环地址，绑了就出不去
                None if route.gateway.is_some() => Ok(Mapped::plain(addr)),
                None => Err(Errno::EHOSTUNREACH),
            }
        }
        Some(AF_INET | AF_INET6) => Err(Errno::EINVAL),
        _ => Ok(Mapped::plain(addr)),
    }
}

/// 别的命名空间 `m` 里的虚拟地址 `ip` 在宿主上是哪个：新命名空间的 127.c.d.1，开机时的
/// 127.255.x.y。
fn host_of(w: &World, m: &NetNs, ip: [u8; 4]) -> [u8; 4] {
    match m.host_net {
        Some([c, d]) => [127, c, d, 1],
        None => init_host(w.init(), ip),
    }
}

/// 开机时的命名空间里的 [`to_host`]。
fn init_to_host(w: &World, me: &Rc<NetNs>, addr: &[u8], usage: Use) -> Result<Mapped, Errno> {
    let Some(ip) = ipv4_of(addr).filter(|ip| ip[0] != 127 && *ip != [0; 4]) else {
        return Ok(Mapped::plain(addr));
    };
    if crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip) {
        return Ok(Mapped {
            addr: with_ipv4(addr, init_host(me, ip)),
            target: Some(me.clone()),
            source: None,
            vaddr: None,
        });
    }
    if usage == Use::Bind {
        return Ok(Mapped::plain(addr));
    }
    // 只看经由虚拟设备的路由：宿主的网段归宿主
    let Some(route) = crate::netdev::route_v4(me, ip) else {
        return Ok(Mapped::plain(addr));
    };
    match w.owner(me, ip) {
        Some(m) => {
            let src = source(me, ip).map(|s| with_ipv4(&any_like(addr), init_host(me, s)));
            Ok(Mapped {
                addr: with_ipv4(addr, host_of(w, m, ip)),
                target: Some(m.clone()),
                source: src,
                vaddr: None,
            })
        }
        None if route.gateway.is_none() => Err(Errno::EHOSTUNREACH),
        None => Ok(Mapped::plain(addr)),
    }
}

/// 与 `addr` 同一地址族、端口为 0 的通配地址。
pub(crate) fn any_like(addr: &[u8]) -> Vec<u8> {
    let mut s = vec![
        0u8;
        if family(addr) == Some(AF_INET6) {
            28
        } else {
            16
        }
    ];
    s[..2].copy_from_slice(&addr[..2]);
    s
}

/// 宿主地址是什么角色（[`from_host`]）。
pub(crate) enum Role<'a> {
    /// 对端。
    Peer,
    /// 本端：guest 绑的是通配地址、还没有连接（报告通配地址），绑在虚拟地址上（宿主上是
    /// 127.a.b.1），对端的宿主地址（面向它的那个地址）。
    Local {
        any: bool,
        vaddr: Option<[u8; 4]>,
        peer: Option<&'a [u8]>,
    },
}

/// 宿主给的地址（本端、对端）换成 `me` 里的进程看到的：自己的段 127.a.b.x 是 127.0.0.x
/// （127.a.b.1 也可能是连到它的 IPv4 的 127.0.0.1、::1，分不出来时按地址族），别的命名
/// 空间的 127.c.d.1 是那个命名空间面向 `me` 的虚拟地址，127.255.x.y 是开机时的命名空间
/// 里的虚拟地址。新的命名空间里经由宿主网络出去的连接，本端的宿主地址换成这个命名空间
/// 发往对端时的源地址。`icmp6` 见 [`to_host`]。
pub(crate) fn from_host(
    w: &World,
    me: &NetNs,
    addr: &[u8],
    role: Role<'_>,
    icmp6: bool,
) -> Vec<u8> {
    let any = matches!(role, Role::Local { any: true, .. });
    let mut out = addr.to_vec();
    if any && matches!(family(addr), Some(AF_INET | AF_INET6)) {
        let range = if family(addr) == Some(AF_INET) {
            4..8
        } else {
            8..24
        };
        if let Some(s) = out.get_mut(range) {
            s.fill(0);
        }
        return out;
    }
    if icmp6 && family(addr) == Some(AF_INET6) {
        return out;
    }
    let Some(x) = ipv4_of(addr) else {
        return out;
    };
    if x == [0; 4] {
        return out;
    }
    // 从外面转发进来的连接（crate::forward）：客户端真实的地址和端口
    if x[..2] == [127, crate::forward::OUTSIDE_BLOCK]
        && let Some((ip, port)) =
            crate::forward::outside_peer(x, u16::from_be_bytes([addr[2], addr[3]]))
    {
        out = with_ipv4(addr, ip);
        out[2..4].copy_from_slice(&port.to_be_bytes());
        return out;
    }
    let guest_of_peer = |p: &[u8]| from_host(w, me, p, Role::Peer, icmp6);
    match me.host_net {
        Some([a, b]) => {
            if x[..3] == [127, a, b] {
                let first = || crate::netdev::local_v4(me).first().map(|(v, _)| *v);
                if x[3] == SELF_VIA_VADDR
                    && let Some(v) = first()
                {
                    return with_ipv4(addr, v);
                }
                if let Role::Local { vaddr, peer, .. } = &role {
                    if let Some(v) = vaddr.filter(|_| x[3] == 1) {
                        return with_ipv4(addr, v);
                    }
                    // 自己连自己的虚拟地址：本端也是虚拟地址
                    if peer.and_then(ipv4_of) == Some([127, a, b, SELF_VIA_VADDR])
                        && let Some(v) = first()
                    {
                        return with_ipv4(addr, v);
                    }
                    // 别的命名空间连过来的：本端是面向对方的那个地址
                    let remote = peer
                        .and_then(ipv4_of)
                        .filter(|p| p[0] != 127 || p[1..3] != [a, b]);
                    if remote.is_some()
                        && let Some(g) = peer.map(guest_of_peer).as_deref().and_then(ipv4_of)
                        && let Some(s) = source(me, g)
                    {
                        return with_ipv4(addr, s);
                    }
                }
                if family(addr) == Some(AF_INET) {
                    out[4..8].copy_from_slice(&[127, 0, 0, x[3]]);
                } else if x[3] == 1 {
                    // 127.a.b.1 是 guest 的 ::1（也可能是连到它的 IPv4 的 127.0.0.1）
                    out[8..24].copy_from_slice(&LOOPBACK6);
                } else {
                    out[20..24].copy_from_slice(&[127, 0, 0, x[3]]);
                }
                return out;
            }
            if let Some(g) = remote_guest(w, me, x) {
                return with_ipv4(addr, g);
            }
            // 经由宿主网络出去的连接：本端是这个命名空间的源地址
            if x[0] != 127
                && let Role::Local { peer, .. } = &role
                && let Some(s) = peer
                    .and_then(ipv4_of)
                    .and_then(|p| source(me, p))
                    .or_else(|| crate::netdev::local_v4(me).first().map(|(a, _)| *a))
            {
                return with_ipv4(addr, s);
            }
            out
        }
        None => remote_guest(w, me, x).map_or(out, |g| with_ipv4(addr, g)),
    }
}

/// 新的网络命名空间 `t` 里有没有在 guest 地址 `dst`（sockaddr，`t` 里看到的）上监听着的 TCP
/// socket（[`crate::nsproxy::Listener`]）：通配地址的接受 IPv4 和 IPv6（AF_INET 的只接受
/// IPv4，设了 `IPV6_V6ONLY` 的只接受 IPv6），绑在某个地址上的只接受发往它的。
pub(crate) fn listening(t: &NetNs, dst: &[u8], udp: bool) -> bool {
    let Some(port) = dst.get(2..4).map(|p| u16::from_be_bytes([p[0], p[1]])) else {
        return true;
    };
    let ip: Vec<u8> = match (family(dst), ipv4_of(dst)) {
        (_, Some(v4)) => v4.to_vec(),
        (Some(AF_INET6), None) if dst.len() >= 24 => dst[8..24].to_vec(),
        _ => return true,
    };
    t.listeners.borrow().iter().any(|l| {
        l.port == port
            && l.udp == udp
            && match &l.addr {
                None if ip.len() == 4 => !l.v6 || !l.v6only,
                None => l.v6,
                Some(a) => *a == ip,
            }
    })
}

/// [`add_listener`] 登记的：guest 的端口、绑的是不是通配地址、绑的虚拟地址、宿主上的本端、
/// 地址族和 `IPV6_V6ONLY`、是不是 UDP。
#[derive(Clone, Copy)]
pub(crate) struct ListenAt<'a> {
    pub port: u16,
    pub any: bool,
    pub vaddr: Option<[u8; 4]>,
    pub host: &'a [u8],
    pub v6: bool,
    pub v6only: bool,
    pub udp: bool,
}

/// 新的网络命名空间里的 TCP socket 开始监听、UDP socket 绑定：登记它（[`listening`]）。
pub(crate) fn add_listener(me: &NetNs, l: &ListenAt<'_>) -> std::num::NonZeroU64 {
    let ListenAt {
        port,
        any,
        vaddr,
        host,
        v6,
        v6only,
        udp,
    } = *l;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 绑的 guest 地址：虚拟地址；127.a.b.x 是 127.0.0.x，AF_INET6 的 127.a.b.1 是 ::1
    let addr = if any {
        None
    } else if let Some(v) = vaddr {
        Some(v.to_vec())
    } else {
        match ipv4_of(host) {
            Some([_, _, _, 1]) if v6 && mapped(&host[8..24]).is_some() => Some(LOOPBACK6.to_vec()),
            // 发给自己的虚拟地址时绑的 127.a.b.254：收的是发往虚拟地址的
            Some([_, _, _, SELF_VIA_VADDR]) => {
                crate::netdev::local_v4(me).first().map(|(a, _)| a.to_vec())
            }
            Some([_, _, _, x]) => Some(vec![127, 0, 0, x]),
            None => None,
        }
    };
    me.listeners.borrow_mut().push(crate::nsproxy::Listener {
        id,
        port,
        addr,
        v6,
        v6only,
        udp,
    });
    std::num::NonZeroU64::new(id).expect("starts at 1")
}

/// 宿主地址 `host` 是 `me` 的 127.a.b.254（发给自己的虚拟地址时绑的源地址）。
pub(crate) fn is_self_source(me: &NetNs, host: &[u8]) -> bool {
    matches!((me.host_net, ipv4_of(host)), (Some([a, b]), Some(x)) if x == [127, a, b, SELF_VIA_VADDR])
}

/// `me` 里的数据报发往自己的虚拟地址 `dst`（guest 的）：端口是绑在 127.a.b.254 上的 socket
/// 的（[`crate::nsproxy::Loopback::self_udp`]，发给自己的虚拟地址的客户端，这是对它的回答）
/// 时，宿主上的目的地 `host` 换成 127.a.b.254。
pub(crate) fn toward_self(me: &NetNs, dst: &[u8], host: Vec<u8>) -> Vec<u8> {
    let (Some([a, b]), Some(ip)) = (me.host_net, ipv4_of(dst)) else {
        return host;
    };
    let port = u16::from_be_bytes([dst[2], dst[3]]);
    if me.lo.borrow().self_udp.contains(&port)
        && crate::netdev::local_v4(me).iter().any(|(x, _)| *x == ip)
    {
        return with_ipv4(&host, [127, a, b, SELF_VIA_VADDR]);
    }
    host
}

/// 绑在 127.a.b.254 上的 socket 收到来自 127.a.b.1 的数据报（`host` 是来源）：对方发往的是
/// 自己的虚拟地址，来源也是它（[`toward_self`]）。
pub(crate) fn from_self(me: &NetNs, host: &[u8]) -> Option<Vec<u8>> {
    let ([a, b], x) = (me.host_net?, ipv4_of(host)?);
    if x != [127, a, b, 1] {
        return None;
    }
    let v = crate::netdev::local_v4(me).first()?.0;
    Some(with_ipv4(host, v))
}

/// `me` 里发往 guest 地址 `peer` 时的本端：`local`（宿主上的本端，取它的端口）换成发往它时
/// 的源地址。
pub(crate) fn local_toward(me: &NetNs, local: &[u8], peer: &[u8]) -> Option<Vec<u8>> {
    let ip = ipv4_of(peer)?;
    let src = source(me, ip)?;
    Some(with_ipv4(local, src))
}

/// nftables 的 OUTPUT nat（[`crate::nft::Nft::nat`]）：`me` 里协议 `proto`（TCP、UDP）
/// 发往 guest 地址 `dst`（sockaddr）的连接、数据报。DNAT 了的返回新的目的地址，规则丢弃时
/// 是 EPERM。`vaddr` 是 socket 绑的虚拟地址（源地址；没有时按路由选），`sport` 取本端端口，
/// `mark` 是 socket 的 `SO_MARK`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn output_nat(
    me: &NetNs,
    proto: u8,
    vaddr: Option<[u8; 4]>,
    sport: impl FnOnce() -> u16,
    dst: &[u8],
    uid: u32,
    gid: u32,
    mark: u32,
) -> Result<Option<Vec<u8>>, Errno> {
    if !me.nft.borrow().has_nat(crate::nft::HOOK_LOCAL_OUT) {
        return Ok(None);
    }
    let Some((ip, _)) = nat_ip(dst) else {
        return Ok(None);
    };
    let src = match vaddr {
        Some(v) if ip.len() == 4 => v.to_vec(),
        _ if is_loopback(&ip) => ip.clone(),
        _ if ip.len() == 4 => source(me, ip[..4].try_into().unwrap())
            .unwrap_or_default()
            .to_vec(),
        _ => vec![0; 16],
    };
    let packet = NatPacket {
        hook: crate::nft::HOOK_LOCAL_OUT,
        proto,
        src,
        sport: sport(),
        uid,
        gid,
        iif: None,
        ct_state: crate::nft::CT_NEW,
        mark,
    };
    hook_nat(me, &packet, dst)
}

/// `me` 里发往 guest 地址 `dst` 的连接、数据报（已经换成宿主上的 `m`）进入的别的命名空间，
/// 那里的 PREROUTING 要执行（[`prerouting_nat`]）：
/// - 目的地在别的命名空间（虚拟地址）；
/// - 新命名空间里经由宿主网络出去时进入开机时的命名空间：网关是它的地址（容器经由外层的
///   bridge 连外网），或者目的地是它的本机地址（宿主的地址：容器连外层地址上发布的端口）。
///   外层的 PREROUTING 对从 bridge 来的做 REDIRECT、DNAT，就能透明代理、劫持容器连外网的
///   连接和 DNS。网关在别的命名空间里（路由器那样的"容器"）时不经过谁的 PREROUTING。
pub(crate) fn entering(w: &World, me: &Rc<NetNs>, m: &Mapped, dst: &[u8]) -> Option<Rc<NetNs>> {
    match &m.target {
        Some(t) if !Rc::ptr_eq(t, me) => Some(t.clone()),
        Some(_) => None,
        None => {
            let init = w.init();
            if me.host_net.is_none() || !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
                return None;
            }
            let ip = ipv4_of(dst)?;
            let via_init = crate::netdev::route_v4(me, ip)
                .and_then(|e| <[u8; 4]>::try_from(e.gateway?.as_slice()).ok())
                .and_then(|gw| w.owner(me, gw))
                .is_some_and(|o| Rc::ptr_eq(o, init));
            (via_init || init_local(init, ip)).then(|| init.clone())
        }
    }
}

/// 开机时的命名空间 `init` 里的本机地址（宿主的接口、lo 和虚拟设备上的）。
fn init_local(init: &NetNs, ip: [u8; 4]) -> bool {
    crate::netdev::interfaces(init)
        .iter()
        .any(|i| i.addrs.iter().any(|(_, a, _)| a[..] == ip[..]))
}

/// 从别的命名空间 `from` 进入 `t` 的连接、数据报（目的地是 `t` 的地址，guest 的 sockaddr
/// `dst`；虚拟地址对谁都一样）经过 `t` 的 PREROUTING nat：DNAT 了的返回 `t` 里看到的新的目的
/// 地址。源地址是 `from` 发往它时选的，入口接口是 `t` 里面向这个源地址的接口。
pub(crate) fn prerouting_nat(
    from: &NetNs,
    t: &NetNs,
    proto: u8,
    sport: u16,
    dst: &[u8],
) -> Result<Option<Vec<u8>>, Errno> {
    if !t.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return Ok(None);
    }
    let Some((ip, true)) = nat_ip(dst) else {
        return Ok(None);
    };
    let src = source(from, ip[..4].try_into().unwrap()).unwrap_or_default();
    let iif = crate::netdev::route_v4(t, src).and_then(|e| {
        let i = crate::netdev::interfaces(t)
            .into_iter()
            .find(|i| i.index == e.index)?;
        Some(crate::nft::Iif {
            index: i.index,
            name: i.name.into_bytes(),
            addr: crate::netdev::first_v4(t, e.index)
                .map(|a| a.to_vec())
                .unwrap_or_default(),
        })
    });
    let packet = NatPacket {
        hook: crate::nft::HOOK_PRE_ROUTING,
        proto,
        src: src.to_vec(),
        sport,
        uid: 0,
        gid: 0,
        iif: Some(iif.unwrap_or_default()),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    hook_nat(t, &packet, dst)
}

/// 从宿主外面（别的设备、手机上的别的 App）连到开机时的命名空间 `init` 里监听着的
/// `dst`（guest 的 sockaddr，通配地址换成宿主的一个地址）时 PREROUTING nat 的结果：源地址
/// 当作 192.0.2.1（TEST-NET-1），入口接口是宿主上第一个有 IPv4 地址、不是回环的接口。
pub(crate) fn external_nat(init: &NetNs, proto: u8, dst: &[u8]) -> Option<Vec<u8>> {
    if !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return None;
    }
    let outer = outer_iface(init);
    let mut dst = dst.to_vec();
    if ipv4_of(&dst) == Some([0; 4]) {
        dst = with_ipv4(&dst, outer.as_ref().map_or([127, 0, 0, 1], |(_, a)| *a));
    }
    let packet = NatPacket {
        hook: crate::nft::HOOK_PRE_ROUTING,
        proto,
        src: EXTERNAL.to_vec(),
        sport: 40000,
        uid: 0,
        gid: 0,
        iif: Some(outer.map(|(i, _)| i).unwrap_or_default()),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    hook_nat(init, &packet, &dst).ok().flatten()
}

/// 从外面来的包的源地址（TEST-NET-1）。
const EXTERNAL: [u8; 4] = [192, 0, 2, 1];

/// 从宿主外面经由开机时的命名空间 `init` 转到 DNAT 的目的地 `there`（guest 地址，在
/// 命名空间 `t` 里）：过 `init` 的 FORWARD（入口是宿主的接口）和 `t` 的 PREROUTING、INPUT 上
/// 的 filter 链，丢弃、reject 了的不转发。
pub(crate) fn external_filter(init: &NetNs, t: Option<&NetNs>, proto: u8, there: &[u8]) -> bool {
    use crate::nft::{HOOK_PRE_ROUTING, Outcome};
    const HOOK_LOCAL_IN: u32 = 1;
    const HOOK_FORWARD: u32 = 2;
    let Some((ip, true)) = nat_ip(there) else {
        return true;
    };
    let ip: [u8; 4] = ip[..].try_into().unwrap();
    let packet = |hook: u32, iif: crate::nft::Iif| NatPacket {
        hook,
        proto,
        src: EXTERNAL.to_vec(),
        sport: 40000,
        uid: 0,
        gid: 0,
        iif: Some(iif),
        ct_state: crate::nft::CT_NEW,
        mark: 0,
    };
    let pass = |o: Outcome| matches!(o, Outcome::Accept | Outcome::Dnat(..));
    let outer = outer_iface(init).map(|(i, _)| i).unwrap_or_default();
    if let Some(t) = t.filter(|t| !std::ptr::eq(*t, init)) {
        if init.nft.borrow().has_filter(HOOK_FORWARD) {
            let oif = iif_facing(init, ip).name;
            if !pass(hook_eval(
                init,
                &packet(HOOK_FORWARD, outer.clone()),
                there,
                Some(oif),
                true,
            )) {
                return false;
            }
        }
        let iif = iif_facing(t, EXTERNAL);
        for hook in [HOOK_PRE_ROUTING, HOOK_LOCAL_IN] {
            if t.nft.borrow().has_filter(hook)
                && !pass(hook_eval(t, &packet(hook, iif.clone()), there, None, true))
            {
                return false;
            }
        }
    } else if init.nft.borrow().has_filter(HOOK_LOCAL_IN)
        && !pass(hook_eval(
            init,
            &packet(HOOK_LOCAL_IN, outer),
            there,
            None,
            true,
        ))
    {
        return false;
    }
    true
}

/// 合成的包里 socket 以外的部分（[`hook_nat`]）。
struct NatPacket {
    hook: u32,
    proto: u8,
    src: Vec<u8>,
    sport: u16,
    uid: u32,
    gid: u32,
    iif: Option<crate::nft::Iif>,
    ct_state: u32,
    /// 包一开始的标记（[`crate::nft::Packet::mark`]）。
    mark: u32,
}

/// sockaddr 里要按哪种包判断：(地址, 是不是 IPv4)。IPv4 映射的 IPv6 地址按 IPv4 的包。
fn nat_ip(dst: &[u8]) -> Option<(Vec<u8>, bool)> {
    match (family(dst), ipv4_of(dst)) {
        (Some(AF_INET | AF_INET6), Some(x)) => Some((x.to_vec(), true)),
        (Some(AF_INET6), None) if dst.len() >= 24 => Some((dst[8..24].to_vec(), false)),
        _ => None,
    }
}

fn is_loopback(a: &[u8]) -> bool {
    (a.len() == 4 && a[0] == 127) || a == std::net::Ipv6Addr::LOCALHOST.octets()
}

/// `ns` 的钩子 `p.hook` 上的 nat（[`crate::nft::Nft::nat`]）：发往 guest 地址 `dst` 的包，
/// DNAT 了的返回新的目的地址（端口没换的照旧），规则丢弃时是 EPERM。
fn hook_nat(ns: &NetNs, p: &NatPacket, dst: &[u8]) -> Result<Option<Vec<u8>>, Errno> {
    let Some((_, v4)) = nat_ip(dst) else {
        return Ok(None);
    };
    match hook_eval(ns, p, dst, None, false) {
        crate::nft::Outcome::Accept => Ok(None),
        crate::nft::Outcome::Drop | crate::nft::Outcome::Reject(_) => Err(Errno::EPERM),
        crate::nft::Outcome::Dnat(a, port) => {
            let mut out = if v4 {
                with_ipv4(dst, a[..4].try_into().map_err(|_| Errno::EINVAL)?)
            } else {
                let mut o = dst.to_vec();
                o[8..24].copy_from_slice(&a);
                o
            };
            if let Some(port) = port {
                out[2..4].copy_from_slice(&port.to_be_bytes());
            }
            Ok(Some(out))
        }
    }
}

/// 按 `p` 合成发往 `dst` 的包，跑 `ns` 的钩子 `p.hook` 上的 filter 链（`filter`）或者 nat 链。
/// `oif` 是给定的出口接口的名字（FORWARD 上），None 时按路由。
fn hook_eval(
    ns: &NetNs,
    p: &NatPacket,
    dst: &[u8],
    oif: Option<Vec<u8>>,
    filter: bool,
) -> crate::nft::Outcome {
    let Some((ip, v4)) = nat_ip(dst) else {
        return crate::nft::Outcome::Accept;
    };
    let local = |a: &[u8]| {
        is_loopback(a)
            || crate::netdev::interfaces(ns)
                .iter()
                .any(|i| i.addrs.iter().any(|(_, x, _)| x == a))
    };
    let src = if p.src.len() == ip.len() {
        p.src.clone()
    } else {
        vec![0; ip.len()]
    };
    let addr_type = |a: &[u8]| crate::nft::addr_type(a, local);
    // 出口接口的编号和名字：给了名字的（FORWARD 上）按名字找，本机的是 lo，别的按路由
    let oif = || -> (u32, Vec<u8>) {
        let found = |f: &dyn Fn(&appine_hal::net::Interface) -> bool| {
            crate::netdev::interfaces(ns)
                .into_iter()
                .find(|i| f(i))
                .map(|i| (i.index, i.name.into_bytes()))
        };
        if let Some(o) = &oif {
            return found(&|i| i.name.as_bytes() == &o[..]).unwrap_or((0, o.clone()));
        }
        if local(&ip) {
            return found(&|i| i.name == "lo").unwrap_or((1, b"lo".to_vec()));
        }
        let index = v4
            .then(|| crate::netdev::route_v4(ns, ip[..4].try_into().unwrap()))
            .flatten()
            .map(|e| e.index);
        found(&|i| Some(i.index) == index).unwrap_or_default()
    };
    let packet = crate::nft::Packet {
        src: &src,
        dst: &ip,
        proto: p.proto,
        sport: p.sport,
        dport: u16::from_be_bytes([dst[2], dst[3]]),
        uid: p.uid,
        gid: p.gid,
        addr_type: &addr_type,
        oif: &oif,
        hook: p.hook,
        iif: p.iif.clone(),
        ct_state: p.ct_state,
        mark: p.mark,
    };
    let nft = ns.nft.borrow();
    if filter {
        nft.filter(&packet)
    } else {
        nft.nat(&packet)
    }
}

/// `t` 里面向 IPv4 地址 `src` 的接口（包从 `src` 来时进来的那个）：直连的网段所在的，
/// 没有时是默认路由的出口。
fn iif_facing(t: &NetNs, src: [u8; 4]) -> crate::nft::Iif {
    let Some(e) = crate::netdev::route_v4(t, src) else {
        return Default::default();
    };
    let name = crate::netdev::interfaces(t)
        .into_iter()
        .find(|i| i.index == e.index)
        .map(|i| i.name.into_bytes())
        .unwrap_or_default();
    crate::nft::Iif {
        index: e.index,
        name,
        addr: crate::netdev::first_v4(t, e.index)
            .map(|a| a.to_vec())
            .unwrap_or_default(),
    }
}

/// 宿主上第一个有 IPv4 地址、不是回环的接口：从外面进来的包的入口，出到外面的包的出口。
fn outer_iface(init: &NetNs) -> Option<(crate::nft::Iif, [u8; 4])> {
    crate::netdev::interfaces(init).into_iter().find_map(|i| {
        let a = i
            .addrs
            .iter()
            .find(|(f, a, _)| *f == AF_INET && a.len() == 4 && a[0] != 127)?;
        let addr: [u8; 4] = a.1[..].try_into().ok()?;
        Some((
            crate::nft::Iif {
                index: i.index,
                name: i.name.clone().into_bytes(),
                addr: addr.to_vec(),
            },
            addr,
        ))
    })
}

/// [`path_filter`] 里包依次进入的一个命名空间：钩子、入口接口、出口接口（FORWARD 上）。
struct Step<'a> {
    ns: &'a Rc<NetNs>,
    hook: u32,
    iif: crate::nft::Iif,
    oif: Option<Vec<u8>>,
}

impl<'a> Step<'a> {
    fn new(ns: &'a Rc<NetNs>, hook: u32, iif: crate::nft::Iif, oif: Option<Vec<u8>>) -> Step<'a> {
        Step { ns, hook, iif, oif }
    }
}

/// [`path_filter`] 的参数：包是什么样的。
pub(crate) struct Flow<'a> {
    pub proto: u8,
    /// socket 绑的虚拟地址（源地址；没有时按路由选）。
    pub vaddr: Option<[u8; 4]>,
    pub sport: u16,
    /// OUTPUT nat 之后 `me` 里看到的目的地。
    pub dst: &'a [u8],
    /// 到了目的地的命名空间里看到的（PREROUTING nat 之后）。
    pub there: &'a [u8],
    pub uid: u32,
    pub gid: u32,
    /// UDP 回给收到过的对方（连接跟踪的 ESTABLISHED）。
    pub established: bool,
    /// socket 的 `SO_MARK`：OUTPUT 和经由 lo 回到自己的命名空间的包带着它，经由 veth 进入
    /// 别的命名空间时清掉。
    pub mark: u32,
}

/// `me` 里的 socket 发出的连接、数据报经过的过滤链（docs/rounds/175-filter.md），只看 IPv4：
///
/// 1. `me` 的 OUTPUT。
/// 2. 新命名空间里的，经由开机时的命名空间：出到宿主网络（`t` 是 None）走它的 FORWARD，出口是
///    宿主的接口；到它自己走 INPUT；到别的命名空间而目的地不在 `me` 的直连网段里（经由网关）
///    走 FORWARD。
/// 3. 到了目的地的命名空间 `t`：INPUT（开机时的命名空间见上）。
///
/// 进入一个命名空间时先过它 PREROUTING 上的 filter 链（raw 表这些）。返回结果和是不是在 OUTPUT
/// 上丢弃、拒绝的。
pub(crate) fn path_filter(
    w: &World,
    me: &Rc<NetNs>,
    t: Option<&Rc<NetNs>>,
    f: &Flow<'_>,
) -> (crate::nft::Outcome, bool) {
    use crate::nft::{HOOK_LOCAL_OUT, HOOK_PRE_ROUTING, Outcome};
    const HOOK_LOCAL_IN: u32 = 1;
    const HOOK_FORWARD: u32 = 2;
    let ct_state = if f.established {
        crate::nft::CT_ESTABLISHED
    } else {
        crate::nft::CT_NEW
    };
    let (Some((ip, true)), Some((final_ip, true))) = (nat_ip(f.dst), nat_ip(f.there)) else {
        return (Outcome::Accept, false);
    };
    let ip: [u8; 4] = ip[..].try_into().unwrap();
    let final_ip: [u8; 4] = final_ip[..].try_into().unwrap();
    let src = match f.vaddr {
        Some(v) => v,
        None if ip[0] == 127 => ip,
        None => source(me, ip).unwrap_or([0; 4]),
    };
    let packet = |hook: u32, iif: Option<crate::nft::Iif>, ns: &NetNs| NatPacket {
        hook,
        proto: f.proto,
        src: src.to_vec(),
        sport: f.sport,
        uid: f.uid,
        gid: f.gid,
        iif,
        ct_state,
        mark: if std::ptr::eq(ns, me.as_ref()) {
            f.mark
        } else {
            0
        },
    };
    let pass = |o: &Outcome| matches!(o, Outcome::Accept | Outcome::Dnat(..));
    // 1. OUTPUT
    if me.nft.borrow().has_filter(HOOK_LOCAL_OUT) {
        let o = hook_eval(me, &packet(HOOK_LOCAL_OUT, None, me), f.dst, None, true);
        if !pass(&o) {
            return (o, true);
        }
    }
    // 依次进入的：(命名空间, 钩子, 入口接口, 出口接口)
    let init = w.init();
    let lo = crate::nft::Iif {
        index: 1,
        name: b"lo".to_vec(),
        addr: vec![127, 0, 0, 1],
    };
    let mut steps: Vec<Step<'_>> = Vec::new();
    let new_ns = me.host_net.is_some();
    match t {
        None if new_ns => {
            let oif = outer_iface(init).map(|(i, _)| i.name).unwrap_or_default();
            steps.push(Step::new(
                init,
                HOOK_FORWARD,
                iif_facing(init, src),
                Some(oif),
            ));
        }
        None => {}
        Some(x) if Rc::ptr_eq(x, me) => steps.push(Step::new(me, HOOK_LOCAL_IN, lo, None)),
        Some(x) if Rc::ptr_eq(x, init) => {
            steps.push(Step::new(init, HOOK_LOCAL_IN, iif_facing(init, src), None))
        }
        Some(x) => {
            // 不经由开机时的命名空间的：发往直连网段里 x 的地址（之后 x 的 PREROUTING 可能
            // 再 DNAT）。发往网关后面的地址、外层的本机地址，在外层的 PREROUTING 上 DNAT 到
            // x 的（容器连发布的端口、连外网被 DNAT 到别的容器），都经过外层的 FORWARD
            let direct = crate::netdev::route_v4(me, ip).is_some_and(|e| e.gateway.is_none())
                && !init_local(init, ip);
            if new_ns && !direct {
                let oif = iif_facing(init, final_ip).name;
                steps.push(Step::new(
                    init,
                    HOOK_FORWARD,
                    iif_facing(init, src),
                    Some(oif),
                ));
            }
            steps.push(Step::new(x, HOOK_LOCAL_IN, iif_facing(x, src), None));
        }
    }
    for Step { ns, hook, iif, oif } in steps {
        let (pre, this) = {
            let nft = ns.nft.borrow();
            (nft.has_filter(HOOK_PRE_ROUTING), nft.has_filter(hook))
        };
        // 回环上自己发给自己的也经过 PREROUTING，Linux 一样
        if pre {
            let o = hook_eval(
                ns,
                &packet(HOOK_PRE_ROUTING, Some(iif.clone()), ns),
                f.there,
                None,
                true,
            );
            if !pass(&o) {
                return (o, false);
            }
        }
        if this {
            let o = hook_eval(ns, &packet(hook, Some(iif), ns), f.there, oif, true);
            if !pass(&o) {
                return (o, false);
            }
        }
    }
    (Outcome::Accept, false)
}

/// 宿主上别的命名空间的回环地址（127.c.d.1、127.255.x.y）在 `me` 里看到的虚拟地址。
fn remote_guest(w: &World, me: &NetNs, x: [u8; 4]) -> Option<[u8; 4]> {
    if x[0] != 127 {
        return None;
    }
    if x[1] == INIT_BLOCK {
        return init_guest(w.init(), x);
    }
    let m = w.by_block(x[1], x[2]).filter(|_| x[3] == 1)?;
    facing(m, me)
}

/// 宿主地址所在的命名空间（低端口映射的登记表按它，[`crate::portmap`]）：别的命名空间的
/// 段是那个，127.255.x.y 是开机时的；别的是 None（按 socket 自己的）。
pub(crate) fn owner_of_host(w: &World, addr: &[u8]) -> Option<Rc<NetNs>> {
    let x = ipv4_of(addr).filter(|x| x[0] == 127)?;
    if x[1] == INIT_BLOCK {
        return Some(w.init().clone());
    }
    w.by_block(x[1], x[2]).cloned()
}

/// 是 IPv4、IPv6 的地址。
pub(crate) fn is_inet(addr: &[u8]) -> bool {
    matches!(family(addr), Some(AF_INET | AF_INET6))
}

/// guest 绑的是通配地址（0.0.0.0、::、::ffff:0.0.0.0）。
pub(crate) fn is_any(addr: &[u8]) -> bool {
    match family(addr) {
        Some(AF_INET) => addr.get(4..8) == Some(&[0; 4]),
        Some(AF_INET6) => addr
            .get(8..24)
            .is_some_and(|ip| ip == [0; 16] || mapped(ip) == Some([0; 4])),
        _ => false,
    }
}

/// 没有绑定的 socket 先绑到的宿主地址（端口 0）。
pub(crate) fn implicit_bind(ns: &NetNs, family: i32, icmp6: bool) -> Option<Vec<u8>> {
    let [a, b] = ns.host_net?;
    Some(if icmp6 {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[8..24].copy_from_slice(&LOOPBACK6);
        s
    } else if family == AF_INET6 {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[18..20].copy_from_slice(&[0xff, 0xff]);
        s[20..24].copy_from_slice(&[127, a, b, 1]);
        s
    } else {
        let mut s = vec![0u8; 16];
        s[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        s[4..8].copy_from_slice(&[127, a, b, 1]);
        s
    })
}

/// lo 这个接口（netlink、procfs 的回答用）。
pub(crate) fn lo_interface(ns: &NetNs) -> Interface {
    let lo = ns.lo.borrow();
    let mut addrs: Vec<(i32, Vec<u8>, u8)> = lo
        .v4
        .iter()
        .map(|(a, len)| (AF_INET, a.to_vec(), *len))
        .collect();
    if lo.v6 {
        addrs.push((AF_INET6, LOOPBACK6.to_vec(), 128));
    }
    Interface {
        name: "lo".into(),
        index: LO_INDEX,
        flags: if lo.up {
            IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP
        } else {
            IFF_LOOPBACK
        },
        mtu: lo.mtu.unwrap_or(65536),
        mac: None,
        addrs,
    }
}

/// 把 lo 打开或关上（`RTM_NEWLINK`、`SIOCSIFFLAGS`）：打开时没有 IPv4 地址就加上
/// 127.0.0.1/8（Linux 的 `inetdev_event`），并且加上 ::1（addrconf）；关上时 ::1 随之去掉。
pub(crate) fn set_up(lo: &mut Loopback, up: bool) {
    if up && !lo.up {
        if lo.v4.is_empty() {
            lo.v4.push(([127, 0, 0, 1], 8));
        }
        lo.v6 = true;
    } else if !up {
        lo.v6 = false;
    }
    lo.up = up;
}

/// netlink 修改 lo 的请求（整条消息）：`RTM_NEWADDR`、`RTM_DELADDR`、`RTM_NEWLINK`
/// （`RTM_SETLINK`）。返回 None 时不是这几种；Some 里是错误码（0 是成功）。要在命名空间的
/// 属主里有 `CAP_NET_ADMIN`（EPERM，Linux 的 `netlink_net_capable`）。
pub(crate) fn configure(ns: &NetNs, cred: &Cred, req: &[u8]) -> Option<i32> {
    const RTM_NEWLINK: u16 = 16;
    const RTM_SETLINK: u16 = 19;
    const RTM_NEWADDR: u16 = 20;
    const RTM_DELADDR: u16 = 21;
    const NLM_F_EXCL: u16 = 0x200;
    const IFA_ADDRESS: u16 = 1;
    const IFA_LOCAL: u16 = 2;
    let ty = u16::from_le_bytes([req[4], req[5]]);
    if !matches!(ty, RTM_NEWLINK | RTM_SETLINK | RTM_NEWADDR | RTM_DELADDR) {
        return None;
    }
    if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
        return Some(Errno::EPERM.0);
    }
    let body = &req[16..];
    let mut lo = ns.lo.borrow_mut();
    Some(match ty {
        // struct ifinfomsg：地址族、类型、编号、标志、要改的标志
        RTM_NEWLINK | RTM_SETLINK => {
            let Some(b) = body.get(..16) else {
                return Some(Errno::EINVAL.0);
            };
            let index = i32::from_le_bytes(b[4..8].try_into().unwrap());
            let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let change = u32::from_le_bytes(b[12..16].try_into().unwrap());
            if index != LO_INDEX as i32 {
                return Some(Errno::ENODEV.0);
            }
            if change & IFF_UP != 0 || change == 0 {
                set_up(&mut lo, flags & IFF_UP != 0);
            }
            0
        }
        // struct ifaddrmsg：地址族、前缀长度、标志、范围、编号，后面是 IFA_LOCAL、IFA_ADDRESS
        _ => {
            let Some(b) = body.get(..8) else {
                return Some(Errno::EINVAL.0);
            };
            let (family, prefix) = (i32::from(b[0]), b[1]);
            let index = u32::from_le_bytes(b[4..8].try_into().unwrap());
            if index != LO_INDEX {
                return Some(Errno::ENODEV.0);
            }
            let attrs = crate::route::attrs(&body[8..]);
            let addr = attrs
                .iter()
                .find(|(t, _)| *t == IFA_LOCAL)
                .or_else(|| attrs.iter().find(|(t, _)| *t == IFA_ADDRESS))
                .map(|(_, d)| d.to_vec());
            let excl = u16::from_le_bytes([req[6], req[7]]) & NLM_F_EXCL != 0;
            match (family, addr) {
                (AF_INET, Some(a)) if a.len() == 4 && prefix <= 32 => {
                    let a: [u8; 4] = a.try_into().unwrap();
                    let at = lo.v4.iter().position(|(x, _)| *x == a);
                    match (ty, at) {
                        (RTM_NEWADDR, Some(_)) if excl => Errno::EEXIST.0,
                        (RTM_NEWADDR, Some(i)) => {
                            lo.v4[i].1 = prefix;
                            0
                        }
                        (RTM_NEWADDR, None) => {
                            lo.v4.push((a, prefix));
                            0
                        }
                        (_, Some(i)) => {
                            lo.v4.remove(i);
                            0
                        }
                        (_, None) => Errno::EADDRNOTAVAIL.0,
                    }
                }
                (AF_INET6, Some(a)) if a == LOOPBACK6 => match (ty, lo.v6) {
                    (RTM_NEWADDR, true) if excl => Errno::EEXIST.0,
                    (RTM_NEWADDR, _) => {
                        lo.v6 = true;
                        0
                    }
                    (_, true) => {
                        lo.v6 = false;
                        0
                    }
                    (_, false) => Errno::EADDRNOTAVAIL.0,
                },
                // 别的 IPv6 地址到不了宿主上，不能配
                (AF_INET6, Some(_)) => Errno::EOPNOTSUPP.0,
                (AF_INET | AF_INET6, _) => Errno::EINVAL.0,
                _ => Errno::EAFNOSUPPORT.0,
            }
        }
    })
}

/// 接口 ioctl（`struct ifreq`，40 字节，前 16 字节是名字）：新的命名空间里只有 lo。
/// 返回 None 时不是这里处理的。
pub(crate) fn interface_ioctl(
    ns: &NetNs,
    cred: &Cred,
    request: u32,
    ifr: &mut [u8; 40],
) -> Option<Result<(), Errno>> {
    const SIOCGIFNAME: u32 = 0x8910;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCSIFFLAGS: u32 = 0x8914;
    const SIOCGIFADDR: u32 = 0x8915;
    const SIOCGIFNETMASK: u32 = 0x891b;
    const SIOCGIFMTU: u32 = 0x8921;
    const SIOCSIFMTU: u32 = 0x8922;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const SIOCGIFINDEX: u32 = 0x8933;
    const SIOCGIFTXQLEN: u32 = 0x8942;
    const ARPHRD_LOOPBACK: u16 = 772;
    let name = ifr[..16].split(|&c| c == 0).next().unwrap_or_default();
    if request == SIOCGIFNAME {
        let index = i32::from_le_bytes(ifr[16..20].try_into().unwrap());
        if index != LO_INDEX as i32 {
            return Some(Err(Errno::ENODEV));
        }
        ifr[..16].fill(0);
        ifr[..2].copy_from_slice(b"lo");
        return Some(Ok(()));
    }
    let known = [
        SIOCGIFFLAGS,
        SIOCSIFFLAGS,
        SIOCSIFMTU,
        SIOCGIFADDR,
        SIOCGIFNETMASK,
        SIOCGIFMTU,
        SIOCGIFHWADDR,
        SIOCGIFINDEX,
        SIOCGIFTXQLEN,
    ];
    if !known.contains(&request) {
        return None;
    }
    if name != b"lo" {
        return Some(Err(Errno::ENODEV));
    }
    let lo_if = lo_interface(ns);
    let data = &mut ifr[16..];
    if matches!(request, SIOCSIFFLAGS | SIOCSIFMTU) {
        if !cred.ns_capable(&ns.owner, cap::NET_ADMIN) {
            return Some(Err(Errno::EPERM));
        }
        let mut lo = ns.lo.borrow_mut();
        if request == SIOCSIFFLAGS {
            let flags = u16::from_le_bytes([data[0], data[1]]);
            set_up(&mut lo, u32::from(flags) & IFF_UP != 0);
        } else {
            // 与 Linux 的 dev_set_mtu 一样不能小于 IPv4 的最小值 68（loopback 没有上限）
            let mtu = i32::from_le_bytes(data[..4].try_into().unwrap());
            if mtu < 68 {
                return Some(Err(Errno::EINVAL));
            }
            lo.mtu = Some(mtu as u32);
        }
        return Some(Ok(()));
    }
    data.fill(0);
    let sockaddr = |data: &mut [u8], a: [u8; 4]| {
        data[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        data[4..8].copy_from_slice(&a);
    };
    match request {
        SIOCGIFFLAGS => data[..2].copy_from_slice(&(lo_if.flags as u16).to_le_bytes()),
        SIOCGIFADDR | SIOCGIFNETMASK => {
            let Some(&(a, len)) = ns.lo.borrow().v4.first() else {
                return Some(Err(Errno::EADDRNOTAVAIL));
            };
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            let value = if request == SIOCGIFADDR {
                a
            } else {
                mask.to_be_bytes()
            };
            sockaddr(data, value);
        }
        SIOCGIFMTU => data[..4].copy_from_slice(&(lo_if.mtu as i32).to_le_bytes()),
        SIOCGIFHWADDR => data[..2].copy_from_slice(&ARPHRD_LOOPBACK.to_le_bytes()),
        SIOCGIFINDEX => data[..4].copy_from_slice(&(LO_INDEX as i32).to_le_bytes()),
        _ => data[..4].copy_from_slice(&1000i32.to_le_bytes()),
    }
    Some(Ok(()))
}

/// `SIOCGIFCONF`：配了 IPv4 地址、起来了的 lo（Linux 的 `inet_gifconf` 只列 IPv4 地址）。
pub(crate) fn ifconf(ns: &NetNs) -> Vec<u8> {
    let lo = ns.lo.borrow();
    let mut out = Vec::new();
    for (a, _) in lo.v4.iter().filter(|_| lo.up) {
        let mut r = [0u8; 40];
        r[..2].copy_from_slice(b"lo");
        r[16..18].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        r[20..24].copy_from_slice(a);
        out.extend_from_slice(&r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::RefCell;

    fn ns() -> NetNs {
        NetNs {
            id: 1,
            owner: crate::cred::UserNs::init(),
            host_net: Some([1, 2]),
            lo: Default::default(),
            devs: Default::default(),
            nft: Default::default(),
            listeners: Default::default(),
        }
    }

    fn v4(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut s = vec![0u8; 16];
        s[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        s[2..4].copy_from_slice(&port.to_be_bytes());
        s[4..8].copy_from_slice(&ip);
        s
    }

    fn v6(ip: [u8; 16], port: u16) -> Vec<u8> {
        let mut s = vec![0u8; 28];
        s[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
        s[2..4].copy_from_slice(&port.to_be_bytes());
        s[8..24].copy_from_slice(&ip);
        s
    }

    fn up(ns: &NetNs) {
        set_up(&mut ns.lo.borrow_mut(), true);
    }

    /// 从外面转发进来的连接（127.254.x.y，crate::forward）：对端报告成客户端真实的地址和端口，
    /// 宿主端口对不上的不换。
    #[test]
    fn outside_peers_show_the_real_client() {
        let n = Rc::new(ns());
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, n.clone()],
        };
        crate::forward::record_outside_for_test(
            [127, 254, 200, 7],
            [192, 168, 1, 50],
            51000,
            51000,
        );
        let got = from_host(&w, &n, &v4([127, 254, 200, 7], 51000), Role::Peer, false);
        assert_eq!(got, v4([192, 168, 1, 50], 51000));
        let other = from_host(&w, &n, &v4([127, 254, 200, 7], 51001), Role::Peer, false);
        assert_ne!(other[4..8], [192, 168, 1, 50]);
    }

    /// 监听的登记：通配地址按地址族、`IPV6_V6ONLY`，绑了地址的只接受发往它的。
    #[test]
    fn listeners_match_like_linux() {
        let n = ns();
        let host = |x: u8| v4([127, 1, 2, x], 0);
        let mapped = |x: u8| {
            let mut a = [0u8; 16];
            a[10..12].copy_from_slice(&[0xff, 0xff]);
            a[12..].copy_from_slice(&[127, 1, 2, x]);
            v6(a, 0)
        };
        let eth0 = [10, 9, 0, 2];
        let a = add_listener(
            &n,
            &ListenAt {
                port: 7000,
                any: true,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7001,
                any: true,
                vaddr: None,
                host: &mapped(1),
                v6: true,
                v6only: true,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7002,
                any: false,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7003,
                any: false,
                vaddr: Some(eth0),
                host: &host(1),
                v6: false,
                v6only: false,
                udp: false,
            },
        );
        add_listener(
            &n,
            &ListenAt {
                port: 7004,
                any: false,
                vaddr: None,
                host: &mapped(1),
                v6: true,
                v6only: false,
                udp: false,
            },
        );
        // 0.0.0.0：IPv4 的都行，纯 IPv6 的不行
        assert!(listening(&n, &v4(eth0, 7000), false));
        assert!(listening(&n, &v4([127, 0, 0, 1], 7000), false));
        assert!(!listening(&n, &v6(LOOPBACK6, 7000), false));
        // [::] 且 IPV6_V6ONLY：只有 IPv6
        assert!(listening(&n, &v6(LOOPBACK6, 7001), false));
        assert!(!listening(&n, &v4(eth0, 7001), false));
        // 127.0.0.1、eth0 的地址、::1 只接受发往它的
        assert!(listening(&n, &v4([127, 0, 0, 1], 7002), false));
        assert!(!listening(&n, &v4(eth0, 7002), false));
        assert!(listening(&n, &v4(eth0, 7003), false));
        assert!(!listening(&n, &v4([127, 0, 0, 1], 7003), false));
        assert!(listening(&n, &v6(LOOPBACK6, 7004), false));
        assert!(!listening(&n, &v4([127, 0, 0, 1], 7004), false));
        // UDP 和 TCP 分开登记
        add_listener(
            &n,
            &ListenAt {
                port: 7005,
                any: true,
                vaddr: None,
                host: &host(1),
                v6: false,
                v6only: false,
                udp: true,
            },
        );
        assert!(listening(&n, &v4(eth0, 7005), true));
        assert!(!listening(&n, &v4(eth0, 7005), false));
        assert!(!listening(&n, &v4(eth0, 7000), true));
        // 没有监听的端口；去掉之后也没有了
        assert!(!listening(&n, &v4(eth0, 7999), false));
        n.listeners.borrow_mut().retain(|l| l.id != a.get());
        assert!(!listening(&n, &v4(eth0, 7000), false));
    }

    /// 只有开机时的命名空间和 `ns` 的世界里换算（只看地址）。
    fn th(ns: &Rc<NetNs>, addr: &[u8], usage: Use, icmp6: bool) -> Result<Vec<u8>, Errno> {
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, ns.clone()],
        };
        to_host(&w, ns, addr, usage, icmp6).map(|m| m.addr)
    }

    fn fh(ns: &Rc<NetNs>, addr: &[u8], any: bool, icmp6: bool) -> Vec<u8> {
        let w = World {
            all: vec![crate::nsproxy::NsProxy::init().net, ns.clone()],
        };
        let role = if any {
            Role::Local {
                any: true,
                vaddr: None,
                peer: None,
            }
        } else {
            Role::Peer
        };
        from_host(&w, ns, addr, role, icmp6)
    }

    #[test]
    fn loopback_addresses_map_to_the_namespace_range() {
        let ns = Rc::new(ns());
        // lo 还关着：通配地址能绑，127.0.0.1 绑不上、连不到，外面的地址总是连不到
        assert_eq!(
            th(&ns, &v4([0; 4], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 80), Use::Reach, false),
            Err(Errno::EADDRNOTAVAIL)
        );
        up(&ns);
        assert_eq!(ns.lo.borrow().v4, [([127, 0, 0, 1], 8)]);
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            th(&ns, &v4([127, 0, 0, 9], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 9], 80)
        );
        assert_eq!(
            th(&ns, &v4([8, 8, 8, 8], 53), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            th(&ns, &v4([127, 1, 0, 1], 80), Use::Bind, false),
            Err(Errno::EADDRNOTAVAIL)
        );
        // ::1 和 :: 是 IPv4 映射的 127.a.b.1，反过来是 ::1
        let mapped = {
            let mut ip = [0u8; 16];
            ip[10..12].copy_from_slice(&[0xff, 0xff]);
            ip[12..].copy_from_slice(&[127, 1, 2, 1]);
            ip
        };
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 80), Use::Reach, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(
            th(&ns, &v6([0; 16], 80), Use::Bind, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(fh(&ns, &v6(mapped, 80), false, false), v6(LOOPBACK6, 80));
        assert_eq!(fh(&ns, &v6(mapped, 80), true, false), v6([0; 16], 80));
        assert_eq!(
            fh(&ns, &v4([127, 1, 2, 7], 9), false, false),
            v4([127, 0, 0, 7], 9)
        );
        assert_eq!(fh(&ns, &v4([127, 1, 2, 1], 9), true, false), v4([0; 4], 9));
        let mut doc = [0u8; 16];
        doc[0] = 0x20;
        assert_eq!(
            th(&ns, &v6(doc, 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        // ICMPv6 的 socket：::1 是宿主的 ::1，通配地址也绑到它，别的照样到不了
        assert_eq!(
            th(&ns, &v6(LOOPBACK6, 0), Use::Reach, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            th(&ns, &v6([0; 16], 0), Use::Bind, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            th(&ns, &v6(doc, 0), Use::Reach, true),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(fh(&ns, &v6(LOOPBACK6, 0), false, true), v6(LOOPBACK6, 0));
        assert_eq!(fh(&ns, &v6(LOOPBACK6, 0), true, true), v6([0; 16], 0));
        // 开机时的命名空间不换
        let init = crate::nsproxy::NsProxy::init().net;
        assert_eq!(
            th(&init, &v4([8, 8, 8, 8], 53), Use::Reach, false).unwrap(),
            v4([8, 8, 8, 8], 53)
        );
        let _ = Rc::clone(&init);
    }

    #[test]
    fn virtual_addresses_map_across_namespaces() {
        let init = crate::nsproxy::NsProxy::init().net;
        let net = |b: u8| {
            Rc::new(NetNs {
                id: u64::from(b),
                owner: crate::cred::UserNs::init(),
                host_net: Some([7, b]),
                lo: Default::default(),
                devs: RefCell::new(crate::netdev::Devices::for_new_ns(&Default::default())),
                nft: Default::default(),
                listeners: Default::default(),
            })
        };
        let (a, b) = (net(1), net(2));
        let gw = [10, 50, 0, 1];
        crate::netdev::test_dev(&init, "br0", gw, 24, None);
        crate::netdev::test_dev(&a, "eth0", [10, 50, 0, 2], 24, Some(gw));
        crate::netdev::test_dev(&b, "eth0", [10, 50, 0, 3], 24, Some(gw));
        let w = World {
            all: vec![init.clone(), a.clone(), b.clone()],
        };
        let reach = |me: &Rc<NetNs>, ip| to_host(&w, me, &v4(ip, 80), Use::Reach, false);
        let target = |m: &Mapped| m.target.as_ref().map(|t| t.id);
        // 别的容器、网关、外面、直连网段里没有的地址
        let m = reach(&a, [10, 50, 0, 3]).unwrap();
        assert_eq!(
            (m.addr.clone(), target(&m)),
            (v4([127, 7, 2, 1], 80), Some(2))
        );
        assert_eq!(m.source, Some(v4([127, 7, 1, 1], 0)));
        let m = reach(&a, gw).unwrap();
        assert_eq!(m.addr, v4([127, INIT_BLOCK, 0, 1], 80));
        let m = reach(&a, [8, 8, 8, 8]).unwrap();
        assert_eq!(
            (m.addr.clone(), m.target.is_none(), m.source),
            (v4([8, 8, 8, 8], 80), true, None)
        );
        assert_eq!(reach(&a, [10, 50, 0, 9]).err(), Some(Errno::EHOSTUNREACH));
        // 绑自己的地址：宿主上是 127.a.b.1，记下虚拟地址；别人的绑不上
        let m = to_host(&w, &a, &v4([10, 50, 0, 2], 80), Use::Bind, false).unwrap();
        assert_eq!(
            (m.addr, m.vaddr),
            (v4([127, 7, 1, 1], 80), Some([10, 50, 0, 2]))
        );
        assert_eq!(
            to_host(&w, &a, &v4([10, 50, 0, 3], 80), Use::Bind, false).err(),
            Some(Errno::EADDRNOTAVAIL)
        );
        // 开机时的命名空间：连容器时先绑网关地址在宿主上的那个，自己的地址绑到它
        let m = reach(&init, [10, 50, 0, 2]).unwrap();
        assert_eq!(m.addr, v4([127, 7, 1, 1], 80));
        assert_eq!(m.source, Some(v4([127, INIT_BLOCK, 0, 1], 0)));
        let m = to_host(&w, &init, &v4(gw, 53), Use::Bind, false).unwrap();
        assert_eq!(m.addr, v4([127, INIT_BLOCK, 0, 1], 53));
        assert_eq!(
            reach(&init, [8, 8, 8, 8]).unwrap().addr,
            v4([8, 8, 8, 8], 80)
        );
        // 反过来：别的命名空间的段、127.255.x.y 换回虚拟地址；本端面向对方
        let peer = |me: &NetNs, x| from_host(&w, me, &v4(x, 9), Role::Peer, false);
        assert_eq!(peer(&b, [127, 7, 1, 1]), v4([10, 50, 0, 2], 9));
        assert_eq!(peer(&a, [127, INIT_BLOCK, 0, 1]), v4(gw, 9));
        assert_eq!(peer(&init, [127, 7, 2, 1]), v4([10, 50, 0, 3], 9));
        assert_eq!(peer(&a, [8, 8, 4, 4]), v4([8, 8, 4, 4], 9));
        let local = |me: &NetNs, x, p| {
            let p = v4(p, 1);
            let role = Role::Local {
                any: false,
                vaddr: None,
                peer: Some(&p),
            };
            from_host(&w, me, &v4(x, 9), role, false)
        };
        assert_eq!(
            local(&a, [127, 7, 1, 1], [127, 7, 2, 1]),
            v4([10, 50, 0, 2], 9)
        );
        assert_eq!(
            local(&a, [192, 168, 1, 5], [8, 8, 8, 8]),
            v4([10, 50, 0, 2], 9)
        );
        // 没有默认路由时外面到不了
        let mut r = vec![AF_INET as u8, 0, 0, 0, 254, 0, 255, 0, 0, 0, 0, 0];
        crate::net::attr(&mut r, 5, &gw);
        let del = crate::net::message(25, 1, 1, 0, &r);
        let none = |_: u16, _: u32| -> Result<Rc<NetNs>, Errno> { Err(Errno::ESRCH) };
        crate::netdev::request(&a, &Cred::root(), &del, &none)
            .unwrap()
            .unwrap();
        assert_eq!(reach(&a, [8, 8, 8, 8]).err(), Some(Errno::ENETUNREACH));
    }

    #[test]
    fn netlink_configures_lo() {
        let ns = ns();
        let root = Cred::root();
        let msg = |ty: u16, flags: u16, body: &[u8]| {
            let mut m = crate::net::message(ty, flags | 1, 1, 0, body);
            m.truncate(16 + body.len());
            m
        };
        // bwrap 的 loopback_setup：先加 127.0.0.1/8，再打开 lo
        let mut addr = vec![AF_INET as u8, 8, 0x80, 254, 1, 0, 0, 0];
        crate::net::attr(&mut addr, 2, &[127, 0, 0, 1]);
        crate::net::attr(&mut addr, 1, &[127, 0, 0, 1]);
        assert_eq!(configure(&ns, &root, &msg(20, 0x600, &addr)), Some(0));
        assert_eq!(
            configure(&ns, &root, &msg(20, 0x600, &addr)),
            Some(Errno::EEXIST.0)
        );
        let mut link = vec![0u8; 16];
        link[4..8].copy_from_slice(&1i32.to_le_bytes());
        link[8..12].copy_from_slice(&IFF_UP.to_le_bytes());
        link[12..16].copy_from_slice(&IFF_UP.to_le_bytes());
        assert_eq!(configure(&ns, &root, &msg(16, 0, &link)), Some(0));
        let lo = lo_interface(&ns);
        assert_eq!(lo.flags & IFF_UP, IFF_UP);
        assert_eq!(lo.addrs.len(), 2);
        // 别的接口没有；没有 CAP_NET_ADMIN 不能配
        link[4..8].copy_from_slice(&2i32.to_le_bytes());
        assert_eq!(
            configure(&ns, &root, &msg(16, 0, &link)),
            Some(Errno::ENODEV.0)
        );
        let mut nobody = Cred::root();
        nobody.cap_effective = 0;
        assert_eq!(
            configure(&ns, &nobody, &msg(16, 0, &link)),
            Some(Errno::EPERM.0)
        );
        assert_eq!(configure(&ns, &root, &msg(18, 0, &link)), None);
    }
}
