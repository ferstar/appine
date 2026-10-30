//! AF_UNIX socket 的 syscall（数据结构在 [`crate::unix`]）。
//!
//! 由 [`super::net`] 在发现 socket 是 AF_UNIX 时转过来。阻塞的操作经由
//! [`Cx::wait_objects`] 等其他线程改变 channel 的状态（它们都会请求唤醒那个 channel，
//! 见 [`UnixSocket::wake_objects`]），可以被信号打断，并按 `SO_RCVTIMEO`/`SO_SNDTIMEO`
//! 超时。

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::fs::{AT_FDCWD, lookup_at, parent_at};
use super::net::{Message, SocketRef, epipe};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::FileRef;
use crate::kernel::{Cx, request_wake_object, wake};
use crate::net::{AF_UNIX, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM, Socket};
use crate::unix::{
    CAPACITY, ChannelRef, Link, Name, Packet, Received, SOCK_SEQPACKET, SOCKADDR_UN_LEN, Ucred,
    UnixSocket, unnamed,
};
use crate::vfs::Node;
use crate::vfs::store::mode;

const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_PASSCRED: i32 = 16;
const SO_PEERCRED: i32 = 17;
const SO_RCVTIMEO: i32 = 20;
const SO_SNDTIMEO: i32 = 21;
const SO_ACCEPTCONN: i32 = 30;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;

const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
/// 一条消息最多传递的打开文件数（Linux 的 `SCM_MAX_FD`）。
const SCM_MAX_FD: usize = 253;

const MSG_PEEK: u32 = 0x2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_WAITALL: u32 = 0x100;
const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

/// `cmsghdr` 的长度（64 位）。
const CMSG_HDR: usize = 16;
/// listen 的 backlog 上限（`SOMAXCONN`）。
const SOMAXCONN: usize = 4096;

/// 检查 socket 类型和协议，返回实际的类型。
fn socket_type(ty: i32, protocol: i32) -> Result<i32, Errno> {
    if protocol != 0 && protocol != AF_UNIX {
        return Err(Errno::EPROTONOSUPPORT);
    }
    match ty {
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET => Ok(ty),
        // 与 Linux 相同，SOCK_RAW 当作数据报
        SOCK_RAW => Ok(SOCK_DGRAM),
        _ => Err(Errno::ESOCKTNOSUPPORT),
    }
}

/// 新建一个 AF_UNIX socket。
pub(super) fn socket(ty: i32, protocol: i32) -> Result<Socket, Errno> {
    Ok(Socket::Unix(UnixSocket::new(socket_type(ty, protocol)?)))
}

/// socketpair(2)：两个互相连接的 socket。
pub(super) fn pair(cx: &Cx<'_>, ty: i32, protocol: i32) -> Result<(Socket, Socket), Errno> {
    let (a, b) = UnixSocket::pair(socket_type(ty, protocol)?, cx.ucred());
    Ok((Socket::Unix(a), Socket::Unix(b)))
}

/// 借用 AF_UNIX socket 的内部状态。
fn with<R>(sock: &SocketRef, f: impl FnOnce(&mut UnixSocket) -> R) -> R {
    match &mut *sock.borrow_mut() {
        Socket::Unix(u) => f(u),
        _ => unreachable!("not an AF_UNIX socket"),
    }
}

/// guest 给出的地址。
enum Addr<'a> {
    /// 只有地址族：bind 时自动绑定。
    Unnamed,
    /// 路径（不含结尾的 NUL）。
    Path(&'a [u8]),
    /// 抽象地址：`sun_path` 的全部字节。
    Abstract(&'a [u8]),
}

fn parse(addr: &[u8]) -> Result<Addr<'_>, Errno> {
    if addr.len() < 2 || addr.len() > SOCKADDR_UN_LEN {
        return Err(Errno::EINVAL);
    }
    if u16::from_le_bytes([addr[0], addr[1]]) as i32 != AF_UNIX {
        return Err(Errno::EINVAL);
    }
    let path = &addr[2..];
    Ok(match path.first() {
        None => Addr::Unnamed,
        Some(0) => Addr::Abstract(path),
        Some(_) => Addr::Path(&path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())]),
    })
}

