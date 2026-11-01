//! 从宿主外面连到发布的端口（docs/rounds/173-port-forwarding.md）。
//!
//! 开机时的命名空间里有 PREROUTING 的 DNAT 规则（podman、docker 发布的端口）时，从 appine
//! 外面（别的设备、手机上的浏览器）连到宿主上这个端口的连接，到的是 guest 在宿主上监听着的
//! socket（podman 占着端口、从不 accept 的那个）。这样的 socket 复制一份交给这里的宿主线程：
//! 它接受连接，连到 DNAT 的目的地（容器在宿主上的回环地址），在两边之间转发数据，相当于
//! 内核层里的 docker-proxy。UDP 的按来源各开一个宿主 socket 转发，闲置两分钟就关掉。
//!
//! 外面的客户端是 IPv4、不是回环地址时（别的设备），转发的连接先绑一个 127.254.x.y 和客户端的
//! 端口再连过去，登记下来（[`outside_peer`]）：容器里 accept、getpeername 看到的是客户端真实的
//! 地址，与 Linux 上 DNAT 一样（docs/rounds/176-real-client-address.md）。回环地址来的（手机上
//! 的别的 App、浏览器开 localhost）照旧绑网关在宿主上的地址：Linux 上从本机连发布的端口时
//! netavark 做 masquerade、docker 经由 docker-proxy，容器看到的也是网关。
//!
//! 线程只碰宿主 fd，不拿 BKL。要转发哪些端口、转到哪里由 `syscall::net` 按规则算好交来
//! （[`Forwarder::add`]），socket 关掉、规则变了时丢掉 [`Handle`] 就不再转发。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const EAGAIN: i32 = 11;
const EINPROGRESS: i32 = 115;
const SOL_SOCKET: i32 = 1;
const SO_ERROR: i32 = 4;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SHUT_WR: i32 = 1;
/// 每个方向缓冲的最多字节数（满了就先不读）。
const BUFFER: usize = 64 * 1024;
/// UDP 的转发 socket 闲置多久关掉（docker-proxy 是 90 秒）。
const UDP_IDLE: Duration = Duration::from_secs(120);

/// 宿主上 127.254.0.0/16 留给从外面转发进来的连接（新的网络命名空间的段不取它）。
pub(crate) const OUTSIDE_BLOCK: u8 = 254;

/// 从外面转发进来的连接绑的宿主地址 → (客户端的地址, 客户端的端口, 绑上的宿主端口)。
/// 进程里一份：地址按进程里的计数器分配，不会重复。
static OUTSIDE: Mutex<Option<Outside>> = Mutex::new(None);

#[derive(Default)]
struct Outside {
    next: u16,
    peers: HashMap<[u8; 4], ([u8; 4], u16, u16)>,
}

/// 宿主地址 `host`（127.254.x.y）、端口 `port` 是从外面转发进来的连接时，客户端真实的地址和
/// 端口。
pub(crate) fn outside_peer(host: [u8; 4], port: u16) -> Option<([u8; 4], u16)> {
    let g = OUTSIDE.lock().unwrap();
    let &(ip, cport, hport) = g.as_ref()?.peers.get(&host)?;
    (hport == port).then_some((ip, cport))
}

/// 给一个从外面来的连接分一个 127.254.x.y（跳过 .0.0）。
fn take_outside() -> [u8; 4] {
    let mut g = OUTSIDE.lock().unwrap();
    let o = g.get_or_insert_with(Default::default);
    loop {
        o.next = o.next.wrapping_add(1);
        let [a, b] = o.next.to_be_bytes();
        let addr = [127, OUTSIDE_BLOCK, a, b];
        if o.next != 0 && !o.peers.contains_key(&addr) {
            return addr;
        }
    }
}

fn record_outside(host: [u8; 4], client: [u8; 4], cport: u16, hport: u16) {
    let mut g = OUTSIDE.lock().unwrap();
    g.get_or_insert_with(Default::default)
        .peers
        .insert(host, (client, cport, hport));
}

/// 测试里登记一个从外面转发进来的连接（[`outside_peer`]）。
#[cfg(test)]
pub(crate) fn record_outside_for_test(host: [u8; 4], client: [u8; 4], cport: u16, hport: u16) {
    record_outside(host, client, cport, hport);
}

