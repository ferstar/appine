//! AF_UNIX sockets (the kernel/unix map): emulated entirely in process, never going through the
//! host.
//!
//! - A stream (`SOCK_STREAM`) or `SOCK_SEQPACKET` connection is a pair of one-way [`Channel`]s;
//!   each datagram socket has a receive queue, and senders put messages straight into the peer's
//!   queue.
//! - Addresses: a path address creates a socket node in the inode store and is looked up by inode;
//!   abstract addresses (`sun_path` starting with NUL) exist only in [`Names`].
//! - `SCM_RIGHTS` passes the open files themselves, installed into the receiving process's fd
//!   table on receipt. `SO_PEERCRED` gives the peer's pid and effective user ID and group ID;
//!   `SCM_CREDENTIALS` gives the sender's pid and real user ID and group ID, unless the sender
//!   declared others.
//! - Sockets that refer to each other only through in-flight `SCM_RIGHTS` messages form `Rc`
//!   cycles (open file → socket → receive queue → message → open file) that the program can no
//!   longer reach. As with Linux's unix_gc, such sockets are found after a unix socket is closed
//!   and after a process exits, and their receive queues are emptied ([`collect`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::cell::RefCell;
use crate::fd::{FdTable, FileKind, FileRef, OpenFile};
use crate::kernel::{Kernel, request_wake_object, wake};
use crate::net::{AF_UNIX, SOCK_DGRAM, SOCK_STREAM, Socket};
use crate::vfs::store::Ino;

pub const SOCK_SEQPACKET: i32 = 5;

/// Default `SO_SNDBUF` (208 KiB, Linux's `net.core.wmem_default`).
pub const CAPACITY: usize = 212_992;

/// Linux 64-bit `sizeof(struct sk_buff)` + `sizeof(struct skb_shared_info)` overhead constants
/// matching arm64 Linux 6.1/6.8 slab and page allocations.
const SKB_HEAD_OVERHEAD: usize = 256;
const SKB_SHINFO_OVERHEAD: usize = 320;
const SKB_PAGE_SIZE: usize = 4096;
/// Maximum linear head fitting in one 4 KiB page (`SKB_MAX_HEAD(0)` = 3776 B).
const SKB_MAX_HEAD: usize = SKB_PAGE_SIZE - SKB_SHINFO_OVERHEAD;
/// Maximum linear kmalloc payload for datagram/seqpacket skbs (`16 KiB - sizeof(skb_shared_info)`).
const SKB_MAX_LINEAR_DGRAM: usize = 16_384 - SKB_SHINFO_OVERHEAD;
/// Maximum payload per `SOCK_STREAM` chunk (`SKB_MAX_HEAD(0) + 8 * PAGE_SIZE` = 36,544 B,
/// matching arm64 Linux's `unix_stream_sendmsg` paged chunking).
pub(crate) const STREAM_MAX_CHUNK: usize = SKB_MAX_HEAD + 8 * SKB_PAGE_SIZE;

fn linear_truesize(head_len: usize) -> usize {
    let alloc = (head_len + SKB_SHINFO_OVERHEAD)
        .next_power_of_two()
        .max(512);
    SKB_HEAD_OVERHEAD + alloc
}

/// Linux 64-bit `skb->truesize` for a queued `SOCK_DGRAM` / `SOCK_SEQPACKET` packet (and zero-byte
/// packets): linear kmalloc buckets up to 16 KiB (`SKB_MAX_LINEAR_DGRAM`), then a 4 KiB linear
/// head (4352 B) plus 4 KiB paged fragments.
pub(crate) fn packet_truesize(len: usize) -> usize {
    if len <= SKB_MAX_LINEAR_DGRAM {
        linear_truesize(len)
    } else {
        linear_truesize(SKB_MAX_HEAD)
            .saturating_add((len - SKB_MAX_HEAD).next_multiple_of(SKB_PAGE_SIZE))
    }
}

/// Linux 64-bit `skb->truesize` for one `SOCK_STREAM` chunk allocated by `unix_stream_sendmsg`:
/// payloads up to `SKB_MAX_HEAD` (3776 B) use a linear kmalloc bucket; larger chunks spill
/// `min(len, PAGE_ALIGN(len - SKB_MAX_HEAD))` into 4 KiB paged fragments and keep the remainder
/// in a linear head.
pub(crate) fn stream_truesize(len: usize) -> usize {
    let data_len = len
        .saturating_sub(SKB_MAX_HEAD)
        .next_multiple_of(SKB_PAGE_SIZE)
        .min(len);
    let head_len = len - data_len;
    linear_truesize(head_len).saturating_add(data_len.next_multiple_of(SKB_PAGE_SIZE))
}

/// Maximum payload per `SOCK_STREAM` chunk for a sender with send buffer `sndbuf`
/// (Linux's `min((sk_sndbuf >> 1) - 64, STREAM_MAX_CHUNK)`).
pub(crate) fn stream_chunk_limit(sndbuf: usize) -> usize {
    (sndbuf >> 1).saturating_sub(64).clamp(1, STREAM_MAX_CHUNK)
}

/// Maximum single datagram or seqpacket payload for a sender with send buffer `sndbuf`
/// (Linux's `sk_sndbuf - 32`).
pub(crate) fn dgram_max_len(sndbuf: usize) -> usize {
    sndbuf.saturating_sub(32)
}

/// Shared cell holding a socket's bound `sockaddr_un` bytes (empty while unbound) so packets and
/// peers observe post-send/post-connect `bind()` at receive/`getpeername` time without retaining
/// the sending `UnixSocket`.
pub(crate) type AddrRef = Rc<RefCell<Vec<u8>>>;

/// Shared sender state carried by queued packets: live bound address cell, `sk_wmem_alloc`
/// (`outq`) / `sk_sndbuf` (`sndbuf`) accounting, and write-space arrival counter (`taken`).
/// Packets resolve the sender's address at receive time and uncharge `outq` (waking blocked
/// senders and `POLLOUT` waiters) on drop without retaining the sending `UnixSocket`.
pub(crate) struct SenderState {
    pub addr: AddrRef,
    pub stream: bool,
    pub outq: std::cell::Cell<usize>,
    pub sndbuf: std::cell::Cell<usize>,
    pub taken: std::cell::Cell<u64>,
}

impl SenderState {
    fn new(addr: AddrRef, stream: bool) -> Rc<Self> {
        Rc::new(Self {
            addr,
            stream,
            outq: std::cell::Cell::new(0),
            sndbuf: std::cell::Cell::new(CAPACITY),
            taken: std::cell::Cell::new(0),
        })
    }

    pub(crate) fn wake_object(&self) -> wake::Object {
        wake::of_value(self)
    }

    pub(crate) fn sndbuf(&self) -> usize {
        self.sndbuf.get()
    }

    /// Updates `SO_SNDBUF` and wakes any blocked senders or `POLLOUT` waiters when capacity grows.
    pub(crate) fn set_sndbuf(&self, sndbuf: usize) {
        let old = self.sndbuf.replace(sndbuf);
        if sndbuf > old {
            if self.writable() {
                self.taken.set(self.taken.get().wrapping_add(1));
            }
            request_wake_object(self.wake_object());
        }
    }

    /// Computes `skb->truesize` for a packet of `len` payload bytes sent by this socket.
    pub(crate) fn truesize(&self, len: usize) -> usize {
        if self.stream {
            stream_truesize(len)
        } else {
            packet_truesize(len)
        }
    }

    /// Whether the sender has room to allocate another packet (Linux's `sk_wmem_alloc < sk_sndbuf`).
    pub(crate) fn has_room(&self) -> bool {
        self.outq.get() < self.sndbuf.get()
    }

    /// Linux's `unix_writable(sk)`: `(sk_wmem_alloc << 2) <= sk_sndbuf`, where `sk_wmem_alloc`
    /// starts at 1 (so `(outq + 1) * 4 <= sndbuf`).
    pub(crate) fn writable(&self) -> bool {
        self.outq.get().saturating_add(1).saturating_mul(4) <= self.sndbuf.get()
    }

    /// Charges one packet and gives it a sender reference to release on dequeue or drop.
    pub(crate) fn charge(self: &Rc<Self>, truesize: usize) -> Rc<Self> {
        self.outq.set(self.outq.get().saturating_add(truesize));
        self.clone()
    }

    /// Uncharges a consumed or dropped packet and wakes any waiting sender or `POLLOUT` waiter.
    pub(crate) fn uncharge(&self, truesize: usize) {
        self.outq.set(self.outq.get().saturating_sub(truesize));
        if self.writable() {
            self.taken.set(self.taken.get().wrapping_add(1));
        }
        request_wake_object(self.wake_object());
    }
}

