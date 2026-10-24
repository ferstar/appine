//! AF_UNIX socket（设计文档 §10）：完全在进程内模拟，不经过宿主。
//!
//! - 流式（`SOCK_STREAM`）和 `SOCK_SEQPACKET` 的连接是一对单向的 [`Channel`]；数据报
//!   socket 各有一个接收队列，发送方直接放进对方的队列。
//! - 地址：路径地址在 inode 仓库里建一个 socket 节点，按 inode 查找；抽象地址
//!   （`sun_path` 以 NUL 开头）只存在于 [`Names`]。
//! - `SCM_RIGHTS` 传递的是打开文件本身，接收时装进接收进程的 fd 表。`SO_PEERCRED`、
//!   `SCM_CREDENTIALS` 给出 guest 的 pid，uid、gid 总是 0。
//! - socket 经由自己传递自己形成的环不会被回收（Linux 用专门的垃圾回收处理这种情况）。

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::fd::FileRef;
use crate::kernel::request_wake;
use crate::net::{AF_UNIX, SOCK_DGRAM, Socket};
use crate::vfs::store::Ino;

pub const SOCK_SEQPACKET: i32 = 5;

/// 每个方向最多缓冲的字节数，与 Linux 默认的 `SO_SNDBUF`（208 KiB）相同。
pub const CAPACITY: usize = 212_992;

/// `sockaddr_un` 的长度。
pub const SOCKADDR_UN_LEN: usize = 110;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLHUP: i16 = 0x010;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;
const POLLRDHUP: i16 = 0x2000;

/// `struct ucred`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    /// guest 进程的凭据：所有进程都是 root。
    pub fn of(pid: i32) -> Ucred {
        Ucred {
            pid,
            uid: 0,
            gid: 0,
        }
    }

    pub fn encode(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..].copy_from_slice(&self.gid.to_le_bytes());
        b
    }
}

/// 没有绑定地址的 socket 的 `sockaddr_un`：只有地址族。
pub fn unnamed() -> Vec<u8> {
    (AF_UNIX as u16).to_le_bytes().to_vec()
}

/// 一条消息：流式 socket 的一次写入，或者一个数据报。
pub(crate) struct Packet {
    pub data: Vec<u8>,
    /// 流式 socket 已经读走的字节。
    pub off: usize,
    /// `SCM_RIGHTS` 带来的打开文件。
    pub fds: Vec<FileRef>,
    /// 发送方的凭据。
    pub cred: Ucred,
    /// 发送方的地址（数据报的 recvfrom）。
    pub from: Vec<u8>,
}

impl Packet {
    fn remaining(&self) -> usize {
        self.data.len() - self.off
    }
}

/// 读到的数据。
pub(crate) struct Received {
    pub data: Vec<u8>,
    /// 消息的完整长度（数据报被截断时大于 `data`）。
    pub len: usize,
    pub fds: Vec<FileRef>,
    pub cred: Ucred,
    pub from: Vec<u8>,
}

/// 一个方向上缓冲的消息。
#[derive(Default)]
pub(crate) struct Channel {
    pub packets: VecDeque<Packet>,
    /// 还没读走的字节数。
    pub bytes: usize,
    /// 不会再有新数据（发送方关闭或 `SHUT_WR`，或接收方 `SHUT_RD`）：读完就是 EOF。
    pub eof: bool,
    /// 接收方不再读（关闭或 `SHUT_RD`）：写入失败。
    pub closed: bool,
}

pub(crate) type ChannelRef = Rc<RefCell<Channel>>;

impl Channel {
    /// 还能放下的字节数。
    pub fn space(&self) -> usize {
        CAPACITY.saturating_sub(self.bytes)
    }

    /// 与 Linux 相同：缓冲的数据不超过容量的四分之一时才报告可写。
    pub fn writable(&self) -> bool {
        self.bytes * 4 <= CAPACITY
    }

    pub fn push(&mut self, p: Packet) {
        self.bytes += p.remaining();
        self.packets.push_back(p);
        request_wake();
    }

    /// 接收方关闭：丢弃缓冲的消息。消息交给调用方在释放借用之后析构，因为里面的
    /// 打开文件析构时可能要用到别的 channel。
    fn discard(&mut self) -> VecDeque<Packet> {
        self.closed = true;
        self.bytes = 0;
        std::mem::take(&mut self.packets)
    }

