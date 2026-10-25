//! socket 类 syscall（设计文档 §10）。
//!
//! 宿主 socket 是非阻塞的；guest 的阻塞调用在 EAGAIN 时经由 [`Cx::wait`] 等宿主 fd
//! 就绪，期间释放 BKL，可以被信号打断，并按 `SO_RCVTIMEO`/`SO_SNDTIMEO` 超时。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{SysResult, get, put, unix};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, request_wake};
use crate::net::{
    AF_INET, AF_INET6, AF_NETLINK, AF_UNIX, InetSocket, NetlinkSocket, SOCK_DGRAM, SOCK_RAW,
    SOCK_STREAM, Socket, is_stub, servfail, stub_addr,
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
const IPPROTO_UDP: i32 = 17;
const IPPROTO_ICMPV6: i32 = 58;
const IPPROTO_UDPLITE: i32 = 136;
const IPPROTO_MPTCP: i32 = 262;

const SOL_SOCKET: i32 = 1;
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

/// 等宿主 socket 就绪：非阻塞时直接 EAGAIN，超时时返回 `timeout_err`。
fn wait_host(
    cx: &mut Cx<'_>,
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
    cx.wait(deadline, &mut pfd)
}

pub(super) fn socket(cx: &mut Cx<'_>, domain: i32, ty: u32, protocol: i32) -> SysResult {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let base = (ty & SOCK_TYPE_MASK) as i32;
    let sock = match domain {
        AF_INET | AF_INET6 => {
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
            Socket::Inet(InetSocket::new(fd, domain, host_ty))
        }
        AF_NETLINK if base == SOCK_RAW || base == SOCK_DGRAM => {
            Socket::Netlink(NetlinkSocket::new(protocol))
        }
        AF_NETLINK => return Err(Errno::ESOCKTNOSUPPORT),
        AF_UNIX => unix::socket(base, protocol)?,
        _ => return Err(Errno::EAFNOSUPPORT),
    };
    install(cx, Rc::new(RefCell::new(sock)), ty).map(|fd| fd as u64)
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
    // 1024 以下的端口要求 CAP_NET_BIND_SERVICE
    let family = addr
        .get(..2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as i32);
    if matches!(family, Some(AF_INET | AF_INET6)) && addr.len() >= 4 {
        let port = u16::from_be_bytes([addr[2], addr[3]]);
        if port != 0 && port < 1024 && !cx.cred().capable(crate::cred::cap::NET_BIND_SERVICE) {
            return Err(Errno::EACCES);
        }
    }
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => appine_hal::net::bind(s.fd, &addr).map_err(Errno)?,
        Socket::Netlink(s) => {
            let pid = addr
                .get(4..8)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            s.pid = if pid == 0 { cx.tgid() as u32 } else { pid };
        }
        Socket::Unix(_) => unreachable!("handled above"),
    }
    Ok(0)
}