/// 按地址找到绑定在上面的 socket。
fn find(cx: &Cx<'_>, addr: &[u8]) -> Result<SocketRef, Errno> {
    let name = match parse(addr)? {
        Addr::Unnamed => return Err(Errno::EINVAL),
        Addr::Abstract(p) => Name::Abstract(p.to_vec()),
        Addr::Path(p) => {
            let node = lookup_at(cx, AT_FDCWD, p, true)?;
            let vfs = cx.vfs()?;
            match node {
                Node::Store(_, ino) if vfs.kind(&cx.view(&vfs), &node)? == mode::S_IFSOCK => {
                    // 连接路径地址要求对 socket 文件有写权限
                    cx.cred()
                        .permission(&vfs.store().stat(ino)?, crate::cred::MAY_WRITE)?;
                    Name::Path(ino)
                }
                _ => return Err(Errno::ECONNREFUSED),
            }
        }
    };
    cx.kr().unix.lookup(&name).ok_or(Errno::ECONNREFUSED)
}

pub(super) fn bind(cx: &mut Cx<'_>, sock: &SocketRef, addr: &[u8]) -> Result<(), Errno> {
    if with(sock, |u| !u.addr.is_empty()) {
        return Err(Errno::EINVAL);
    }
    let mut bound = unnamed();
    match parse(addr)? {
        Addr::Unnamed => {
            let path = cx.k().unix.autobind(sock).ok_or(Errno::EADDRINUSE)?;
            bound.extend_from_slice(&path);
        }
        Addr::Abstract(p) => {
            if !cx.k().unix.insert(Name::Abstract(p.to_vec()), sock) {
                return Err(Errno::EADDRINUSE);
            }
            bound.extend_from_slice(p);
        }
        Addr::Path(p) => {
            // 与 Linux 相同，在文件系统里建一个 socket 节点；已经存在时失败。路径是 / 或者
            // 以 . 结尾时，它指的是一个已有的目录
            let (mnt, dir, name) = parent_at(cx, AT_FDCWD, p)?;
            if name == b"." || name == b".." {
                return Err(Errno::EADDRINUSE);
            }
            let vfs = cx.vfs()?;
            let cred = cx.cred();
            let in_use = |e: Errno| {
                if e == Errno::EEXIST {
                    Errno::EADDRINUSE
                } else {
                    e
                }
            };
            let dir_st = super::fs::may_create(&vfs, &cred, (mnt, dir), name).map_err(in_use)?;
            let (uid, gid, perm) = cred.new_owner(&dir_st, 0o777 & !cx.umask(), false);
            let ino = vfs
                .store()
                .mknod(dir, name, mode::S_IFSOCK | perm, 0, uid, gid)
                .map_err(in_use)?;
            super::inotify::dir_event(cx.kr(), dir, name, super::inotify::IN_CREATE, 0);
            cx.k().unix.insert(Name::Path(ino), sock);
            bound.extend_from_slice(p);
            bound.push(0);
        }
    }
    with(sock, |u| u.addr = bound);
    Ok(())
}

pub(super) fn listen(u: &mut UnixSocket, backlog: i32, cred: Ucred) -> Result<(), Errno> {
    let backlog = usize::try_from(backlog).map_or(SOMAXCONN, |b| b.min(SOMAXCONN));
    if u.ty == SOCK_DGRAM {
        return Err(Errno::EOPNOTSUPP);
    }
    if u.addr.is_empty() {
        return Err(Errno::EINVAL);
    }
    match &mut u.link {
        Link::None => {
            u.link = Link::Listening {
                backlog,
                pending: Default::default(),
                cred,
            };
            Ok(())
        }
        Link::Listening { backlog: b, .. } => {
            *b = backlog;
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

/// 阻塞操作的公共部分：非阻塞或超时返回 EAGAIN，有信号返回 ERESTARTSYS，否则等到
/// 有线程改变了 channel `on` 的状态（监听 socket 的连接队列记在它的接收队列上）。
fn block(
    cx: &mut Cx<'_>,
    nonblock: bool,
    deadline: Option<Instant>,
    on: &ChannelRef,
) -> Result<(), Errno> {
    if nonblock || deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Errno::EAGAIN);
    }
    if cx.interrupted() {
        return Err(Errno::ERESTARTSYS);
    }
    cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(on)])
}

fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.map(|t| Instant::now() + t)
}

pub(super) fn accept(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
) -> Result<(SocketRef, Vec<u8>), Errno> {
    let (timeout, queue) = with(sock, |u| (u.rcvtimeo, u.rx.clone()));
    let deadline = deadline(timeout);
    loop {
        let new = with(sock, |u| match &mut u.link {
            Link::Listening { pending, .. } => Ok(pending.pop_front()),
            _ => Err(Errno::EINVAL),
        })?;
        if let Some(new) = new {
            // 等 backlog 有空位的 connect 可以继续了
            request_wake_object(wake::of(&queue));
            let peer = with(&new, |u| u.peer_name()).unwrap_or_else(unnamed);
            return Ok((new, peer));
        }
        block(cx, nonblock, deadline, &queue)?;
    }
}

pub(super) fn connect(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    addr: &[u8],
) -> Result<(), Errno> {
    let (ty, timeout) = with(sock, |u| (u.ty, u.sndtimeo));
    if ty == SOCK_DGRAM {
        return connect_dgram(cx, sock, addr);
    }
    let cred = cx.ucred();
    let deadline = deadline(timeout);
    loop {
        with(sock, |u| match u.link {
            Link::None => Ok(()),
            Link::Listening { .. } => Err(Errno::EINVAL),
            _ => Err(Errno::EISCONN),
        })?;
        let target = find(cx, addr)?;
        if Rc::ptr_eq(&target, sock) {
            return Err(Errno::ECONNREFUSED);
        }
        let queue = {
            let mut t = target.borrow_mut();
            let Socket::Unix(server) = &mut *t else {
                return Err(Errno::ECONNREFUSED);
            };
            if server.ty != ty {
                return Err(Errno::EPROTOTYPE);
            }
            let Link::Listening {
                backlog,
                pending,
                cred: server_cred,
            } = &mut server.link
            else {
                return Err(Errno::ECONNREFUSED);
            };
            // 与 Linux 相同，最多容纳 backlog + 1 个等待 accept 的连接
            if pending.len() <= *backlog {
                let (end, own) = with(sock, |u| {
                    (
                        u.connect_stream(&server.addr, *server_cred, cred),
                        wake::of(&u.rx),
                    )
                });
                pending.push_back(Rc::new(RefCell::new(Socket::Unix(end))));
                server.connections += 1;
                request_wake_object(wake::of(&server.rx));
                // 连上了：poll 着这个 socket 的线程登记的是它自己的接收队列
                request_wake_object(own);
                return Ok(());
            }
            server.rx.clone()
        };
        block(cx, nonblock, deadline, &queue)?;
    }
}

fn connect_dgram(cx: &mut Cx<'_>, sock: &SocketRef, addr: &[u8]) -> Result<(), Errno> {
    // AF_UNSPEC：断开
    if addr.len() >= 2 && addr[..2] == [0, 0] {
        with(sock, |u| u.link = Link::None);
        return Ok(());
    }
    let target = find(cx, addr)?;
    let link = match &*target.borrow() {
        Socket::Unix(p) if p.ty == SOCK_DGRAM => Link::Dgram {
            tx: Rc::downgrade(&p.rx),
            peer_addr: p.name(),
        },
        Socket::Unix(_) => return Err(Errno::EPROTOTYPE),
        _ => return Err(Errno::ECONNREFUSED),
    };
    with(sock, |u| u.link = link);
    Ok(())
}