/// The default `net.unix.max_dgram_qlen`: datagrams a socket's queue holds before senders other
/// than its peer wait.
pub(crate) const MAX_DGRAM_QLEN: usize = 10;

/// The length of a `sockaddr_un`.
pub const SOCKADDR_UN_LEN: usize = 110;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;
const POLLRDHUP: i16 = 0x2000;

/// `struct ucred`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    /// No SCM credentials were recorded. Invalid kernel IDs translate to the receiver's overflow IDs.
    pub(crate) const UNSTAMPED: Self = Self {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };

    /// The process with pid `pid` (global) and user ID and group ID `uid` and `gid` (kernel IDs:
    /// the effective ones for `SO_PEERCRED`, the real or declared ones for `SCM_CREDENTIALS`).
    pub fn of(pid: i32, uid: u32, gid: u32) -> Ucred {
        Ucred { pid, uid, gid }
    }

    pub fn encode(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..].copy_from_slice(&self.gid.to_le_bytes());
        b
    }
}

// ---- Sockets ----

/// Connection state.
pub(crate) enum Link {
    /// Not connected yet.
    None,
    /// Listening: the server-side sockets already connected and waiting for accept.
    Listening {
        backlog: usize,
        pending: VecDeque<Rc<RefCell<Socket>>>,
        /// The process that called listen; it is the `SO_PEERCRED` connecting sides see.
        cred: Ucred,
    },
    /// The two directions of a stream connection.
    Stream {
        rx: ChannelRef,
        tx: ChannelRef,
        peer_addr: AddrRef,
    },
    /// A datagram socket connected to the peer's receive queue.
    Dgram {
        tx: Weak<RefCell<Channel>>,
        peer_addr: AddrRef,
    },
}

/// An AF_UNIX socket.
pub(crate) struct UnixSocket {
    pub ty: i32,
    /// The bound address (`sockaddr_un` bytes), empty when unbound. Shared with outgoing packets
    /// and connected peers so later `bind()` calls are observed at receive/query time.
    pub addr: AddrRef,
    pub link: Link,
    /// The datagram socket's own receive queue.
    pub rx: ChannelRef,
    /// The peer's credentials (`SO_PEERCRED`): recorded at socketpair and stream connect.
    pub peer_cred: Option<Ucred>,
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    /// The datagram socket did `SHUT_WR`.
    pub shut_wr: bool,
    /// The number of connections a listening socket has received (`EPOLLET` triggers on each).
    pub connections: u64,
    /// The server end of a connection not accepted yet (Linux's embryo): closing it with its
    /// listener resets the connection.
    pub embryo: bool,
    /// The new network namespace it was created in, if any (abstract addresses are kept apart by
    /// it, and interface ioctls look at its lo).
    pub net: Option<Rc<crate::nsproxy::NetNs>>,
    /// The generic `SOL_SOCKET` options (`SO_SNDBUF` and `SO_RCVLOWAT` are enforced; `SO_RCVBUF`
    /// and the remaining options are recorded only, matching Linux AF_UNIX).
    pub opts: crate::net::SockOpts,
    /// Shared sender state carried by outgoing packets (bound address cell, `SIOCOUTQ` /
    /// `sk_wmem_alloc` counter, `SO_SNDBUF` cap, and write-space wake object).
    pub outq: Rc<SenderState>,
    /// Creator's filesystem UID (`sock_i_uid`).
    pub uid: u32,
    /// Socket inode group, initialized from the creator's filesystem GID.
    pub gid: u32,
}

impl UnixSocket {
    /// A new socket; a datagram socket's queue takes Linux's default limit
    /// ([`UnixSocket::set_max_qlen`] applies the namespace's).
    pub fn new(ty: i32) -> UnixSocket {
        let rx = ChannelRef::default();
        if ty == SOCK_DGRAM {
            rx.borrow_mut().max_qlen = Some(MAX_DGRAM_QLEN);
        }
        let addr = Rc::new(RefCell::new(Vec::new()));
        let outq = SenderState::new(addr.clone(), ty == SOCK_STREAM);
        UnixSocket {
            ty,
            addr,
            link: Link::None,
            rx,
            peer_cred: None,
            rcvtimeo: None,
            sndtimeo: None,
            shut_wr: false,
            connections: 0,
            embryo: false,
            net: None,
            opts: crate::net::SockOpts::default(),
            outq,
            uid: 0,
            gid: 0,
        }
    }

    /// The queue on which peers can see this socket's SO_PASSCRED setting.
    pub fn receive_queue(&self) -> &ChannelRef {
        match &self.link {
            Link::Stream { rx, .. } => rx,
            _ => &self.rx,
        }
    }

    /// A datagram socket's queue limit (`net.unix.max_dgram_qlen` of the namespace it is created
    /// in, as an unsigned number like `sk_max_ack_backlog`).
    pub fn set_max_qlen(&mut self, qlen: i64) {
        if self.ty == SOCK_DGRAM {
            self.rx.borrow_mut().max_qlen = Some(qlen as u32 as usize);
        }
    }

    /// Connects a datagram socket (`Link::Dgram`) or disconnects it (`Link::None`), noting the
    /// peer in its own queue for senders ([`Channel::holds_back`], [`Channel::may_receive_from`]),
    /// updating Linux's datagram `TCP_ESTABLISHED` state for `/proc/net/unix`, and carrying out
    /// `unix_dgram_disconnected` when leaving a previous peer. Returns any `SCM_RIGHTS` files
    /// from purged queued datagrams so the caller can release them after borrows end.
    pub fn set_dgram_link(&mut self, link: Link) -> Vec<FileRef> {
        let old_tx = match std::mem::replace(&mut self.link, link) {
            Link::Dgram { tx, .. } => Some(tx),
            _ => None,
        };
        let new_tx = match &self.link {
            Link::Dgram { tx, .. } => Some(tx),
            _ => None,
        };
        let same_peer = old_tx
            .as_ref()
            .zip(new_tx)
            .is_some_and(|(old, new)| Weak::ptr_eq(old, new));
        let own_rx = Rc::downgrade(&self.rx);
        {
            let mut rx = self.rx.borrow_mut();
            rx.peer = new_tx.cloned();
            rx.established = new_tx.is_some();
        }
        // Mark the new peer established (self is already updated above on self-connect).
        if let Some(tx) = new_tx
            && !Weak::ptr_eq(tx, &own_rx)
            && let Some(peer) = tx.upgrade()
        {
            peer.borrow_mut().established = true;
        }
        let mut dropped_fds = Vec::new();
        if let Some(old) = old_tx
            && !same_peer
        {
            let stale = self.rx.borrow_mut().purge();
            let had_unread = !stale.is_empty();
            for mut p in stale {
                dropped_fds.append(&mut p.fds);
            }
            if let Some(peer) = old.upgrade()
                && !Rc::ptr_eq(&peer, &self.rx)
            {
                let mut p = peer.borrow_mut();
                if p.peer.is_none() {
                    p.established = false;
                }
                // Linux's unix_dgram_disconnected: if one end of a bidirectional dgram link
                // disconnects with unread messages queued, signal ECONNRESET to the peer.
                if !p.closed
                    && had_unread
                    && p.peer.as_ref().is_some_and(|w| Weak::ptr_eq(w, &own_rx))
                {
                    p.reset = true;
                    drop(p);
                    request_wake_object(wake::of(&peer));
                }
            }
        }
        dropped_fds
    }

