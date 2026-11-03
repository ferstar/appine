//! socket 类 syscall（kernel/net 的 map）。
//!
//! 宿主 socket 是非阻塞的；guest 的阻塞调用在 EAGAIN 时经由 [`Cx::wait`] 等宿主 fd
//! 就绪，期间释放 BKL，可以被信号打断，并按 `SO_RCVTIMEO`/`SO_SNDTIMEO` 超时。

use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{SysResult, get, put, unix};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, request_wake_object, wake};
use crate::net::{
    AF_INET, AF_INET6, AF_NETLINK, AF_UNIX, InetSocket, NETLINK_GENERIC, NETLINK_KOBJECT_UEVENT,
    NETLINK_NETFILTER, NETLINK_ROUTE, NetlinkSocket, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM, Socket,
    is_stub, servfail, stub_addr,
};
use crate::process::Signal;
use crate::signal::{self, SigInfo};
use crate::uaccess::UserSlice;

/// socket 和它所在的打开文件。
pub(crate) type SocketRef = Rc<RefCell<Socket>>;

const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_NONBLOCK: u32 = oflags::O_NONBLOCK;
const SOCK_CLOEXEC: u32 = oflags::O_CLOEXEC;
const IPPROTO_ICMP: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IPV6: i32 = 41;
const IPV6_V6ONLY: i32 = 26;
const IPPROTO_UDP: i32 = 17;
const IPPROTO_ICMPV6: i32 = 58;
const IPPROTO_UDPLITE: i32 = 136;
const IPPROTO_MPTCP: i32 = 262;

const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_RCVTIMEO: i32 = 20;
const SO_SNDTIMEO: i32 = 21;
const SO_ERROR: i32 = 4;
const SO_MARK: i32 = 36;

const MSG_PEEK: u32 = 0x2;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_ERRQUEUE: u32 = 0x2000;
const MSG_WAITALL: u32 = 0x100;
const MSG_NOSIGNAL: u32 = 0x4000;
const MSG_FASTOPEN: u32 = 0x2000_0000;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;

/// sockaddr 的最大长度（`sockaddr_storage`）。
const SOCKADDR_MAX: usize = 128;
/// 一次收发的最大字节数。
const MSG_MAX: usize = 1 << 20;

/// fd 对应的 socket；不是 socket 时返回 ENOTSOCK。
fn socket_of(cx: &Cx<'_>, fd: i32) -> Result<(SocketRef, bool), Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        _ if f.flags & oflags::O_PATH != 0 => Err(Errno::EBADF),
        FileKind::Socket(s) => Ok((s.clone(), f.nonblocking())),
        _ => Err(Errno::ENOTSOCK),
    }
}

/// AF_UNIX socket 要等待或者用到 socket 本身的操作交给 [`unix`]。
fn is_unix(sock: &SocketRef) -> bool {
    matches!(&*sock.borrow(), Socket::Unix(_))
}

/// 读取 guest 给出的 sockaddr。
fn read_addr(cx: &Cx<'_>, addr: u64, len: u32) -> Result<Vec<u8>, Errno> {
    let len = len as usize;
    if len > SOCKADDR_MAX {
        return Err(Errno::EINVAL);
    }
    let mut b = vec![0u8; len];
    cx.mm().read(UserSlice::new(addr, len), &mut b)?;
    Ok(b)
}

/// 把地址写回 guest：`addrlen` 指向缓冲区长度，写回实际长度（缓冲区不够时截断）。
/// accept、recvfrom 的 `addr` 可以为 NULL，表示不要地址。
fn write_addr(cx: &Cx<'_>, addr: u64, addrlen: u64, value: &[u8]) -> Result<(), Errno> {
    if addr == 0 || addrlen == 0 {
        return Ok(());
    }
    write_addr_to(cx, addr, addrlen, value)
}

/// getsockname、getpeername：`addrlen` 必须有效，长度为负时 EINVAL。
fn write_addr_to(cx: &Cx<'_>, addr: u64, addrlen: u64, value: &[u8]) -> Result<(), Errno> {
    let room = usize::try_from(i32::from_le_bytes(get(cx, addrlen)?)).map_err(|_| Errno::EINVAL)?;
    let n = room.min(value.len());
    cx.mm().write(UserSlice::new(addr, n), &value[..n])?;
    put(cx, addrlen, (value.len() as u32).to_le_bytes())
}

/// 等宿主 socket `sock`（宿主 fd 是 `fd`）就绪：非阻塞时直接 EAGAIN，超时时返回
/// `timeout_err`。只登记这个 socket（[`wake_objects`]）：别的文件上的修改不叫醒它，
/// 原来系统里每一次管道、PTY 的读写都把阻塞在 recv 里的线程叫醒一遍。
fn wait_host(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    fd: i32,
    events: i16,
    nonblock: bool,
    deadline: Option<Instant>,
    timeout_err: Errno,
) -> Result<(), Errno> {
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(timeout_err);
    }
    if cx.interrupted() {
        return Err(Errno::ERESTARTSYS);
    }
    let mut pfd = [(fd, events, 0)];
    cx.wait_objects(deadline, &mut pfd, wake::FILE, &[wake::of(sock)])
}

pub(super) fn socket(cx: &mut Cx<'_>, domain: i32, ty: u32, protocol: i32) -> SysResult {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let base = (ty & SOCK_TYPE_MASK) as i32;
    // 建在新的网络命名空间里的 socket 记下它（crate::netns）
    let net = cx.task.ns.borrow().net.clone();
    let owner = net.owner.clone();
    let net = net.host_net.is_some().then_some(net);
    let sock = match domain {
        AF_INET | AF_INET6 => {
            // raw socket 要在网络命名空间的属主里有 CAP_NET_RAW（Linux 的 inet_create：协议号
            // 不对的先报告，交给宿主）。宿主上用 ping socket 模拟的 raw ICMP 不需要特权，不
            // 查的话普通用户也建得出来，busybox 的 ping 就不会退回 ping socket
            // guest 的 raw ICMP socket 换成不需要特权的 ping socket
            let icmp = matches!(
                (domain, protocol),
                (AF_INET, IPPROTO_ICMP) | (AF_INET6, IPPROTO_ICMPV6)
            );
            // 别的 raw socket 与 Android 上的 App 一样建不了：宿主有 CAP_NET_RAW 时（开发用的
            // Linux 宿主）也不交给它，不然发出去的报文引起的 ICMP 到不了模拟的 raw ICMP
            // socket，mtr 这类用 raw socket 发、raw ICMP socket 收的程序就两头对不上（它们
            // 遇到 EPERM 会退回 ping socket）
            if base == SOCK_RAW
                && (1..256).contains(&protocol)
                && (!icmp || !cx.cred().ns_capable(&owner, crate::cred::cap::NET_RAW))
            {
                return Err(Errno::EPERM);
            }
            let host_ty = if base == SOCK_RAW && icmp {
                SOCK_DGRAM
            } else {
                base
            };
            let fd = appine_hal::net::socket(domain, host_ty, protocol).map_err(Errno)?;
            let mut s = InetSocket::new(fd, domain, host_ty);
            s.raw = base == SOCK_RAW;
            if s.raw {
                // 宿主上替它打开 IP_RECVERR 等（crate::rawicmp），收到的都经由汇集点
                for &(level, name) in crate::rawicmp::RawIcmp::host_options(domain) {
                    let _ = appine_hal::net::setsockopt(fd, level, name, &1i32.to_le_bytes());
                }
                let tap = crate::rawicmp::tap_for(cx.k(), &net)?;
                s.icmp = Some(crate::rawicmp::RawIcmp::new(domain, tap));
                // 定时器线程替 poll、epoll 的等待者看着汇集点：可能是新建的，叫醒它
                crate::kernel::signal_later(&cx.shared.timers);
            }
            if protocol != 0 {
                s.proto = protocol;
            }
            s.net = net;
            Socket::Inet(s)
        }
        AF_NETLINK if base == SOCK_RAW || base == SOCK_DGRAM => {
            // NETLINK_ROUTE 回答接口和地址的查询，NETLINK_GENERIC 只有 nlctrl 一个族
            // （Linux 上总是有它），NETLINK_NETFILTER 是 nftables 的子集（crate::nft）；NETLINK_KOBJECT_UEVENT 只用来听热插拔事件（mdev、udev），
            // 这里不会有。别的协议与 Linux 上没有编进内核的子系统一样是 EPROTONOSUPPORT：
            // 原来照样建出来、收到请求也不回答，libaudit 等 ACK 等 40 秒之后失败，PAM 因此
            // 拒绝登录（Debian 的 sshd、login）
            if !matches!(
                protocol,
                NETLINK_ROUTE | NETLINK_GENERIC | NETLINK_KOBJECT_UEVENT | NETLINK_NETFILTER
            ) {
                return Err(Errno::EPROTONOSUPPORT);
            }
            let mut s = NetlinkSocket::new(protocol);
            s.net = net;
            Socket::Netlink(s)
        }
        AF_NETLINK => return Err(Errno::ESOCKTNOSUPPORT),
        AF_UNIX => {
            let mut s = unix::socket(base, protocol)?;
            if let Socket::Unix(u) = &mut s {
                u.net = net;
            }
            s
        }
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    let sock = Rc::new(RefCell::new(sock));
    if let Socket::Inet(InetSocket {
        icmp: Some(r),
        fd,
        family,
        ..
    }) = &*sock.borrow()
    {
        crate::rawicmp::register(&r.tap, &sock, *fd, *family);
    }
    install(cx, sock, ty).map(|fd| fd as u64)
}

/// 还没有绑定的宿主 socket 在 connect、sendto、listen 之前先绑到 `source`
/// （[`crate::netns::Mapped::source`]）：新的网络命名空间里是它的回环地址（Linux 自动绑定时
/// 绑的是命名空间里的通配地址，宿主上绑通配地址就到了所有接口上），交给宿主网络的
/// （`source` 是 None）由宿主替它绑；开机时的命名空间里连别的命名空间时是面向对方的虚拟
/// 地址在宿主上的那个，对方看到的对端才对。
fn ensure_bound(s: &mut InetSocket, source: Option<&[u8]>) -> Result<(), Errno> {
    if let Some(net) = s.net.clone() {
        if !s.bound {
            match source {
                Some(addr) => {
                    appine_hal::net::bind(s.fd, addr).map_err(Errno)?;
                    s.any = true;
                    register_udp(s);
                    // 发给自己的虚拟地址的 UDP socket：登记端口，回答才送得到（crate::netns）
                    if s.ty == SOCK_DGRAM
                        && crate::netns::is_self_source(&net, addr)
                        && let Some(p) = appine_hal::net::sockname(s.fd, false)
                            .ok()
                            .as_deref()
                            .and_then(port_of)
                    {
                        net.lo.borrow_mut().self_udp.push(p);
                        s.self_port = Some(p);
                    }
                }
                // 交给宿主网络的：宿主替它绑通配地址
                None => s.wide = true,
            }
            s.bound = true;
        }
    } else if let Some(addr) = source
        && appine_hal::net::sockname(s.fd, false)
            .ok()
            .is_some_and(|a| port_of(&a) == Some(0) && crate::netns::is_any(&a))
        && appine_hal::net::bind(s.fd, addr).is_ok()
    {
        // guest 看来是自动绑定的通配地址（连接之前 getsockname 报告它）
        s.any = true;
    }
    Ok(())
}

/// 新的网络命名空间里绑了的 UDP socket 登记在命名空间里（[`crate::netns::listening`]）：
/// 发往那里的数据报按它判断有没有人收。显式绑的、发送之前替它绑的都算。
fn register_udp(s: &mut InetSocket) {
    let Some(net) = s.net.clone() else {
        return;
    };
    if s.ty != SOCK_DGRAM || s.icmp.is_some() || s.listener.is_some() || map_proto(s) != IPPROTO_UDP
    {
        return;
    }
    let Ok(local) = appine_hal::net::sockname(s.fd, false) else {
        return;
    };
    let Some(host) = port_of(&local).filter(|&p| p != 0) else {
        return;
    };
    let port = match s.local_map {
        Some((g, h)) if h == host => g,
        _ => host,
    };
    let any = s.any || s.wide || crate::netns::is_any(&local);
    let at = crate::netns::ListenAt {
        port,
        any,
        vaddr: s.vaddr,
        host: &local,
        v6: s.family == AF_INET6,
        v6only: s.v6only,
        udp: true,
    };
    s.listener = Some(crate::netns::add_listener(&net, &at));
}

/// 这个 socket 所在的网络命名空间（开机时的记作 None）。
fn net_of(cx: &Cx<'_>, s: &InetSocket) -> Rc<crate::nsproxy::NetNs> {
    s.net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone())
}

/// 低端口映射的登记表里网络命名空间的键（[`ns_key`]）。
fn key_of(n: &Rc<crate::nsproxy::NetNs>) -> usize {
    if n.host_net.is_none() {
        0
    } else {
        Rc::as_ptr(n) as usize
    }
}

/// 把 socket 放进 fd 表；`flags` 里可以有 `SOCK_NONBLOCK`、`SOCK_CLOEXEC`。
fn install(cx: &mut Cx<'_>, sock: SocketRef, flags: u32) -> Result<i32, Errno> {
    let file = OpenFile::new(
        FileKind::Socket(sock),
        oflags::O_RDWR | (flags & SOCK_NONBLOCK),
    );
    cx.fds_mut().install(file, flags & SOCK_CLOEXEC != 0, 0)
}

pub(super) fn socketpair(
    cx: &mut Cx<'_>,
    domain: i32,
    ty: u32,
    protocol: i32,
    sv: u64,
) -> SysResult {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    match domain {
        AF_UNIX => {}
        // 和 Linux 一样先按普通的 socket 检查类型和协议，再报告不支持 socketpair
        AF_INET | AF_INET6 => {
            let ok = match (ty & SOCK_TYPE_MASK) as i32 {
                SOCK_STREAM => matches!(protocol, 0 | IPPROTO_TCP | IPPROTO_MPTCP),
                SOCK_DGRAM => matches!(protocol, 0 | IPPROTO_UDP | IPPROTO_ICMP | IPPROTO_UDPLITE),
                // raw socket 不能用协议 0
                SOCK_RAW => protocol != 0,
                _ => false,
            };
            return Err(if ok {
                Errno::EOPNOTSUPP
            } else {
                Errno::EPROTONOSUPPORT
            });
        }
        AF_NETLINK => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::EAFNOSUPPORT),
    }
    let (a, b) = unix::pair(cx, (ty & SOCK_TYPE_MASK) as i32, protocol)?;
    let fa = install(cx, Rc::new(RefCell::new(a)), ty)?;
    let fds = install(cx, Rc::new(RefCell::new(b)), ty).and_then(|fb| {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&fa.to_le_bytes());
        out[4..].copy_from_slice(&fb.to_le_bytes());
        put(cx, sv, out).inspect_err(|_| {
            let _ = cx.fds_mut().close(fb);
        })
    });
    if let Err(e) = fds {
        let _ = cx.fds_mut().close(fa);
        return Err(e);
    }
    Ok(0)
}