pub(super) fn listen(cx: &mut Cx<'_>, fd: i32, backlog: i32) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => appine_hal::net::listen(s.fd, backlog).map_err(Errno)?,
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
    let (host_fd, family, ty, timeout) = match &*sock.borrow() {
        Socket::Inet(s) => (s.fd, s.family, s.ty, s.rcvtimeo),
        Socket::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        Socket::Unix(_) => unreachable!("handled by accept4"),
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match appine_hal::net::accept(host_fd) {
            Ok((new, peer)) => {
                let new = Socket::Inet(InetSocket::new(new, family, ty));
                return Ok((Rc::new(RefCell::new(new)), peer));
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                wait_host(cx, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)?;
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
    let host_fd = match &mut *sock.borrow_mut() {
        Socket::Inet(s) => {
            // UDP 连到 DNS 存根：只记下来，之后的 send 由存根处理
            if s.ty == SOCK_DGRAM {
                s.stub_peer = is_stub(&addr);
                if s.stub_peer {
                    return Ok(0);
                }
            }
            s.fd
        }
        Socket::Netlink(_) => return Ok(0),
        Socket::Unix(_) => unreachable!("handled above"),
    };
    match appine_hal::net::connect(host_fd, &addr) {
        Ok(()) => return Ok(0),
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
        cx.wait(None, &mut pfd)?;
    }
    let err = appine_hal::net::getsockopt(host_fd, SOL_SOCKET, SO_ERROR, 4).map_err(Errno)?;
    match i32::from_le_bytes(err[..4].try_into().unwrap()) {
        // 宿主 socket 在套接字层还是"正在连接"，再 connect 一次才变成"已连接"：不然
        // guest 之后再 connect 会成功，而 Linux 上阻塞的 connect 返回之后再 connect 是
        // EISCONN
        0 => {
            let _ = appine_hal::net::connect(host_fd, &addr);
            Ok(0)
        }
        e => Err(Errno(e)),
    }
}

fn sockname(cx: &mut Cx<'_>, fd: i32, addr: u64, addrlen: u64, peer: bool) -> SysResult {
    let (sock, _) = socket_of(cx, fd)?;
    let value = match &*sock.borrow() {
        Socket::Inet(s) if peer && s.stub_peer => stub_addr(s.family),
        Socket::Inet(s) => appine_hal::net::sockname(s.fd, peer).map_err(Errno)?,
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
    match &mut *sock.borrow_mut() {
        Socket::Inet(s) => match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO) => s.rcvtimeo = timeval(&v),
            (SOL_SOCKET, SO_SNDTIMEO) => s.sndtimeo = timeval(&v),
            _ => appine_hal::net::setsockopt(s.fd, level, name, &v).map_err(Errno)?,
        },
        // netlink 的缓冲区大小、组播等选项都接受，但不起作用
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
    let out = match &*sock.borrow() {
        Socket::Inet(s) => match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO) => encode_tv(s.rcvtimeo),
            (SOL_SOCKET, SO_SNDTIMEO) => encode_tv(s.sndtimeo),
            _ => appine_hal::net::getsockopt(s.fd, level, name, room.min(256)).map_err(Errno)?,
        },
        Socket::Netlink(_) => 0i32.to_le_bytes().to_vec(),
        Socket::Unix(u) => unix::getsockopt(u, level, name)?,
    };
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
            let to_stub = s.ty == SOCK_DGRAM && (is_stub(addr) || (addr.is_empty() && s.stub_peer));
            (s.fd, s.ty, s.sndtimeo, to_stub)
        }
        Socket::Netlink(s) => {
            s.handle(data, appine_hal::net::interfaces);
            request_wake();
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
        }
        request_wake();
        return Ok(data.len());
    }
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut done = 0;
    loop {
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
                // 流式 socket 的阻塞发送要把数据全部发出去
                if ty != SOCK_STREAM || done == data.len() || nonblock {
                    return Ok(done);
                }
            }
            Err(e) if e == Errno::EAGAIN.0 => {
                if let Err(err) = wait_host(cx, host_fd, POLLOUT, nonblock, deadline, Errno::EAGAIN)
                {
                    return if done > 0 { Ok(done) } else { Err(err) };
                }
            }
            Err(e) if e == Errno::EPIPE.0 => return Err(epipe(cx, flags)),
            Err(e) => return if done > 0 { Ok(done) } else { Err(Errno(e)) },
        }
    }
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
                None
            }
            Socket::Unix(_) => unreachable!("handled above"),
        };
        match host {
            Some(h) => break h,
            // netlink 没有回答可读：除非被信号打断，否则一直等
            None => {
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                if cx.interrupted() {
                    return Err(Errno::ERESTARTSYS);
                }
                cx.wait(None, &mut [])?;
            }
        }
    };
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut buf = vec![0u8; max];
    let mut got = 0;
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
                return Ok(Message {
                    data: buf,
                    len,
                    addr: r.addr,
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
                    return Ok(Message {
                        len: got,
                        data: buf,
                        addr: Vec::new(),
                        control: Vec::new(),
                        flags: 0,
                    });
                }
                wait_host(cx, host_fd, POLLIN, nonblock, deadline, Errno::EAGAIN)?;
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
    let host_fd = match &*sock.borrow() {
        Socket::Inet(s) => Some(s.fd),
        Socket::Unix(u) if request == FIONREAD => {
            return put(cx, arg, (u.unread() as i32).to_le_bytes()).map(|()| 0);
        }
        Socket::Netlink(_) | Socket::Unix(_) => None,
    };
    const SIOCATMARK: u32 = 0x8905;
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

/// ppoll、epoll 用的就绪状态：(已经知道的事件, 需要问宿主的 fd)。
pub(crate) fn readiness(sock: &Socket) -> (i16, Option<i32>) {
    match sock {
        Socket::Inet(s) if !s.answers.is_empty() => (POLLIN | POLLOUT, None),
        Socket::Inet(s) => (0, Some(s.fd)),
        Socket::Netlink(s) => (POLLOUT | if s.queue.is_empty() { 0 } else { POLLIN }, None),
        Socket::Unix(u) => (u.readiness(), None),
    }
}