    /// socketpair: two sockets connected to each other.
    pub fn pair(ty: i32, cred: Ucred) -> (UnixSocket, UnixSocket) {
        let mut a = UnixSocket::new(ty);
        let mut b = UnixSocket::new(ty);
        let a_addr = a.addr.clone();
        let b_addr = b.addr.clone();
        if ty == SOCK_DGRAM {
            let _ = a.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&b.rx),
                peer_addr: b_addr,
            });
            let _ = b.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&a.rx),
                peer_addr: a_addr,
            });
        } else {
            let ab = Channel::connected(false);
            let ba = Channel::connected(false);
            a.link = Link::Stream {
                rx: ba.clone(),
                tx: ab.clone(),
                peer_addr: b_addr,
            };
            b.link = Link::Stream {
                rx: ab,
                tx: ba,
                peer_addr: a_addr,
            };
        }
        a.peer_cred = Some(cred);
        b.peer_cred = Some(cred);
        (a, b)
    }

    /// Stream connect to the listening socket with address `server_addr`: returns the server end,
    /// which the caller puts into the listening socket's wait queue.
    pub fn connect_stream(
        &mut self,
        server_addr: &AddrRef,
        server_cred: Ucred,
        cred: Ucred,
    ) -> UnixSocket {
        let down = Channel::connected(self.rx.borrow().passcred);
        // Linux records credentials for an unaccepted peer. accept installs the listener's setting.
        let up = Channel::connected(true);
        let mut server = UnixSocket::new(self.ty);
        // As in Linux's unix_stream_connect, the new end is created in the connecting side's
        // network namespace
        server.net = self.net.clone();
        *server.addr.borrow_mut() = server_addr.borrow().clone();
        server.embryo = true;
        server.link = Link::Stream {
            rx: up.clone(),
            tx: down.clone(),
            peer_addr: self.addr.clone(),
        };
        server.peer_cred = Some(cred);
        self.link = Link::Stream {
            rx: down,
            tx: up,
            peer_addr: server_addr.clone(),
        };
        self.peer_cred = Some(server_cred);
        server
    }

    /// FIONREAD: for streams and seqpacket connections the buffered bytes, for datagrams the length
    /// of the next datagram; None on a listener.
    pub fn unread(&self) -> Option<usize> {
        // unix_inq_len: none on a listener (EINVAL), every byte queued on a stream or
        // seqpacket connection, the next datagram's length
        match &self.link {
            Link::Listening { .. } => None,
            Link::Stream { rx, .. } => Some(rx.borrow().bytes),
            _ => Some(self.rx.borrow().packets.front().map_or(0, |p| p.data.len())),
        }
    }

    /// SIOCOUTQ (`TIOCOUTQ`): queued packet truesize bytes sent by this socket still in peer
    /// buffers (Linux's `unix_outq_len`, returning 0 on listeners and unconnected sockets with
    /// nothing queued).
    pub fn outq(&self) -> usize {
        self.outq.outq.get()
    }

    /// The events ppoll and epoll report, by the same rules as Linux's `unix_poll` and
    /// `unix_dgram_poll`.
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
                // The peer closed with data it had not read (unix_poll's sk_err)
                if r.reset {
                    ev |= POLLERR;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                    if t.eof || t.closed {
                        ev |= POLLHUP;
                    }
                }
                // Linux's unix_poll: writable when sender's sk_wmem_alloc is within sk_sndbuf/4.
                // On peer close, the peer discards its receive queue so outq drops to 0.
                if self.outq.writable() {
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
                if r.reset {
                    ev |= POLLERR;
                }
                if r.eof {
                    ev |= POLLIN | POLLRDNORM | POLLRDHUP;
                    // Both directions shut down (unix_dgram_poll's SHUTDOWN_MASK)
                    if self.shut_wr {
                        ev |= POLLHUP;
                    }
                }
                // unix_dgram_poll: check sender's unix_writable(sk), and when connected to a peer
                // not connected back to this socket, also unix_recvq_full(other).
                let writable = self.outq.writable()
                    && match &self.link {
                        Link::Dgram { tx, .. } => tx
                            .upgrade()
                            .is_none_or(|c| !c.borrow().holds_back(&Rc::downgrade(&self.rx))),
                        _ => true,
                    };
                if writable {
                    ev |= POLLOUT | POLLWRNORM;
                }
                ev
            }
        }
    }

    /// The wake objects poll and epoll register when waiting on it: its own receive queue
    /// (messages of a datagram socket; while listening, an incoming connection and accept taking
    /// one also request a wake of it), its own sender state (write-space drains and `SO_SNDBUF`
    /// increases), both directions of a stream connection, and for a connected datagram socket the
    /// peer's receive queue (`max_dgram_qlen`).
    pub fn wake_objects(&self, out: &mut Vec<wake::Object>) {
        out.push(wake::of(&self.rx));
        out.push(self.outq.wake_object());
        match &self.link {
            Link::Stream { rx, tx, .. } => {
                out.push(wake::of(rx));
                out.push(wake::of(tx));
            }
            Link::Dgram { tx, .. } => {
                if let Some(c) = tx.upgrade() {
                    out.push(wake::of(&c));
                }
            }
            Link::None | Link::Listening { .. } => {}
        }
    }

    /// How many events have arrived in each of the read and write directions (`syscall::poll`'s
    /// data generations).
    pub fn arrivals(&self) -> [u64; 2] {
        let out = self.outq.taken.get();
        match &self.link {
            Link::Listening { .. } => [self.connections, 0],
            Link::Stream { rx, .. } => [rx.borrow().arrivals, out],
            Link::Dgram { tx, .. } => [
                self.rx.borrow().arrivals,
                out.wrapping_add(tx.upgrade().map_or(0, |c| c.borrow().taken)),
            ],
            Link::None => [self.rx.borrow().arrivals, out],
        }
    }

    /// shutdown(2): `how` is SHUT_RD(0), SHUT_WR(1) or SHUT_RDWR(2).
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
        self.wake();
    }

    /// Wakes the threads waiting on this socket ([`UnixSocket::wake_objects`]).
    fn wake(&self) {
        let mut objects = Vec::new();
        self.wake_objects(&mut objects);
        objects.into_iter().for_each(request_wake_object);
    }

    /// The number of the network namespace it is in.
    pub fn net_id(&self) -> u64 {
        self.net
            .as_ref()
            .map_or(crate::nsproxy::INIT_NET_NS, |n| n.id)
    }

    /// Its own address; only the address family when unbound.
    pub fn name(&self) -> Vec<u8> {
        socket_name(&self.addr)
    }

    /// The peer's address, None when not connected.
    pub fn peer_name(&self) -> Option<Vec<u8>> {
        match &self.link {
            Link::Stream { peer_addr, .. } | Link::Dgram { peer_addr, .. } => {
                Some(socket_name(peer_addr))
            }
            Link::None | Link::Listening { .. } => None,
        }
    }
}

impl Drop for UnixSocket {
    /// The peer reads EOF and its writes fail, connections waiting for accept close with it, and
    /// connects waiting for room in the backlog check again ([`UnixSocket::wake_objects`] includes
    /// its own receive queue). A connection closed with data this end had not read, or never
    /// accepted, is reset: the peer's next read once its data is read, `SO_ERROR` and `POLLERR`
    /// report `ECONNRESET` (Linux's `unix_release_sock`).
    fn drop(&mut self) {
        self.wake();
        let had_pending =
            matches!(&self.link, Link::Listening { pending, .. } if !pending.is_empty());
        let stale = match &self.link {
            Link::Stream { rx, tx, .. } => {
                let mut t = tx.borrow_mut();
                t.eof = true;
                t.reset |= self.embryo || !rx.borrow().packets.is_empty();
                drop(t);
                rx.borrow_mut().discard()
            }
            _ => self.rx.borrow_mut().discard(),
        };
        let had_fds = stale.iter().any(|p| !p.fds.is_empty());
        drop(stale);
        if had_fds || had_pending {
            defer_collect();
        }
    }
}

fn socket_name(addr: &AddrRef) -> Vec<u8> {
    let addr = addr.borrow();
    if addr.is_empty() {
        unnamed()
    } else {
        addr.clone()
    }
}

/// The `sockaddr_un` of a socket with no bound address: only the address family.
pub fn unnamed() -> Vec<u8> {
    (AF_UNIX as u16).to_le_bytes().to_vec()
}

// ---- Names ----

/// A name in the address table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Name {
    /// A path address: the inode of the socket node in the store.
    Path(Ino),
    /// An abstract address: the number of its network namespace, the socket type and all bytes
    /// of `sun_path` (starting with NUL). As on Linux, abstract addresses of different network
    /// namespaces or socket types are unrelated (`unix_abstract_hash` mixes the type in), while
    /// path addresses are not kept apart.
    Abstract(u64, i32, Vec<u8>),
}

/// Bound addresses (kernel-wide) and pending server embryos waiting for accept. After a socket
/// closes its weak reference dies, and the address or embryo entry is released with it.
#[derive(Default)]
pub(crate) struct Names {
    map: BTreeMap<Name, Weak<RefCell<Socket>>>,
    next_auto: u32,
    embryos: Vec<Weak<RefCell<Socket>>>,
}

impl Names {
    /// Autobind: as on Linux, allocates in network namespace `net` an abstract address of `\0`
    /// followed by 5 hex digits, free among sockets of type `ty`.
    pub fn autobind(&mut self, net: u64, ty: i32, sock: &Rc<RefCell<Socket>>) -> Option<Vec<u8>> {
        for _ in 0..0x100000 {
            self.next_auto = (self.next_auto + 1) & 0xfffff;
            let mut path = vec![0u8];
            path.extend_from_slice(format!("{:05x}", self.next_auto).as_bytes());
            if self.insert(Name::Abstract(net, ty, path.clone()), sock) {
                return Some(path);
            }
        }
        None
    }