pub(super) fn bind(cx: &mut Cx<'_>, fd: i32, addr: u64, len: u32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let addr = read_addr(cx, addr, len)?;
    if is_unix(&sock) {
        return unix::bind(cx, &sock, &addr).map(|()| 0);
    }
    // 新的网络命名空间里先换成宿主的地址（不是本地地址时 EADDRNOTAVAIL）
    let (net, icmp6, raw) = match &*sock.borrow() {
        Socket::Inet(s) => (s.net.clone(), s.icmp6(), s.icmp.is_some()),
        _ => (None, false, false),
    };
    // raw socket 没有端口（Linux 的 raw_bind 不看它）。宿主的 ping socket 却把它当作 echo 的
    // 编号：busybox 的 traceroute 把收的 socket 绑到发的 socket 的地址上（getsockname 拿到
    // 的，带着端口），两个就共用一个编号，回答到了另一个上
    let mut addr = addr;
    if raw && addr.len() >= 4 {
        addr[2..4].fill(0);
    }
    let w = cx.kr().world();
    let me = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
    let mapped = crate::netns::to_host(&w, &me, &addr, crate::netns::Use::Bind, icmp6)?;
    // 新的网络命名空间里数据报 socket 绑通配地址、端口 0（busybox 的 ping、Java 的
    // DatagramSocket 这样做）：宿主上也绑通配地址，不然发不到外面（Socket::wide）
    let wide = net.is_some()
        && !icmp6
        && !raw
        && matches!(&*sock.borrow(), Socket::Inet(s) if s.ty == SOCK_DGRAM)
        && crate::netns::is_any(&addr)
        && port_of(&addr) == Some(0);
    let host_addr = if wide {
        crate::netns::any_like(&addr)
    } else {
        mapped.addr
    };
    // ip_unprivileged_port_start（默认 1024）以下的端口要求网络命名空间的属主里的
    // CAP_NET_BIND_SERVICE（Linux 的 inet_port_requires_bind_service）
    let family = addr
        .get(..2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as i32);
    if matches!(family, Some(AF_INET | AF_INET6)) && addr.len() >= 4 {
        let port = u16::from_be_bytes([addr[2], addr[3]]);
        let ns = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
        let owner = ns.owner.clone();
        let prot_sock = ns.devs.borrow().conf.prot_sock;
        if port != 0
            && i64::from(port) < prot_sock
            && !cx
                .cred()
                .ns_capable(&owner, crate::cred::cap::NET_BIND_SERVICE)
        {
            return Err(Errno::EACCES);
        }
    }
    let map = cx.k().portmap.clone();
    // 新的命名空间里新映射了低端口：转发到它的目的地址要重新算（crate::forward）
    let mut remap = false;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 流量算在哪个接口上重新认（connect 也一样）
            s.route = None;
            // 1024 以下的端口宿主不让绑原来的就换一个（crate::portmap）；新的网络命名空间里
            // 显式绑的端口都换（宿主上不占这个端口，外层和别的命名空间照样绑得了）
            let own = s.net.is_some() && !wide;
            match port_of(&host_addr)
                .filter(|&p| mappable(s) && p != 0 && (own || map.borrow().maps(p)))
            {
                Some(port) if own => {
                    let fd = s.fd;
                    let try_bind = |h: u16| {
                        let mut a = host_addr.clone();
                        set_port(&mut a, h);
                        appine_hal::net::bind(fd, &a)
                    };
                    let bound = || {
                        appine_hal::net::sockname(fd, false)
                            .ok()
                            .and_then(|a| port_of(&a))
                    };
                    let (host, lease) = crate::portmap::bind_any(
                        &map,
                        ns_key(s),
                        map_proto(s),
                        port,
                        try_bind,
                        bound,
                    )?;
                    s.lease = Some(lease);
                    s.local_map = Some((port, host));
                    remap = true;
                }
                Some(port) => {
                    let fd = s.fd;
                    let try_bind = |h: u16| {
                        let mut a = host_addr.clone();
                        set_port(&mut a, h);
                        appine_hal::net::bind(fd, &a)
                    };
                    let bound = || {
                        appine_hal::net::sockname(fd, false)
                            .ok()
                            .and_then(|a| port_of(&a))
                    };
                    let (host, lease, new) =
                        crate::portmap::bind(&map, ns_key(s), map_proto(s), port, try_bind, bound)?;
                    if new && host != port {
                        let proto = if map_proto(s) == IPPROTO_TCP {
                            "tcp"
                        } else {
                            "udp"
                        };
                        crate::klog::printk(
                            &cx.kr().klog,
                            crate::klog::level::INFO,
                            format_args!("appine: port {port}/{proto} is {host} on the host"),
                        );
                    }
                    s.lease = Some(lease);
                    s.local_map = Some((port, host));
                }
                None => appine_hal::net::bind(s.fd, &host_addr).map_err(Errno)?,
            }
            if s.net.is_some() {
                s.bound = true;
                s.any = crate::netns::is_any(&addr);
                s.vaddr = mapped.vaddr;
                s.wide = wide;
                register_udp(s);
            }
            // 开机时的命名空间里绑了端口的 UDP socket：PREROUTING 有 DNAT 时转发外面来的
            if s.ty == SOCK_DGRAM {
                forward_socket(cx.kr(), s);
            }
        }
        Socket::Netlink(s) => {
            let pid = addr
                .get(4..8)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            s.pid = if pid == 0 {
                cx.pid_out(cx.tgid()) as u32
            } else {
                pid
            };
        }
        Socket::Unix(_) => unreachable!("handled above"),
    }
    if remap
        && cx
            .kr()
            .init_ns
            .net
            .nft
            .borrow()
            .has_nat(crate::nft::HOOK_PRE_ROUTING)
    {
        forward_all(cx.kr());
    }
    Ok(0)
}

pub(super) fn listen(cx: &mut Cx<'_>, fd: i32, backlog: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            let src = s
                .net
                .as_ref()
                .and_then(|ns| crate::netns::implicit_bind(ns, s.family, s.icmp6()));
            ensure_bound(s, src.as_deref())?;
            appine_hal::net::listen(s.fd, backlog).map_err(Errno)?;
            s.listening = true;
            // 新的网络命名空间里的 TCP 监听登记在命名空间里：连过来时按它判断（crate::netns）
            if let Some(net) = &s.net
                && s.listener.is_none()
                && map_proto(s) == IPPROTO_TCP
                && let Ok(local) = appine_hal::net::sockname(s.fd, false)
                && let Some(host) = port_of(&local)
            {
                let port = match s.local_map {
                    Some((g, h)) if h == host => g,
                    _ => host,
                };
                let v6 = s.family == AF_INET6;
                let at = crate::netns::ListenAt {
                    port,
                    any: s.any,
                    vaddr: s.vaddr,
                    host: &local,
                    v6,
                    v6only: s.v6only,
                    udp: false,
                };
                s.listener = Some(crate::netns::add_listener(net, &at));
            }
            // 开机时的命名空间里的监听：PREROUTING 有 DNAT 时转发外面来的连接（crate::forward）
            forward_socket(cx.kr(), s);
            // 别的设备连得到的监听（设计文档 §12 #7）：App 发通知，告诉用户连宿主上的哪个端口
            if let Ok(local) = appine_hal::net::sockname(s.fd, false)
                && let (Some(ip), Some(host)) = (ip_of(&local), port_of(&local))
                && !ip.is_loopback()
            {
                let port = match s.local_map {
                    Some((g, h)) if h == host => g,
                    _ => host,
                };
                let comm = cx.task.comm.get();
                let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
                let comm = String::from_utf8_lossy(&comm[..len]).into_owned();
                cx.kr().portmap.borrow_mut().listened(comm, port, host);
            }
        }
        Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        Socket::Unix(u) => unix::listen(u, backlog, cx.ucred())?,
    }
    Ok(0)
}

pub(super) fn accept4(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64, flags: u32) -> SysResult {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let (sock, nonblock) = socket_of(cx, fd)?;
    let (new, peer) = if is_unix(&sock) {
        unix::accept(cx, &sock, nonblock)?
    } else {
        accept_host(cx, &sock, nonblock)?
    };
    let fd = install(cx, new, flags)?;
    write_addr(cx, addr, addrlen, &peer)?;
    Ok(fd as u64)
}

/// 从宿主 socket 接受一个连接：返回新的 socket 和对方的地址。
fn accept_host(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
) -> Result<(SocketRef, Vec<u8>), Errno> {
    let (host_fd, family, ty, proto, timeout, net, local_map, vaddr, mark) = match &*sock.borrow() {
        Socket::Inet(s) => (
            s.fd,
            s.family,
            s.ty,
            s.proto,
            s.rcvtimeo,
            s.net.clone(),
            s.local_map,
            s.vaddr,
            s.mark(),
        ),
        Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        Socket::Unix(_) => unreachable!("handled by accept4"),
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match appine_hal::net::accept(host_fd) {
            Ok((new, peer)) => {
                let mut new = InetSocket::new(new, family, ty);
                new.proto = proto;
                new.local_map = local_map;
                new.net = net.clone();
                // 接受的连接沿用监听的 SO_MARK（inet_csk_clone_lock 的 ir_mark）
                if mark != 0 {
                    new.track_mut().mark = mark;
                }
                // 发起的一端被 nat 换过目的地址的：领走这个命名空间里看到的原目的地址
                // （SO_ORIGINAL_DST，crate::origdst）
                if ty == SOCK_STREAM
                    && !cx.kr().orig_dst.is_empty()
                    && let Ok(local) = appine_hal::net::sockname(new.fd, false)
                    && let (Some(p), Some(l)) = (
                        crate::origdst::end_of(&peer),
                        crate::origdst::end_of(&local),
                    )
                {
                    let ns = net.as_ref().map_or(cx.kr().init_ns.net.id, |n| n.id);
                    if let Some(o) = cx.k().orig_dst.claim((p, l), ns) {
                        new.track_mut().orig_dst = Some(o);
                    }
                }
                if proto == IPPROTO_TCP && net.is_none() {
                    cx.k().traffic.protocols.tcp_passive_opens += 1;
                }
                let w = cx.kr().world();
                let peer = unmap_addr(&cx.kr().portmap, &w, &new, peer, false);
                let me = net.clone().unwrap_or_else(|| cx.kr().init_ns.net.clone());
                let peer = crate::netns::from_host(&w, &me, &peer, crate::netns::Role::Peer, false);
                new.bound = net.is_some();
                new.vaddr = vaddr;
                new.net = net;
                return Ok((Rc::new(RefCell::new(Socket::Inet(new))), peer));
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                wait_host(cx, sock, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)?;
            }
            Err(e) => return Err(Errno(e)),
        }
    }
}