/// 解析 sendmsg 的控制消息，取出 `SCM_RIGHTS` 要传递的打开文件。
fn rights(cx: &Cx<'_>, control: &[u8]) -> Result<Vec<FileRef>, Errno> {
    let mut files = Vec::new();
    let mut off = 0;
    while off + CMSG_HDR <= control.len() {
        let h = &control[off..];
        let len = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(h[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(h[12..16].try_into().unwrap());
        if len < CMSG_HDR || len > h.len() {
            return Err(Errno::EINVAL);
        }
        match (level, ty) {
            (SOL_SOCKET, SCM_RIGHTS) => {
                let n = (len - CMSG_HDR) / 4;
                if files.len() + n > SCM_MAX_FD {
                    return Err(Errno::EINVAL);
                }
                for fd in h[CMSG_HDR..CMSG_HDR + n * 4].chunks_exact(4) {
                    files.push(cx.fds().get(i32::from_le_bytes(fd.try_into().unwrap()))?);
                }
            }
            // 所有进程都是 root，可以声明任何凭据；接收方看到的总是真实的发送方
            (SOL_SOCKET, SCM_CREDENTIALS) => {}
            _ => return Err(Errno::EINVAL),
        }
        off += len.next_multiple_of(8);
    }
    Ok(files)
}

pub(super) fn send(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    data: &[u8],
    addr: &[u8],
    control: &[u8],
    flags: u32,
) -> Result<usize, Errno> {
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let mut files = rights(cx, control)?;
    let cred = cx.ucred();
    let (ty, timeout) = with(sock, |u| (u.ty, u.sndtimeo));
    let deadline = deadline(timeout);
    if ty == SOCK_DGRAM {
        return send_dgram(cx, sock, nonblock, deadline, data, addr, files, cred);
    }
    let mut done = 0;
    loop {
        let tx = with(sock, |u| match &u.link {
            Link::Stream { tx, .. } => Ok(tx.clone()),
            _ => Err(Errno::ENOTCONN),
        })?;
        {
            let mut c = tx.borrow_mut();
            if c.eof || c.closed {
                drop(c);
                let e = epipe(cx, flags);
                return if done > 0 { Ok(done) } else { Err(e) };
            }
            if ty == SOCK_SEQPACKET {
                if data.len() > CAPACITY {
                    return Err(Errno::EMSGSIZE);
                }
                if c.space() >= data.len() {
                    let inflight = !files.is_empty();
                    c.push(packet(data, files, cred, Vec::new()));
                    if inflight {
                        crate::unix::note_inflight(cx.k(), &tx);
                    }
                    return Ok(data.len());
                }
            } else {
                if data.is_empty() && files.is_empty() {
                    return Ok(0);
                }
                // 流式 socket 可以只放下一部分；打开文件随第一部分传递
                let n = c.space().min(data.len() - done);
                if n > 0 || data.is_empty() {
                    if !files.is_empty() {
                        crate::unix::note_inflight(cx.k(), &tx);
                    }
                    c.push(packet(
                        &data[done..done + n],
                        std::mem::take(&mut files),
                        cred,
                        Vec::new(),
                    ));
                    done += n;
                    if done == data.len() {
                        return Ok(done);
                    }
                    continue;
                }
            }
        }
        if let Err(e) = block(cx, nonblock, deadline, &tx) {
            return if done > 0 { Ok(done) } else { Err(e) };
        }
    }
}

fn packet(data: &[u8], fds: Vec<FileRef>, cred: Ucred, from: Vec<u8>) -> Packet {
    Packet {
        data: data.to_vec(),
        off: 0,
        fds,
        cred,
        from,
    }
}

#[allow(clippy::too_many_arguments)]
fn send_dgram(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    deadline: Option<Instant>,
    data: &[u8],
    addr: &[u8],
    files: Vec<FileRef>,
    cred: Ucred,
) -> Result<usize, Errno> {
    let (own_rx, from, connected, shut_wr) = with(sock, |u| {
        let connected = match &u.link {
            Link::Dgram { tx, .. } => Some(tx.clone()),
            _ => None,
        };
        (Rc::downgrade(&u.rx), u.addr.clone(), connected, u.shut_wr)
    });
    if shut_wr {
        return Err(epipe(cx, 0));
    }
    let rx: ChannelRef = if addr.is_empty() {
        connected
            .ok_or(Errno::ENOTCONN)?
            .upgrade()
            .ok_or(Errno::ECONNREFUSED)?
    } else {
        let target = find(cx, addr)?;
        let t = target.borrow();
        let Socket::Unix(p) = &*t else {
            return Err(Errno::ECONNREFUSED);
        };
        if p.ty != SOCK_DGRAM {
            return Err(Errno::EPROTOTYPE);
        }
        // 对方连着别的 socket 时只收那个 socket 发来的数据报
        if let Link::Dgram { tx, .. } = &p.link
            && !Weak::ptr_eq(tx, &own_rx)
        {
            return Err(Errno::EPERM);
        }
        p.rx.clone()
    };
    if data.len() > CAPACITY {
        return Err(Errno::EMSGSIZE);
    }
    let mut files = Some(files);
    loop {
        {
            let mut c = rx.borrow_mut();
            if c.closed {
                return Err(Errno::ECONNREFUSED);
            }
            if c.space() >= data.len() {
                let files = files.take().unwrap_or_default();
                if !files.is_empty() {
                    crate::unix::note_inflight(cx.k(), &rx);
                }
                c.push(packet(data, files, cred, from));
                return Ok(data.len());
            }
        }
        block(cx, nonblock, deadline, &rx)?;
    }
}

pub(super) fn recv(
    cx: &mut Cx<'_>,
    sock: &SocketRef,
    nonblock: bool,
    max: usize,
    control_len: usize,
    flags: u32,
) -> Result<Message, Errno> {
    let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
    let peek = flags & MSG_PEEK != 0;
    let (ty, timeout, passcred) = with(sock, |u| (u.ty, u.rcvtimeo, u.passcred));
    let deadline = deadline(timeout);
    let wait_all = flags & MSG_WAITALL != 0 && ty == SOCK_STREAM && !peek;
    // MSG_WAITALL 时已经读到的部分
    let mut part: Option<Received> = None;
    loop {
        let rx = with(sock, |u| match &u.link {
            Link::Stream { rx, .. } => Ok(rx.clone()),
            Link::Listening { .. } => Err(Errno::EINVAL),
            Link::None if u.ty != SOCK_DGRAM => Err(Errno::ENOTCONN),
            _ => Ok(u.rx.clone()),
        })?;
        let (got, eof) = {
            let mut c = rx.borrow_mut();
            let room = max - part.as_ref().map_or(0, |p| p.data.len());
            let got = if ty == SOCK_STREAM {
                Some(c.read_stream(room, peek)).filter(|r| !r.data.is_empty() || !r.fds.is_empty())
            } else {
                c.read_packet(room, peek)
            };
            (got, c.eof)
        };
        if let Some(r) = got {
            let r = match part.take() {
                Some(mut p) => {
                    p.data.extend_from_slice(&r.data);
                    p.len = p.data.len();
                    p.fds.extend(r.fds);
                    p
                }
                None => r,
            };
            if !wait_all || r.data.len() >= max || !r.fds.is_empty() {
                return Ok(deliver(cx, r, passcred, control_len, flags));
            }
            part = Some(r);
            continue;
        }
        if eof {
            return Ok(match part.take() {
                Some(r) => deliver(cx, r, passcred, control_len, flags),
                None => Message {
                    data: Vec::new(),
                    len: 0,
                    addr: Vec::new(),
                    control: Vec::new(),
                    flags: 0,
                },
            });
        }
        if let Err(e) = block(cx, nonblock, deadline, &rx) {
            return match part.take() {
                Some(r) => Ok(deliver(cx, r, passcred, control_len, flags)),
                None => Err(e),
            };
        }
    }
}

/// 把读到的数据交给 guest：附上凭据，把传来的打开文件装进当前进程的 fd 表。控制缓冲区
/// 放不下的部分丢弃并置 `MSG_CTRUNC`，与 Linux 相同。
fn deliver(
    cx: &mut Cx<'_>,
    r: Received,
    passcred: bool,
    control_len: usize,
    flags: u32,
) -> Message {
    let mut control = Vec::new();
    let mut out_flags = if r.len > r.data.len() { MSG_TRUNC } else { 0 };
    if passcred {
        if control_len >= CMSG_HDR + 12 {
            // pid 是接收者看到的（Linux 的 scm_recv 用 pid_vnr）
            let cred = Ucred {
                pid: cx.pid_out(r.cred.pid),
                ..r.cred
            };
            push_cmsg(&mut control, SCM_CREDENTIALS, &cred.encode());
        } else {
            out_flags |= MSG_CTRUNC;
        }
    }
    if !r.fds.is_empty() {
        let room = control_len.saturating_sub(control.len() + CMSG_HDR) / 4;
        let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
        let mut fds = Vec::new();
        let total = r.fds.len();
        for file in r.fds.into_iter().take(room) {
            match cx.fds_mut().install(file, cloexec, 0) {
                Ok(fd) => fds.extend_from_slice(&fd.to_le_bytes()),
                Err(_) => break,
            }
        }
        if fds.len() / 4 < total {
            out_flags |= MSG_CTRUNC;
        }
        if !fds.is_empty() {
            push_cmsg(&mut control, SCM_RIGHTS, &fds);
        }
    }
    control.truncate(control_len);
    Message {
        data: r.data,
        len: r.len,
        addr: r.from,
        control,
        flags: out_flags,
    }
}

/// 追加一条 `SOL_SOCKET` 的控制消息，按 8 字节对齐补齐。
fn push_cmsg(out: &mut Vec<u8>, ty: i32, data: &[u8]) {
    out.extend_from_slice(&((CMSG_HDR + data.len()) as u64).to_le_bytes());
    out.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(8), 0);
}

fn timeval(b: &[u8]) -> Option<Duration> {
    let sec = i64::from_le_bytes(b.get(..8)?.try_into().ok()?);
    let usec = i64::from_le_bytes(b.get(8..16)?.try_into().ok()?);
    let d = Duration::from_secs(sec.max(0) as u64) + Duration::from_micros(usec.max(0) as u64);
    (!d.is_zero()).then_some(d)
}

fn encode_timeval(t: Option<Duration>) -> Vec<u8> {
    let t = t.unwrap_or_default();
    let mut b = (t.as_secs() as i64).to_le_bytes().to_vec();
    b.extend_from_slice(&(t.subsec_micros() as i64).to_le_bytes());
    b
}

pub(super) fn setsockopt(
    u: &mut UnixSocket,
    level: i32,
    name: i32,
    value: &[u8],
) -> Result<(), Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EOPNOTSUPP);
    }
    let int = value
        .get(..4)
        .map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap()));
    match name {
        SO_PASSCRED => u.passcred = int != 0,
        SO_RCVTIMEO => u.rcvtimeo = timeval(value),
        SO_SNDTIMEO => u.sndtimeo = timeval(value),
        // 缓冲区大小、SO_REUSEADDR 等：接受，但不起作用
        _ => {}
    }
    Ok(())
}