fn forget_outside(host: [u8; 4]) {
    if let Some(o) = OUTSIDE.lock().unwrap().as_mut() {
        o.peers.remove(&host);
    }
}

/// accept 到的客户端地址里的 IPv4 地址和端口：IPv4 映射的 IPv6 地址取 IPv4 的部分；回环地址、
/// 纯 IPv6 的是 None。
fn outside_client(addr: &[u8]) -> Option<([u8; 4], u16)> {
    const MAPPED: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
    let port = u16::from_be_bytes([*addr.get(2)?, *addr.get(3)?]);
    let ip: [u8; 4] = match family(addr) {
        2 => addr.get(4..8)?.try_into().ok()?,
        10 if addr.get(8..20)? == MAPPED => addr.get(20..24)?.try_into().ok()?,
        _ => return None,
    };
    (ip[0] != 127).then_some((ip, port))
}

/// 一个要转发的端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub udp: bool,
    /// 宿主上的目的地（sockaddr）。
    pub addr: Vec<u8>,
    /// 连过去之前绑的宿主地址（端口 0）：容器看到的来源（开机时的命名空间面向它的地址）。
    pub source: Option<Vec<u8>>,
}

/// 转发线程和交给它的端口。
#[derive(Default)]
pub(crate) struct Forwarder {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    event: std::sync::OnceLock<appine_hal::Event>,
}

#[derive(Default)]
struct State {
    next: u64,
    /// 要转发的端口：复制出来的监听 fd 和目的地。
    ports: HashMap<u64, (i32, Target)>,
    /// 不再转发的端口复制出来的 fd：由线程关掉（它可能正在 poll 它们）。
    closing: Vec<i32>,
    running: bool,
    /// 内核没了（[`Forwarder`] 丢掉了）：线程关掉手上的 fd 退出。
    stop: bool,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        if let Some(e) = self.shared.event.get() {
            e.signal();
        }
    }
}

/// 转发着的一个端口：丢掉时不再转发。
pub(crate) struct Handle {
    shared: Arc<Shared>,
    id: u64,
    pub target: Target,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("id", &self.id)
            .field("target", &self.target)
            .finish()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().unwrap();
        if let Some((fd, _)) = st.ports.remove(&self.id) {
            st.closing.push(fd);
        }
        drop(st);
        if let Some(e) = self.shared.event.get() {
            e.signal();
        }
    }
}

impl Forwarder {
    /// 转发宿主上监听着的 `fd`（TCP 的监听 socket，UDP 的绑定了的 socket）到 `target`。
    pub(crate) fn add(&self, fd: i32, target: Target) -> Result<Handle, i32> {
        let event = match self.shared.event.get() {
            Some(e) => e,
            None => {
                let e = appine_hal::Event::new()?;
                let _ = self.shared.event.set(e);
                self.shared.event.get().unwrap()
            }
        };
        let dup = appine_hal::dup(fd)?;
        let mut st = self.shared.state.lock().unwrap();
        st.next += 1;
        let id = st.next;
        st.ports.insert(id, (dup, target.clone()));
        if !st.running {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("forward".into())
                .spawn(move || run(&shared));
            if spawned.is_err() {
                st.ports.remove(&id);
                appine_hal::net::close(dup);
                return Err(11);
            }
            st.running = true;
        }
        drop(st);
        event.signal();
        Ok(Handle {
            shared: self.shared.clone(),
            id,
            target,
        })
    }
}

/// 一条转发着的 TCP 连接。
struct Conn {
    client: i32,
    upstream: i32,
    /// 绑的 127.254.x.y（[`outside_peer`]），连接结束时去掉登记。
    outside: Option<[u8; 4]>,
    /// 连到目的地完成了。
    ready: bool,
    to_up: Vec<u8>,
    to_client: Vec<u8>,
    client_eof: bool,
    up_eof: bool,
    /// 已经对对方 shutdown(SHUT_WR) 了。
    up_shut: bool,
    client_shut: bool,
}

impl Conn {
    fn done(&self) -> bool {
        self.client_eof && self.up_eof && self.to_up.is_empty() && self.to_client.is_empty()
    }

    fn close(&self) {
        appine_hal::net::close(self.client);
        appine_hal::net::close(self.upstream);
        if let Some(a) = self.outside {
            forget_outside(a);
        }
    }
}