pub(super) fn connect(cx: &mut Cx<'_>, fd: i32, addr: u64, len: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let addr = read_addr(cx, addr, len)?;
    if is_unix(&sock) {
        return unix::connect(cx, &sock, nonblock, &addr).map(|()| 0);
    }
    let map = cx.k().portmap.clone();
    let w = cx.kr().world();
    let init = cx.kr().init_ns.net.clone();
    let cred = cx.cred();
    let (host_addr, tcp, host_fd, origs) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 本端地址可能变了：流量算在哪个接口上重新认
            s.route = None;
            // UDP 连到 DNS 存根：只记下来，之后的 send 由存根处理（新的网络命名空间里没有它）
            if s.net.is_none() && s.ty == SOCK_DGRAM {
                s.stub_peer = is_stub(&addr);
                if s.stub_peer {
                    return Ok(0);
                }
            }
            // 地址换成宿主上的（crate::netns），没绑定的先绑上。nftables 的 OUTPUT nat
            // （crate::nft）先换目的地址：guest 看到的对端仍是它连的，收到的数据报的来源
            // 换回来
            let me = s.net.clone().unwrap_or_else(|| init.clone());
            let Routed {
                m,
                dst,
                there,
                actual,
                verdict,
                origs,
            } = route(&w, s, &me, &addr, &cred)?;
            // 过滤链：TCP 被拒绝的马上失败，被丢弃的连到黑洞上等超时（OUTPUT 上的丢弃
            // Linux 也是重传到超时）；UDP 照常连接，之后不带地址的发送不发出去
            let blocked = !matches!(
                verdict.0,
                crate::nft::Outcome::Accept | crate::nft::Outcome::Dnat(..)
            );
            if blocked && s.ty == SOCK_STREAM {
                if let crate::nft::Outcome::Reject(e) = verdict.0 {
                    return Err(Errno(e));
                }
                s.guest_peer = Some(addr.clone());
                let hole = blackhole(cx.kr()).ok_or(Errno::ETIMEDOUT)?;
                let hole = if s.family == AF_INET6 {
                    let mut a = vec![0u8; 28];
                    a[..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
                    a[2..4].copy_from_slice(&hole[2..4]);
                    a[18..20].copy_from_slice(&[0xff, 0xff]);
                    a[20..24].copy_from_slice(&hole[4..8]);
                    a
                } else {
                    hole
                };
                (hole, false, s.fd, None)
            } else {
                // 连到新的网络命名空间里：那里没有对得上的监听就是 ECONNREFUSED（宿主上开机时的
                // 命名空间里监听通配地址的 socket 也盖住这个地址，不能交给宿主去连）
                if s.ty == SOCK_STREAM
                    && let Some(t) = &m.target
                    && t.host_net.is_some()
                    && !crate::netns::listening(t, &there, false)
                {
                    return Err(Errno::ECONNREFUSED);
                }
                let back: Vec<_> = actual.iter().map(|t| (t.clone(), addr.clone())).collect();
                if !back.is_empty() || s.track.is_some() {
                    let t = s.track_mut();
                    t.nat_back = back;
                    t.blocked = None;
                }
                let dst = &dst;
                if crate::netns::is_inet(&addr) {
                    ensure_bound(s, m.source.as_deref())?;
                }
                // getpeername 报告连的那个地址（宿主上的对端换不回来时也对，比如连自己的地址）
                s.guest_peer = crate::netns::is_inet(&addr).then(|| addr.clone());
                // 本机上映射了的低端口换成宿主上的（crate::portmap），按目的地所在的命名空间
                let key = m.target.as_ref().map_or(0, key_of);
                let to = if s.ty == SOCK_DGRAM {
                    crate::netns::toward_self(&me, dst, m.addr)
                } else {
                    m.addr
                };
                let (host_addr, pm) = map_dest(&map, s, to, key);
                s.peer_map = pm;
                // 宿主上绑在通配地址上的数据报 socket 连到映射的目的地：宿主 socket 不连接，
                // 这里记下对端（InetSocket::vconn）；原来宿主上连着的先断开
                let was = s.vconn.take();
                if s.wide && s.ty == SOCK_DGRAM && m.target.is_some() {
                    // 宿主上真连着才断开；断开会放掉内核分的端口（绑的是端口 0 时，
                    // udp_disconnect），绑回原来的，getsockname 和登记的端口才不变
                    if was.is_none() && appine_hal::net::sockname(s.fd, true).is_ok() {
                        let old = appine_hal::net::sockname(s.fd, false).ok();
                        let _ = appine_hal::net::connect(s.fd, &[0u8; 16]);
                        if let Some(o) = old
                            && appine_hal::net::sockname(s.fd, false)
                                .ok()
                                .as_deref()
                                .and_then(port_of)
                                != port_of(&o)
                        {
                            let _ = appine_hal::net::bind(s.fd, &o);
                        }
                    }
                    s.vconn = Some(host_addr);
                    s.peer = Some(addr.clone());
                    s.any = false;
                    return Ok(0);
                }
                if was.is_some() && !crate::netns::is_inet(&addr) {
                    s.peer = None;
                    return Ok(0);
                }
                // 连接了的 UDP 的目的地被过滤链丢弃：OUTPUT 上的发送时是 EPERM，别处的悄悄丢掉
                if blocked {
                    s.track_mut().blocked = Some(if verdict.1 { Errno::EPERM.0 } else { 0 });
                }
                // 新的网络命名空间里的流量不算进宿主的接口（/proc/net/snmp 也不数）
                let tcp = s.net.is_none() && s.ty == SOCK_STREAM && s.proto == IPPROTO_TCP;
                let stream = s.ty == SOCK_STREAM && nat_proto(s) == Some(IPPROTO_TCP as u8);
                (host_addr, tcp, s.fd, stream.then_some(origs))
            }
        }
        Socket::Netlink(_) => return Ok(0),
        Socket::Unix(_) => unreachable!("handled above"),
    };
    let r = appine_hal::net::connect(host_fd, &host_addr);
    if tcp {
        count_connect(cx, &sock, r.err().map(Errno));
    }
    // 被 nat 换过目的地址的 TCP 连接按宿主上的四元组记下原目的地址，接受的一端在 accept 时
    // 领走；没被 nat 的去掉同一个四元组以前留下的（crate::origdst）。阻塞的 connect 也在
    // 放开 BKL 等待之前记下，对方不会先 accept
    if let Some(origs) = origs
        && (r.is_ok() || r == Err(Errno::EINPROGRESS.0))
        && (!origs.is_empty() || !cx.kr().orig_dst.is_empty())
        && let Ok(local) = appine_hal::net::sockname(host_fd, false)
        && let (Some(a), Some(b)) = (
            crate::origdst::end_of(&local),
            crate::origdst::end_of(&host_addr),
        )
    {
        cx.k().orig_dst.register((a, b), origs);
    }
    let connected = addr.get(..2) != Some(&[0, 0]);
    if (r.is_ok() || r == Err(Errno::EINPROGRESS.0))
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
    {
        s.any &= !connected;
    }
    match r {
        Ok(()) => {
            // 数据报 socket 记下对端，AF_UNSPEC 是断开
            if let Socket::Inet(s) = &mut *sock.borrow_mut()
                && s.ty != SOCK_STREAM
            {
                s.peer = connected.then(|| addr.clone());
            }
            return Ok(0);
        }
        Err(e) if e == Errno::EINPROGRESS.0 && !nonblock => {}
        Err(e) => return Err(Errno(e)),
    }
    // 阻塞的 connect：等连接完成，再取连接的结果
    loop {
        let mut pfd = [(host_fd, POLLOUT, 0)];
        appine_hal::poll(&mut pfd, Some(Duration::ZERO)).map_err(Errno)?;
        if pfd[0].2 != 0 {
            break;
        }
        if cx.interrupted() {
            return Err(Errno::EINTR);
        }
        cx.wait_objects(None, &mut pfd, wake::FILE, &[wake::of(&sock)])?;
    }
    let err = appine_hal::net::getsockopt(host_fd, SOL_SOCKET, SO_ERROR, 4).map_err(Errno)?;
    match i32::from_le_bytes(err[..4].try_into().unwrap()) {
        // 宿主 socket 在套接字层还是"正在连接"，再 connect 一次才变成"已连接"：不然
        // guest 之后再 connect 会成功，而 Linux 上阻塞的 connect 返回之后再 connect 是
        // EISCONN
        0 => {
            let _ = appine_hal::net::connect(host_fd, &host_addr);
            if let Socket::Inet(s) = &mut *sock.borrow_mut() {
                s.opening = false;
            }
            Ok(0)
        }
        e => {
            if tcp {
                count_connect(cx, &sock, Some(Errno(e)));
            }
            Err(Errno(e))
        }
    }
}

/// TCP 主动连接的计数（`/proc/net/snmp` 的 ActiveOpens、AttemptFails，[`crate::traffic`]）：
/// 宿主发出了 SYN（connect 成功、正在连接、被拒绝或者超时）时数一次 ActiveOpens，之后
/// 看到这次连接失败时数一次 AttemptFails。`err` 是这一次 connect 或者阻塞等到的结果。
/// 非阻塞的连接失败了、guest 只从 poll 和 SO_ERROR 得知时，只数得到 SO_ERROR 是
/// ECONNREFUSED 的（[`getsockopt`]）。
fn count_connect(cx: &mut Cx<'_>, sock: &SocketRef, err: Option<Errno>) {
    let Socket::Inet(s) = &mut *sock.borrow_mut() else {
        return;
    };
    let p = &mut cx.k().traffic.protocols;
    match err {
        None | Some(Errno::EINPROGRESS) => {
            if !s.opening {
                p.tcp_active_opens += 1;
                s.opening = true;
            }
        }
        // 前一次 connect 发出的 SYN 失败了（非阻塞时再 connect 一次得到的）
        Some(Errno::ECONNREFUSED | Errno::ETIMEDOUT | Errno::EHOSTUNREACH | Errno::ENETUNREACH)
            if s.opening =>
        {
            p.tcp_attempt_fails += 1;
            s.opening = false;
        }
        // 这一次发出的 SYN 马上就失败了
        Some(Errno::ECONNREFUSED | Errno::ETIMEDOUT) => {
            p.tcp_active_opens += 1;
            p.tcp_attempt_fails += 1;
        }
        Some(_) => {}
    }
}

fn sockname(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64, peer: bool) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let value = match &*sock.borrow() {
        Socket::Inet(s) if peer && s.stub_peer => stub_addr(s.family),
        // 汇集点看着的 UDP socket 的宿主连接可能改过（[`send`]），对端报告 guest 连的
        Socket::Inet(InetSocket {
            udp_tap: Some(_),
            peer: Some(p),
            ..
        }) if peer => p.clone(),
        // 记下了对端的（InetSocket::vconn）：宿主 socket 没有连接
        Socket::Inet(InetSocket {
            vconn: Some(_),
            guest_peer: Some(p),
            ..
        }) if peer => p.clone(),
        Socket::Inet(s) => inet_name(cx, s, peer)?,
        Socket::Netlink(_) if peer => {
            let mut b = vec![0u8; 12];
            b[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
            b
        }
        Socket::Netlink(s) => s.sockaddr(),
        Socket::Unix(u) => unix::sockname(u, peer)?,
    };
    write_addr_to(cx, addr, addrlen, &value).map(|()| 0)
}

/// 宿主 socket 的本端（`peer` 时是对端）地址换成 guest 看到的（getsockname、getpeername）。
fn inet_name(cx: &Cx<'_>, s: &InetSocket, peer: bool) -> Result<Vec<u8>, Errno> {
    let mut a = appine_hal::net::sockname(s.fd, peer).map_err(Errno)?;
    // raw socket 的本端"端口"是协议号（Linux 的 inet_create 设的 inet_sport），宿主的
    // ping socket 报告的是它的编号
    if s.icmp.is_some() && !peer && a.len() >= 4 {
        a[2..4].copy_from_slice(&(s.proto as u16).to_be_bytes());
    }
    let w = cx.kr().world();
    let a = unmap_addr(&cx.kr().portmap, &w, s, a, !peer);
    let me = net_of(cx, s);
    let other = if peer {
        None
    } else {
        appine_hal::net::sockname(s.fd, true).ok()
    };
    if peer && let Some(g) = &s.guest_peer {
        return Ok(g.clone());
    }
    // 记下了对端的（InetSocket::vconn）：本端是面向对端的那个地址
    if !peer
        && s.vconn.is_some()
        && let Some(g) = s.guest_peer.as_deref()
        && let Some(local) = crate::netns::local_toward(&me, &a, g)
    {
        return Ok(local);
    }
    let role = if peer {
        crate::netns::Role::Peer
    } else {
        crate::netns::Role::Local {
            any: s.any,
            vaddr: s.vaddr,
            peer: other.as_deref(),
        }
    };
    Ok(crate::netns::from_host(&w, &me, &a, role, s.icmp6()))
}

pub(super) fn getsockname(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64) -> SysResult {
    sockname(cx, fd, addr, addrlen, false)
}

pub(super) fn getpeername(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64) -> SysResult {
    sockname(cx, fd, addr, addrlen, true)
}

fn timeval(b: &[u8]) -> Option<Duration> {
    let sec = i64::from_le_bytes(b.get(..8)?.try_into().ok()?);
    let usec = i64::from_le_bytes(b.get(8..16)?.try_into().ok()?);
    let d = Duration::from_secs(sec.max(0) as u64) + Duration::from_micros(usec.max(0) as u64);
    (!d.is_zero()).then_some(d)
}

pub(super) fn setsockopt(
    cx: &mut Cx<'_>,
    fd: i32,
    level: i32,
    name: i32,
    value: u64,
    len: u32,
) -> SysResult {
    const SO_SNDBUF: i32 = 7;
    const SO_RCVBUF: i32 = 8;
    const SO_SNDBUFFORCE: i32 = 32;
    const SO_RCVBUFFORCE: i32 = 33;
    let (sock, _) = socket_of(cx, fd)?;
    let len = (len as usize).min(256);
    let mut v = vec![0u8; len];
    cx.mm().read(UserSlice::new(value, len), &mut v)?;
    // 不受上限约束的 SO_*BUFFORCE 要求 CAP_NET_ADMIN，App 在宿主上没有：有这个能力的
    // guest 进程退回到普通的 SO_*BUF（受宿主的上限约束）
    let name = match (level, name) {
        (SOL_SOCKET, SO_SNDBUFFORCE | SO_RCVBUFFORCE)
            if !cx.cred().capable(crate::cred::cap::NET_ADMIN) =>
        {
            return Err(Errno::EPERM);
        }
        (SOL_SOCKET, SO_SNDBUFFORCE) => SO_SNDBUF,
        (SOL_SOCKET, SO_RCVBUFFORCE) => SO_RCVBUF,
        _ => name,
    };
    // raw ICMP 自己的选项（crate::rawicmp::RawIcmp）
    if let Socket::Inet(InetSocket { icmp: Some(r), .. }) = &mut *sock.borrow_mut()
        && let Some(res) = r.setsockopt(level, name, &v)
    {
        return res.map(|()| 0).map_err(Errno);
    }
    // UDP：记下设过 TTL、自己打开的 IP_RECVERR（crate::rawicmp）。汇集点看着它时宿主上的
    // IP_RECVERR 总是开着：guest 关掉它只记下来，打开它之后不再看着，宿主上的就是 guest 的
    let udp = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => crate::rawicmp::udp_setsockopt(s, level, name, &v),
        _ => None,
    };
    match udp {
        Some(true) => {
            crate::rawicmp::untap(&sock);
            return Ok(0);
        }
        Some(false) => return Ok(0),
        None => {}
    }
    // 经典 BPF 过滤器（SO_ATTACH_FILTER、SO_ATTACH_REUSEPORT_CBPF）：struct sock_fprog 里指向
    // 指令的指针换成宿主上的（dockerd 在 -p 的监听 socket 上装丢包的过滤器）。指令数是 0 或者
    // 超过 BPF_MAXINSNS 是 EINVAL，指令的检查交给宿主
    const SO_ATTACH_FILTER: i32 = 26;
    const SO_ATTACH_REUSEPORT_CBPF: i32 = 51;
    let inet_fd = match &*sock.borrow() {
        Socket::Inet(s) => Some(s.fd),
        _ => None,
    };
    if let Some(host_fd) = inet_fd
        && level == SOL_SOCKET
        && matches!(name, SO_ATTACH_FILTER | SO_ATTACH_REUSEPORT_CBPF)
    {
        let fprog = v.get(..16).ok_or(Errno::EINVAL)?;
        let n = usize::from(u16::from_le_bytes([fprog[0], fprog[1]]));
        if n == 0 || n > 4096 {
            return Err(Errno::EINVAL);
        }
        let ptr = u64::from_le_bytes(fprog[8..16].try_into().unwrap());
        let mut insns = vec![0u8; n * 8];
        cx.mm().read(UserSlice::new(ptr, n * 8), &mut insns)?;
        let mut host = [0u8; 16];
        host[..2].copy_from_slice(&(n as u16).to_le_bytes());
        host[8..16].copy_from_slice(&(insns.as_ptr() as u64).to_le_bytes());
        appine_hal::net::setsockopt(host_fd, level, name, &host).map_err(Errno)?;
        return Ok(0);
    }
    // SO_MARK：只记下（InetSocket::mark），本机发出的包在 nftables 里带着它。Linux 要在 socket
    // 的网络命名空间的属主里有 CAP_NET_RAW 或者 CAP_NET_ADMIN（sk_setsockopt）
    if (level, name) == (SOL_SOCKET, SO_MARK)
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
    {
        let mark = v.get(..4).ok_or(Errno::EINVAL)?;
        let owner = net_of(cx, s).owner.clone();
        let cred = cx.cred();
        if !cred.ns_capable(&owner, crate::cred::cap::NET_RAW)
            && !cred.ns_capable(&owner, crate::cred::cap::NET_ADMIN)
        {
            return Err(Errno::EPERM);
        }
        let mark = u32::from_le_bytes(mark.try_into().unwrap());
        if mark != 0 || s.track.is_some() {
            s.track_mut().mark = mark;
        }
        return Ok(0);
    }
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO) => s.rcvtimeo = timeval(&v),
            (SOL_SOCKET, SO_SNDTIMEO) => s.sndtimeo = timeval(&v),
            // 新的网络命名空间里宿主 socket 要用 IPv4 映射的地址：只记下来
            (IPPROTO_IPV6, IPV6_V6ONLY) if s.net.is_some() && s.family == AF_INET6 => {
                let on = v.get(..4).ok_or(Errno::EINVAL)?;
                s.v6only = i32::from_le_bytes(on.try_into().unwrap()) != 0;
            }
            _ => appine_hal::net::setsockopt(s.fd, level, name, &v).map_err(Errno)?,
        },
        // 别的选项（缓冲区大小、组播等）都接受，但不起作用
        Socket::Netlink(s) if (level, name) == (SOL_SOCKET, SO_RCVTIMEO) => {
            s.rcvtimeo = timeval(&v)
        }
        // SOL_NETLINK 的 NETLINK_CAP_ACK、NETLINK_EXT_ACK
        Socket::Netlink(s) if level == 270 && (name == 10 || name == 11) => {
            let on = v.get(..4).ok_or(Errno::EINVAL)?;
            let on = i32::from_le_bytes(on.try_into().unwrap()) != 0;
            if name == 10 {
                s.cap_ack = on;
            } else {
                s.ext_ack = on;
            }
        }
        Socket::Netlink(_) => {}
        Socket::Unix(u) => unix::setsockopt(u, level, name, &v)?,
    }
    Ok(0)
}