    /// 流式读取：按顺序取最多 `max` 字节。与 Linux 相同，带着打开文件的消息读完后停下，
    /// 不同发送方的数据不合并到一次读取里。
    pub fn read_stream(&mut self, max: usize, peek: bool) -> Received {
        let mut data = Vec::new();
        let mut fds = Vec::new();
        let mut cred = None;
        let mut i = 0;
        while let Some(p) = self.packets.get_mut(i) {
            let room = max - data.len();
            // 只带打开文件、没有数据的消息在没有空间时也要读出来
            if (room == 0 && p.remaining() > 0) || cred.is_some_and(|c| c != p.cred) {
                break;
            }
            cred = Some(p.cred);
            let n = room.min(p.remaining());
            data.extend_from_slice(&p.data[p.off..p.off + n]);
            let had_fds = !p.fds.is_empty();
            if peek {
                fds.extend(p.fds.iter().cloned());
                i += 1;
            } else {
                p.off += n;
                self.bytes -= n;
                fds.append(&mut p.fds);
                if p.remaining() == 0 {
                    self.packets.pop_front();
                }
            }
            if had_fds || data.len() >= max {
                break;
            }
        }
        if !peek && !data.is_empty() {
            request_wake();
        }
        Received {
            len: data.len(),
            data,
            fds,
            cred: cred.unwrap_or_default(),
            from: Vec::new(),
        }
    }

    /// 取一条完整的消息（数据报、`SOCK_SEQPACKET`），超出 `max` 的部分丢弃。
    pub fn read_packet(&mut self, max: usize, peek: bool) -> Option<Received> {
        let (data, len, fds, cred, from) = if peek {
            let p = self.packets.front()?;
            let n = p.data.len().min(max);
            (
                p.data[..n].to_vec(),
                p.data.len(),
                p.fds.clone(),
                p.cred,
                p.from.clone(),
            )
        } else {
            let mut p = self.packets.pop_front()?;
            self.bytes -= p.data.len();
            let len = p.data.len();
            p.data.truncate(max);
            request_wake();
            (p.data, len, std::mem::take(&mut p.fds), p.cred, p.from)
        };
        Some(Received {
            data,
            len,
            fds,
            cred,
            from,
        })
    }
}

/// 连接状态。
pub(crate) enum Link {
    /// 还没有连接。
    None,
    /// 在监听：已经连上、等着 accept 的服务端 socket。
    Listening {
        backlog: usize,
        pending: VecDeque<Rc<RefCell<Socket>>>,
        /// 调用 listen 的进程，是连接方看到的 `SO_PEERCRED`。
        cred: Ucred,
    },
    /// 流式连接的两个方向。
    Stream {
        rx: ChannelRef,
        tx: ChannelRef,
        peer_addr: Vec<u8>,
    },
    /// 数据报 socket 连到了对方的接收队列。
    Dgram {
        tx: Weak<RefCell<Channel>>,
        peer_addr: Vec<u8>,
    },
}

/// 一个 AF_UNIX socket。
pub(crate) struct UnixSocket {
    pub ty: i32,
    /// 绑定的地址（`sockaddr_un` 的字节），没有绑定时为空。
    pub addr: Vec<u8>,
    pub link: Link,
    /// 数据报 socket 自己的接收队列。
    pub rx: ChannelRef,
    /// 对方的凭据（`SO_PEERCRED`）：socketpair 和流式连接时记下。
    pub peer_cred: Option<Ucred>,
    /// `SO_PASSCRED`：每次接收都附上发送方的凭据。
    pub passcred: bool,
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    /// 数据报 socket 做过 `SHUT_WR`。
    pub shut_wr: bool,
}

impl UnixSocket {
    pub fn new(ty: i32) -> UnixSocket {
        UnixSocket {
            ty,
            addr: Vec::new(),
            link: Link::None,
            rx: ChannelRef::default(),
            peer_cred: None,
            passcred: false,
            rcvtimeo: None,
            sndtimeo: None,
            shut_wr: false,
        }
    }

    /// socketpair：两个互相连接的 socket。
    pub fn pair(ty: i32, cred: Ucred) -> (UnixSocket, UnixSocket) {
        let mut a = UnixSocket::new(ty);
        let mut b = UnixSocket::new(ty);
        if ty == SOCK_DGRAM {
            a.link = Link::Dgram {
                tx: Rc::downgrade(&b.rx),
                peer_addr: unnamed(),
            };
            b.link = Link::Dgram {
                tx: Rc::downgrade(&a.rx),
                peer_addr: unnamed(),
            };
        } else {
            let (ab, ba) = (ChannelRef::default(), ChannelRef::default());
            a.link = Link::Stream {
                rx: ba.clone(),
                tx: ab.clone(),
                peer_addr: unnamed(),
            };
            b.link = Link::Stream {
                rx: ab,
                tx: ba,
                peer_addr: unnamed(),
            };
        }
        a.peer_cred = Some(cred);
        b.peer_cred = Some(cred);
        (a, b)
    }