    /// Registers an address; returns false when a live socket already holds it.
    pub fn insert(&mut self, name: Name, sock: &Rc<RefCell<Socket>>) -> bool {
        self.map.retain(|_, w| w.strong_count() > 0);
        if self.lookup(&name).is_some() {
            return false;
        }
        self.map.insert(name, Rc::downgrade(sock));
        true
    }

    pub fn lookup(&self, name: &Name) -> Option<Rc<RefCell<Socket>>> {
        self.map.get(name).and_then(Weak::upgrade)
    }

    /// Records a server embryo queued on a listener so `/proc/net/unix` lists it in the
    /// connecting socket's network namespace without borrowing any listener socket.
    pub fn add_embryo(&mut self, sock: &Rc<RefCell<Socket>>) {
        self.embryos.retain(|w| w.strong_count() > 0);
        self.embryos.push(Rc::downgrade(sock));
    }

    /// Removes an embryo once accepted.
    pub fn remove_embryo(&mut self, sock: &Rc<RefCell<Socket>>) {
        let ptr = Rc::as_ptr(sock);
        self.embryos
            .retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), ptr));
    }
}

// ---- Channels ----

/// One message: one write on a stream socket, or one datagram.
pub(crate) struct Packet {
    pub data: Vec<u8>,
    /// The bytes of a stream socket already read.
    pub off: usize,
    /// The open files `SCM_RIGHTS` brought.
    pub fds: Vec<FileRef>,
    /// The recorded sender credentials, or `Ucred::UNSTAMPED`.
    pub cred: Ucred,
    /// Fallback static sender address (used when `outq` is `None`).
    pub from: Vec<u8>,
    /// Shared sender state (`addr` resolved at receive time + `SIOCOUTQ` counter uncharged on drop).
    pub outq: Option<Rc<SenderState>>,
}

impl Packet {
    fn remaining(&self) -> usize {
        self.data.len() - self.off
    }

    fn truesize(&self) -> usize {
        self.outq.as_ref().map_or_else(
            || packet_truesize(self.data.len()),
            |s| s.truesize(self.data.len()),
        )
    }

    fn sender_addr(&self) -> Vec<u8> {
        self.outq
            .as_ref()
            .map_or_else(|| self.from.clone(), |s| s.addr.borrow().clone())
    }

    fn uncharge(&mut self, n: usize) {
        if let Some(s) = self.outq.take() {
            s.uncharge(n);
        }
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        let ts = self.truesize();
        self.uncharge(ts);
    }
}

/// Data read.
pub(crate) struct Received {
    pub data: Vec<u8>,
    /// The full length of the message (greater than `data` when a datagram was truncated).
    pub len: usize,
    pub fds: Vec<FileRef>,
    pub cred: Ucred,
    pub from: Vec<u8>,
}

/// The messages buffered in one direction.
#[derive(Default)]
pub(crate) struct Channel {
    /// The receiving socket's SO_PASSCRED flag, also set while a connection is awaiting accept.
    pub passcred: bool,
    pub packets: VecDeque<Packet>,
    /// Payload bytes not yet read (`FIONREAD`).
    pub bytes: usize,
    /// No new data will come (the sender closed or did `SHUT_WR`, or the receiver did `SHUT_RD`):
    /// EOF once everything is read.
    pub eof: bool,
    /// The receiver no longer reads (closed or `SHUT_RD`): writes fail.
    pub closed: bool,
    /// The number of messages put in: as on Linux, every arrival triggers the receiver's `EPOLLET`
    /// once more.
    pub arrivals: u64,
    /// Datagram receive-queue drain counter for `EPOLLET` `POLLOUT` on peers held back by
    /// `max_qlen` (`SenderState::taken` tracks per-sender `sk_wmem_alloc` drains).
    pub taken: u64,
    /// A datagram socket's receive queue: the messages it holds before senders wait (Linux's
    /// `sk_max_ack_backlog`, from `net.unix.max_dgram_qlen` when the socket was created), except
    /// its own peer ([`Channel::holds_back`]); None for stream channels.
    pub max_qlen: Option<usize>,
    /// The receive queue of the socket this one's datagram socket is connected to (Linux's
    /// `unix_peer`), `None` when unconnected.
    pub peer: Option<Weak<RefCell<Channel>>>,
    /// Whether this datagram socket is in Linux's `TCP_ESTABLISHED` state for `/proc/net/unix`
    /// (`SS_CONNECTED` = 3 vs `SS_UNCONNECTED` = 1): set on both ends by `connect()` and
    /// `socketpair()`, cleared on disconnect and on the previous peer when unconnected.
    pub established: bool,
    /// The sending end of a connection closed without reading everything sent to it, or before
    /// it was accepted, or a bidirectional datagram peer disconnected with unread messages: the
    /// receiver's pending `ECONNRESET` (its `sk_err`), cleared once reported
    /// ([`Channel::take_reset`]).
    pub reset: bool,
}

pub(crate) type ChannelRef = Rc<RefCell<Channel>>;

impl Channel {
    /// One direction of a stream or seqpacket connection.
    fn connected(passcred: bool) -> ChannelRef {
        Rc::new(RefCell::new(Self {
            passcred,
            ..Self::default()
        }))
    }

    /// Reports the pending `ECONNRESET` once (Linux's `sock_error`).
    pub fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }

    pub fn push(&mut self, p: Packet) {
        self.bytes += p.remaining();
        self.packets.push_back(p);
        self.arrivals += 1;
        request_wake_object(wake::of_value(self));
    }

    /// Stream read: takes up to `max` bytes in order. As on Linux, it stops after a message
    /// carrying open files is read. With SO_PASSCRED, a change of credentials also ends the read.
    pub fn read_stream(&mut self, max: usize, peek: bool, passcred: bool) -> Received {
        let mut data = Vec::new();
        let mut fds = Vec::new();
        let mut cred = None;
        let from = self
            .packets
            .front()
            .map(Packet::sender_addr)
            .unwrap_or_default();
        let mut i = 0;
        let mut popped = false;
        while let Some(p) = self.packets.get_mut(i) {
            if passcred && cred.is_some_and(|c| c != p.cred) {
                break;
            }
            // Even a zero-length receive takes the first packet's files and leaves its data.
            let room = max - data.len();
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
                    // Dropping `pkt` uncharges the sender's `SenderState` and wakes writers.
                    let _pkt = self.packets.pop_front().unwrap();
                    popped = true;
                }
            }
            if had_fds || data.len() >= max {
                break;
            }
        }
        if !peek && (!data.is_empty() || popped) {
            request_wake_object(wake::of_value(self));
        }
        Received {
            len: data.len(),
            data,
            fds,
            cred: cred.unwrap_or(Ucred::UNSTAMPED),
            from,
        }
    }

    /// Takes one whole message (datagram, `SOCK_SEQPACKET`), dropping what exceeds `max`.
    pub fn read_packet(&mut self, max: usize, peek: bool) -> Option<Received> {
        let (data, len, fds, cred, from) = if peek {
            let p = self.packets.front()?;
            let n = p.data.len().min(max);
            (
                p.data[..n].to_vec(),
                p.data.len(),
                p.fds.clone(),
                p.cred,
                p.sender_addr(),
            )
        } else {
            let mut p = self.packets.pop_front()?;
            let len = p.data.len();
            let ts = p.truesize();
            let from = p.sender_addr();
            self.bytes -= len;
            // Taking the payload changes truesize. Release the original charge before the move.
            p.uncharge(ts);
            let mut data = std::mem::take(&mut p.data);
            data.truncate(max);
            let fds = std::mem::take(&mut p.fds);
            let cred = p.cred;
            if self.max_qlen.is_some() {
                self.taken = self.taken.wrapping_add(1);
            }
            request_wake_object(wake::of_value(self));
            (data, len, fds, cred, from)
        };
        Some(Received {
            data,
            len,
            fds,
            cred,
            from,
        })
    }

    /// Whether a datagram from the socket receiving into `sender` is allowed by `unix_may_send`:
    /// a datagram socket connected to a specific peer accepts datagrams only from that peer.
    pub fn may_receive_from(&self, sender: &Weak<RefCell<Channel>>) -> bool {
        self.peer.as_ref().is_none_or(|p| Weak::ptr_eq(p, sender))
    }

    /// Whether a datagram from the socket receiving into `sender` must wait (Linux's
    /// `unix_recvq_full` for a receiver whose peer is not that socket): this queue holds more than
    /// its limit.
    pub fn holds_back(&self, sender: &Weak<RefCell<Channel>>) -> bool {
        self.max_qlen.is_some_and(|m| self.packets.len() > m)
            && !self.peer.as_ref().is_some_and(|p| Weak::ptr_eq(p, sender))
    }

    /// Empties queued messages without closing the channel (Linux's `unix_dgram_disconnected`),
    /// returning them so the caller can drop any open files after borrows end, and waking writers.
    fn purge(&mut self) -> VecDeque<Packet> {
        let stale = std::mem::take(&mut self.packets);
        if !stale.is_empty() {
            self.bytes = 0;
            if self.max_qlen.is_some() {
                self.taken = self.taken.wrapping_add(1);
            }
            request_wake_object(wake::of_value(self));
        }
        stale
    }

    /// The receiver closed: discard the buffered messages. The messages are handed to the caller
    /// to drop after its borrows are released, because the open files in them may need other
    /// channels when dropped.
    fn discard(&mut self) -> VecDeque<Packet> {
        self.closed = true;
        self.bytes = 0;
        std::mem::take(&mut self.packets)
    }
}