pub(super) fn getsockopt(
    cx: &mut Cx<'_>,
    fd: i32,
    level: i32,
    name: i32,
    value: u64,
    len: u64,
) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let room = usize::try_from(i32::from_le_bytes(get(cx, len)?)).map_err(|_| Errno::EINVAL)?;
    let encode_tv = |t: Option<Duration>| {
        let t = t.unwrap_or_default();
        let mut b = (t.as_secs() as i64).to_le_bytes().to_vec();
        b.extend_from_slice(&(t.subsec_micros() as i64).to_le_bytes());
        b
    };
    let pid_ns = cx.pid_ns();
    // SO_ORIGINAL_DST：Linux 只写 sockaddr，optlen 原样写回
    if let Some(r) = original_dst(cx, &sock, level, name, room) {
        let out = r?;
        cx.mm().write(UserSlice::new(value, out.len()), &out)?;
        return Ok(0);
    }
    // 宿主替 raw ICMP socket、汇集点看着的 UDP socket 记下的 ICMP 错误 guest 不该看到
    // （crate::rawicmp）
    if (level, name) == (SOL_SOCKET, SO_ERROR)
        && let Some(t) = crate::rawicmp::before_io(cx.kr(), &sock)
    {
        let fd = match &*sock.borrow() {
            Socket::Inet(s) => s.fd,
            _ => unreachable!("raw ICMP and UDP sockets are inet"),
        };
        take_errors(cx, &t, fd);
    }
    // raw ICMP 自己的选项（crate::rawicmp::RawIcmp），汇集点看着的 UDP socket 的 IP_RECVERR，
    // 汇集点替它记下的要报告的错误
    let raw = match &*sock.borrow() {
        Socket::Inet(s) if (level, name) == (SOL_SOCKET, SO_ERROR) && s.icmp_err.get() != 0 => {
            Some(Ok(s.icmp_err.replace(0).to_le_bytes().to_vec()))
        }
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => r.getsockopt(level, name),
        Socket::Inet(s) => crate::rawicmp::udp_getsockopt(s, level, name).map(Ok),
        _ => None,
    };
    let out = if let Some(r) = raw {
        r.map_err(Errno)?
    } else {
        match &*sock.borrow() {
            Socket::Inet(s) => match (level, name) {
                (IPPROTO_IPV6, IPV6_V6ONLY) if s.net.is_some() && s.family == AF_INET6 => {
                    (s.v6only as i32).to_le_bytes().to_vec()
                }
                (SOL_SOCKET, SO_RCVTIMEO) => encode_tv(s.rcvtimeo),
                (SOL_SOCKET, SO_SNDTIMEO) => encode_tv(s.sndtimeo),
                // SO_MARK：记下的那个（宿主上的是 Android 的 netd 设的）
                (SOL_SOCKET, SO_MARK) => s.mark().to_le_bytes().to_vec(),
                // raw ICMP 在宿主上是 ping socket
                (SOL_SOCKET, SO_TYPE) if s.raw => SOCK_RAW.to_le_bytes().to_vec(),
                _ => {
                    appine_hal::net::getsockopt(s.fd, level, name, room.min(256)).map_err(Errno)?
                }
            },
            Socket::Netlink(s) if (level, name) == (SOL_SOCKET, SO_RCVTIMEO) => {
                encode_tv(s.rcvtimeo)
            }
            Socket::Netlink(_) => 0i32.to_le_bytes().to_vec(),
            Socket::Unix(u) => unix::getsockopt(u, level, name, &pid_ns, &cx.cred())?,
        }
    };
    // 非阻塞的 TCP 连接被拒绝：guest 从这里得知（宿主读一次就清掉，不会数两次）
    if (level, name) == (SOL_SOCKET, SO_ERROR)
        && out.get(..4) == Some(&Errno::ECONNREFUSED.0.to_le_bytes())
    {
        let opening = matches!(&*sock.borrow(), Socket::Inet(s) if s.opening);
        if opening {
            count_connect(cx, &sock, Some(Errno::ECONNREFUSED));
        }
    }
    let n = room.min(out.len());
    cx.mm().write(UserSlice::new(value, n), &out[..n])?;
    put(cx, len, (n as u32).to_le_bytes()).map(|()| 0)
}

/// `SO_ORIGINAL_DST`（`SOL_IP`）、`IP6T_SO_ORIGINAL_DST`（`SOL_IPV6`）：照 Linux 的
/// `getorigdst`、`ipv6_getorigdst`（`nf_conntrack_proto.c`，[`crate::origdst`]），`room` 是 guest
/// 给的 optlen。None 是交给宿主：不是这个选项、不是 TCP、UDP 的 inet socket，AF_INET 的 socket 上
/// 的 `SOL_IPV6`（Linux 是 EOPNOTSUPP）。
///
/// - 不是 TCP 的是 ENOPROTOOPT，optlen 不够（16、28）是 EINVAL，都在查找之前。
/// - 连接跟踪按包的协议族分开：IPv4（含 IPv4 映射的地址）的连接只有 `SOL_IP` 找得到，IPv6
///   的只有 `SOL_IPV6`，另一个是 ENOENT。
/// - 发起连接的一端（connect 过的）：原方向的原目的地址就是它连的那个（guest 看到的对端），
///   命名空间里的包经过连接跟踪（[`crate::nft::Nft::tracks`]）才找得到。
/// - 接受的一端：发起的一端被 nat 换过目的地址的，报告 accept 时领到的；不然经过连接跟踪
///   时是自己的本端地址（没被 nat 的连接，原目的地址就是它）。
/// - 没有连接的（监听着的、还没连的）：Linux 拿全零的对端去找，ENOENT。
fn original_dst(
    cx: &Cx<'_>,
    sock: &SocketRef,
    level: i32,
    name: i32,
    room: usize,
) -> Option<Result<Vec<u8>, Errno>> {
    const IPPROTO_IP: i32 = 0;
    const SO_ORIGINAL_DST: i32 = 80;
    let b = sock.borrow();
    let Socket::Inet(s) = &*b else {
        return None;
    };
    if name != SO_ORIGINAL_DST || !matches!(level, IPPROTO_IP | IPPROTO_IPV6) || s.icmp.is_some() {
        return None;
    }
    let v6 = level == IPPROTO_IPV6;
    if v6 && s.family != AF_INET6 {
        return None;
    }
    match nat_proto(s) {
        Some(p) if p == IPPROTO_TCP as u8 && s.ty == SOCK_STREAM => {}
        Some(_) => return Some(Err(Errno::ENOPROTOOPT)),
        None => return None,
    }
    if room < if v6 { 28 } else { 16 } {
        return Some(Err(Errno::EINVAL));
    }
    // 协议族按连接的地址：发起的一端看它连的（新命名空间里 AF_INET6 的 socket 的本端是
    // 127.a.b.1，换回来是 ::1，分不出连的是不是 IPv4 映射的地址），接受的一端看本端
    let found = (|| {
        let local = inet_name(cx, s, false).ok()?;
        let fam_of = s.guest_peer.as_deref().unwrap_or(&local);
        let tracks = net_of(cx, s)
            .nft
            .borrow()
            .tracks(crate::origdst::is_v6(fam_of));
        let orig = if let Some(p) = &s.guest_peer {
            tracks.then(|| p.clone())?
        } else {
            appine_hal::net::sockname(s.fd, true).ok()?;
            match s.track.as_ref().and_then(|t| t.orig_dst.clone()) {
                Some(o) => o,
                None if tracks => local,
                None => return None,
            }
        };
        crate::origdst::encode(&orig, v6).filter(|_| crate::origdst::is_v6(&orig) == v6)
    })();
    Some(found.ok_or(Errno::ENOENT))
}

pub(super) fn shutdown(cx: &mut Cx<'_>, fd: i32, how: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => appine_hal::net::shutdown(s.fd, how).map_err(Errno)?,
        Socket::Netlink(_) => {}
        Socket::Unix(u) => unix::shutdown(u, how)?,
    }
    Ok(0)
}

/// netlink 的回答用的宿主信息：接口取自宿主，流量计数和宿主交来的路由取自内核。
/// 新的网络命名空间里只有它的 lo（[`crate::netns`]），流量不计。
struct KernelHost<'a> {
    k: &'a crate::kernel::Kernel,
    cred: crate::cred::Cred,
    net: Option<Rc<crate::nsproxy::NetNs>>,
    /// 发请求的进程的 pid 命名空间和 fd 表（RTM_GETNSID 的 NETNSA_PID、NETNSA_FD）。
    pid_ns: Rc<crate::pidns::PidNs>,
    fds: Rc<crate::cell::RefCell<crate::fd::FdTable>>,
    /// 发请求的进程在它的 pid 命名空间里的编号和名字（NFT_MSG_NEWGEN 的 PROC_PID、PROC_NAME）。
    proc_pid: u32,
    comm: [u8; 16],
}

impl KernelHost<'_> {
    /// 这个 socket 所在的网络命名空间（开机时的那个记作 None）。
    fn ns(&self) -> Rc<crate::nsproxy::NetNs> {
        self.net
            .clone()
            .unwrap_or_else(|| self.k.init_ns.net.clone())
    }

    /// 按进程（`by_pid`）或者命名空间文件找网络命名空间：进程要还没退出（Linux 的
    /// get_net_ns_by_pid，ESRCH），fd 要是网络命名空间的文件（get_net_ns_by_fd，EINVAL）。
    fn net_of(&self, by_pid: bool, value: u32) -> Result<Rc<crate::nsproxy::NetNs>, Errno> {
        if by_pid {
            let tid = self.pid_ns.find(value as i32).ok_or(Errno::ESRCH)?;
            // 虚拟的 1 号进程（没有 Task）在开机时的命名空间里
            if tid == crate::kernel::INIT && !self.k.tasks.contains_key(&tid) {
                return Ok(self.k.init_ns.net.clone());
            }
            let t = self.k.tasks.get(&tid).ok_or(Errno::ESRCH)?;
            return Ok(t.ns.borrow().net.clone());
        }
        let file = self.fds.borrow().get(value as i32)?;
        match &file.borrow().kind {
            FileKind::Proc {
                ns: Some(crate::nsproxy::NsRef::Net(n)),
                ..
            } => Ok(n.clone()),
            _ => Err(Errno::EINVAL),
        }
    }
}

impl crate::net::Host for KernelHost<'_> {
    fn interfaces(&self) -> Vec<appine_hal::net::Interface> {
        crate::netdev::base(&self.ns())
    }

    fn links(&self) -> Vec<crate::net::Link> {
        let ns = self.ns();
        let mut all: Vec<crate::net::Link> = crate::netdev::base(&ns)
            .into_iter()
            .map(crate::net::Link::Host)
            .collect();
        for v in crate::netdev::views(&ns) {
            let nsid = v
                .link
                .as_ref()
                .and_then(|(_, other)| other.as_ref())
                .map(|o| crate::netdev::nsid_alloc(&ns, o));
            all.push(crate::net::Link::Virt(v, nsid));
        }
        all.sort_by_key(|l| match l {
            crate::net::Link::Host(i) => i.index,
            crate::net::Link::Virt(v, _) => v.index,
        });
        all
    }

    fn tables(&self) -> Vec<crate::route::Entry> {
        crate::netdev::tables(&self.ns(), self.routes())
    }

    fn all_interfaces(&self) -> Vec<appine_hal::net::Interface> {
        crate::netdev::interfaces(&self.ns())
    }

    fn init_net(&self) -> bool {
        self.net.is_none()
    }

    fn stats(&self, name: &str) -> crate::traffic::IfStats {
        match &self.net {
            Some(_) => Default::default(),
            None => self.k.traffic.get(name),
        }
    }

    fn routes(&self) -> Option<Vec<appine_hal::net::Route>> {
        match &self.net {
            Some(_) => None,
            None => crate::route::host_routes(self.k),
        }
    }

    fn tx_queue_len(&self, name: &str) -> Option<u32> {
        match &self.net {
            Some(_) => Some(1000),
            None => appine_hal::net::tx_queue_len(name),
        }
    }

    fn uid(&self) -> u32 {
        self.cred.ruid
    }

    fn configure(&self, req: &[u8]) -> Option<Result<(), crate::netdev::Fail>> {
        // IFLA_NET_NS_PID（19）是进程，IFLA_NET_NS_FD（28）是命名空间文件
        let resolve = |attr: u16, value: u32| self.net_of(attr == 19, value);
        crate::netdev::request(&self.ns(), &self.cred, req, &resolve)
    }

    fn peer_net(&self, attr: u16, value: u32) -> Result<Option<i32>, i32> {
        let peer = self
            .net_of(attr == crate::net::NETNSA_PID, value)
            .map_err(|e| e.0)?;
        Ok(crate::netdev::nsid(&self.ns(), &peer))
    }

    fn netfilter(&self, data: &[u8], peer: &crate::nft::Peer<'_>) -> Vec<Vec<u8>> {
        let comm = self.comm.split(|&c| c == 0).next().unwrap_or_default();
        let peer = crate::nft::Peer {
            proc_pid: self.proc_pid,
            comm,
            ..*peer
        };
        crate::nft::handle(&self.ns(), &self.cred, data, &peer)
    }
}

