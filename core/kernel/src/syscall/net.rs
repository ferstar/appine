//! socket 类 syscall（设计文档 §10）。
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
    NETLINK_ROUTE, NetlinkSocket, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM, Socket, is_stub, servfail,
    stub_addr,
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
            // （Linux 上总是有它）；NETLINK_KOBJECT_UEVENT 只用来听热插拔事件（mdev、udev），
            // 这里不会有。别的协议与 Linux 上没有编进内核的子系统一样是 EPROTONOSUPPORT：
            // 原来照样建出来、收到请求也不回答，libaudit 等 ACK 等 40 秒之后失败，PAM 因此
            // 拒绝登录（Debian 的 sshd、login）
            if !matches!(
                protocol,
                NETLINK_ROUTE | NETLINK_GENERIC | NETLINK_KOBJECT_UEVENT
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

/// 新的网络命名空间里还没有绑定的宿主 socket：先绑到这个命名空间的回环地址（Linux 自动
/// 绑定时绑的是命名空间里的通配地址，宿主上绑通配地址就到了所有接口上）。
fn ensure_bound(s: &mut InetSocket) -> Result<(), Errno> {
    if let Some(ns) = &s.net
        && !s.bound
        && let Some(addr) = crate::netns::implicit_bind(ns, s.family, s.icmp6())
    {
        appine_hal::net::bind(s.fd, &addr).map_err(Errno)?;
        s.bound = true;
        s.any = true;
    }
    Ok(())
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
    let host_addr = match &net {
        Some(ns) => crate::netns::to_host(ns, &addr, crate::netns::Use::Bind, icmp6)?,
        None => addr.clone(),
    };
    // 1024 以下的端口要求网络命名空间的属主里的 CAP_NET_BIND_SERVICE
    let family = addr
        .get(..2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as i32);
    if matches!(family, Some(AF_INET | AF_INET6)) && addr.len() >= 4 {
        let port = u16::from_be_bytes([addr[2], addr[3]]);
        let owner = net
            .as_ref()
            .map_or_else(crate::cred::UserNs::init, |n| n.owner.clone());
        if port != 0
            && port < 1024
            && !cx
                .cred()
                .ns_capable(&owner, crate::cred::cap::NET_BIND_SERVICE)
        {
            return Err(Errno::EACCES);
        }
    }
    let map = cx.k().portmap.clone();
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 流量算在哪个接口上重新认（connect 也一样）
            s.route = None;
            // 1024 以下的端口在宿主上换一个（crate::portmap）
            match port_of(&host_addr).filter(|&p| mappable(s) && map.borrow().maps(p)) {
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
                        crate::portmap::bind(&map, ns_key(s), s.proto, port, try_bind, bound)?;
                    if new {
                        let proto = if s.proto == IPPROTO_TCP { "tcp" } else { "udp" };
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
    Ok(0)
}

pub(super) fn listen(cx: &mut Cx<'_>, fd: i32, backlog: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            ensure_bound(s)?;
            appine_hal::net::listen(s.fd, backlog).map_err(Errno)?;
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
    let (host_fd, family, ty, proto, timeout, net, local_map) = match &*sock.borrow() {
        Socket::Inet(s) => (
            s.fd,
            s.family,
            s.ty,
            s.proto,
            s.rcvtimeo,
            s.net.clone(),
            s.local_map,
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
                if proto == IPPROTO_TCP && net.is_none() {
                    cx.k().traffic.protocols.tcp_passive_opens += 1;
                }
                let peer = unmap_addr(&cx.kr().portmap, &new, peer, false);
                let peer = match &net {
                    Some(ns) => crate::netns::from_host(ns, &peer, false, false),
                    None => peer,
                };
                new.bound = net.is_some();
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
    let (host_fd, tcp, host_addr) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 本端地址可能变了：流量算在哪个接口上重新认
            s.route = None;
            // 新的网络命名空间里只到得了 lo（crate::netns）；DNS 存根也不在里面
            if let Some(ns) = s.net.clone() {
                let host_addr =
                    crate::netns::to_host(&ns, &addr, crate::netns::Use::Reach, s.icmp6())?;
                ensure_bound(s)?;
                // 本机上映射了的低端口换成宿主上的（crate::portmap）
                let (host_addr, pm) = map_dest(&map, s, host_addr);
                s.peer_map = pm;
                // 它的流量不算进宿主的接口（/proc/net/snmp 也不数）
                (s.fd, false, host_addr)
            } else {
                // UDP 连到 DNS 存根：只记下来，之后的 send 由存根处理
                if s.ty == SOCK_DGRAM {
                    s.stub_peer = is_stub(&addr);
                    if s.stub_peer {
                        return Ok(0);
                    }
                }
                let (host_addr, pm) = map_dest(&map, s, addr.clone());
                s.peer_map = pm;
                (
                    s.fd,
                    s.ty == SOCK_STREAM && s.proto == IPPROTO_TCP,
                    host_addr,
                )
            }
        }
        Socket::Netlink(_) => return Ok(0),
        Socket::Unix(_) => unreachable!("handled above"),
    };
    let r = appine_hal::net::connect(host_fd, &host_addr);
    if tcp {
        count_connect(cx, &sock, r.err().map(Errno));
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
        Socket::Inet(s) => {
            let mut a = appine_hal::net::sockname(s.fd, peer).map_err(Errno)?;
            // raw socket 的本端"端口"是协议号（Linux 的 inet_create 设的 inet_sport），宿主的
            // ping socket 报告的是它的编号
            if s.icmp.is_some() && !peer && a.len() >= 4 {
                a[2..4].copy_from_slice(&(s.proto as u16).to_be_bytes());
            }
            let a = unmap_addr(&cx.kr().portmap, s, a, !peer);
            match &s.net {
                Some(ns) => crate::netns::from_host(ns, &a, !peer && s.any, s.icmp6()),
                None => a,
            }
        }
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
}

impl crate::net::Host for KernelHost<'_> {
    fn interfaces(&self) -> Vec<appine_hal::net::Interface> {
        match &self.net {
            Some(ns) => vec![crate::netns::lo_interface(ns)],
            None => appine_hal::net::interfaces(),
        }
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

    fn configure(&self, req: &[u8]) -> Option<i32> {
        match &self.net {
            Some(ns) => crate::netns::configure(ns, &self.cred, req),
            None => crate::net::configure_host_link(
                &appine_hal::net::interfaces(),
                self.cred.capable(crate::cred::cap::NET_ADMIN),
                req,
            ),
        }
    }

    /// 进程要还没退出（Linux 的 get_net_ns_by_pid），fd 要是网络命名空间的文件
    /// （get_net_ns_by_fd）。
    fn peer_net(&self, attr: u16, value: u32) -> Result<(), i32> {
        if attr == crate::net::NETNSA_PID {
            let tid = self.pid_ns.find(value as i32).ok_or(Errno::ESRCH.0)?;
            if !self.k.tasks.contains_key(&tid) {
                return Err(Errno::ESRCH.0);
            }
            return Ok(());
        }
        let file = self.fds.borrow().get(value as i32).map_err(|e| e.0)?;
        match &file.borrow().kind {
            FileKind::Proc {
                ns: Some(crate::nsproxy::NsRef::Net(_)),
                ..
            } => Ok(()),
            _ => Err(Errno::EINVAL.0),
        }
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
            };
            s.handle(data, &host);
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
    // 新的网络命名空间里：目的地址换成宿主的（到不了时 ENETUNREACH），没绑定的先绑上
    let host_addr = match &mut *sock.borrow_mut() {
        Socket::Inet(s) if s.net.is_some() && ty != SOCK_STREAM => {
            let ns = s.net.clone().unwrap();
            let a = if addr.is_empty() {
                Vec::new()
            } else {
                crate::netns::to_host(&ns, addr, crate::netns::Use::Reach, s.icmp6())?
            };
            ensure_bound(s)?;
            Some(a)
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
            let (a, pm) = map_dest(&cx.kr().portmap, s, addr.to_vec());
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
    r
}

/// 两个 IPv4、IPv6 地址的协议族、端口和地址都相同。
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
    !s.raw && matches!(s.proto, IPPROTO_TCP | IPPROTO_UDP)
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

/// 要连、要发往宿主地址 `addr`：本机上映射了的低端口换成宿主端口（[`crate::portmap`]）。
/// 返回换过的地址和 (guest 端口, 宿主端口)。guest 里没有程序绑着这个端口时不换：手机上
/// 别的 App 恰好占着宿主端口时连过去就错了，应该与原来一样连不上。
fn map_dest(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    s: &InetSocket,
    mut addr: Vec<u8>,
) -> (Vec<u8>, Option<(u16, u16)>) {
    let Some(port) = port_of(&addr) else {
        return (addr, None);
    };
    let host = {
        let m = map.borrow();
        if !mappable(s) || !m.maps(port) {
            return (addr, None);
        }
        m.host_port(ns_key(s), s.proto, port)
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
/// 对端用低端口绑定过）。
pub(crate) fn unmap_addr(
    map: &crate::cell::RefCell<crate::portmap::PortMap>,
    s: &InetSocket,
    mut addr: Vec<u8>,
    local: bool,
) -> Vec<u8> {
    let Some(port) = port_of(&addr) else {
        return addr;
    };
    let known = if local { s.local_map } else { s.peer_map };
    let guest = match known {
        Some((g, h)) if h == port => Some(g),
        _ if local || !mappable(s) => None,
        _ => map
            .borrow()
            .guest_port(ns_key(s), s.proto, port)
            .filter(|_| is_local(&addr)),
    };
    if let Some(g) = guest {
        set_port(&mut addr, g);
    }
    addr
}

/// 宿主上的本端 IPv4 地址换成 guest 看到的（新的网络命名空间里要换算，[`crate::netns`]）。
fn local_addr(s: &InetSocket, a: [u8; 4]) -> [u8; 4] {
    let Some(ns) = &s.net else {
        return a;
    };
    let mut sa = vec![0u8; 16];
    sa[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[4..8].copy_from_slice(&a);
    let g = crate::netns::from_host(ns, &sa, false, false);
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
                        let a = unmap_addr(&cx.kr().portmap, s, r.addr, false);
                        match &s.net {
                            Some(ns) => crate::netns::from_host(ns, &a, false, s.icmp6()),
                            None => a,
                        }
                    }
                    _ => r.addr,
                };
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
        let addr = match &s.net {
            Some(ns) => crate::netns::from_host(ns, &e.addr, false, s.icmp6()),
            None => e.addr.clone(),
        };
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
            let src = match &s.net {
                Some(ns) => crate::netns::from_host(ns, &it.src, false, s.icmp6()),
                None => it.src.clone(),
            };
            let (mut data, control) = raw.present(&it, key, &src, |a| local_addr(s, a));
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
    let host_fd = match &*sock.borrow() {
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

/// 新的网络命名空间里的接口 ioctl：只有 lo。
fn netns_ioctl(cx: &mut Cx<'_>, ns: &crate::nsproxy::NetNs, request: u32, arg: u64) -> SysResult {
    const SIOCGIFCONF: u32 = 0x8912;
    if request == SIOCGIFCONF {
        let conf: [u8; 16] = get(cx, arg)?;
        let len = i32::from_le_bytes(conf[..4].try_into().unwrap()).max(0) as usize;
        let buf = u64::from_le_bytes(conf[8..].try_into().unwrap());
        let all = crate::netns::ifconf(ns);
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
            if guest_buf != 0 {
                cx.mm().write(
                    UserSlice::new(guest_buf, got.min(len)),
                    &host[..got.min(len)],
                )?;
            }
            put(cx, arg, (got as i32).to_le_bytes()).map(|()| 0)
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