// ---- Received and in-flight files ----

/// The files imported by sendmsg before its first packet is queued. Enumeration upgrades
/// the weak registration only while snapshotting under the BKL.
pub(crate) struct SendingRights {
    files: Vec<FileRef>,
}

impl SendingRights {
    pub fn track(k: &mut Kernel, files: Vec<FileRef>) -> Option<Rc<Self>> {
        if files.is_empty() {
            return None;
        }
        let pending = Rc::new(Self { files });
        k.unix_sending.retain(|s| s.strong_count() != 0);
        k.unix_sending.push(Rc::downgrade(&pending));
        Some(pending)
    }

    pub fn into_packet(self: Rc<Self>, k: &mut Kernel, channel: &ChannelRef) -> Vec<FileRef> {
        note_inflight(k, channel);
        let mut pending = Rc::into_inner(self).expect("sendmsg owns its pending SCM_RIGHTS");
        std::mem::take(&mut pending.files)
    }
}

impl Drop for SendingRights {
    fn drop(&mut self) {
        // A failed send may have held the last external reference to a queued socket cycle.
        if !self.files.is_empty() {
            defer_collect();
        }
    }
}

/// SCM_RIGHTS numbers have been reserved and encoded in the control message. They remain
/// unavailable to other threads until recvmsg finishes writing its result. The kernel tracks
/// this delivery weakly so in-flight file enumerations still see its files during a page fill.
pub(crate) struct PendingRights {
    table: Weak<RefCell<FdTable>>,
    files: Vec<(i32, FileRef)>,
}

impl PendingRights {
    pub fn new(table: &Rc<RefCell<FdTable>>) -> Self {
        Self {
            table: Rc::downgrade(table),
            files: Vec::new(),
        }
    }

    pub fn push(&mut self, fd: i32, file: FileRef) {
        self.files.push((fd, file));
    }

    pub fn track(self, k: &mut Kernel) -> Option<Rc<Self>> {
        if self.files.is_empty() {
            return None;
        }
        let pending = Rc::new(self);
        k.unix_receiving
            .retain(|delivery| delivery.strong_count() != 0);
        k.unix_receiving.push(Rc::downgrade(&pending));
        Some(pending)
    }

    pub fn install(self: Rc<Self>, table: &mut FdTable, cloexec: bool) {
        // Enumerators hold a strong reference only while snapshotting under the BKL.
        let mut pending = Rc::into_inner(self).expect("recvmsg owns its pending SCM_RIGHTS");
        for (fd, file) in pending.files.drain(..) {
            table.fill(fd, file, cloexec);
        }
    }
}

impl Drop for PendingRights {
    fn drop(&mut self) {
        if !self.files.is_empty() {
            if let Some(table) = self.table.upgrade() {
                let mut table = table.borrow_mut();
                for (fd, _) in &self.files {
                    table.unreserve(*fd);
                }
            }
            if self.files.iter().any(|(_, file)| is_unix_file(file)) {
                defer_collect();
            }
        }
    }
}

/// In-flight sockets (those only in `SCM_RIGHTS` messages) and pending server embryos waiting for
/// `accept`: `/proc/net/unix` lists them too (filtered by each socket's own `net` namespace).
/// Does not borrow any socket `RefCell`.
pub(crate) fn inflight_sockets(k: &Kernel) -> Vec<Rc<RefCell<Socket>>> {
    let mut out = collect_inflight(k, |f| match &f.borrow().kind {
        FileKind::Socket(s) => Some(s.clone()),
        _ => None,
    })
    .expect("in-flight AF_UNIX channels are available");
    out.extend(k.unix.embryos.iter().filter_map(Weak::upgrade));
    out
}

/// The open files carried by messages in flight (each as many times as it is carried).
pub(crate) fn inflight_files(k: &Kernel) -> Vec<FileRef> {
    try_inflight_files(k).expect("in-flight AF_UNIX channels are available")
}

/// Mount collection can retry when a channel is borrowed during teardown.
pub(crate) fn try_inflight_files(k: &Kernel) -> Option<Vec<FileRef>> {
    collect_inflight(k, |file| Some(file.clone()))
}

fn collect_inflight<T>(
    k: &Kernel,
    mut select: impl FnMut(&FileRef) -> Option<T>,
) -> Option<Vec<T>> {
    let mut out = Vec::new();
    for sending in k.unix_sending.iter().filter_map(Weak::upgrade) {
        out.extend(sending.files.iter().filter_map(&mut select));
    }
    for delivery in k.unix_receiving.iter().filter_map(Weak::upgrade) {
        out.extend(delivery.files.iter().filter_map(|(_, file)| select(file)));
    }
    for channel in k.unix_inflight.iter().filter_map(Weak::upgrade) {
        for packet in &channel.try_borrow().ok()?.packets {
            out.extend(packet.fds.iter().filter_map(&mut select));
        }
    }
    Some(out)
}

// ---- /proc/net/unix ----

/// `/proc/net/unix`: all AF_UNIX sockets. Abstract addresses start with `@`, and the NULs in them
/// are shown as `@` too.
pub(crate) fn proc_listing(sockets: &[(crate::syscall::SocketRef, u32)]) -> String {
    let mut s = String::from("Num       RefCount Protocol Flags    Type St Inode Path\n");
    for (sock, _) in sockets {
        let b = sock.borrow();
        let Socket::Unix(u) = &*b else {
            continue;
        };
        let addr = u.addr.borrow();
        // Connections not yet accepted: as on Linux they belong to the connecting socket's network
        // namespace, have no inode, carry the listening address, and show state SS_CONNECTING (2).
        if u.embryo {
            line(&mut s, 0, u.ty, 2, 0, &addr);
            continue;
        }
        // __SO_ACCEPTCON; SS_UNCONNECTED, SS_CONNECTED
        let (flags, state) = match &u.link {
            Link::Listening { .. } => (0x10000, 1),
            Link::Stream { .. } => (0, 3),
            Link::Dgram { .. } | Link::None => (
                0,
                if u.ty == SOCK_DGRAM && u.rx.borrow().established {
                    3
                } else {
                    1
                },
            ),
        };
        let ino = crate::pseudo::socket_ino(sock);
        line(&mut s, flags, u.ty, state, ino, &addr);
    }
    s
}

/// One line of `/proc/net/unix`.
fn line(s: &mut String, flags: u32, ty: i32, state: u32, ino: u64, addr: &[u8]) {
    use std::fmt::Write as _;
    write!(
        s,
        "0000000000000000: 00000002 00000000 {flags:08X} {ty:04X} {state:02X} {ino:5}"
    )
    .unwrap();
    match addr.get(2..) {
        Some([0, name @ ..]) => {
            let name: Vec<u8> = name
                .iter()
                .map(|&c| if c == 0 { b'@' } else { c })
                .collect();
            write!(s, " @{}", String::from_utf8_lossy(&name)).unwrap();
        }
        Some(path) if !path.is_empty() => {
            let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            write!(s, " {}", String::from_utf8_lossy(&path[..end])).unwrap();
        }
        _ => {}
    }
    s.push('\n');
}

// ---- Garbage collection ----