/// 发送一条消息（send、sendto、sendmsg、write 共用）。
pub(crate) fn send(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    data: &[u8],
    addr: &[u8],
    control: &[u8],
    flags: u32,
) -> Result<usize, Errno> {
    if is_unix(sock) {
        return unix::send(cx, sock, nonblock, data, addr, control, flags);
    }
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let (host_fd, ty, timeout, to_stub) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            if let Some(r) = &mut s.icmp {
                r.sent(data);
            }
            let to_stub = s.net.is_none()
                && s.ty == SOCK_DGRAM
                && (is_stub(addr) || (addr.is_empty() && s.stub_peer));
            (s.fd, s.ty, s.sndtimeo, to_stub)
        }
        Socket::Netlink(s) => {
            let host = KernelHost {
                k: cx.kr(),
                cred: cx.cred(),
                net: s.net.clone(),
                pid_ns: cx.pid_ns(),
                fds: cx.t.fds.clone(),
                proc_pid: cx.pid_ns().vnr(cx.task.tgid).unwrap_or(0) as u32,
                comm: cx.task.comm.get(),
            };
            // 开机时的命名空间里的 nftables 规则集改了：重新看要转发哪些端口
            let init = &cx.kr().init_ns.net;
            let before = (s.net.is_none() && s.protocol == NETLINK_NETFILTER)
                .then(|| init.nft.borrow().generation);
            s.handle(data, &host);
            if before.is_some_and(|g| g != init.nft.borrow().generation) {
                forward_all(cx.kr());
            }
            request_wake_object(wake::of(sock));
            return Ok(data.len());
        }
        Socket::Unix(_) => unreachable!("handled above"),
    };
    if to_stub {
        // DNS 存根：释放 BKL 交给宿主解析，回答放进这个 socket 的队列
        let query = data.to_vec();
        let answer = cx
            .unlocked(|| appine_hal::net::dns_query(&query))
            .unwrap_or_else(|e| {
                crate::debug::log(format_args!("DNS stub: query failed: errno {e}"));
                servfail(&query)
            });

        if let Socket::Inet(s) = &mut *sock.borrow_mut() {
            s.answers.push_back(answer);
            s.answered += 1;
        }
        request_wake_object(wake::of(sock));
        return Ok(data.len());
    }
    // 目的地址换成宿主的（crate::netns：到不了时 ENETUNREACH），没绑定的先绑上；记下
    // 目的地所在的命名空间（低端口映射按它的）
    let w = cx.kr().world();
    let init = cx.kr().init_ns.net.clone();
    let cred = cx.cred();
    let mut key = None;
    // 绑在宿主通配地址上的 socket 发往映射的目的地：指定源地址（Socket::wide）
    let mut pktinfo = None;
    // 连接了的 UDP 的目的地被过滤链丢弃了（connect 时记下的）
    if addr.is_empty()
        && let Socket::Inet(s) = &*sock.borrow()
        && s.ty != SOCK_STREAM
        && let Some(e) = s.track.as_ref().and_then(|k| k.blocked)
    {
        return if e == 0 {
            Ok(data.len())
        } else {
            Err(Errno(e))
        };
    }
    let host_addr = match &mut *sock.borrow_mut() {
        Socket::Inet(s) if ty != SOCK_STREAM && (s.net.is_some() || !addr.is_empty()) => {
            let me = s.net.clone().unwrap_or_else(|| init.clone());
            if let (true, Some(v)) = (addr.is_empty(), &s.vconn) {
                pktinfo = crate::netns::implicit_bind(&me, s.family, false);
                Some(v.clone())
            } else if addr.is_empty() {
                let src = crate::netns::implicit_bind(&me, s.family, s.icmp6());
                ensure_bound(s, src.as_deref())?;
                Some(Vec::new())
            } else {
                // nftables 的 nat 换了目的地址的：记下，回来的数据报的来源换回去
                let Routed {
                    m,
                    dst,
                    there,
                    actual,
                    verdict,
                    ..
                } = route(&w, s, &me, addr, &cred)?;
                // 过滤链丢弃、拒绝了的：OUTPUT 上的是 EPERM（Linux 的 ip_local_out），别处的
                // 悄悄丢掉
                match verdict {
                    (crate::nft::Outcome::Accept | crate::nft::Outcome::Dnat(..), _) => {}
                    (_, true) => return Err(Errno::EPERM),
                    (_, false) => return Ok(data.len()),
                }
                // 发往新的网络命名空间里没人绑的端口（或者只绑了别的地址）：Linux 上丢掉，
                // 不能交给宿主（外层绑通配地址的 UDP socket 会收到）
                if nat_proto(s) == Some(IPPROTO_UDP as u8)
                    && let Some(t) = &m.target
                    && t.host_net.is_some()
                    && !crate::netns::listening(t, &there, true)
                {
                    return Ok(data.len());
                }
                if let Some(t) = &actual
                    && !s
                        .track
                        .as_ref()
                        .is_some_and(|k| k.nat_back.iter().any(|(x, o)| x == t && o == addr))
                {
                    let back = &mut s.track_mut().nat_back;
                    if back.len() >= NAT_BACK_MAX {
                        back.remove(0);
                    }
                    back.push((t.clone(), addr.to_vec()));
                }
                let dst = &dst[..];
                if crate::netns::is_inet(addr) {
                    // 新命名空间里还没绑定的 UDP socket 第一次发往命名空间里的地址：宿主上也
                    // 绑通配地址（Socket::wide），下面用 IP_PKTINFO 指定源地址，与先绑了通配
                    // 地址、端口 0 的（musl 的解析器、Node、Go、Java 这样做）一样。绑在回环地址
                    // 上的话之后再发往外面就出不去（宿主是 EINVAL）。发给自己的虚拟地址的照旧
                    // 绑 127.a.b.254：回答按登记的端口送回那里（Loopback::self_udp）
                    let source = m.source.as_deref().filter(|src| {
                        s.net.is_none()
                            || nat_proto(s) != Some(IPPROTO_UDP as u8)
                            || crate::netns::is_self_source(&me, src)
                    });
                    ensure_bound(s, source)?;
                }
                key = Some(m.target.as_ref().map_or(0, key_of));
                if s.wide && m.target.is_some() {
                    pktinfo = crate::netns::implicit_bind(&me, s.family, false);
                }
                // 开机时的命名空间里绑在通配地址上的（UDP 的服务端回答新命名空间里的客户端）：
                // 宿主选的源地址是 127.0.0.1，对方要看到的是面向它的那个虚拟地址（没绑定的
                // 由 ensure_bound 绑上了）
                if s.net.is_none()
                    && let Some(src) = &m.source
                    && appine_hal::net::sockname(s.fd, false)
                        .ok()
                        .is_some_and(|a| crate::netns::is_any(&a))
                {
                    pktinfo = Some(src.clone());
                }
                Some(crate::netns::toward_self(&me, dst, m.addr))
            }
        }
        _ => None,
    };
    let addr = host_addr.as_deref().unwrap_or(addr);
    // TCP 不看给的地址（MSG_FASTOPEN 除外，Linux 的 tcp_sendmsg_locked），这里也不交给宿主：
    // 宿主的 LSM 可能按地址族检查它（Ubuntu 24.04 的 AppArmor 拒绝 AF_UNSPEC，EAFNOSUPPORT）
    let addr = if ty == SOCK_STREAM && flags & MSG_FASTOPEN == 0 {
        &[][..]
    } else {
        addr
    };
    // 发往本机上映射了的低端口：换成宿主上的（crate::portmap）
    let mapped = match &*sock.borrow() {
        Socket::Inet(s) if !addr.is_empty() => {
            let k = key.unwrap_or_else(|| ns_key(s));
            let (a, pm) = map_dest(&cx.kr().portmap, s, addr.to_vec(), k);
            pm.map(|_| a)
        }
        _ => None,
    };
    let addr = mapped.as_deref().unwrap_or(addr);
    // raw ICMP socket、交给汇集点看着的 UDP socket（crate::rawicmp）：宿主替它记下的 ICMP
    // 错误会让发送失败，先取走；因为它失败了也取走再试一次
    let tap = crate::rawicmp::before_io(cx.kr(), sock);
    if let Some(t) = &tap {
        take_errors(cx, t, host_fd);
        if let Some(e) = crate::rawicmp::take_error(sock) {
            return Err(e);
        }
        // 已连接的 UDP socket 发往别的地址（busybox 的 traceroute 连上目的地址之后每个探测包
        // 换一个端口）：宿主只把与 socket 的连接对得上的报文引起的错误交给它，先改连到这个
        // 地址。guest 看到的对端仍是它连的（getpeername）
        let connected_udp = matches!(&*sock.borrow(),
            Socket::Inet(s) if s.icmp.is_none() && s.peer.is_some());
        if connected_udp && !addr.is_empty() {
            let current = appine_hal::net::sockname(host_fd, true).unwrap_or_default();
            if !same_endpoint(&current, addr) {
                let _ = appine_hal::net::connect(host_fd, addr);
            }
        }
    }
    let with_src = pktinfo.map(|src| with_pktinfo(control, &src));
    let control = with_src.as_deref().unwrap_or(control);
    let mut retried = false;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut done = 0;
    let mut sent_datagram = false;
    let r = loop {
        let r = appine_hal::net::sendmsg(
            host_fd,
            &data[done..],
            addr,
            control,
            flags as i32 & !(MSG_DONTWAIT | MSG_NOSIGNAL) as i32,
        );
        match r {
            Ok(n) => {
                done += n;
                sent_datagram = ty != SOCK_STREAM;
                // 流式 socket 的阻塞发送要把数据全部发出去
                if ty != SOCK_STREAM || done == data.len() || nonblock {
                    break Ok(done);
                }
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                if let Err(err) = wait_host(
                    cx,
                    sock,
                    host_fd,
                    POLLOUT,
                    nonblock,
                    deadline,
                    Errno::EAGAIN,
                ) {
                    break if done > 0 { Ok(done) } else { Err(err) };
                }
            }
            Err(e) if e == Errno::EPIPE.0 => break Err(epipe(cx, flags)),
            Err(e) if !retried && tap.is_some() && crate::rawicmp::from_icmp(e) => {
                if let Some(t) = &tap {
                    take_errors(cx, t, host_fd);
                }
                retried = true;
            }
            Err(e) => break if done > 0 { Ok(done) } else { Err(Errno(e)) },
        }
    };
    if done > 0 || sent_datagram {
        account(cx, sock, false, done, addr);
    }
    // 宿主自动绑定的（新命名空间里第一次发往外面的）：这时才知道端口，登记它
    if sent_datagram
        && let Socket::Inet(s) = &mut *sock.borrow_mut()
        && s.listener.is_none()
    {
        register_udp(s);
    }
    r
}

