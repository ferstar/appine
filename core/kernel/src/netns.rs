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

/// guest 给的地址换成宿主上的。不是 IPv4、IPv6 的（AF_UNSPEC 断开）原样返回。
///
/// `icmp6` 是 ICMPv6 的 socket：它发不到 IPv4 映射的地址，::1 换成宿主真正的 ::1（echo 由
/// 宿主内核回答，没有别的东西经由它），通配地址也绑到 ::1，别的地址照样到不了。
pub(crate) fn to_host(ns: &NetNs, addr: &[u8], usage: Use, icmp6: bool) -> Result<Vec<u8>, Errno> {
    let Some([a, b]) = ns.host_net else {
        return Ok(addr.to_vec());
    };
    let lo = ns.lo.borrow();
    let fail = || match usage {
        Use::Bind => Errno::EADDRNOTAVAIL,
        Use::Reach => Errno::ENETUNREACH,
    };
    // 127.0.0.x 换成 127.a.b.x，通配地址只在 bind 时可以
    let v4 = |ip: [u8; 4]| -> Result<[u8; 4], Errno> {
        if ip == [0; 4] && usage == Use::Bind {
            Ok([127, a, b, 1])
        } else if ip[..3] == [127, 0, 0] && lo.has_v4(ip, usage) {
            Ok([127, a, b, ip[3]])
        } else {
            Err(fail())
        }
    };
    match family(addr) {
        Some(AF_INET) if addr.len() >= 8 => {
            let mut out = addr.to_vec();
            out[4..8].copy_from_slice(&v4(addr[4..8].try_into().unwrap())?);
            Ok(out)
        }
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
            Ok(out)
        }
        Some(AF_INET6) if addr.len() >= 24 => {
            let ip = &addr[8..24];
            let host = if ip == [0; 16] && usage == Use::Bind {
                [127, a, b, 1]
            } else if ip == LOOPBACK6 {
                // 没有 ::1 时连接选不出源地址
                if !(lo.up && lo.v6) {
                    return Err(Errno::EADDRNOTAVAIL);
                }
                [127, a, b, 1]
            } else if let Some(m) = mapped(ip) {
                v4(m)?
            } else {
                return Err(fail());
            };
            let mut out = addr.to_vec();
            out[8..18].fill(0);
            out[18..20].copy_from_slice(&[0xff, 0xff]);
            out[20..24].copy_from_slice(&host);
            // scope id
            if let Some(s) = out.get_mut(24..28) {
                s.fill(0);
            }
            Ok(out)
        }
        Some(AF_INET | AF_INET6) => Err(Errno::EINVAL),
        _ => Ok(addr.to_vec()),
    }
}