thread_local! {
    /// An AF_UNIX socket or packet carrying open files was dropped on a path without `&mut Kernel`
    /// (`UnixSocket::drop`, AIO poll completion, etc.); collect cycles at the next sweep.
    #[allow(clippy::missing_const_for_thread_local)]
    static DEFERRED_COLLECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Schedules an AF_UNIX cycle collection on the current thread's next sweep (`collect_deferred`).
pub(crate) fn defer_collect() {
    DEFERRED_COLLECT.set(true);
}

/// Takes and clears the deferred cycle-collection flag.
pub(crate) fn take_deferred() -> bool {
    DEFERRED_COLLECT.replace(false)
}

/// Runs [`collect`] if a drop path scheduled one and any in-flight channels remain.
pub(crate) fn collect_deferred(k: &mut Kernel) {
    if take_deferred() && !k.unix_inflight.is_empty() {
        collect(k);
    }
}

/// Whether `f` is an AF_UNIX socket open file.
pub(crate) fn is_unix_file(f: &FileRef) -> bool {
    unix_socket(f).is_some()
}

/// Records a channel whose messages carry open files; [`collect`] finds in-flight sockets here.
fn note_inflight(k: &mut Kernel, c: &ChannelRef) {
    let weak = Rc::downgrade(c);
    if !k.unix_inflight.iter().any(|w| w.ptr_eq(&weak)) {
        k.unix_inflight.push(weak);
    }
}

/// Some open files were closed (close, dup2 over an fd, execve and so on): releases them, and
/// collects in-flight cycles when any of them is a unix socket.
pub(crate) fn released(k: &mut Kernel, files: Vec<FileRef>) {
    let unix = files.iter().any(is_unix_file);
    drop(files);
    if unix || take_deferred() {
        collect(k);
    }
}

/// Collects in-flight sockets the program cannot reach (Linux's unix_gc). Candidates are unix
/// sockets all of whose references come from in-flight messages; references starting from the
/// candidates' receive queues do not count, candidates with references left are reachable from
/// outside, and so are the candidates in their receive queues. The remaining candidates have their
/// receive queues emptied, which breaks the cycles. A socket with an extra reference temporarily
/// held elsewhere (such as by a thread blocked on it) is not a candidate, so this can only collect
/// too little, never wrongly.
pub(crate) fn collect(k: &mut Kernel) {
    loop {
        take_deferred();
        if !collect_once(k) {
            return;
        }
        if !take_deferred() || k.unix_inflight.is_empty() {
            return;
        }
    }
}

fn collect_once(k: &mut Kernel) -> bool {
    type Key = *const RefCell<OpenFile>;
    k.unix_inflight.retain(|w| {
        w.upgrade()
            .is_some_and(|c| c.borrow().packets.iter().any(|p| !p.fds.is_empty()))
    });
    // In-flight open files and how many times each is referenced by in-flight messages
    let mut inflight: HashMap<Key, (FileRef, usize)> = HashMap::new();
    for c in k.unix_inflight.iter().filter_map(Weak::upgrade) {
        for p in &c.borrow().packets {
            for f in &p.fds {
                inflight
                    .entry(Rc::as_ptr(f))
                    .or_insert_with(|| (f.clone(), 0))
                    .1 += 1;
            }
        }
    }
    // The reference counts also include our own one held above
    let mut candidates: HashMap<Key, usize> = inflight
        .iter()
        .filter(|(_, (f, n))| Rc::strong_count(f) == n + 1 && unix_socket(f).is_some())
        .map(|(&key, &(_, n))| (key, n))
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let children = |key: Key| -> Vec<Key> {
        let Some(s) = unix_socket(&inflight[&key].0) else {
            return Vec::new();
        };
        let Socket::Unix(u) = &*s.borrow() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in inboxes(u) {
            for p in &c.borrow().packets {
                out.extend(p.fds.iter().map(Rc::as_ptr));
            }
        }
        out
    };
    let keys: Vec<Key> = candidates.keys().copied().collect();
    for &key in &keys {
        for child in children(key) {
            if let Some(n) = candidates.get_mut(&child) {
                *n -= 1;
            }
        }
    }
    let mut work: Vec<Key> = keys.iter().copied().filter(|k| candidates[k] > 0).collect();
    let mut reachable: HashSet<Key> = work.iter().copied().collect();
    while let Some(key) = work.pop() {
        for child in children(key) {
            if candidates.contains_key(&child) && reachable.insert(child) {
                work.push(child);
            }
        }
    }
    // Drop the messages after all borrows are released: the open files in them need other
    // channels when dropped
    let mut garbage = Vec::new();
    for key in keys.into_iter().filter(|k| !reachable.contains(k)) {
        let Some(s) = unix_socket(&inflight[&key].0) else {
            continue;
        };
        if let Socket::Unix(u) = &*s.borrow() {
            for c in inboxes(u) {
                let mut c = c.borrow_mut();
                c.bytes = 0;
                garbage.extend(std::mem::take(&mut c.packets));
            }
        }
    }
    let freed = !garbage.is_empty();
    drop(inflight);
    drop(garbage);
    freed
}

/// The channels holding messages sent to `u`: a datagram socket's receive queue, the receive
/// direction of a stream connection, and for a listening socket also those of the connections
/// waiting for accept (Linux's GC also counts them as received by the listening socket).
fn inboxes(u: &UnixSocket) -> Vec<ChannelRef> {
    let mut out = vec![u.rx.clone()];
    match &u.link {
        Link::Stream { rx, .. } => out.push(rx.clone()),
        Link::Listening { pending, .. } => {
            for s in pending {
                if let Socket::Unix(e) = &*s.borrow()
                    && let Link::Stream { rx, .. } = &e.link
                {
                    out.push(rx.clone());
                }
            }
        }
        _ => {}
    }
    out
}

/// Returns the socket of `f` when it is an AF_UNIX socket.
fn unix_socket(f: &FileRef) -> Option<Rc<RefCell<Socket>>> {
    match &f.borrow().kind {
        FileKind::Socket(s) if matches!(&*s.borrow(), Socket::Unix(_)) => Some(s.clone()),
        _ => None,
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
            cred: Ucred::of(pid, 0, 0),
            from: Vec::new(),
            outq: None,
        }
    }

    /// The open files of a pair of connected stream sockets, and the channel from `a` to `b`.
    fn pair() -> (FileRef, FileRef, ChannelRef) {
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(1, 0, 0));
        let Link::Stream { tx, .. } = &a.link else {
            unreachable!()
        };
        let tx = tx.clone();
        let file = |u| {
            let s = Rc::new(RefCell::new(Socket::Unix(u)));
            OpenFile::new(FileKind::Socket(s), oflags::O_RDWR)
        };
        (file(a), file(b), tx)
    }

    fn send(k: &mut Kernel, c: &ChannelRef, f: &FileRef) {
        let mut p = packet(b"x", 1);
        let rights = SendingRights::track(k, vec![f.clone()]).unwrap();
        p.fds = rights.into_packet(k, c);
        c.borrow_mut().push(p);
    }

    #[test]
    fn collects_sockets_only_reachable_from_their_own_queue() {
        let mut k = Kernel::new(crate::process::Tier::Interpreter);
        // b is in its own receive queue
        let (a, b, to_b) = pair();
        send(&mut k, &to_b, &b);
        let gone = Rc::downgrade(&b);
        drop((a, b, to_b));
        // In-flight c has d in its receive queue, and c itself is in the queue of a socket still
        // open: both are reachable
        let (keep, keep_peer, to_keep_peer) = pair();
        let (c, c_peer, to_c_peer) = pair();
        let (d, d_peer, _) = pair();
        send(&mut k, &to_c_peer, &d_peer);
        send(&mut k, &to_keep_peer, &c_peer);
        let (kept_c, kept_d) = (Rc::downgrade(&c_peer), Rc::downgrade(&d_peer));
        drop((c, c_peer, to_c_peer, d, d_peer, to_keep_peer));
        collect(&mut k);
        assert!(gone.upgrade().is_none());
        assert!(kept_c.upgrade().is_some() && kept_d.upgrade().is_some());
        assert_eq!(inflight_sockets(&k).len(), 2);
        // Once the outermost is closed too, the whole chain is collected
        drop((keep, keep_peer));
        collect(&mut k);
        assert!(kept_c.upgrade().is_none() && kept_d.upgrade().is_none());
        assert!(inflight_sockets(&k).is_empty());
    }

    #[test]
    fn failed_send_releases_the_last_external_reference_to_a_socket_cycle() {
        let mut k = Kernel::new(crate::process::Tier::Interpreter);
        let (peer, file, channel) = pair();
        send(&mut k, &channel, &file);
        let weak = Rc::downgrade(&file);
        let pending = SendingRights::track(&mut k, vec![file]).unwrap();
        drop((peer, channel));
        collect(&mut k);
        assert!(weak.upgrade().is_some());

        drop(pending);
        collect_deferred(&mut k);
        assert!(weak.upgrade().is_none());
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

        let peek = c.read_stream(100, true, true);
        assert_eq!(peek.data, b"abcd");
        assert_eq!(peek.fds.len(), 1);
        assert_eq!(c.bytes, 8);

        let r = c.read_stream(3, false, true);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"abc"[..], 1));
        // The rest of a message with open files is read as usual, but the open files are
        // delivered only once
        let r = c.read_stream(100, false, true);
        assert_eq!((r.data.as_slice(), r.fds.len()), (&b"def"[..], 0));
        // SO_PASSCRED leaves the second sender for the next read
        let r = c.read_stream(100, false, true);
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
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(7, 0, 0));
        assert_eq!(b.peer_cred, Some(Ucred::of(7, 0, 0)));
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
            assert_eq!(rx.borrow_mut().read_stream(10, false, false).data, b"bye");
            assert!(tx.borrow().closed);
        }
    }

    #[test]
    fn abstract_names_are_released_with_the_socket() {
        let mut names = Names::default();
        let s = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        let name = Name::Abstract(0, SOCK_STREAM, b"\0test".to_vec());
        assert!(names.insert(name.clone(), &s));
        assert!(!names.insert(name.clone(), &s));
        let auto = names.autobind(0, SOCK_STREAM, &s).unwrap();
        assert_eq!(auto, b"\x0000001");
        drop(s);
        assert!(names.lookup(&name).is_none());
        let t = Rc::new(RefCell::new(Socket::Unix(UnixSocket::new(SOCK_STREAM))));
        assert!(names.insert(name, &t));
    }

    #[test]
    fn dgram_may_receive_from_tracks_peer_reconnection() {
        let a = Rc::new(RefCell::new(Channel::default()));
        let b = Rc::new(RefCell::new(Channel::default()));
        let c = Rc::new(RefCell::new(Channel::default()));
        let wa = Rc::downgrade(&a);
        let wc = Rc::downgrade(&c);
        assert!(b.borrow().may_receive_from(&wa));
        b.borrow_mut().peer = Some(wa.clone());
        assert!(b.borrow().may_receive_from(&wa));
        assert!(!b.borrow().may_receive_from(&wc));
        b.borrow_mut().peer = Some(wc.clone());
        assert!(!b.borrow().may_receive_from(&wa));
        assert!(b.borrow().may_receive_from(&wc));
        drop(c);
        // Peer socket closed after reconnect: still not connected back to `a`
        assert!(!b.borrow().may_receive_from(&wa));
    }

    #[test]
    fn socket_drop_schedules_deferred_cycle_collection() {
        let _ = take_deferred();
        let mut k = Kernel::new(crate::process::Tier::Interpreter);
        let (outer_a, outer_b, to_outer_b) = pair();
        let (cyc_a, cyc_b, to_cyc_b) = pair();
        send(&mut k, &to_cyc_b, &cyc_b);
        send(&mut k, &to_outer_b, &cyc_b);
        let gone = Rc::downgrade(&cyc_b);
        drop((cyc_a, cyc_b, to_cyc_b, to_outer_b));
        // Closing the outer socket drops its queued packet holding `cyc_b` and schedules GC
        drop((outer_a, outer_b));
        assert!(gone.upgrade().is_some());
        collect_deferred(&mut k);
        assert!(gone.upgrade().is_none());
        assert!(inflight_sockets(&k).is_empty());
    }

    #[test]
    fn dgram_disconnect_purges_resets_peer_and_updates_established_state() {
        let mut a = UnixSocket::new(SOCK_DGRAM);
        let mut b = UnixSocket::new(SOCK_DGRAM);
        let c = UnixSocket::new(SOCK_DGRAM);
        assert!(!a.rx.borrow().established && !b.rx.borrow().established);

        // Unidirectional a -> b marks both established; disconnecting clears both.
        assert!(
            a.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&b.rx),
                peer_addr: b.addr.clone(),
            })
            .is_empty()
        );
        assert!(a.rx.borrow().established && b.rx.borrow().established);
        assert!(a.set_dgram_link(Link::None).is_empty());
        assert!(!a.rx.borrow().established && !b.rx.borrow().established);

        // Mutual a <-> b: b disconnecting with unread data purges b and resets a.
        assert!(
            a.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&b.rx),
                peer_addr: b.addr.clone(),
            })
            .is_empty()
        );
        assert!(
            b.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&a.rx),
                peer_addr: a.addr.clone(),
            })
            .is_empty()
        );
        let mut p = packet(b"hello", 1);
        let f = OpenFile::new(FileKind::Host(1), oflags::O_RDONLY);
        p.fds.push(f.clone());
        p.outq = Some(a.outq.charge(p.truesize()));
        b.rx.borrow_mut().push(p);
        assert_eq!(a.outq(), packet_truesize(5));

        // Reconnecting to the same peer does not purge or reset.
        assert!(
            b.set_dgram_link(Link::Dgram {
                tx: Rc::downgrade(&a.rx),
                peer_addr: a.addr.clone(),
            })
            .is_empty()
        );
        assert_eq!(b.rx.borrow().bytes, 5);
        assert!(!a.rx.borrow().reset);

        // Reconnecting b to c purges b's queue, returns its fds, uncharges a.outq, and resets a.
        let dropped = b.set_dgram_link(Link::Dgram {
            tx: Rc::downgrade(&c.rx),
            peer_addr: c.addr.clone(),
        });
        assert_eq!(dropped.len(), 1);
        assert_eq!(b.rx.borrow().bytes, 0);
        assert_eq!(a.outq(), 0);
        assert!(a.rx.borrow().established);
        assert!(b.rx.borrow().established && c.rx.borrow().established);
        assert_ne!(a.readiness() & POLLERR, 0);
        assert!(a.rx.borrow_mut().take_reset());
        assert_eq!(a.readiness() & POLLERR, 0);

        // Self-connect and self-disconnect do not panic or self-reset.
        let mut s = UnixSocket::new(SOCK_DGRAM);
        let own = Rc::downgrade(&s.rx);
        let own_addr = s.addr.clone();
        assert!(
            s.set_dgram_link(Link::Dgram {
                tx: own,
                peer_addr: own_addr,
            })
            .is_empty()
        );
        s.rx.borrow_mut().push(packet(b"self", 1));
        assert!(s.set_dgram_link(Link::None).is_empty());
        assert_eq!(s.rx.borrow().bytes, 0);
        assert!(!s.rx.borrow().reset && !s.rx.borrow().established);
    }

    #[test]
    fn outq_tracks_truesize_across_partial_reads_zero_byte_packets_and_drop() {
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(1, 0, 0));
        let Link::Stream { tx, .. } = &a.link else {
            unreachable!()
        };
        let mut p = packet(b"abcdef", 1);
        let ts = a.outq.truesize(p.data.len());
        assert_eq!(ts, 768);
        p.outq = Some(a.outq.charge(ts));
        tx.borrow_mut().push(p);
        assert_eq!(a.outq(), ts);
        // Peek and partial stream reads retain packet truesize until the packet is fully consumed
        // or dropped (matching Linux's sk_wmem_alloc), while FIONREAD reports exact payload bytes.
        let Link::Stream { rx, .. } = &b.link else {
            unreachable!()
        };
        assert_eq!(b.unread(), Some(6));
        assert_eq!(rx.borrow_mut().read_stream(2, true, false).data, b"ab");
        assert_eq!(a.outq(), ts);
        assert_eq!(rx.borrow_mut().read_stream(2, false, false).data, b"ab");
        assert_eq!(b.unread(), Some(4));
        assert_eq!(a.outq(), ts);
        // Dropping the receiver discards the remainder and returns outq to zero.
        drop(b);
        assert_eq!(a.outq(), 0);

        // Zero-byte datagrams charge truesize and cap a mutual socketpair at 278 packets.
        let (da, db) = UnixSocket::pair(SOCK_DGRAM, Ucred::of(1, 0, 0));
        let mut queued = 0;
        while da.outq.has_room() {
            let mut zp = packet(b"", 1);
            zp.outq = Some(da.outq.charge(da.outq.truesize(0)));
            db.rx.borrow_mut().push(zp);
            queued += 1;
        }
        assert_eq!(queued, 278);
        assert_eq!(da.outq(), 278 * 768);
        assert_eq!(db.unread(), Some(0));
        assert_eq!(da.readiness() & POLLOUT, 0);
        db.rx.borrow_mut().read_packet(0, false).unwrap();
        assert_eq!(da.outq(), 277 * 768);
        drop(db);
        assert_eq!(da.outq(), 0);
    }

    #[test]
    fn draining_zero_byte_stream_scm_rights_updates_writable_and_taken() {
        let (sa, sb) = UnixSocket::pair(SOCK_STREAM, Ucred::of(1, 0, 0));
        let Link::Stream { tx, .. } = &sa.link else {
            unreachable!()
        };
        let Link::Stream { rx, .. } = &sb.link else {
            unreachable!()
        };
        let dummy = OpenFile::new(FileKind::Host(1), oflags::O_RDONLY);
        let mut queued = 0;
        while sa.outq.has_room() {
            let mut p = packet(b"", 1);
            p.fds.push(dummy.clone());
            p.outq = Some(sa.outq.charge(sa.outq.truesize(0)));
            tx.borrow_mut().push(p);
            queued += 1;
        }
        assert_eq!(queued, 278);
        assert!(!sa.outq.has_room());
        assert_eq!(sa.readiness() & POLLOUT, 0);
        assert_eq!(sa.outq.taken.get(), 0);

        // Draining one zero-byte packet restores has_room() even before writable() flips.
        let first = rx.borrow_mut().read_stream(16, false, false);
        assert!(first.data.is_empty() && first.fds.len() == 1);
        assert!(sa.outq.has_room());
        assert_eq!(sa.outq(), 277 * 768);

        // Draining past the quarter-capacity threshold restores POLLOUT and advances `taken`.
        let mut drained = 1;
        while sa.readiness() & POLLOUT == 0 {
            let r = rx.borrow_mut().read_stream(16, false, false);
            assert!(r.data.is_empty() && r.fds.len() == 1);
            drained += 1;
        }
        assert!(drained > 1 && drained < queued);
        assert!(sa.outq.writable());
        assert!(sa.arrivals()[1] > 0);
    }

    #[test]
    fn sender_addr_updates_at_receive_and_peer_query_time() {
        let (a, b) = UnixSocket::pair(SOCK_DGRAM, Ucred::of(1, 0, 0));
        let mut p = packet(b"hi", 1);
        p.outq = Some(a.outq.charge(p.truesize()));
        b.rx.borrow_mut().push(p);
        assert_eq!(b.peer_name(), Some(unnamed()));

        // Binding after send updates both getpeername and the queued packet's receive-time addr.
        let mut bound = unnamed();
        bound.extend_from_slice(b"\0late-bind");
        *a.addr.borrow_mut() = bound.clone();
        assert_eq!(b.peer_name(), Some(bound.clone()));

        // Dropping the sender before receive still preserves its last bound address on the packet.
        drop(a);
        let received = b.rx.borrow_mut().read_packet(16, false).unwrap();
        assert_eq!(received.from, bound);
    }

    #[test]
    fn cross_netns_pending_embryo_appears_in_client_namespace() {
        let mut k = Kernel::new(crate::process::Tier::Interpreter);
        let client_ns = Rc::new(crate::nsproxy::NetNs {
            id: 2,
            owner: crate::cred::UserNs::init(),
            host_net: Some([0, 2]),
            lo: RefCell::default(),
            devs: RefCell::default(),
            nft: Default::default(),
            listeners: Default::default(),
        });

        // Listener lives in the initial network namespace; client lives in `client_ns`.
        let mut listener = UnixSocket::new(SOCK_STREAM);
        let mut srv_addr = unnamed();
        srv_addr.extend_from_slice(b"/tmp/cross-ns.sock\0");
        *listener.addr.borrow_mut() = srv_addr.clone();
        let mut client = UnixSocket::new(SOCK_STREAM);
        client.net = Some(client_ns.clone());
        let embryo = client.connect_stream(&listener.addr, Ucred::of(1, 0, 0), Ucred::of(2, 0, 0));
        let embryo_sock = Rc::new(RefCell::new(Socket::Unix(embryo)));
        listener.link = Link::Listening {
            backlog: 4,
            pending: VecDeque::from([embryo_sock.clone()]),
            cred: Ucred::of(1, 0, 0),
        };
        k.unix.add_embryo(&embryo_sock);
        let listener_sock = Rc::new(RefCell::new(Socket::Unix(listener)));
        let client_sock = Rc::new(RefCell::new(Socket::Unix(client)));

        // inflight_sockets returns the embryo without borrowing any socket RefCell.
        let _listener_mut = listener_sock.borrow_mut();
        let inflight = inflight_sockets(&k);
        assert_eq!(inflight.len(), 1);
        assert!(Rc::ptr_eq(&inflight[0], &embryo_sock));
        drop(_listener_mut);

        // Listener's netns sees only the listening socket (state 01), NOT the cross-netns embryo.
        let init_listing = proc_listing(&[(listener_sock.clone(), 0)]);
        assert!(init_listing.contains("00010000 0001 01"));
        assert!(!init_listing.contains("00000000 0001 02     0 /tmp/cross-ns.sock"));

        // Client's netns sees its connected client (state 03) AND the state-02 inode-0 embryo.
        let client_listing = proc_listing(&[(client_sock.clone(), 0), (embryo_sock.clone(), 0)]);
        assert!(client_listing.contains("00000000 0001 02     0 /tmp/cross-ns.sock"));

        // Once accepted, removing the embryo clears it from inflight_sockets and transitions state.
        if let Socket::Unix(e) = &mut *embryo_sock.borrow_mut() {
            e.embryo = false;
        }
        k.unix.remove_embryo(&embryo_sock);
        assert!(inflight_sockets(&k).is_empty());
        let after_accept = proc_listing(&[(embryo_sock.clone(), 0)]);
        assert!(!after_accept.contains("00000000 0001 02     0"));
        assert!(after_accept.contains("00000000 0001 03"));
        assert!(after_accept.contains("/tmp/cross-ns.sock"));
    }

    #[test]
    fn truesize_and_chunk_limits_match_linux_arm64() {
        // Datagram/seqpacket slab buckets up to 16 KiB, then 4352 B head + 4 KiB paged frags.
        for (len, want) in [
            (0, 768),
            (1, 768),
            (192, 768),
            (193, 1280),
            (704, 1280),
            (705, 2304),
            (1728, 2304),
            (1729, 4352),
            (3776, 4352),
            (3777, 8448),
            (7872, 8448),
            (7873, 16640),
            (16064, 16640),
            (16384, 20736),
            (32768, 37120),
            (65536, 69888),
            (131072, 135424),
            (200000, 200960),
        ] {
            assert_eq!(packet_truesize(len), want, "packet_truesize({len})");
        }

        // Stream chunks spill above SKB_MAX_HEAD (3776) into 4 KiB pages.
        for (len, want) in [
            (0, 768),
            (1, 768),
            (192, 768),
            (704, 1280),
            (1728, 2304),
            (3776, 4352),
            (3777, 4864),
            (4032, 4864),
            (4096, 4864),
            (4097, 4864),
            (8192, 8960),
            (13680, 14592),
            (16320, 17152),
            (32704, 33536),
            (36544, 37120),
        ] {
            assert_eq!(stream_truesize(len), want, "stream_truesize({len})");
        }

        assert_eq!(stream_chunk_limit(4608), 2240);
        assert_eq!(stream_chunk_limit(8192), 4032);
        assert_eq!(stream_chunk_limit(32768), 16320);
        assert_eq!(stream_chunk_limit(65536), 32704);
        assert_eq!(stream_chunk_limit(212992), 36544);
        assert_eq!(dgram_max_len(4608), 4576);
        assert_eq!(dgram_max_len(212992), 212960);
    }

    #[test]
    fn sndbuf_writable_boundary_and_unconnected_dgram_pollout() {
        // Exact Linux unix_writable boundary: (outq + 1) * 4 <= sndbuf.
        let (a, b) = UnixSocket::pair(SOCK_STREAM, Ucred::of(1, 0, 0));
        let Link::Stream { tx, .. } = &a.link else {
            unreachable!()
        };
        a.outq.set_sndbuf(6144);
        let mut p1 = packet(b"a", 1);
        p1.outq = Some(a.outq.charge(a.outq.truesize(1)));
        tx.borrow_mut().push(p1);
        assert_ne!(a.readiness() & POLLOUT, 0);

        let mut p2 = packet(b"b", 1);
        p2.outq = Some(a.outq.charge(a.outq.truesize(1)));
        tx.borrow_mut().push(p2);
        assert_eq!(a.outq(), 1536);
        assert_eq!(a.readiness() & POLLOUT, 0);

        // Raising sndbuf by 4 bytes flips POLLOUT on and bumps write arrival counter.
        let before = a.arrivals()[1];
        a.outq.set_sndbuf(6148);
        assert_ne!(a.readiness() & POLLOUT, 0);
        assert!(a.arrivals()[1] > before);
        drop(b);

        // Unconnected datagram socket clears POLLOUT when its own outq exceeds sndbuf / 4.
        let cli = UnixSocket::new(SOCK_DGRAM);
        let srv = UnixSocket::new(SOCK_DGRAM);
        cli.outq.set_sndbuf(4608);
        assert_ne!(cli.readiness() & POLLOUT, 0);
        let mut dp = packet(&[0u8; 1024], 1);
        dp.outq = Some(cli.outq.charge(cli.outq.truesize(1024)));
        srv.rx.borrow_mut().push(dp);
        assert_eq!(cli.outq(), 2304);
        assert_eq!(cli.readiness() & POLLOUT, 0);
        srv.rx.borrow_mut().read_packet(1024, false).unwrap();
        assert_ne!(cli.readiness() & POLLOUT, 0);
    }
}