/// 在控制消息 `control` 后面加一个指定源地址 `src`（sockaddr）的 `IP_PKTINFO`
/// （`IPV6_PKTINFO`，IPv4 映射的地址），guest 自己给了的不加。
fn with_pktinfo(control: &[u8], src: &[u8]) -> Vec<u8> {
    const IPPROTO_IP: i32 = 0;
    const IP_PKTINFO: i32 = 8;
    const IPV6_PKTINFO: i32 = 50;
    // struct cmsghdr：cmsg_len（size_t）、cmsg_level、cmsg_type，内容按 8 字节对齐
    let mut off = 0;
    while off + 16 <= control.len() {
        let len = u64::from_le_bytes(control[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(control[off + 8..off + 12].try_into().unwrap());
        let ty = i32::from_le_bytes(control[off + 12..off + 16].try_into().unwrap());
        if (level == IPPROTO_IP && ty == IP_PKTINFO)
            || (level == IPPROTO_IPV6 && ty == IPV6_PKTINFO)
        {
            return control.to_vec();
        }
        if len < 16 {
            break;
        }
        off += len.next_multiple_of(8);
    }
    let (level, ty, data) = if u16::from_le_bytes([src[0], src[1]]) as i32 == AF_INET6 {
        // struct in6_pktinfo：地址、接口编号
        let mut d = src[8..24].to_vec();
        d.extend_from_slice(&0i32.to_le_bytes());
        (IPPROTO_IPV6, IPV6_PKTINFO, d)
    } else {
        // struct in_pktinfo：接口编号、ipi_spec_dst（源地址）、ipi_addr
        let mut d = 0i32.to_le_bytes().to_vec();
        d.extend_from_slice(&src[4..8]);
        d.extend_from_slice(&[0; 4]);
        (IPPROTO_IP, IP_PKTINFO, d)
    };
    let mut out = control.to_vec();
    out.resize(out.len().next_multiple_of(8), 0);
    out.extend_from_slice(&((16 + data.len()) as u64).to_le_bytes());
    out.extend_from_slice(&level.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&data);
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

/// 两个 IPv4、IPv6 地址的协议族、端口和地址都相同。
/// 从宿主外面连到开机时的命名空间里的 `s`（TCP 的监听、绑了端口的 UDP）时，PREROUTING
/// 的 nat 把它转到哪里：宿主上的目的地（容器的回环地址，低端口换成映射的），以及连过去时
/// 绑的源地址（开机时的命名空间面向它的地址）。
fn forward_target(k: &crate::kernel::Kernel, s: &InetSocket) -> Option<crate::forward::Target> {
    if s.net.is_some() || s.raw || s.icmp.is_some() {
        return None;
    }
    let udp = match (s.ty, map_proto(s)) {
        (SOCK_STREAM, IPPROTO_TCP) => false,
        (SOCK_DGRAM, IPPROTO_UDP) => true,
        _ => return None,
    };
    let init = &k.init_ns.net;
    if !init.nft.borrow().has_nat(crate::nft::HOOK_PRE_ROUTING) {
        return None;
    }
    let mut local = appine_hal::net::sockname(s.fd, false).ok()?;
    let host_port = port_of(&local).filter(|&p| p != 0)?;
    if !udp && !s.listening {
        return None;
    }
    // 规则里是 guest 的端口
    if let Some((g, h)) = s.local_map
        && h == host_port
    {
        set_port(&mut local, g);
    }
    let proto = if udp { IPPROTO_UDP } else { IPPROTO_TCP };
    let dst = crate::netns::external_nat(init, proto as u8, &local)?;
    let w = k.world();
    let m = crate::netns::to_host(&w, init, &dst, crate::netns::Use::Reach, false).ok()?;
    // 过滤链（FORWARD 等）丢弃、拒绝的不转发
    if !crate::netns::external_filter(init, m.target.as_deref(), proto as u8, &dst) {
        return None;
    }
    let mut addr = m.addr;
    let key = m.target.as_ref().map_or(0, key_of);
    let map = k.portmap.borrow();
    if let Some(p) = port_of(&addr)
        && let Some(h) = map.host_port(key, proto, p)
    {
        set_port(&mut addr, h);
    }
    Some(crate::forward::Target {
        udp,
        addr,
        source: m.source,
    })
}

/// 按现在的规则决定 `s` 要不要转发、转到哪里（[`crate::forward`]）。
fn forward_socket(k: &crate::kernel::Kernel, s: &mut InetSocket) {
    let want = forward_target(k, s);
    if want.as_ref() == s.fwd.as_ref().map(|h| &h.target) {
        return;
    }
    // 先交新的再丢旧的：中间没有空档
    let new = want.and_then(|t| k.forward.add(s.fd, t).ok()).map(Box::new);
    s.fwd = new;
}

/// 开机时的命名空间里的 nftables 规则集改了之后，重新看所有 socket。
fn forward_all(k: &crate::kernel::Kernel) {
    for (sock, _) in crate::pseudo::sockets(k) {
        if let Ok(mut b) = sock.try_borrow_mut()
            && let Socket::Inet(s) = &mut *b
            && s.net.is_none()
        {
            forward_socket(k, s);
        }
    }
}

/// [`route`] 的结果。
struct Routed {
    /// 宿主上的目的地（[`crate::netns::to_host`]），源地址按 `me` 这边选。
    m: crate::netns::Mapped,
    /// OUTPUT nat 之后的目的地（`me` 里看到的）。
    dst: Vec<u8>,
    /// 最后的目的地在它所在的命名空间（`m.target`）里看到的样子（PREROUTING nat 之后）。
    there: Vec<u8>,
    /// nat 换过目的地时，`me` 里看到的实际的目的地：从它回来的数据报报告成来自 guest 给的
    /// 地址（[`crate::net::Track::nat_back`]）。
    actual: Option<Vec<u8>>,
    /// 经过的过滤链的结果，以及是不是在 OUTPUT 上（[`crate::netns::path_filter`]）。
    verdict: (crate::nft::Outcome, bool),
    /// 被 nat 换过目的地址的命名空间（编号）和那里看到的原目的地址：`me` 里是 guest 给的，
    /// 进入的别的命名空间里是那里的 PREROUTING 之前的（`SO_ORIGINAL_DST`，[`crate::origdst`]）。
    origs: Vec<(u64, Vec<u8>)>,
}

/// connect、sendto 的目的地址 `addr`（guest 的）：先过 `me` 的 OUTPUT nat，换成宿主上的；
/// 进入别的命名空间的（[`crate::netns::entering`]）再过那里的 PREROUTING nat，按那个命名空间
/// 换成宿主上的（容器经由网关、外层地址连发布的端口）。
fn route(
    w: &crate::netns::World,
    s: &InetSocket,
    me: &Rc<crate::nsproxy::NetNs>,
    addr: &[u8],
    cred: &crate::cred::Cred,
) -> Result<Routed, Errno> {
    let nat = nat_dest(s, me, addr, cred)?;
    let mut origs = Vec::new();
    if nat.is_some() {
        origs.push((me.id, addr.to_vec()));
    }
    let dst = nat.clone().unwrap_or_else(|| addr.to_vec());
    let mut m = crate::netns::to_host(w, me, &dst, crate::netns::Use::Reach, s.icmp6())?;
    let mut actual = nat;
    let mut there = dst.clone();
    if let Some(proto) = nat_proto(s)
        && let Some(t) = crate::netns::entering(w, me, &m, &dst)
        && let Some(d2) = crate::netns::prerouting_nat(me, &t, proto, local_port(s), &dst)?
    {
        let m2 = crate::netns::to_host(w, &t, &d2, crate::netns::Use::Reach, s.icmp6())?;
        origs.push((t.id, dst.clone()));
        there = d2;
        actual = Some(crate::netns::from_host(
            w,
            me,
            &m2.addr,
            crate::netns::Role::Peer,
            s.icmp6(),
        ));
        // 原来经由宿主网络的没有选源地址：DNAT 到了命名空间里的，新命名空间里绑它的回环地址，
        // 对方才认得出来源；DNAT 到外面的别的地址时仍不绑，绑了就出不去
        let source = m.source.take().or_else(|| {
            m2.target.as_ref()?;
            me.host_net
                .and_then(|_| crate::netns::implicit_bind(me, s.family, s.icmp6()))
        });
        m = crate::netns::Mapped {
            addr: m2.addr,
            target: m2.target,
            source,
            vaddr: m.vaddr,
        };
    }
    // 经过的过滤链（crate::netns::path_filter）：UDP 回给收到过的对方是 ESTABLISHED 的
    let verdict = match nat_proto(s) {
        Some(proto) => {
            let established = proto == IPPROTO_UDP as u8
                && s.track
                    .as_ref()
                    .is_some_and(|k| k.seen.iter().any(|p| same_endpoint(p, addr)));
            let flow = crate::netns::Flow {
                proto,
                vaddr: s.vaddr,
                sport: local_port(s),
                dst: &dst,
                there: &there,
                uid: cred.euid,
                gid: cred.egid,
                established,
                mark: s.mark(),
            };
            crate::netns::path_filter(w, me, m.target.as_ref(), &flow)
        }
        None => (crate::nft::Outcome::Accept, false),
    };
    Ok(Routed {
        m,
        dst,
        there,
        actual,
        verdict,
        origs,
    })
}

/// 过滤链丢弃的 TCP 连接连到的宿主地址：一个接受队列已经满了、从不 accept 的宿主监听，再来
/// 的 SYN 宿主悄悄丢掉，连接照 Linux 的重传超时失败（ETIMEDOUT），guest 设的超时照样起作用。
/// 每个内核一个，取不到时是 None（这时 connect 马上是 ETIMEDOUT）。
fn blackhole(k: &crate::kernel::Kernel) -> Option<Vec<u8>> {
    k.blackhole
        .get_or_init(|| {
            const POLLOUT: i16 = 4;
            let lo = appine_hal::net::sockaddr(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
            let l = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).ok()?;
            appine_hal::net::bind(l, &lo).ok()?;
            appine_hal::net::listen(l, 0).ok()?;
            let addr = appine_hal::net::sockname(l, false).ok()?;
            // 填满接受队列：连到一个连不上为止（backlog 0 也可能先放进一两个）
            let connects = |wait: Duration| -> Option<(i32, bool)> {
                let c = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).ok()?;
                let _ = appine_hal::net::connect(c, &addr);
                let mut p = [(c, POLLOUT, 0)];
                let _ = appine_hal::poll(&mut p, Some(wait));
                let err = appine_hal::net::getsockopt(c, SOL_SOCKET, SO_ERROR, 4).ok()?;
                Some((c, p[0].2 & POLLOUT != 0 && err[..4] == [0; 4]))
            };
            for _ in 0..4 {
                let (c, done) = connects(Duration::from_millis(200))?;
                if !done {
                    appine_hal::net::close(c);
                    // 监听和填进去的连接一直留着
                    return Some(addr);
                }
            }
            None
        })
        .clone()
}

/// 要过 nftables 的 nat 的 socket 的协议：TCP（含 MPTCP）、UDP。
fn nat_proto(s: &InetSocket) -> Option<u8> {
    match (s.ty, s.proto) {
        (SOCK_STREAM, 0 | IPPROTO_TCP | IPPROTO_MPTCP) => Some(IPPROTO_TCP as u8),
        (SOCK_DGRAM, 0 | IPPROTO_UDP) if s.icmp.is_none() => Some(IPPROTO_UDP as u8),
        _ => None,
    }
}

/// 宿主 socket 的本端端口（还没绑定时是 0）。
fn local_port(s: &InetSocket) -> u16 {
    appine_hal::net::sockname(s.fd, false)
        .ok()
        .and_then(|a| port_of(&a))
        .unwrap_or(0)
}

/// 一个未连接的数据报 socket 记下的 DNAT（[`crate::net::Track::nat_back`]）、收到过的对方
/// （[`crate::net::Track::seen`]）各最多这么多个。
const NAT_BACK_MAX: usize = 16;

/// nftables 的 OUTPUT nat（[`crate::netns::output_nat`]）换过的目的地址；TCP、UDP 以外的不看。
fn nat_dest(
    s: &InetSocket,
    me: &crate::nsproxy::NetNs,
    dst: &[u8],
    cred: &crate::cred::Cred,
) -> Result<Option<Vec<u8>>, Errno> {
    let Some(proto) = nat_proto(s) else {
        return Ok(None);
    };
    crate::netns::output_nat(
        me,
        proto,
        s.vaddr,
        || local_port(s),
        dst,
        cred.euid,
        cred.egid,
        s.mark(),
    )
}

fn same_endpoint(a: &[u8], b: &[u8]) -> bool {
    let n = match a.get(..2).map(|f| u16::from_le_bytes([f[0], f[1]]) as i32) {
        Some(AF_INET) => 8,
        Some(AF_INET6) => 24,
        _ => return false,
    };
    a.get(..n).is_some() && a.get(..n) == b.get(..n)
}

/// 取走宿主记在 `fd` 上的 ICMP 错误（分给 raw socket，[`crate::rawicmp::dispatch`]）。
fn take_errors(cx: &mut Cx<'_>, tap: &Rc<RefCell<crate::rawicmp::Tap>>, fd: i32) {
    let arrivals = crate::rawicmp::dispatch(tap, Some(fd));
    crate::rawicmp::account(cx.k(), arrivals);
}

/// 记下宿主 socket 上收发的流量（[`crate::traffic`]）。
fn account(cx: &mut Cx<'_>, sock: &SocketRef, rx: bool, bytes: usize, peer: &[u8]) {
    // 新的网络命名空间里的流量不算进宿主的接口
    if let Socket::Inet(s) = &mut *sock.borrow_mut()
        && s.net.is_none()
    {
        cx.k().traffic.account(s, rx, bytes, peer);
    }
}

/// sockaddr（AF_INET、AF_INET6）里的端口。
fn port_of(addr: &[u8]) -> Option<u16> {
    let family = i32::from(u16::from_le_bytes([*addr.first()?, *addr.get(1)?]));
    match family {
        AF_INET | AF_INET6 if addr.len() >= 4 => Some(u16::from_be_bytes([addr[2], addr[3]])),
        _ => None,
    }
}

fn set_port(addr: &mut [u8], port: u16) {
    addr[2..4].copy_from_slice(&port.to_be_bytes());
}

/// 映射了的低端口在这个 socket 上起作用（[`crate::portmap`]）：TCP、UDP，不是 raw socket。
fn mappable(s: &InetSocket) -> bool {
    !s.raw && matches!(map_proto(s), IPPROTO_TCP | IPPROTO_UDP)
}

/// 低端口映射的登记表里的协议：MPTCP 的端口与 TCP 的是同一些（Go 1.24 起监听默认用 MPTCP）。
fn map_proto(s: &InetSocket) -> i32 {
    if s.proto == IPPROTO_MPTCP {
        IPPROTO_TCP
    } else {
        s.proto
    }
}

/// 登记表里的网络命名空间：开机时的是 0。
fn ns_key(s: &InetSocket) -> usize {
    s.net.as_ref().map_or(0, |n| Rc::as_ptr(n) as usize)
}

/// sockaddr 里的 IP（IPv4 映射的 IPv6 地址取 IPv4 的那部分）。
fn ip_of(addr: &[u8]) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let family = i32::from(u16::from_le_bytes([*addr.first()?, *addr.get(1)?]));
    match family {
        AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(addr.get(4..8)?).ok()?,
        ))),
        AF_INET6 => {
            let v6 = Ipv6Addr::from(<[u8; 16]>::try_from(addr.get(8..24)?).ok()?);
            Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4))
        }
        _ => None,
    }
}

/// 宿主地址是本机的：回环、通配地址，或者宿主接口上的（连自己的 wlan0 地址）。
fn is_local(addr: &[u8]) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let Some(ip) = ip_of(addr) else {
        return false;
    };
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    appine_hal::net::interfaces().iter().any(|i| {
        i.addrs.iter().any(|(_, a, _)| {
            let own = match a.len() {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&a[..]).unwrap())),
                16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&a[..]).unwrap())),
                _ => return false,
            };
            own == ip
        })
    })
}

/// 要连、要发往宿主地址 `addr`：本机上映射了的低端口换成宿主端口（[`crate::portmap`]），
/// 按目的地所在的网络命名空间（`key`，[`key_of`]）的登记。返回换过的地址和 (guest 端口,
/// 宿主端口)。guest 里没有程序绑着这个端口时不换：手机上别的 App 恰好占着宿主端口时连
/// 过去就错了，应该与原来一样连不上。
fn map_dest(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    s: &InetSocket,
    mut addr: Vec<u8>,
    key: usize,
) -> (Vec<u8>, Option<(u16, u16)>) {
    let Some(port) = port_of(&addr) else {
        return (addr, None);
    };
    let host = {
        if !mappable(s) {
            return (addr, None);
        }
        // 低端口和新命名空间里显式绑的端口才有登记，没有登记的不换
        map.borrow().host_port(key, map_proto(s), port)
    };
    match host {
        Some(h) if is_local(&addr) => {
            set_port(&mut addr, h);
            (addr, Some((port, h)))
        }
        _ => (addr, None),
    }
}

/// 宿主给的本端（`local`）或者对端地址里，映射来的宿主端口换回 guest 的：本端按 socket
/// 绑的映射；对端先按 connect 时记下的，再看是不是本机上映射来的（UDP 的来源、accept 到的
/// 对端用低端口绑定过），按对端地址所在的网络命名空间（[`crate::netns::owner_of_host`]）。
pub(crate) fn unmap_addr(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    w: &crate::netns::World,
    s: &InetSocket,
    mut addr: Vec<u8>,
    local: bool,
) -> Vec<u8> {
    let Some(port) = port_of(&addr) else {
        return addr;
    };
    let known = if local { s.local_map } else { s.peer_map };
    // 从外面转发进来的连接的来源不是映射来的端口（crate::forward）
    let outside = ip_of(&addr).is_some_and(|ip| match ip {
        std::net::IpAddr::V4(v) => v.octets()[..2] == [127, crate::forward::OUTSIDE_BLOCK],
        std::net::IpAddr::V6(_) => false,
    });
    let guest = match known {
        Some((g, h)) if h == port => Some(g),
        _ if local || !mappable(s) || outside => None,
        _ => {
            let key =
                crate::netns::owner_of_host(w, &addr).map_or_else(|| ns_key(s), |n| key_of(&n));
            map.borrow()
                .guest_port(key, map_proto(s), port)
                .filter(|_| is_local(&addr))
        }
    };
    if let Some(g) = guest {
        set_port(&mut addr, g);
    }
    addr
}