    /// 自己的地址；没有绑定时只有地址族。
    pub fn name(&self) -> Vec<u8> {
        if self.addr.is_empty() {
            unnamed()
        } else {
            self.addr.clone()
        }
    }

    /// 对方的地址，没有连接时为 None。
    pub fn peer_name(&self) -> Option<Vec<u8>> {
        match &self.link {
            Link::Stream { peer_addr, .. } | Link::Dgram { peer_addr, .. } => {
                Some(peer_addr.clone())
            }
            Link::None | Link::Listening { .. } => None,
        }
    }

    /// 流式连接到正在监听、地址为 `server_addr` 的 socket：返回服务端那一端，由调用方
    /// 放进监听 socket 的等待队列。
    pub fn connect_stream(
        &mut self,
        server_addr: &[u8],
        server_cred: Ucred,
        cred: Ucred,
    ) -> UnixSocket {
        let (up, down) = (ChannelRef::default(), ChannelRef::default());
        let mut server = UnixSocket::new(self.ty);
        server.addr = server_addr.to_vec();
        server.link = Link::Stream {
            rx: up.clone(),
            tx: down.clone(),
            peer_addr: self.name(),
        };
        server.peer_cred = Some(cred);
        self.link = Link::Stream {
            rx: down,
            tx: up,
            peer_addr: server_addr.to_vec(),
        };
        self.peer_cred = Some(server_cred);
        server
    }

    /// shutdown(2)：`how` 是 SHUT_RD(0)、SHUT_WR(1) 或 SHUT_RDWR(2)。
    pub fn shutdown(&mut self, how: i32) {
        let (rd, wr) = (how != 1, how != 0);
        match &self.link {
            Link::Stream { rx, tx, .. } => {
                if rd {
                    let mut r = rx.borrow_mut();
                    r.eof = true;
                    r.closed = true;
                }
                if wr {
                    tx.borrow_mut().eof = true;
                }
            }
            _ => {
                if rd {
                    self.rx.borrow_mut().eof = true;
                }
                self.shut_wr |= wr;
            }
        }
        request_wake();
    }

    /// FIONREAD：流式是缓冲的字节数，数据报是下一个数据报的长度。
    pub fn unread(&self) -> usize {
        match &self.link {
            Link::Stream { rx, .. } if self.ty == crate::net::SOCK_STREAM => rx.borrow().bytes,
            Link::Stream { rx, .. } => rx.borrow().packets.front().map_or(0, |p| p.data.len()),
            _ => self.rx.borrow().packets.front().map_or(0, |p| p.data.len()),
        }
    }

    /// ppoll、epoll 报告的事件，规则与 Linux 的 `unix_poll`、`unix_dgram_poll` 相同。
    pub fn readiness(&self) -> i16 {
        match &self.link {
            Link::Listening { pending, .. } => {
                if pending.is_empty() {
                    0
                } else {
                    POLLIN | POLLRDNORM
                }
            }
            Link::Stream { rx, tx, .. } => {
                let (r, t) = (rx.borrow(), tx.borrow());
                let mut ev = 0;
                if !r.packets.is_empty() {
                    ev |= POLLIN | POLLRDNORM;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                    if t.eof || t.closed {
                        ev |= POLLHUP;
                    }
                }
                // 对方已经关闭时也报告可写，让写的一方能拿到 EPIPE
                if t.writable() || t.closed {
                    ev |= POLLOUT | POLLWRNORM;
                }
                ev
            }
            Link::None if self.ty != SOCK_DGRAM => POLLOUT | POLLWRNORM | POLLHUP,
            Link::None | Link::Dgram { .. } => {
                let r = self.rx.borrow();
                let mut ev = 0;
                if !r.packets.is_empty() {
                    ev |= POLLIN | POLLRDNORM;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                }
                let peer_full = match &self.link {
                    Link::Dgram { tx, .. } => tx.upgrade().is_some_and(|c| !c.borrow().writable()),
                    _ => false,
                };
                if !peer_full {
                    ev |= POLLOUT | POLLWRNORM;
                }
                ev
            }
        }
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        let stale = match &self.link {
            Link::Stream { rx, tx, .. } => {
                tx.borrow_mut().eof = true;
                rx.borrow_mut().discard()
            }
            _ => self.rx.borrow_mut().discard(),
        };
        drop(stale);
        request_wake();
    }
}

/// 地址表里的名字。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Name {
    /// 路径地址：仓库里 socket 节点的 inode。
    Path(Ino),
    /// 抽象地址：`sun_path` 的全部字节（以 NUL 开头）。
    Abstract(Vec<u8>),
}