/// `pid_ns` 是调用者的 pid 命名空间：`SO_PEERCRED` 的 pid 按它算。
pub(super) fn getsockopt(
    u: &UnixSocket,
    level: i32,
    name: i32,
    pid_ns: &crate::pidns::PidNs,
) -> Result<Vec<u8>, Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EOPNOTSUPP);
    }
    let int = |v: i32| v.to_le_bytes().to_vec();
    Ok(match name {
        SO_TYPE => int(u.ty),
        SO_ERROR | SO_PROTOCOL => int(0),
        SO_DOMAIN => int(AF_UNIX),
        SO_SNDBUF | SO_RCVBUF => int(CAPACITY as i32),
        SO_PASSCRED => int(u.passcred as i32),
        SO_ACCEPTCONN => int(matches!(u.link, Link::Listening { .. }) as i32),
        SO_RCVTIMEO => encode_timeval(u.rcvtimeo),
        SO_SNDTIMEO => encode_timeval(u.sndtimeo),
        // 没有对方时与 Linux 相同：pid 为 0，uid、gid 是 overflowuid
        SO_PEERCRED => u
            .peer_cred
            .map(|c| Ucred {
                pid: pid_ns.vnr(c.pid).unwrap_or(0),
                ..c
            })
            .unwrap_or(Ucred {
                pid: 0,
                uid: 65534,
                gid: 65534,
            })
            .encode()
            .to_vec(),
        _ => return Err(Errno::ENOPROTOOPT),
    })
}

pub(super) fn sockname(u: &UnixSocket, peer: bool) -> Result<Vec<u8>, Errno> {
    if peer {
        u.peer_name().ok_or(Errno::ENOTCONN)
    } else {
        Ok(u.name())
    }
}

pub(super) fn shutdown(u: &mut UnixSocket, how: i32) -> Result<(), Errno> {
    if !(0..=2).contains(&how) {
        return Err(Errno::EINVAL);
    }
    if matches!(u.link, Link::None | Link::Listening { .. }) && u.ty != SOCK_DGRAM {
        return Err(Errno::ENOTCONN);
    }
    u.shutdown(how);
    Ok(())
}