/// 宿主上的本端 IPv4 地址换成 guest 看到的（[`crate::netns`]）。
fn local_addr(
    w: &crate::netns::World,
    me: &crate::nsproxy::NetNs,
    s: &InetSocket,
    a: [u8; 4],
) -> [u8; 4] {
    let mut sa = vec![0u8; 16];
    sa[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[4..8].copy_from_slice(&a);
    let role = crate::netns::Role::Local {
        any: false,
        vaddr: s.vaddr,
        peer: None,
    };
    let g = crate::netns::from_host(w, me, &sa, role, false);
    g.get(4..8).and_then(|b| b.try_into().ok()).unwrap_or(a)
}

/// 对方已经关闭：除非带着 `MSG_NOSIGNAL`，给当前线程发 SIGPIPE。
pub(super) fn epipe(cx: &mut Cx<'_>, flags: u32) -> Errno {
    if flags & MSG_NOSIGNAL == 0 {
        let task = cx.task.clone();
        signal::send_to_task(cx.kr(), &task, Signal::PIPE, SigInfo::kernel());
    }
    Errno::EPIPE
}

/// 收到的一条消息。
pub(crate) struct Message {
    pub data: Vec<u8>,
    /// 消息的完整长度（`MSG_TRUNC` 时可能大于 `data`）。
    pub len: usize,
    pub addr: Vec<u8>,
    pub control: Vec<u8>,
    pub flags: u32,
}

/// 接收一条消息（recv、recvfrom、recvmsg、read 共用），最多 `max` 字节。
pub(crate) fn recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    let max = max.min(MSG_MAX);
    if is_unix(sock) {
        return unix::recv(cx, sock, nonblock, max, control_len, flags);
    }
    // 读错误队列（MSG_ERRQUEUE）从不等待：没有排队的错误时 EAGAIN
    let nonblock = nonblock || flags & (MSG_DONTWAIT | MSG_ERRQUEUE) != 0;
    // raw ICMP socket 收到的报文都经由汇集点（crate::rawicmp）
    let raw = match &*sock.borrow() {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => Some(r.tap.clone()),
        _ => None,
    };
    if let Some(tap) = raw {
        return raw_recv(cx, sock, &tap, nonblock, max, control_len, flags);
    }
    // 进程内产生的数据报：DNS 存根的回答、netlink 的回答
    let local = |q: &mut std::collections::VecDeque<Vec<u8>>, addr: Vec<u8>| {
        let d = if flags & MSG_PEEK != 0 {
            q.front().cloned()
        } else {
            q.pop_front()
        }?;
        let n = d.len().min(max);
        Some(Message {
            data: d[..n].to_vec(),
            len: d.len(),
            addr,
            control: Vec::new(),
            flags: if d.len() > n { MSG_TRUNC } else { 0 },
        })
    };
    // netlink 等回答的期限（SO_RCVTIMEO），第一次没有回答可读时定下
    let mut netlink_deadline = None;
    let (host_fd, ty, timeout) = loop {
        let host = match &mut *sock.borrow_mut() {
            Socket::Inet(s) => {
                let addr = stub_addr(s.family);
                if let Some(m) = local(&mut s.answers, addr) {
                    return Ok(m);
                }
                Some((s.fd, s.ty, s.rcvtimeo))
            }
            Socket::Netlink(s) => {
                let addr = {
                    let mut b = vec![0u8; 12];
                    b[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
                    b
                };
                if let Some(m) = local(&mut s.queue, addr) {
                    return Ok(m);
                }
                let timeo = s.rcvtimeo;
                netlink_deadline.get_or_insert_with(|| timeo.map(|t| Instant::now() + t));
                None
            }
            Socket::Unix(_) => unreachable!("handled above"),
        };
        match host {
            Some(h) => break h,
            // netlink 没有回答可读：等到被信号打断或者 SO_RCVTIMEO 到期（原来不看它，一直等）
            None => {
                let deadline = netlink_deadline.flatten();
                if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(Errno::EAGAIN);
                }
                if cx.interrupted() {
                    return Err(Errno::ERESTARTSYS);
                }
                cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(sock)])?;
            }
        }
    };
    // 交给汇集点看着的 UDP socket：宿主替它记下的 ICMP 错误会让接收失败，先取走；因为它
    // 失败了也取走再试一次
    let tap = crate::rawicmp::before_io(cx.kr(), sock);
    if let Some(t) = &tap {
        take_errors(cx, t, host_fd);
        if flags & MSG_ERRQUEUE == 0
            && let Some(e) = crate::rawicmp::take_error(sock)
        {
            return Err(e);
        }
    }
    let mut retried = false;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut buf = vec![0u8; max];
    let mut got = 0;
    // 偷看的数据之后还会再读到，错误队列里不是收到的流量
    let counted = flags & (MSG_PEEK | MSG_ERRQUEUE) == 0;
    loop {
        let host_flags = flags & !(MSG_DONTWAIT | MSG_WAITALL);
        match appine_hal::net::recvmsg(host_fd, &mut buf[got..], control_len, host_flags as i32) {
            Ok(r) => {
                // 记下了对端的（InetSocket::vconn）：不是来自它的丢掉（偷看到的也要取走）
                let foreign = matches!(&*sock.borrow(),
                    Socket::Inet(s) if s.vconn.as_ref().is_some_and(|v| !same_endpoint(v, &r.addr)));
                if foreign {
                    if flags & MSG_PEEK != 0 {
                        let mut skip = vec![0u8; 1];
                        let _ = appine_hal::net::recvmsg(host_fd, &mut skip, 0, 0);
                    }
                    continue;
                }
                let wait_all = flags & MSG_WAITALL != 0 && ty == SOCK_STREAM;
                got += r.len.min(buf.len() - got);
                if wait_all && r.len > 0 && got < max {
                    continue;
                }
                buf.truncate(got);
                let len = if ty == SOCK_STREAM { got } else { r.len };
                if counted {
                    account(cx, sock, true, len, &r.addr);
                }
                let addr = match &*sock.borrow() {
                    Socket::Inet(s) if !r.addr.is_empty() => {
                        // 来自本机上映射了的低端口：报告 guest 的端口（crate::portmap）
                        let w = cx.kr().world();
                        let a = unmap_addr(&cx.kr().portmap, &w, s, r.addr, false);
                        let me = net_of(cx, s);
                        let a = match s.self_port.and_then(|_| crate::netns::from_self(&me, &a)) {
                            Some(v) => v,
                            None => crate::netns::from_host(
                                &w,
                                &me,
                                &a,
                                crate::netns::Role::Peer,
                                s.icmp6(),
                            ),
                        };
                        // 来自 DNAT 换成的目的地：报告 guest 发往的那个
                        match s
                            .track
                            .as_ref()
                            .and_then(|k| k.nat_back.iter().find(|(t, _)| same_endpoint(t, &a)))
                        {
                            Some((_, o)) => o.clone(),
                            None => a,
                        }
                    }
                    _ => r.addr,
                };
                // UDP：记下收到过的对方，回给它们的是 ESTABLISHED 的（过滤链）
                if let Socket::Inet(s) = &mut *sock.borrow_mut()
                    && s.ty == SOCK_DGRAM
                    && s.icmp.is_none()
                    && crate::netns::is_inet(&addr)
                    && flags & MSG_PEEK == 0
                    && !s
                        .track
                        .as_ref()
                        .is_some_and(|k| k.seen.iter().any(|p| same_endpoint(p, &addr)))
                {
                    let seen = &mut s.track_mut().seen;
                    if seen.len() >= NAT_BACK_MAX {
                        seen.remove(0);
                    }
                    seen.push(addr.clone());
                }
                return Ok(Message {
                    data: buf,
                    len,
                    addr,
                    control: r.control,
                    flags: r.flags as u32,
                });
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                // 有 DNS 存根的回答进来也要醒：它们不经过宿主 socket
                if let Socket::Inet(s) = &mut *sock.borrow_mut()
                    && let Some(m) = local(&mut s.answers, stub_addr(s.family))
                {
                    return Ok(m);
                }
                if got > 0 {
                    buf.truncate(got);
                    if counted {
                        account(cx, sock, true, got, &[]);
                    }
                    return Ok(Message {
                        len: got,
                        data: buf,
                        addr: Vec::new(),
                        control: Vec::new(),
                        flags: 0,
                    });
                }
                wait_host(cx, sock, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)?;
            }
            Err(e) if !retried && tap.is_some() && crate::rawicmp::from_icmp(e) => {
                if let Some(t) = &tap {
                    take_errors(cx, t, host_fd);
                }
                retried = true;
            }
            Err(e) => return Err(Errno(e)),
        }
    }
}

/// raw ICMP socket 的接收：报文在汇集点里它自己的队列中（[`crate::rawicmp`]），先取下
/// 宿主上已经到了的，没有就连同汇集点的宿主 fd 一起等。
fn raw_recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    tap: &Rc<RefCell<crate::rawicmp::Tap>>,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    // 错误队列：guest 打开了 IP_RECVERR 时汇集点放进来的（从不等待）
    if flags & MSG_ERRQUEUE != 0 {
        let arrivals = crate::rawicmp::dispatch(tap, None);
        crate::rawicmp::account(cx.k(), arrivals);
        let e = crate::rawicmp::take_err(tap, sock).ok_or(Errno::EAGAIN)?;
        let s = sock.borrow();
        let Socket::Inet(
            s @ InetSocket {
                icmp: Some(raw), ..
            },
        ) = &*s
        else {
            unreachable!("raw ICMP socket");
        };
        let w = cx.kr().world();
        let me = net_of(cx, s);
        let addr = crate::netns::from_host(&w, &me, &e.addr, crate::netns::Role::Peer, s.icmp6());
        let (control, mut out_flags) =
            crate::rawicmp::fit_control(&raw.err_control(&e), control_len);
        let mut data = e.data;
        let len = data.len();
        if len > max {
            data.truncate(max);
            out_flags |= MSG_TRUNC;
        }
        return Ok(Message {
            data,
            len,
            addr,
            control,
            flags: out_flags | MSG_ERRQUEUE,
        });
    }
    let key = wake::of(sock);
    let timeout = match &*sock.borrow() {
        Socket::Inet(s) => s.rcvtimeo,
        _ => None,
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let arrivals = crate::rawicmp::dispatch(tap, None);
        crate::rawicmp::account(cx.k(), arrivals);
        if let Some(e) = crate::rawicmp::take_error(sock) {
            return Err(e);
        }
        if let Some(it) = crate::rawicmp::take(tap, key, flags & MSG_PEEK != 0) {
            let s = sock.borrow();
            let Socket::Inet(
                s @ InetSocket {
                    icmp: Some(raw), ..
                },
            ) = &*s
            else {
                unreachable!("raw ICMP socket");
            };
            let w = cx.kr().world();
            let me = net_of(cx, s);
            let src =
                crate::netns::from_host(&w, &me, &it.src, crate::netns::Role::Peer, s.icmp6());
            let (mut data, control) = raw.present(&it, key, &src, |a| local_addr(&w, &me, s, a));
            let len = data.len();
            let (control, mut out_flags) = crate::rawicmp::fit_control(&control, control_len);
            if len > max {
                data.truncate(max);
                out_flags |= MSG_TRUNC;
            }
            return Ok(Message {
                data,
                len,
                addr: src,
                control,
                flags: out_flags,
            });
        }
        if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        let fd = tap.borrow().fd();
        cx.wait_objects(
            deadline,
            &mut [(fd, POLLIN, 0)],
            wake::FILE,
            &[wake::of(sock)],
        )?;
    }
}

pub(super) fn sendto(
    cx: &mut Cx<'_>,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    addr: u64,
    addrlen: u32,
) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let len = len.min(MSG_MAX);
    let mut data = vec![0u8; len];
    cx.mm().read(UserSlice::new(buf, len), &mut data)?;
    let addr = if addr != 0 {
        read_addr(cx, addr, addrlen)?
    } else {
        Vec::new()
    };
    send(cx, &sock, nonblock, &data, &addr, &[], flags).map(|n| n as u64)
}

pub(super) fn recvfrom(
    cx: &mut Cx<'_>,
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    addr: u64,
    addrlen: u64,
) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let m = recv(cx, &sock, nonblock, len, 0, flags)?;
    cx.mm().write(UserSlice::new(buf, m.data.len()), &m.data)?;
    write_addr(cx, addr, addrlen, &m.addr)?;
    Ok(if flags & MSG_TRUNC != 0 {
        m.len
    } else {
        m.data.len()
    } as u64)
}

/// guest 的 `struct msghdr`。
struct MsgHdr {
    name: u64,
    namelen: u32,
    /// (基址, 长度)。
    iovecs: Vec<(u64, usize)>,
    control: u64,
    controllen: usize,
}

fn read_msghdr(cx: &Cx<'_>, msg: u64) -> Result<MsgHdr, Errno> {
    let b: [u8; 56] = get(cx, msg)?;
    let word = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let name = word(0);
    let namelen = u32::from_le_bytes(b[8..12].try_into().unwrap());
    let iov = word(16);
    let iovlen = word(24) as usize;
    if iovlen > 1024 {
        return Err(Errno::EMSGSIZE);
    }
    let mut iovecs = Vec::with_capacity(iovlen);
    for i in 0..iovlen as u64 {
        let v: [u8; 16] = get(cx, iov.wrapping_add(i * 16))?;
        iovecs.push((
            u64::from_le_bytes(v[..8].try_into().unwrap()),
            u64::from_le_bytes(v[8..].try_into().unwrap()) as usize,
        ));
    }
    Ok(MsgHdr {
        name,
        namelen,
        iovecs,
        control: word(32),
        controllen: word(40) as usize,
    })
}

pub(super) fn sendmsg(cx: &mut Cx<'_>, fd: i32, msg: u64, flags: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let MsgHdr {
        name,
        namelen,
        iovecs,
        control,
        controllen,
    } = read_msghdr(cx, msg)?;
    let mut data = Vec::new();
    for (base, len) in iovecs {
        let len = len.min(MSG_MAX - data.len());
        let start = data.len();
        data.resize(start + len, 0);
        cx.mm()
            .read(UserSlice::new(base, len), &mut data[start..])?;
    }
    let addr = if name != 0 && namelen > 0 {
        read_addr(cx, name, namelen)?
    } else {
        Vec::new()
    };
    let mut cmsg = vec![0u8; controllen.min(4096)];
    if !cmsg.is_empty() {
        cx.mm()
            .read(UserSlice::new(control, cmsg.len()), &mut cmsg)?;
    }
    send(cx, &sock, nonblock, &data, &addr, &cmsg, flags).map(|n| n as u64)
}

/// `struct mmsghdr` 的大小：msghdr 后面跟着 msg_len。
const MMSGHDR_SIZE: u64 = 64;
/// sendmmsg、recvmmsg 一次最多处理的消息个数（`UIO_MAXIOV`）。
const MMSG_MAX: u32 = 1024;
const MSG_WAITFORONE: u32 = 0x1_0000;

