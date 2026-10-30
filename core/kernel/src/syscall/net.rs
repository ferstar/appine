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

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;

/// sockaddr 的最大长度（`sockaddr_storage`）。
const SOCKADDR_MAX: usize = 128;
/// 一次收发的最大字节数。
const MSG_MAX: usize = 1 << 20;
/// 一个 ICMP 报文最长的长度（IP 的总长度是 16 位）。
const RAW_ICMP_MAX: usize = 1 << 16;

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
            if base == SOCK_RAW
                && (1..256).contains(&protocol)
                && !cx.cred().ns_capable(&owner, crate::cred::cap::NET_RAW)
            {
                return Err(Errno::EPERM);
            }
            // guest 的 raw ICMP socket 换成不需要特权的 ping socket
            let icmp = matches!(
                (domain, protocol),
                (AF_INET, IPPROTO_ICMP) | (AF_INET6, IPPROTO_ICMPV6)
            );
            let host_ty = if base == SOCK_RAW && icmp {
                SOCK_DGRAM
            } else {
                base
            };
            let fd = appine_hal::net::socket(domain, host_ty, protocol).map_err(Errno)?;
            let mut s = InetSocket::new(fd, domain, host_ty);
            s.raw = base == SOCK_RAW;
            if s.raw && icmp {
                let r = crate::net::RawIcmp::new(domain);
                for &(level, name) in r.host_options() {
                    let _ = appine_hal::net::setsockopt(fd, level, name, &1i32.to_le_bytes());
                }
                s.icmp = Some(r);
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
    install(cx, Rc::new(RefCell::new(sock)), ty).map(|fd| fd as u64)
}