/// UDP：一个来源的转发 socket。
struct Relay {
    fd: i32,
    port: u64,
    client: Vec<u8>,
    last: Instant,
}

fn family(addr: &[u8]) -> i32 {
    i32::from(u16::from_le_bytes([addr[0], addr[1]]))
}

/// 新建连到 `t` 的宿主 socket（非阻塞，连接可能还在进行），返回 (fd, 连上了, 绑的
/// 127.254.x.y)。`client` 是外面的客户端的 IPv4 地址和端口时，先绑一个 127.254.x.y 和这个
/// 端口（绑不上这个端口就让宿主选），登记下来；不然绑 `t.source`。
fn open(t: &Target, client: Option<([u8; 4], u16)>) -> Result<(i32, bool, Option<[u8; 4]>), i32> {
    let fd = appine_hal::net::socket(
        family(&t.addr),
        if t.udp { SOCK_DGRAM } else { SOCK_STREAM },
        0,
    )?;
    let mut outside = None;
    let bound = match client.filter(|_| family(&t.addr) == 2) {
        Some((ip, port)) => {
            let host = take_outside();
            let at = |p: u16| appine_hal::net::sockaddr(std::net::SocketAddr::from((host, p)));
            let r =
                appine_hal::net::bind(fd, &at(port)).or_else(|_| appine_hal::net::bind(fd, &at(0)));
            if r.is_ok() {
                let hport = appine_hal::net::sockname(fd, false)
                    .ok()
                    .map_or(0, |a| u16::from_be_bytes([a[2], a[3]]));
                record_outside(host, ip, port, hport);
                outside = Some(host);
            }
            r
        }
        None => t
            .source
            .as_ref()
            .map_or(Ok(()), |src| appine_hal::net::bind(fd, src)),
    };
    if let Err(e) = bound {
        appine_hal::net::close(fd);
        return Err(e);
    }
    match appine_hal::net::connect(fd, &t.addr) {
        Ok(()) => Ok((fd, true, outside)),
        Err(EINPROGRESS) => Ok((fd, false, outside)),
        Err(e) => {
            appine_hal::net::close(fd);
            if let Some(a) = outside {
                forget_outside(a);
            }
            Err(e)
        }
    }
}

/// 从 `fd` 读到 `buf` 里（最多到 [`BUFFER`]）：读到结尾是 Ok(false)。
fn fill(fd: i32, buf: &mut Vec<u8>) -> Result<bool, i32> {
    let mut chunk = vec![0u8; BUFFER - buf.len()];
    match appine_hal::read(fd, &mut chunk) {
        Ok(0) => Ok(false),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(EAGAIN) => Ok(true),
        Err(e) => Err(e),
    }
}