/// sendmmsg：逐个 sendmsg。第一个就失败时返回错误，否则返回发出的个数。
pub(super) fn sendmmsg(cx: &mut Cx<'_>, fd: i32, vec: u64, vlen: u32, flags: u32) -> SysResult {
    let mut done = 0;
    for i in 0..vlen.min(MMSG_MAX) as u64 {
        let hdr = vec + i * MMSGHDR_SIZE;
        match sendmsg(cx, fd, hdr, flags) {
            Ok(n) => put(cx, hdr + 56, (n as u32).to_le_bytes())?,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        done += 1;
    }
    Ok(done)
}

/// recvmmsg：逐个 recvmsg。`MSG_WAITFORONE` 时收到第一个之后不再等待；超时只在
/// 每收到一个之后检查，与 Linux 相同。
pub(super) fn recvmmsg(
    cx: &mut Cx<'_>,
    fd: i32,
    vec: u64,
    vlen: u32,
    flags: u32,
    timeout: u64,
) -> SysResult {
    let deadline = super::poll::deadline_ts(cx, timeout)?;
    let mut flags = flags;
    let mut done = 0;
    for i in 0..vlen.min(MMSG_MAX) as u64 {
        let hdr = vec + i * MMSGHDR_SIZE;
        match recvmsg(cx, fd, hdr, flags & !MSG_WAITFORONE) {
            Ok(n) => put(cx, hdr + 56, (n as u32).to_le_bytes())?,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        done += 1;
        if flags & MSG_WAITFORONE != 0 {
            flags |= MSG_DONTWAIT;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Ok(done)
}

pub(super) fn recvmsg(cx: &mut Cx<'_>, fd: i32, msg: u64, flags: u32) -> SysResult {
    let (sock, nonblock) = socket_of(cx, fd)?;
    let MsgHdr {
        name,
        namelen,
        iovecs,
        control,
        controllen,
    } = read_msghdr(cx, msg)?;
    let total: usize = iovecs.iter().map(|v| v.1).sum();
    let m = recv(cx, &sock, nonblock, total, controllen.min(4096), flags)?;
    // 数据按顺序分散到各个 iovec
    let mut off = 0;
    for (base, len) in iovecs {
        let n = len.min(m.data.len() - off);
        if n == 0 {
            break;
        }
        cx.mm()
            .write(UserSlice::new(base, n), &m.data[off..off + n])?;
        off += n;
    }
    // msg_namelen、msg_controllen、msg_flags 写回 msghdr
    if name != 0 {
        let n = (namelen as usize).min(m.addr.len());
        cx.mm().write(UserSlice::new(name, n), &m.addr[..n])?;
        put(cx, msg + 8, (m.addr.len() as u32).to_le_bytes())?;
    }
    let n = controllen.min(m.control.len());
    if n > 0 {
        cx.mm().write(UserSlice::new(control, n), &m.control[..n])?;
    }
    put(cx, msg + 40, (n as u64).to_le_bytes())?;
    // 与 Linux 一样，msg_flags 里回显调用方给的 MSG_CMSG_CLOEXEC
    const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;
    put(
        cx,
        msg + 48,
        (m.flags | flags & MSG_CMSG_CLOEXEC).to_le_bytes(),
    )?;
    Ok(if flags & MSG_TRUNC != 0 {
        m.len
    } else {
        m.data.len()
    } as u64)
}

/// socket 的 ioctl：FIONREAD，以及接口信息（`SIOCGIF*`）。
pub(super) fn ioctl(cx: &mut Cx<'_>, sock: &SocketRef, request: u32, arg: u64) -> SysResult {
    const FIONREAD: u32 = 0x541b;
    const SIOCATMARK: u32 = 0x8905;
    // 新的网络命名空间里的 socket：接口类 ioctl 只看到它的 lo（crate::netns）
    let net = match &*sock.borrow() {
        Socket::Inet(s) => s.net.clone(),
        Socket::Netlink(s) => s.net.clone(),
        Socket::Unix(u) => u.net.clone(),
    };
    if let Some(ns) = net
        && (0x8910..0x8960).contains(&request)
    {
        return netns_ioctl(cx, &ns, request, arg);
    }
    // 开机时的命名空间里的虚拟设备（crate::netdev）：按名字、编号认出来的不交给宿主
    if (0x8910..0x8960).contains(&request)
        && request != 0x8912
        && let Ok(mut ifr) = get::<40>(cx, arg)
    {
        let init = cx.kr().init_ns.net.clone();
        if let Some(r) = crate::netdev::ioctl(&init, &cx.cred(), request, &mut ifr) {
            r?;
            return put(cx, arg, ifr).map(|()| 0);
        }
    }
    let host_fd = match &*sock.borrow() {
        // DNS 存根的回答在我们自己的队列里，先于宿主 socket 收到的读出（recvmsg 也是这样）：
        // 数据报的 FIONREAD 是下一个数据报的长度。glibc 的解析器按它分配 AAAA 回答的缓冲区
        Socket::Inet(s) if request == FIONREAD && !s.answers.is_empty() => {
            let n = s.answers.front().map_or(0, Vec::len) as i32;
            return put(cx, arg, n.to_le_bytes()).map(|()| 0);
        }
        Socket::Inet(s) => Some(s.fd),
        Socket::Unix(u) if request == FIONREAD => {
            return put(cx, arg, (u.unread() as i32).to_le_bytes()).map(|()| 0);
        }
        Socket::Netlink(_) | Socket::Unix(_) => None,
    };
    match (request, host_fd) {
        // 先问宿主（UDP 是 EINVAL），再写回 guest（地址不对是 EFAULT）
        (SIOCATMARK, Some(fd)) => {
            let mut b = [0u8; 4];
            appine_hal::ioctl(fd, request as u64, &mut b).map_err(Errno)?;
            put(cx, arg, b).map(|()| 0)
        }
        (FIONREAD, Some(fd)) => {
            let mut b = [0u8; 4];
            appine_hal::ioctl(fd, request as u64, &mut b).map_err(Errno)?;
            put(cx, arg, b).map(|()| 0)
        }
        (FIONREAD, None) => Err(Errno::ENOTTY),
        (_, Some(fd)) => interface_ioctl(cx, fd, request, arg),
        // 与 Linux 的 dev_ioctl 一样，接口类 ioctl 不管 socket 的地址族（musl 的
        // if_nametoindex 用的就是 AF_UNIX socket）：借一个临时的宿主 socket
        (_, None) => {
            let fd = appine_hal::net::socket(AF_INET, SOCK_DGRAM, 0).map_err(Errno)?;
            let r = interface_ioctl(cx, fd, request, arg);
            appine_hal::net::close(fd);
            r
        }
    }
}

/// 新的网络命名空间里的接口 ioctl：虚拟设备（[`crate::netdev`]）和 lo。
fn netns_ioctl(
    cx: &mut Cx<'_>,
    ns: &Rc<crate::nsproxy::NetNs>,
    request: u32,
    arg: u64,
) -> SysResult {
    const SIOCGIFCONF: u32 = 0x8912;
    if request == SIOCGIFCONF {
        let conf: [u8; 16] = get(cx, arg)?;
        let len = i32::from_le_bytes(conf[..4].try_into().unwrap()).max(0) as usize;
        let buf = u64::from_le_bytes(conf[8..].try_into().unwrap());
        let mut all = crate::netns::ifconf(ns);
        all.extend(crate::netdev::ifconf(ns));
        // 缓冲区为空时只报告要多大；否则放得下几个整项写几个
        let n = if buf == 0 {
            all.len()
        } else {
            let n = (len / 40 * 40).min(all.len());
            cx.mm().write(UserSlice::new(buf, n), &all[..n])?;
            n
        };
        return put(cx, arg, (n as i32).to_le_bytes()).map(|()| 0);
    }
    let mut ifr: [u8; 40] = get(cx, arg)?;
    if let Some(r) = crate::netdev::ioctl(ns, &cx.cred(), request, &mut ifr) {
        r?;
        return put(cx, arg, ifr).map(|()| 0);
    }
    match crate::netns::interface_ioctl(ns, &cx.cred(), request, &mut ifr) {
        Some(r) => {
            r?;
            put(cx, arg, ifr).map(|()| 0)
        }
        // 别的修改（地址、MTU 等）不支持
        None if matches!(
            request,
            0x8916 | 0x8918 | 0x891a | 0x891c | 0x8923 | 0x8924 | 0x8943
        ) =>
        {
            Err(Errno::EPERM)
        }
        None => Err(Errno::ENOTTY),
    }
}

/// 接口信息的 ioctl，透传给宿主 socket `host_fd`。
fn interface_ioctl(cx: &mut Cx<'_>, host_fd: i32, request: u32, arg: u64) -> SysResult {
    const SIOCGIFCONF: u32 = 0x8912;
    const SIOCGIFFLAGS: u32 = 0x8913;
    const SIOCGIFHWADDR: u32 = 0x8927;
    const IFF_LOOPBACK: u16 = 0x8;
    const ARPHRD_ETHER: u16 = 1;
    const ARPHRD_LOOPBACK: u16 = 772;
    // struct ifreq（40 字节）进、ifreq 出：SIOCGIFNAME、FLAGS、ADDR、DSTADDR、BRDADDR、
    // NETMASK、MTU、HWADDR、INDEX、TXQLEN
    const IFREQ_GETTERS: [u32; 10] = [
        0x8910, 0x8913, 0x8915, 0x8917, 0x8919, 0x891b, 0x8921, 0x8927, 0x8933, 0x8942,
    ];
    match request {
        r if IFREQ_GETTERS.contains(&r) => {
            let mut b: [u8; 40] = get(cx, arg)?;
            appine_hal::ioctl(host_fd, r as u64, &mut b).map_err(Errno)?;
            if r == SIOCGIFHWADDR && b[16..18] == [0, 0] {
                // Android 不让 App 读硬件地址，地址族也是 0：按接口是不是回环补上
                let mut flags = b;
                appine_hal::ioctl(host_fd, SIOCGIFFLAGS as u64, &mut flags).map_err(Errno)?;
                let loopback = u16::from_le_bytes([flags[16], flags[17]]) & IFF_LOOPBACK != 0;
                let ty = if loopback {
                    ARPHRD_LOOPBACK
                } else {
                    ARPHRD_ETHER
                };
                b[16..18].copy_from_slice(&ty.to_le_bytes());
            }
            put(cx, arg, b).map(|()| 0)
        }
        // 修改接口（SIOCSIFFLAGS、ADDR、MTU 等）：App 改不了宿主的接口，与没有
        // CAP_NET_ADMIN 时一样是 EPERM，先读参数（地址不对是 EFAULT）
        0x8914 | 0x8916 | 0x8918 | 0x891a | 0x891c | 0x8922 | 0x8923 | 0x8924 | 0x8943 => {
            let _: [u8; 40] = get(cx, arg)?;
            Err(Errno::EPERM)
        }
        SIOCGIFCONF => {
            // struct ifconf：长度和指向 guest 缓冲区的指针，换成宿主缓冲区
            let conf: [u8; 16] = get(cx, arg)?;
            let len = i32::from_le_bytes(conf[..4].try_into().unwrap()).clamp(0, 1 << 16) as usize;
            let guest_buf = u64::from_le_bytes(conf[8..].try_into().unwrap());
            let mut host = vec![0u8; len];
            let mut req = [0u8; 16];
            req[..4].copy_from_slice(&(len as i32).to_le_bytes());
            let ptr = if guest_buf == 0 {
                0
            } else {
                host.as_mut_ptr() as u64
            };
            req[8..].copy_from_slice(&ptr.to_le_bytes());
            appine_hal::ioctl(host_fd, SIOCGIFCONF as u64, &mut req).map_err(Errno)?;
            let got = i32::from_le_bytes(req[..4].try_into().unwrap()).max(0) as usize;
            // 开机时的命名空间里的虚拟设备接在宿主的后面
            let extra = crate::netdev::ifconf(&cx.kr().init_ns.net);
            if guest_buf == 0 {
                return put(cx, arg, ((got + extra.len()) as i32).to_le_bytes()).map(|()| 0);
            }
            let got = got.min(len);
            cx.mm()
                .write(UserSlice::new(guest_buf, got), &host[..got])?;
            let n = ((len - got) / 40 * 40).min(extra.len());
            cx.mm()
                .write(UserSlice::new(guest_buf + got as u64, n), &extra[..n])?;
            put(cx, arg, ((got + n) as i32).to_le_bytes()).map(|()| 0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// 进程内的数据来过几次：[读方向, 写方向]（`syscall::poll` 的数据代数）。宿主 socket
/// 上的数据不经过内核层，由 [`crate::fd::HostEdges`] 记录。
pub(crate) fn arrivals(sock: &Socket) -> [u64; 2] {
    match sock {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => {
            [crate::rawicmp::queued(&r.tap, wake::of_value(sock)).1, 0]
        }
        Socket::Inet(s) => [s.answered, 0],
        Socket::Netlink(s) => [s.arrivals, 0],
        Socket::Unix(u) => u.arrivals(),
    }
}

/// ppoll、epoll 等 socket 时登记的唤醒对象（`poll::wake_objects`）：AF_UNIX 的 channel；
/// 宿主 socket 和 netlink 是 socket 自己。宿主 socket 上的数据由宿主 poll 等，关闭它
/// （等它的线程的宿主 poll 要放手）、DNS 存根和 netlink 的回答请求它。
pub(crate) fn wake_objects(sock: &RefCell<Socket>, out: &mut Vec<wake::Object>) {
    match &*sock.borrow() {
        Socket::Unix(u) => u.wake_objects(out),
        Socket::Inet(_) | Socket::Netlink(_) => out.push(wake::of(sock)),
    }
}

/// ppoll、epoll 用的就绪状态：(已经知道的事件, 需要问宿主的 fd)。
pub(crate) fn readiness(sock: &Socket) -> (i16, Option<i32>) {
    const POLLRDNORM: i16 = 0x040;
    const POLLWRNORM: i16 = 0x100;
    match sock {
        // raw ICMP socket 收到的在汇集点的队列里（crate::rawicmp），由定时器线程取下来
        Socket::Inet(s @ InetSocket { icmp: Some(r), .. }) => {
            let key = wake::of_value(sock);
            let ready = crate::rawicmp::queued(&r.tap, key).0;
            let read = if ready { POLLIN | POLLRDNORM } else { 0 };
            let errors = s.icmp_err.get() != 0 || crate::rawicmp::has_errors(&r.tap, key);
            let err = if errors { POLLERR } else { 0 };
            (read | err | POLLOUT | POLLWRNORM, None)
        }
        // 汇集点看着的 UDP socket：宿主替它记下的 ICMP 错误让宿主 poll 报告 POLLERR，先取走，
        // 要报告给 guest 的另外记着
        Socket::Inet(s) if s.udp_tap.is_some() => {
            if let Some(t) = &s.udp_tap {
                let _ = crate::rawicmp::dispatch(t, Some(s.fd));
            }
            let err = if s.icmp_err.get() != 0 { POLLERR } else { 0 };
            let stub = if s.answers.is_empty() {
                0
            } else {
                POLLIN | POLLOUT
            };
            (err | stub, Some(s.fd))
        }
        Socket::Inet(s) if !s.answers.is_empty() => (POLLIN | POLLOUT, None),
        Socket::Inet(s) => (0, Some(s.fd)),
        Socket::Netlink(s) => (POLLOUT | if s.queue.is_empty() { 0 } else { POLLIN }, None),
        Socket::Unix(u) => (u.readiness(), None),
    }
}