/// 新的网络命名空间里还没有绑定的宿主 socket：先绑到这个命名空间的回环地址（Linux 自动
/// 绑定时绑的是命名空间里的通配地址，宿主上绑通配地址就到了所有接口上）。
fn ensure_bound(s: &mut InetSocket) -> Result<(), Errno> {
    if let Some(ns) = &s.net
        && !s.bound
        && let Some(addr) = crate::netns::implicit_bind(ns, s.family)
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
    let net = match &*sock.borrow() {
        Socket::Inet(s) => s.net.clone(),
        _ => None,
    };
    let host_addr = match &net {
        Some(ns) => crate::netns::to_host(ns, &addr, crate::netns::Use::Bind)?,
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
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 流量算在哪个接口上重新认（connect 也一样）
            s.route = None;
            appine_hal::net::bind(s.fd, &host_addr).map_err(Errno)?;
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
            appine_hal::net::listen(s.fd, backlog).map_err(Errno)?
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
    let (host_fd, family, ty, proto, timeout, net) = match &*sock.borrow() {
        Socket::Inet(s) => (s.fd, s.family, s.ty, s.proto, s.rcvtimeo, s.net.clone()),
        Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        Socket::Unix(_) => unreachable!("handled by accept4"),
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match appine_hal::net::accept(host_fd) {
            Ok((new, peer)) => {
                let mut new = InetSocket::new(new, family, ty);
                new.proto = proto;
                if proto == IPPROTO_TCP && net.is_none() {
                    cx.k().traffic.protocols.tcp_passive_opens += 1;
                }
                let peer = match &net {
                    Some(ns) => crate::netns::from_host(ns, &peer, false),
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
    let (host_fd, tcp, host_addr) = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // 本端地址可能变了：流量算在哪个接口上重新认
            s.route = None;
            // 新的网络命名空间里只到得了 lo（crate::netns）；DNS 存根也不在里面
            if let Some(ns) = s.net.clone() {
                let host_addr = crate::netns::to_host(&ns, &addr, crate::netns::Use::Reach)?;
                ensure_bound(s)?;
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
                (
                    s.fd,
                    s.ty == SOCK_STREAM && s.proto == IPPROTO_TCP,
                    addr.clone(),
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
        Socket::Inet(s) => {
            let a = appine_hal::net::sockname(s.fd, peer).map_err(Errno)?;
            match &s.net {
                Some(ns) => crate::netns::from_host(ns, &a, !peer && s.any),
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
    // raw ICMP 自己的选项（crate::net::RawIcmp）
    if let Socket::Inet(InetSocket { icmp: Some(r), .. }) = &mut *sock.borrow_mut()
        && let Some(res) = r.setsockopt(level, name, &v)
    {
        return res.map(|()| 0).map_err(Errno);
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
    // raw ICMP 自己的选项（crate::net::RawIcmp）
    let raw = match &*sock.borrow() {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => r.getsockopt(level, name),
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
            Socket::Unix(u) => unix::getsockopt(u, level, name, &pid_ns)?,
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
        crate::netns::configure(self.net.as_ref()?, &self.cred, req)
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
                crate::netns::to_host(&ns, addr, crate::netns::Use::Reach)?
            };
            ensure_bound(s)?;
            Some(a)
        }
        _ => None,
    };
    let addr = host_addr.as_deref().unwrap_or(addr);
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
            Err(e) => break if done > 0 { Ok(done) } else { Err(Errno(e)) },
        }
    };
    if done > 0 || sent_datagram {
        account(cx, sock, false, done, addr);
    }
    r
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

/// 宿主上的本端 IPv4 地址换成 guest 看到的（新的网络命名空间里要换算，[`crate::netns`]）。
fn local_addr(s: &InetSocket, a: [u8; 4]) -> [u8; 4] {
    let Some(ns) = &s.net else {
        return a;
    };
    let mut sa = vec![0u8; 16];
    sa[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[4..8].copy_from_slice(&a);
    let g = crate::netns::from_host(ns, &sa, false);
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
    let (host_fd, ty, timeout, raw) = loop {
        let host = match &mut *sock.borrow_mut() {
            Socket::Inet(s) => {
                let addr = stub_addr(s.family);
                if let Some(m) = local(&mut s.answers, addr) {
                    return Ok(m);
                }
                // raw ICMP：宿主接收时多留控制消息的空间，错误队列里的原样交出
                let raw = s.icmp.as_ref().map(|r| r.control_room());
                let raw = raw.filter(|_| flags & MSG_ERRQUEUE == 0);
                Some((s.fd, s.ty, s.rcvtimeo, raw))
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
    let deadline = timeout.map(|t| Instant::now() + t);
    // raw ICMP 要看到 ICMP 头（类型、id）才改得了，还要知道完整的长度（IP 头里的总长度、
    // MSG_TRUNC），ping socket 的 recvmsg 又不认 MSG_TRUNC 标志：整个收下来，拼上 IP 头
    // 之后再截
    let room = if raw.is_some() { RAW_ICMP_MAX } else { max };
    let mut buf = vec![0u8; room];
    let mut got = 0;
    // 偷看的数据之后还会再读到，错误队列里不是收到的流量
    let counted = flags & (MSG_PEEK | MSG_ERRQUEUE) == 0;
    loop {
        let host_flags = flags & !(MSG_DONTWAIT | MSG_WAITALL);
        let host_control = control_len + raw.unwrap_or(0);
        match appine_hal::net::recvmsg(host_fd, &mut buf[got..], host_control, host_flags as i32) {
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
                    Socket::Inet(InetSocket { net: Some(ns), .. }) if !r.addr.is_empty() => {
                        crate::netns::from_host(ns, &r.addr, false)
                    }
                    _ => r.addr,
                };
                if raw.is_some() {
                    let delivered = match &*sock.borrow() {
                        Socket::Inet(s @ InetSocket { icmp: Some(i), .. }) => {
                            let dst = |a: [u8; 4]| local_addr(s, a);
                            i.deliver(buf, len, &addr, &r.control, dst)
                        }
                        _ => unreachable!("raw ICMP socket"),
                    };
                    let Some((mut data, len, control)) = delivered else {
                        // 过滤器丢掉的：偷看的也要真的读走，不然一直偷看到它
                        if flags & MSG_PEEK != 0 {
                            let _ =
                                appine_hal::net::recvmsg(host_fd, &mut [], 0, MSG_DONTWAIT as i32);
                        }
                        buf = vec![0u8; room];
                        got = 0;
                        continue;
                    };
                    data.truncate(max);
                    let (control, ctrunc) = crate::net::fit_control(&control, control_len);
                    let mut flags = r.flags as u32 & !MSG_TRUNC | ctrunc;
                    if len > max {
                        flags |= MSG_TRUNC;
                    }
                    return Ok(Message {
                        data,
                        len,
                        addr,
                        control,
                        flags,
                    });
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
            Err(e) => return Err(Errno(e)),
        }
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
    match sock {
        Socket::Inet(s) if !s.answers.is_empty() => (POLLIN | POLLOUT, None),
        Socket::Inet(s) => (0, Some(s.fd)),
        Socket::Netlink(s) => (POLLOUT | if s.queue.is_empty() { 0 } else { POLLIN }, None),
        Socket::Unix(u) => (u.readiness(), None),
    }
}