/// 把 `buf` 写到 `fd`（写不完的留着）。
fn drain(fd: i32, buf: &mut Vec<u8>) -> Result<(), i32> {
    while !buf.is_empty() {
        match appine_hal::net::sendmsg(fd, buf, &[], &[], 0) {
            Ok(n) => {
                buf.drain(..n);
            }
            Err(EAGAIN) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 转发线程。
fn run(shared: &Shared) {
    let event = shared.event.get().expect("event before thread");
    let mut ports: HashMap<u64, (i32, Target)> = HashMap::new();
    let mut conns: Vec<Conn> = Vec::new();
    let mut relays: Vec<Relay> = Vec::new();
    loop {
        // 取交来的端口；不再转发的关掉，它们的 UDP 转发 socket 也关掉
        {
            event.clear();
            let mut st = shared.state.lock().unwrap();
            for fd in st.closing.drain(..) {
                ports.retain(|_, (f, _)| *f != fd);
                appine_hal::net::close(fd);
            }
            ports = st.ports.clone();
            if st.stop {
                for (fd, _) in st.ports.values() {
                    appine_hal::net::close(*fd);
                }
                st.ports.clear();
                st.running = false;
                drop(st);
                for c in &conns {
                    c.close();
                }
                for r in &relays {
                    appine_hal::net::close(r.fd);
                }
                return;
            }
        }
        relays.retain(|r| {
            let keep = ports.contains_key(&r.port) && r.last.elapsed() < UDP_IDLE;
            if !keep {
                appine_hal::net::close(r.fd);
            }
            keep
        });
        // 要等的：事件、各个端口、各条连接的两边、各个 UDP 转发 socket
        let mut fds: Vec<(i32, i16, i16)> = vec![(event.fd(), POLLIN, 0)];
        let order: Vec<(u64, i32, bool)> = ports
            .iter()
            .map(|(id, (fd, t))| (*id, *fd, t.udp))
            .collect();
        fds.extend(order.iter().map(|&(_, fd, _)| (fd, POLLIN, 0)));
        for c in &conns {
            let mut ce = 0;
            if !c.client_eof && c.to_up.len() < BUFFER {
                ce |= POLLIN;
            }
            if !c.to_client.is_empty() {
                ce |= POLLOUT;
            }
            let mut ue = 0;
            if !c.ready {
                ue |= POLLOUT;
            } else {
                if !c.up_eof && c.to_client.len() < BUFFER {
                    ue |= POLLIN;
                }
                if !c.to_up.is_empty() {
                    ue |= POLLOUT;
                }
            }
            fds.push((c.client, ce, 0));
            fds.push((c.upstream, ue, 0));
        }
        fds.extend(relays.iter().map(|r| (r.fd, POLLIN, 0)));
        // 下面新接受的连接、新开的转发 socket 不在这一轮里
        let (polled_conns, polled_relays) = (conns.len(), relays.len());
        let timeout = (!relays.is_empty()).then_some(Duration::from_secs(5));
        if appine_hal::poll(&mut fds, timeout).is_err() {
            continue;
        }
        let mut at = 1;
        // 端口：接受连接、收数据报
        for &(id, fd, udp) in &order {
            let ready = fds[at].2 != 0;
            at += 1;
            if !ready {
                continue;
            }
            let target = &ports[&id].1;
            if udp {
                datagrams(id, fd, target, &mut relays);
                continue;
            }
            // 一次最多接受 64 个，别的连接不至于饿着
            for _ in 0..64 {
                let Ok((client, from)) = appine_hal::net::accept(fd) else {
                    break;
                };
                match open(target, outside_client(&from)) {
                    Ok((upstream, ready, outside)) => conns.push(Conn {
                        client,
                        upstream,
                        outside,
                        ready,
                        to_up: Vec::new(),
                        to_client: Vec::new(),
                        client_eof: false,
                        up_eof: false,
                        up_shut: false,
                        client_shut: false,
                    }),
                    // 目的地连不上：客户端看到连接被关掉
                    Err(_) => appine_hal::net::close(client),
                }
            }
        }
        // 连接
        let mut broken = vec![false; conns.len()];
        for (k, c) in conns.iter_mut().enumerate().take(polled_conns) {
            let (cr, ur) = (fds[at + 2 * k].2, fds[at + 2 * k + 1].2);
            let r = step(c, cr, ur);
            broken[k] = r.is_err() || c.done();
        }
        at += 2 * polled_conns;
        let mut i = 0;
        conns.retain(|c| {
            let gone = broken[i];
            i += 1;
            if gone {
                c.close();
            }
            !gone
        });
        // UDP 的回答发回来源
        for (k, r) in relays.iter_mut().enumerate().take(polled_relays) {
            if fds[at + k].2 == 0 {
                continue;
            }
            let Some((port_fd, _)) = ports.get(&r.port) else {
                continue;
            };
            let mut buf = vec![0u8; 65536];
            while let Ok(m) = appine_hal::net::recvmsg(r.fd, &mut buf, 0, 0) {
                let _ = appine_hal::net::sendmsg(
                    *port_fd,
                    &buf[..m.len.min(buf.len())],
                    &r.client,
                    &[],
                    0,
                );
                r.last = Instant::now();
            }
        }
    }
}

/// 一条连接上就绪的事件（`cr` 是客户端那边的，`ur` 是目的地那边的）。
fn step(c: &mut Conn, cr: i16, ur: i16) -> Result<(), i32> {
    if !c.ready {
        if ur == 0 {
            return Ok(());
        }
        let err = appine_hal::net::getsockopt(c.upstream, SOL_SOCKET, SO_ERROR, 4)?;
        match i32::from_le_bytes(err[..4].try_into().unwrap()) {
            0 => c.ready = true,
            e => return Err(e),
        }
    }
    if cr & (POLLIN | POLLHUP | POLLERR) != 0 && !c.client_eof {
        c.client_eof = !fill(c.client, &mut c.to_up)?;
    }
    if ur & (POLLIN | POLLHUP | POLLERR) != 0 && !c.up_eof {
        c.up_eof = !fill(c.upstream, &mut c.to_client)?;
    }
    drain(c.upstream, &mut c.to_up)?;
    drain(c.client, &mut c.to_client)?;
    // 一边读完了、数据都转过去了：对另一边半关
    if c.client_eof && c.to_up.is_empty() && !c.up_shut {
        let _ = appine_hal::net::shutdown(c.upstream, SHUT_WR);
        c.up_shut = true;
    }
    if c.up_eof && c.to_client.is_empty() && !c.client_shut {
        let _ = appine_hal::net::shutdown(c.client, SHUT_WR);
        c.client_shut = true;
    }
    Ok(())
}

/// UDP 的端口上收到的数据报：按来源找（没有就开）转发 socket，发给目的地。
fn datagrams(id: u64, fd: i32, target: &Target, relays: &mut Vec<Relay>) {
    let mut buf = vec![0u8; 65536];
    for _ in 0..64 {
        let Ok(m) = appine_hal::net::recvmsg(fd, &mut buf, 0, 0) else {
            break;
        };
        let data = &buf[..m.len.min(buf.len())];
        let r = match relays
            .iter()
            .position(|r| r.port == id && r.client == m.addr)
        {
            Some(i) => &mut relays[i],
            None => {
                // UDP 照旧绑网关的地址：容器回给真实地址的数据报要经由转发 socket 回去，
                // 地址映射里没有这个方向
                let Ok((rfd, _, _)) = open(target, None) else {
                    continue;
                };
                relays.push(Relay {
                    fd: rfd,
                    port: id,
                    client: m.addr.clone(),
                    last: Instant::now(),
                });
                relays.last_mut().unwrap()
            }
        };
        r.last = Instant::now();
        let _ = appine_hal::net::sendmsg(r.fd, data, &[], &[], 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AF_INET: i32 = 2;

    fn loopback(port: u16) -> Vec<u8> {
        appine_hal::net::sockaddr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
    }

    fn port(fd: i32) -> u16 {
        let a = appine_hal::net::sockname(fd, false).unwrap();
        u16::from_be_bytes([a[2], a[3]])
    }

    /// 阻塞地等 fd 就绪（测试里用）。
    fn wait(fd: i32, ev: i16) {
        let mut f = [(fd, ev, 0)];
        appine_hal::poll(&mut f, Some(Duration::from_secs(5))).unwrap();
        assert_ne!(f[0].2, 0, "timed out");
    }

    /// TCP：外面连到监听着的 socket，数据到了目的地，回答回来，关掉之后两边都结束。
    #[test]
    fn tcp_is_forwarded() {
        let server = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        appine_hal::net::bind(server, &loopback(0)).unwrap();
        appine_hal::net::listen(server, 8).unwrap();
        let held = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        appine_hal::net::bind(held, &loopback(0)).unwrap();
        appine_hal::net::listen(held, 8).unwrap();
        let f = Forwarder::default();
        let h = f
            .add(
                held,
                Target {
                    udp: false,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        // "外面"的客户端
        let c = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        let _ = appine_hal::net::connect(c, &loopback(port(held)));
        wait(server, POLLIN);
        let (s, _) = appine_hal::net::accept(server).unwrap();
        wait(c, POLLOUT);
        appine_hal::net::sendmsg(c, b"ping", &[], &[], 0).unwrap();
        wait(s, POLLIN);
        let mut buf = [0u8; 16];
        assert_eq!(appine_hal::read(s, &mut buf), Ok(4));
        assert_eq!(&buf[..4], b"ping");
        appine_hal::net::sendmsg(s, b"pong!", &[], &[], 0).unwrap();
        appine_hal::net::close(s);
        wait(c, POLLIN);
        assert_eq!(appine_hal::read(c, &mut buf), Ok(5));
        assert_eq!(&buf[..5], b"pong!");
        // 目的地关了：客户端读到结尾
        wait(c, POLLIN);
        assert_eq!(appine_hal::read(c, &mut buf), Ok(0));
        // 不再转发：连进来的没人接受
        drop(h);
        std::thread::sleep(Duration::from_millis(100));
        let c2 = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        let _ = appine_hal::net::connect(c2, &loopback(port(held)));
        let mut f2 = [(server, POLLIN, 0)];
        appine_hal::poll(&mut f2, Some(Duration::from_millis(300))).unwrap();
        assert_eq!(f2[0].2, 0);
        for fd in [server, held, c, c2] {
            appine_hal::net::close(fd);
        }
    }

    /// 外面的客户端（不是回环地址）：转发的连接绑一个 127.254.x.y 和客户端的端口，登记着客户端
    /// 真实的地址；连接结束时去掉。
    #[test]
    fn outside_clients_keep_their_address() {
        // 宿主上一个不是回环的 IPv4 地址当作外面的客户端，没有就不测
        let Some(ip) = appine_hal::net::interfaces().iter().find_map(|i| {
            i.addrs
                .iter()
                .find(|(f, a, _)| *f == AF_INET && a.len() == 4 && a[0] != 127)
                .map(|(_, a, _)| <[u8; 4]>::try_from(&a[..]).unwrap())
        }) else {
            return;
        };
        let at =
            |ip: [u8; 4], p: u16| appine_hal::net::sockaddr(std::net::SocketAddr::from((ip, p)));
        let server = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        appine_hal::net::bind(server, &loopback(0)).unwrap();
        appine_hal::net::listen(server, 8).unwrap();
        let held = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        appine_hal::net::bind(held, &at([0; 4], 0)).unwrap();
        appine_hal::net::listen(held, 8).unwrap();
        let f = Forwarder::default();
        let _h = f
            .add(
                held,
                Target {
                    udp: false,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        let c = appine_hal::net::socket(AF_INET, SOCK_STREAM, 0).unwrap();
        appine_hal::net::bind(c, &at(ip, 0)).unwrap();
        let _ = appine_hal::net::connect(c, &at(ip, port(held)));
        wait(server, POLLIN);
        let (s, peer) = appine_hal::net::accept(server).unwrap();
        let host: [u8; 4] = peer[4..8].try_into().unwrap();
        let hport = u16::from_be_bytes([peer[2], peer[3]]);
        assert_eq!(host[..2], [127, OUTSIDE_BLOCK]);
        // 绑的是客户端的端口
        assert_eq!(hport, port(c));
        assert_eq!(outside_peer(host, hport), Some((ip, port(c))));
        assert_eq!(outside_peer(host, hport.wrapping_add(1)), None);
        // 两边都关掉之后登记没了
        appine_hal::net::close(c);
        appine_hal::net::close(s);
        for _ in 0..50 {
            if outside_peer(host, hport).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(outside_peer(host, hport), None);
        // 回环地址来的不是外面的客户端
        assert_eq!(outside_client(&loopback(4000)), None);
        appine_hal::net::close(server);
        appine_hal::net::close(held);
    }

    /// UDP：数据报转给目的地，回答从监听的 socket 发回来源。
    #[test]
    fn udp_is_forwarded() {
        let server = appine_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        appine_hal::net::bind(server, &loopback(0)).unwrap();
        let held = appine_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        appine_hal::net::bind(held, &loopback(0)).unwrap();
        let f = Forwarder::default();
        let _h = f
            .add(
                held,
                Target {
                    udp: true,
                    addr: loopback(port(server)),
                    source: None,
                },
            )
            .unwrap();
        let c = appine_hal::net::socket(AF_INET, SOCK_DGRAM, 0).unwrap();
        appine_hal::net::sendmsg(c, b"query", &loopback(port(held)), &[], 0).unwrap();
        wait(server, POLLIN);
        let mut buf = [0u8; 64];
        let m = appine_hal::net::recvmsg(server, &mut buf, 0, 0).unwrap();
        assert_eq!(&buf[..m.len], b"query");
        appine_hal::net::sendmsg(server, b"answer", &m.addr, &[], 0).unwrap();
        wait(c, POLLIN);
        let r = appine_hal::net::recvmsg(c, &mut buf, 0, 0).unwrap();
        assert_eq!(&buf[..r.len], b"answer");
        // 回答来自客户端发往的那个端口
        assert_eq!(r.addr[2..8], loopback(port(held))[2..8]);
        for fd in [server, held, c] {
            appine_hal::net::close(fd);
        }
    }
}