/// 已绑定的地址（内核全局）。socket 关闭后弱引用失效，地址随之释放。
#[derive(Default)]
pub(crate) struct Names {
    map: BTreeMap<Name, Weak<RefCell<Socket>>>,
    next_auto: u32,
}

impl Names {
    pub fn lookup(&self, name: &Name) -> Option<Rc<RefCell<Socket>>> {
        self.map.get(name).and_then(Weak::upgrade)
    }

    /// 登记地址；已被活着的 socket 占用时返回 false。
    pub fn insert(&mut self, name: Name, sock: &Rc<RefCell<Socket>>) -> bool {
        self.map.retain(|_, w| w.strong_count() > 0);
        if self.lookup(&name).is_some() {
            return false;
        }
        self.map.insert(name, Rc::downgrade(sock));
        true
    }

    /// 自动绑定：与 Linux 相同，分配一个 `\0` 加 5 位十六进制数字的抽象地址。
    pub fn autobind(&mut self, sock: &Rc<RefCell<Socket>>) -> Option<Vec<u8>> {
        for _ in 0..0x100000 {
            self.next_auto = (self.next_auto + 1) & 0xfffff;
            let mut path = vec![0u8];
            path.extend_from_slice(format!("{:05x}", self.next_auto).as_bytes());
            if self.insert(Name::Abstract(path.clone()), sock) {
                return Some(path);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd::{FileKind, OpenFile, oflags};
    use crate::net::SOCK_STREAM;

    fn packet(data: &[u8], pid: i32) -> Packet {
        Packet {
            data: data.to_vec(),
            off: 0,
            fds: Vec::new(),
            cred: Ucred::of(pid),
            from: Vec::new(),
        }
    }

    #[test]
    fn stream_reads_merge_writes_but_stop_at_passed_fds() {
        let mut c = Channel::default();
        c.push(packet(b"ab", 1));
        let mut with_fd = packet(b"cd", 1);
        with_fd
            .fds
            .push(OpenFile::new(FileKind::Host(1), oflags::O_WRONLY));
        c.push(with_fd);
        c.push(packet(b"ef", 1));
        c.push(packet(b"gh", 2));
        assert_eq!(c.bytes, 8);

        let peek = c.read_stream(100, true);
        assert_eq!(peek.data, b"abcd");
        assert_eq!(peek.fds.len(), 1);
        assert_eq!(c.bytes, 8);

        let r = c.read_stream(3, false);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"abc"[..], 1));
        // 带打开文件的消息剩下的部分照常读出，但打开文件只交付一次
        let r = c.read_stream(100, false);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"def"[..], 0));
        // 不同发送方的数据不合并
        let r = c.read_stream(100, false);
        assert_eq!((r.data.as_slice(), r.cred.pid), (&b"gh"[..], 2));
        assert_eq!(c.bytes, 0);
    }

    #[test]
    fn datagrams_keep_boundaries_and_truncate() {
        let mut c = Channel::default();
        c.push(packet(b"hello", 1));
        c.push(packet(b"x", 1));
        let r = c.read_packet(3, false).unwrap();
        assert_eq!((r.data.as_slice(), r.len), (&b"hel"[..], 5));
        let r = c.read_packet(3, false).unwrap();
        assert_eq!(r.data, b"x");
        assert!(c.read_packet(3, false).is_none());
        assert_eq!(c.bytes, 0);
    }

    #[test]
    fn closing_one_end_shows_eof_and_hangup_on_the_other() {
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(7));
        assert_eq!(b.peer_cred, Some(Ucred::of(7)));
        if let Link::Stream { tx, .. } = &a.link {
            tx.borrow_mut().push(packet(b"bye", 7));
        }
        assert_eq!(b.readiness() & (POLLIN | POLLHUP), POLLIN);
        drop(a);
        let ev = b.readiness();
        assert_eq!(
            ev & (POLLIN | POLLHUP | POLLRDHUP | POLLOUT),
            POLLIN | POLLHUP | POLLRDHUP | POLLOUT
        );
        if let Link::Stream { rx, tx, .. } = &b.link {
            assert_eq!(rx.borrow_mut().read_stream(10, false).data, b"bye");
            assert!(tx.borrow().closed);
        }
    }

    #[test]
    fn abstract_names_are_released_with_the_socket() {
        let mut names = Names::default();
        let s = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        let name = Name::Abstract(b"\0test".to_vec());
        assert!(names.insert(name.clone(), &s));
        assert!(!names.insert(name.clone(), &s));
        let auto = names.autobind(&s).unwrap();
        assert_eq!(auto, b"\x0000001");
        drop(s);
        assert!(names.lookup(&name).is_none());
        let t = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        assert!(names.insert(name, &t));
    }
}