/// 宿主给的地址（本端、对端）换成 guest 看到的。`any` 是 guest 绑的是通配地址、还没有
/// 连接：本端地址报告成通配的。`icmp6` 见 [`to_host`]：宿主的 ::1 就是 guest 的 ::1。
pub(crate) fn from_host(ns: &NetNs, addr: &[u8], any: bool, icmp6: bool) -> Vec<u8> {
    let Some([a, b]) = ns.host_net else {
        return addr.to_vec();
    };
    let mut out = addr.to_vec();
    match family(addr) {
        Some(AF_INET) if addr.len() >= 8 => {
            if any {
                out[4..8].fill(0);
            } else if addr[4..7] == [127, a, b] {
                out[4..8].copy_from_slice(&[127, 0, 0, addr[7]]);
            }
        }
        Some(AF_INET6) if addr.len() >= 24 && icmp6 => {
            if any {
                out[8..24].fill(0);
            }
        }
        Some(AF_INET6) if addr.len() >= 24 => {
            if any {
                out[8..24].fill(0);
            } else if let Some(m) = mapped(&addr[8..24])
                && m[..3] == [127, a, b]
            {
                // 127.a.b.1 是 guest 的 ::1（也可能是连到它的 IPv4 的 127.0.0.1，分不出来）
                if m[3] == 1 {
                    out[8..24].copy_from_slice(&LOOPBACK6);
                } else {
                    out[20..24].copy_from_slice(&[127, 0, 0, m[3]]);
                }
            }
        }
        _ => {}
    }
    out
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
fn set_up(lo: &mut Loopback, up: bool) {
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
    use std::rc::Rc;

    fn ns() -> NetNs {
        NetNs {
            id: 1,
            owner: crate::cred::UserNs::init(),
            host_net: Some([1, 2]),
            lo: Default::default(),
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

    #[test]
    fn loopback_addresses_map_to_the_namespace_range() {
        let ns = ns();
        // lo 还关着：通配地址能绑，127.0.0.1 绑不上、连不到，外面的地址总是连不到
        assert_eq!(
            to_host(&ns, &v4([0; 4], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            to_host(&ns, &v4([127, 0, 0, 1], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            to_host(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            to_host(&ns, &v6(LOOPBACK6, 80), Use::Reach, false),
            Err(Errno::EADDRNOTAVAIL)
        );
        up(&ns);
        assert_eq!(ns.lo.borrow().v4, [([127, 0, 0, 1], 8)]);
        assert_eq!(
            to_host(&ns, &v4([127, 0, 0, 1], 80), Use::Reach, false).unwrap(),
            v4([127, 1, 2, 1], 80)
        );
        assert_eq!(
            to_host(&ns, &v4([127, 0, 0, 9], 80), Use::Bind, false).unwrap(),
            v4([127, 1, 2, 9], 80)
        );
        assert_eq!(
            to_host(&ns, &v4([8, 8, 8, 8], 53), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            to_host(&ns, &v4([127, 1, 0, 1], 80), Use::Bind, false),
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
            to_host(&ns, &v6(LOOPBACK6, 80), Use::Reach, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(
            to_host(&ns, &v6([0; 16], 80), Use::Bind, false).unwrap(),
            v6(mapped, 80)
        );
        assert_eq!(
            from_host(&ns, &v6(mapped, 80), false, false),
            v6(LOOPBACK6, 80)
        );
        assert_eq!(
            from_host(&ns, &v6(mapped, 80), true, false),
            v6([0; 16], 80)
        );
        assert_eq!(
            from_host(&ns, &v4([127, 1, 2, 7], 9), false, false),
            v4([127, 0, 0, 7], 9)
        );
        assert_eq!(
            from_host(&ns, &v4([127, 1, 2, 1], 9), true, false),
            v4([0; 4], 9)
        );
        let mut doc = [0u8; 16];
        doc[0] = 0x20;
        assert_eq!(
            to_host(&ns, &v6(doc, 80), Use::Reach, false),
            Err(Errno::ENETUNREACH)
        );
        // ICMPv6 的 socket：::1 是宿主的 ::1，通配地址也绑到它，别的照样到不了
        assert_eq!(
            to_host(&ns, &v6(LOOPBACK6, 0), Use::Reach, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            to_host(&ns, &v6([0; 16], 0), Use::Bind, true).unwrap(),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            to_host(&ns, &v6(doc, 0), Use::Reach, true),
            Err(Errno::ENETUNREACH)
        );
        assert_eq!(
            from_host(&ns, &v6(LOOPBACK6, 0), false, true),
            v6(LOOPBACK6, 0)
        );
        assert_eq!(
            from_host(&ns, &v6(LOOPBACK6, 0), true, true),
            v6([0; 16], 0)
        );
        // 开机时的命名空间不换
        let init = crate::nsproxy::NsProxy::init().net;
        assert_eq!(
            to_host(&init, &v4([8, 8, 8, 8], 53), Use::Reach, false).unwrap(),
            v4([8, 8, 8, 8], 53)
        );
        let _ = Rc::clone(&init);
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
