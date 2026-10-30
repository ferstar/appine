//! guest 的 raw ICMP socket（设计文档 §10）。Android 上 App 没有 `CAP_NET_RAW`，guest 的
//! raw ICMP socket 在宿主上是 ping socket（不需要特权），这里补上两者的差别：
//!
//! - **收得到到达的所有 ICMP**：Linux 的 raw ICMP socket 收到发给本机的每个 ICMP 报文，
//!   traceroute 用一个 socket 发探测包、另一个 raw socket 收超时和不可达的报文；宿主的
//!   ping socket 只收得到自己的 echo 回答。这里按网络命名空间建一个汇集点（[`Tap`]）：
//!   raw socket 背后的 ping socket，以及设过 TTL、没有连接的 UDP socket（traceroute 的
//!   探测包，[`before_send`]），在宿主上打开 `IP_RECVERR`。它们收到的报文和错误队列里的
//!   ICMP 错误由汇集点取下来（[`dispatch`]），错误拼回完整的 ICMP 错误报文（引用原来的
//!   IP 头和报文的开头），分给同一个命名空间、同一个协议族的每个 raw socket 的队列。raw
//!   socket 收的时候自己先取一遍，poll、epoll 的等待者由定时器线程看着汇集点
//!   （[`check_taps`]）。
//! - **echo 的 id**：ping socket 发出 echo 请求时把 id 换成自己的编号，回答里也是；raw
//!   socket 原样发出，程序（busybox 的 ping 用 pid）按自己的 id 认回答。记下 guest 最后
//!   发出的 id，收到的 echo 回答、错误报文里引用的 echo 请求都换回它，校验和跟着更新。
//! - **IPv4 的 IP 头**：raw socket 收到的数据带 IP 头，这里拼一个 20 字节的头（[`present`]）：
//!   TTL 和目的地址取自宿主的 `IP_TTL`、`IP_PKTINFO` 控制消息（宿主上总是打开，guest 自己
//!   没打开的不交给它），TOS、IP 的 id 和标志都是 0。IPv6 的 raw socket 本来就不带 IP 头。
//! - **raw socket 才有的选项**：`ICMP_FILTER`、`ICMPV6_FILTER` 在分发时过滤；
//!   `IPV6_CHECKSUM`（ICMPv6 的默认是 2）只记下来，宿主的 ping socket 总是算校验和。
//!
//! 替 guest 在宿主上打开的 `IP_RECVERR` 有副作用：宿主把收到的 ICMP 错误记在 socket 上，
//! 下一次收发因此失败。这些 socket 收发之前先取走错误，因为它失败了也取走再试一次。
//!
//! [`present`]: RawIcmp::present

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use appine_hal::Watch;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::kernel::{Kernel, request_wake_object, wake};
use crate::net::{AF_INET, AF_INET6, IPPROTO_ICMP, IPPROTO_UDP, InetSocket, SOCK_DGRAM, Socket};
use crate::nsproxy::NetNs;

const SOL_SOCKET: i32 = 1;
const SOL_IP: i32 = 0;
const SOL_IPV6: i32 = 41;
const SOL_ICMPV6: i32 = 58;
const SOL_RAW: i32 = 255;
const IP_TTL: i32 = 2;
const IP_PKTINFO: i32 = 8;
const IP_RECVERR: i32 = 11;
const IP_RECVTTL: i32 = 12;
const IPV6_UNICAST_HOPS: i32 = 16;
const IPV6_RECVERR: i32 = 25;
const IPV6_HOPLIMIT: i32 = 52;
/// `ICMP_FILTER`（SOL_RAW）、`ICMPV6_FILTER`（SOL_ICMPV6）。
const ICMP_FILTER: i32 = 1;
const IPV6_CHECKSUM: i32 = 7;
const ICMP_ECHOREPLY: u8 = 0;
const ICMP_DEST_UNREACH: u8 = 3;
const ICMP_ECHO: u8 = 8;
const ICMP_PARAMETERPROB: u8 = 12;
const ICMP_FRAG_NEEDED: u8 = 4;
const ICMPV6_PKT_TOOBIG: u8 = 2;
const ICMPV6_PARAMPROB: u8 = 4;
const ICMPV6_ECHO_REQUEST: u8 = 128;
const ICMPV6_ECHO_REPLY: u8 = 129;
const NEXTHDR_ICMPV6: u8 = 58;
const SO_EE_ORIGIN_ICMP: u8 = 2;
const SO_EE_ORIGIN_ICMP6: u8 = 3;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_ERRQUEUE: i32 = 0x2000;
const MSG_CTRUNC: u32 = 0x08;
/// 宿主 `struct cmsghdr` 的长度（arm64 Linux：`size_t cmsg_len` 加两个 int）。
const CMSG_HDR: usize = 16;
/// 一个 ICMP 报文最长的长度（IP 的总长度是 16 位）。
const PACKET_MAX: usize = 1 << 16;
/// 每个 raw socket 排队的字节数上限（Linux 默认的 `rmem_default`），放不下的丢掉。
const QUEUE_MAX: usize = 212_992;
/// 宿主收发时可能留着的控制消息的空间。
const CONTROL_ROOM: usize = 512;

/// guest 的 raw ICMP socket 自己的状态：选项、最后发出的 echo 的 id、它的汇集点。
pub(crate) struct RawIcmp {
    family: i32,
    /// guest 最后发出的 echo 请求的 id（网络字节序）。
    id: Option<[u8; 2]>,
    /// 过滤器，置位的类型丢掉（Linux 按 u32 数组存，IPv4 只用第一个）。
    filter: [u8; 32],
    /// guest 自己打开了 `IP_RECVTTL`、`IP_PKTINFO`（IPv4）。
    recvttl: bool,
    pktinfo: bool,
    /// guest 自己设的 `IP_RECVERR`、`IPV6_RECVERR`：只记下来（推迟项），宿主上总是打开。
    recverr: bool,
    /// `IPV6_CHECKSUM` 的值，-1 是不算。
    checksum: i32,
    /// 收到的报文都经由它，也在那里排队。
    pub tap: Rc<RefCell<Tap>>,
}

impl RawIcmp {
    pub fn new(family: i32, tap: Rc<RefCell<Tap>>) -> RawIcmp {
        RawIcmp {
            family,
            id: None,
            filter: [0; 32],
            recvttl: false,
            pktinfo: false,
            recverr: false,
            checksum: 2,
            tap,
        }
    }

    /// 宿主的 ping socket 上要替 guest 打开的选项。
    pub fn host_options(family: i32) -> &'static [(i32, i32)] {
        match family {
            AF_INET => &[
                (SOL_IP, IP_RECVTTL),
                (SOL_IP, IP_PKTINFO),
                (SOL_IP, IP_RECVERR),
            ],
            _ => &[(SOL_IPV6, IPV6_RECVERR)],
        }
    }

    fn echo(&self) -> (u8, u8) {
        echo_types(self.family)
    }

    /// guest 发出一个报文：是 echo 请求时记下它的 id。
    pub fn sent(&mut self, data: &[u8]) {
        if data.len() >= 8 && data[0] == self.echo().0 {
            self.id = Some([data[4], data[5]]);
        }
    }

    /// 类型 `ty` 的报文能不能通过过滤器。
    fn passes(&self, ty: u8) -> bool {
        let word = |i: usize| u32::from_le_bytes(self.filter[4 * i..4 * i + 4].try_into().unwrap());
        !match self.family {
            AF_INET => ty < 32 && word(0) & (1 << ty) != 0,
            _ => word(ty as usize >> 5) & (1 << (ty & 31)) != 0,
        }
    }

    /// raw socket 自己的选项。不归这里管的返回 None（交给宿主）。
    pub fn setsockopt(&mut self, level: i32, name: i32, v: &[u8]) -> Option<Result<(), i32>> {
        const EINVAL: i32 = 22;
        const ENOPROTOOPT: i32 = 92;
        let int = || match v.len() {
            0 => 0,
            1..4 => v[0] as i32,
            _ => i32::from_le_bytes(v[..4].try_into().unwrap()),
        };
        Some(match (self.family, level, name) {
            // Linux 的 raw_seticmpfilter、rawv6_seticmpfilter：最多拷过滤器那么长，短的只盖住
            // 前面一部分
            (AF_INET, SOL_RAW, ICMP_FILTER) | (AF_INET6, SOL_ICMPV6, ICMP_FILTER) => {
                let n = v.len().min(if self.family == AF_INET { 4 } else { 32 });
                self.filter[..n].copy_from_slice(&v[..n]);
                Ok(())
            }
            (AF_INET, SOL_RAW, _) => Err(ENOPROTOOPT),
            (AF_INET, SOL_IP, IP_RECVTTL) => {
                self.recvttl = int() != 0;
                Ok(())
            }
            (AF_INET, SOL_IP, IP_PKTINFO) => {
                self.pktinfo = int() != 0;
                Ok(())
            }
            (AF_INET, SOL_IP, IP_RECVERR) | (AF_INET6, SOL_IPV6, IPV6_RECVERR) => {
                self.recverr = int() != 0;
                Ok(())
            }
            // RFC 3542 不让在 IPPROTO_IPV6 一级给 ICMPv6 设它（Linux 的 do_rawv6_setsockopt）
            (AF_INET6, SOL_IPV6, IPV6_CHECKSUM) => Err(EINVAL),
            (AF_INET6, SOL_RAW, IPV6_CHECKSUM) => {
                if v.len() < 4 {
                    return Some(Err(EINVAL));
                }
                match int() {
                    n if n > 0 && n & 1 != 0 => Err(EINVAL),
                    n => {
                        self.checksum = n.max(-1);
                        Ok(())
                    }
                }
            }
            (AF_INET6, SOL_RAW, _) => Err(ENOPROTOOPT),
            _ => return None,
        })
    }

    /// raw socket 自己的选项的值。不归这里管的返回 None。
    pub fn getsockopt(&self, level: i32, name: i32) -> Option<Result<Vec<u8>, i32>> {
        const ENOPROTOOPT: i32 = 92;
        let int = |b: bool| (b as i32).to_le_bytes().to_vec();
        Some(Ok(match (self.family, level, name) {
            (AF_INET, SOL_RAW, ICMP_FILTER) => self.filter[..4].to_vec(),
            (AF_INET6, SOL_ICMPV6, ICMP_FILTER) => self.filter.to_vec(),
            (AF_INET, SOL_RAW, _) => return Some(Err(ENOPROTOOPT)),
            (AF_INET, SOL_IP, IP_RECVTTL) => int(self.recvttl),
            (AF_INET, SOL_IP, IP_PKTINFO) => int(self.pktinfo),
            (AF_INET, SOL_IP, IP_RECVERR) | (AF_INET6, SOL_IPV6, IPV6_RECVERR) => int(self.recverr),
            (AF_INET6, SOL_RAW | SOL_IPV6, IPV6_CHECKSUM) => self.checksum.to_le_bytes().to_vec(),
            (AF_INET6, SOL_RAW, _) => return Some(Err(ENOPROTOOPT)),
            _ => return None,
        }))
    }

    /// 队列里的报文 `it` 交给这个 socket（`key` 是它的唤醒对象）时的样子：IPv4 的前面拼上
    /// IP 头，控制消息按它自己的选项给。`src` 是已经换算成 guest 的对端地址，`local` 把
    /// 宿主上的本端地址换成 guest 的。返回数据和控制消息。
    pub fn present(
        &self,
        it: &Item,
        key: usize,
        src: &[u8],
        local: impl Fn([u8; 4]) -> [u8; 4],
    ) -> (Vec<u8>, Vec<u8>) {
        // 别的 socket 收到的报文上的控制消息是按那个 socket 的选项给的，不交给这一个
        let other: &[u8] = if it.origin == key { &it.other } else { &[] };
        if self.family != AF_INET {
            return (it.icmp.clone(), other.to_vec());
        }
        let ttl = it.ttl.unwrap_or(64);
        // struct in_pktinfo：ipi_addr 是 IP 头里的目的地址。ping socket 不填
        // ipi_spec_dst，raw socket 填回答时用的本端地址（Linux 的 fib_compute_spec_dst：
        // 发给本机单播地址的就是这个地址）
        let pktinfo = it.pktinfo.map(|p| {
            let addr = local(p[8..12].try_into().unwrap());
            let spec = match p[4..8] {
                [0, 0, 0, 0] => addr,
                _ => local(p[4..8].try_into().unwrap()),
            };
            let mut out = p;
            out[4..8].copy_from_slice(&spec);
            out[8..12].copy_from_slice(&addr);
            out
        });
        // Linux 的顺序：时间戳这些 socket 一级的在前，接着是 ip_cmsg_recv 的 PKTINFO、TTL，
        // 再是别的 IP 一级的
        let mut control = Vec::new();
        for (level, _, whole, _) in cmsgs(other) {
            if level == SOL_SOCKET {
                control.extend_from_slice(whole);
            }
        }
        if let Some(p) = pktinfo.filter(|_| self.pktinfo) {
            control.extend(cmsg(SOL_IP, IP_PKTINFO, &p));
        }
        if self.recvttl && it.ttl.is_some() {
            control.extend(cmsg(SOL_IP, IP_TTL, &i32::from(ttl).to_le_bytes()));
        }
        for (level, _, whole, _) in cmsgs(other) {
            if level != SOL_SOCKET {
                control.extend_from_slice(whole);
            }
        }
        let total = 20 + it.icmp.len();
        let mut h = vec![0u8; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&(total.min(0xffff) as u16).to_be_bytes());
        h[8] = ttl;
        h[9] = IPPROTO_ICMP as u8;
        if let Some(a) = src.get(4..8) {
            h[12..16].copy_from_slice(a);
        }
        if let Some(p) = pktinfo {
            h[16..20].copy_from_slice(&p[8..12]);
        }
        let sum = checksum(&h);
        h[10..12].copy_from_slice(&sum);
        h.extend_from_slice(&it.icmp);
        (h, control)
    }
}

impl RawIcmp {
    /// 错误队列里的一条交给 guest 时的控制消息：宿主替它打开的 `IP_TTL`、`IP_PKTINFO`
    /// 它自己没打开的去掉。
    pub fn err_control(&self, e: &ErrItem) -> Vec<u8> {
        let mut out = Vec::new();
        for (level, ty, whole, _) in cmsgs(&e.control) {
            let drop = match (self.family, level, ty) {
                (AF_INET, SOL_IP, IP_TTL) => !self.recvttl,
                (AF_INET, SOL_IP, IP_PKTINFO) => !self.pktinfo,
                _ => false,
            };
            if !drop {
                out.extend_from_slice(whole);
            }
        }
        out
    }
}

fn echo_types(family: i32) -> (u8, u8) {
    match family {
        AF_INET => (ICMP_ECHO, ICMP_ECHOREPLY),
        _ => (ICMPV6_ECHO_REQUEST, ICMPV6_ECHO_REPLY),
    }
}

/// 一个网络命名空间里 ICMP 的汇集点（见模块的说明）。
pub(crate) struct Tap {
    /// 来源的宿主 fd：有报文、有错误时它可读。
    watch: Watch,
    /// 所在的网络命名空间，None 是开机时的。
    net: Option<Rc<NetNs>>,
    sources: Vec<Source>,
    receivers: Vec<Receiver>,
}

/// 汇集点取报文的宿主 socket。
struct Source {
    fd: i32,
    sock: Weak<RefCell<Socket>>,
    family: i32,
    /// UDP socket：只取错误队列。
    udp: bool,
    /// 它的唤醒对象（raw socket 认得出哪些是自己收到的）。
    key: usize,
}

/// 一个 raw socket 的队列。
struct Receiver {
    key: usize,
    sock: Weak<RefCell<Socket>>,
    family: i32,
    queue: VecDeque<Item>,
    bytes: usize,
    /// 放进队列的报文数（`EPOLLET` 每来一个都触发）。
    arrivals: u64,
    /// guest 打开了 `IP_RECVERR` 时它的错误队列（`MSG_ERRQUEUE`）。
    errors: VecDeque<ErrItem>,
}

/// raw socket 的错误队列里的一条：与 Linux 的 `ip_recv_error` 交出的一样，数据是原来的
/// 报文（换回了 guest 的 id），对端是原来的目的地址，控制消息里有 `IP_RECVERR`。
pub(crate) struct ErrItem {
    pub data: Vec<u8>,
    /// 宿主上的地址。
    pub addr: Vec<u8>,
    /// 宿主给的控制消息。
    control: Vec<u8>,
    errno: i32,
}

/// 收到的一个 ICMP 报文（IPv4 的不带 IP 头）。
#[derive(Clone)]
pub(crate) struct Item {
    pub icmp: Vec<u8>,
    /// 对端（宿主上的）地址：报文的来源、错误的报告者。
    pub src: Vec<u8>,
    /// IP 头里的 TTL（IPv6 是 hop limit），宿主给了的话。
    ttl: Option<u8>,
    /// IPv4 的 `struct in_pktinfo`，宿主给了的话。
    pktinfo: Option<[u8; 12]>,
    /// 宿主给的别的控制消息（来源 socket 的选项要的）。
    other: Vec<u8>,
    /// 来源 socket 的唤醒对象。
    origin: usize,
}

/// 汇集点从 raw socket 背后的 ping socket 上收下的一个报文：流量要算在那个 socket 上
/// （[`account`]）。
pub(crate) struct Arrival {
    sock: Rc<RefCell<Socket>>,
    bytes: usize,
    peer: Vec<u8>,
}

impl Tap {
    /// 汇集点的宿主 fd：来源上有东西可取时可读。
    pub fn fd(&self) -> i32 {
        self.watch.fd()
    }

    fn add_source(&mut self, sock: &Rc<RefCell<Socket>>, fd: i32, family: i32, udp: bool) {
        // 只看 UDP socket 的出错（错误队列），不看它收到的数据
        if self.watch.add(fd, !udp).is_ok() {
            self.sources.push(Source {
                fd,
                sock: Rc::downgrade(sock),
                family,
                udp,
                key: wake::of(sock),
            });
        }
    }

    fn remove_source(&mut self, fd: i32) {
        self.watch.remove(fd);
        self.sources.retain(|s| s.fd != fd);
    }

    fn has_receivers(&mut self, family: i32) -> bool {
        self.receivers.retain(|r| r.sock.strong_count() > 0);
        self.receivers.iter().any(|r| r.family == family)
    }
}

/// 网络命名空间 `net` 的汇集点，还没有就建一个。
pub(crate) fn tap_for(k: &mut Kernel, net: &Option<Rc<NetNs>>) -> Result<Rc<RefCell<Tap>>, Errno> {
    k.icmp_taps.retain(|t| t.strong_count() > 0);
    if let Some(t) = find_tap(k, net) {
        return Ok(t);
    }
    let tap = Rc::new(RefCell::new(Tap {
        watch: Watch::new().map_err(Errno)?,
        net: net.clone(),
        sources: Vec::new(),
        receivers: Vec::new(),
    }));
    k.icmp_taps.push(Rc::downgrade(&tap));
    Ok(tap)
}

fn find_tap(k: &Kernel, net: &Option<Rc<NetNs>>) -> Option<Rc<RefCell<Tap>>> {
    k.icmp_taps
        .iter()
        .filter_map(Weak::upgrade)
        .find(|t| match (&t.borrow().net, net) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            _ => false,
        })
}

/// 新建的 raw socket `sock`（宿主 fd 是 `fd`）登记到它的汇集点：它的 ping socket 是
/// 来源，它自己有一个队列。
pub(crate) fn register(tap: &Rc<RefCell<Tap>>, sock: &Rc<RefCell<Socket>>, fd: i32, family: i32) {
    let mut t = tap.borrow_mut();
    t.add_source(sock, fd, family, false);
    t.receivers.push(Receiver {
        key: wake::of(sock),
        sock: Rc::downgrade(sock),
        family,
        queue: VecDeque::new(),
        bytes: 0,
        arrivals: 0,
        errors: VecDeque::new(),
    });
}

/// 取下来源上已经到了的报文和错误，分给各个 raw socket 的队列。`only` 是只取这一个宿主
/// fd（收发之前清掉它上面的错误）。调用方不能借着任何 socket。返回从 ping socket 上收下
/// 的报文，流量要记下来（[`account`]）。
pub(crate) fn dispatch(tap: &Rc<RefCell<Tap>>, only: Option<i32>) -> Vec<Arrival> {
    let sources: Vec<_> = {
        let mut t = tap.borrow_mut();
        t.sources.retain(|s| s.sock.strong_count() > 0);
        t.sources
            .iter()
            .filter(|s| only.is_none_or(|fd| fd == s.fd))
            .filter_map(|s| Some((s.fd, s.sock.upgrade()?, s.family, s.udp, s.key)))
            .collect()
    };
    let ids = guest_ids(tap);
    let mut items = Vec::new();
    let mut arrivals = Vec::new();
    let mut buf = vec![0u8; PACKET_MAX];
    for (fd, sock, family, udp, key) in sources {
        // 没有打开 IP_RECVERR 的 socket 上，Linux 只把硬错误报告给已经连接的
        // （__udp4_lib_err、raw_err、ping_err 的 harderr），而且错误要与连接对得上：UDP 的
        // 宿主连接可能为了收到错误改过（syscall::net::send），这里按 guest 连的对端比
        // raw socket 的 guest 自己打开了 IP_RECVERR：错误另外进它的错误队列，每个都报告
        // （raw_err）
        let (peer, recverr) = match &*sock.borrow() {
            Socket::Inet(s) => {
                let recverr = s.icmp.as_ref().is_some_and(|r| r.recverr);
                (
                    s.peer.clone().filter(|_| !recverr && !s.guest_recverr),
                    recverr,
                )
            }
            _ => (None, false),
        };
        let net = tap.borrow().net.clone();
        let flags = MSG_ERRQUEUE | MSG_DONTWAIT;
        while let Ok(r) = appine_hal::net::recvmsg(fd, &mut buf, CONTROL_ROOM, flags) {
            let data = &buf[..r.len.min(buf.len())];
            let it = match family {
                AF_INET => error_v4(fd, udp, &ids, key, data, &r),
                _ => error_v6(fd, udp, &ids, key, data, &r),
            };
            let Some((it, errno, original)) = it else {
                continue;
            };
            let dst = match &net {
                Some(ns) => crate::netns::from_host(ns, &r.addr, false, family == AF_INET6 && !udp),
                None => r.addr.clone(),
            };
            let to_peer = peer.as_ref().is_some_and(|p| same_peer(p, &dst, udp));
            if recverr {
                let e = ErrItem {
                    data: original,
                    addr: r.addr.clone(),
                    control: r.control.clone(),
                    errno,
                };
                let mut t = tap.borrow_mut();
                if let Some(rx) = t.receivers.iter_mut().find(|rx| rx.key == key)
                    && rx.errors.len() < 64
                {
                    rx.errors.push_back(e);
                }
            }
            if recverr || to_peer && hard(family, it.icmp[0], it.icmp[1]) {
                if let Socket::Inet(s) = &*sock.borrow() {
                    s.icmp_err.set(errno);
                }
                request_wake_object(wake::of(&sock));
            }
            items.push((family, it));
        }
        if udp {
            continue;
        }
        while let Ok(r) = appine_hal::net::recvmsg(fd, &mut buf, CONTROL_ROOM, MSG_DONTWAIT) {
            let mut icmp = buf[..r.len.min(buf.len())].to_vec();
            if icmp.first() == Some(&echo_types(family).1) {
                restore_id(&mut icmp, &ids);
            }
            arrivals.push(Arrival {
                sock: sock.clone(),
                bytes: r.len,
                peer: r.addr.clone(),
            });
            if !icmp.is_empty() {
                items.push((family, data_item(family, icmp, r.addr, &r.control, key)));
            }
        }
    }
    deliver(tap, items);
    arrivals
}

/// 宿主上各个 raw socket 的 ping socket 的编号（echo 的 id）和 guest 最后发出的 id。回答、
/// 错误不一定到发出它的那个 ping socket 上：guest 可能把收的 socket 绑到发的 socket 的
/// 地址上（busybox 的 traceroute），宿主按编号找 socket，找到的是先绑上的那个。
fn guest_ids(tap: &Rc<RefCell<Tap>>) -> Vec<([u8; 2], [u8; 2])> {
    let socks: Vec<_> = tap
        .borrow()
        .sources
        .iter()
        .filter(|s| !s.udp)
        .filter_map(|s| s.sock.upgrade())
        .collect();
    socks
        .iter()
        .filter_map(|sock| {
            let Socket::Inet(InetSocket {
                icmp: Some(r), fd, ..
            }) = &*sock.borrow()
            else {
                return None;
            };
            let guest = r.id?;
            let name = appine_hal::net::sockname(*fd, false).ok()?;
            Some((name.get(2..4)?.try_into().ok()?, guest))
        })
        .collect()
}

/// 把报文放进同一个协议族、过滤器放得过的 raw socket 的队列，叫醒等它们的线程。
fn deliver(tap: &Rc<RefCell<Tap>>, items: Vec<(i32, Item)>) {
    if items.is_empty() {
        return;
    }
    let mut woken = Vec::new();
    let mut t = tap.borrow_mut();
    t.receivers.retain(|r| r.sock.strong_count() > 0);
    for r in &mut t.receivers {
        let Some(sock) = r.sock.upgrade() else {
            continue;
        };
        let s = sock.borrow();
        let Socket::Inet(InetSocket {
            icmp: Some(raw), ..
        }) = &*s
        else {
            continue;
        };
        let mut got = false;
        for (family, it) in &items {
            if *family != r.family || !raw.passes(it.icmp[0]) || r.bytes + it.icmp.len() > QUEUE_MAX
            {
                continue;
            }
            r.bytes += it.icmp.len();
            r.arrivals += 1;
            r.queue.push_back(it.clone());
            got = true;
        }
        drop(s);
        if got {
            woken.push(sock);
        }
    }
    drop(t);
    for s in woken {
        request_wake_object(wake::of(&s));
    }
}

/// 从 raw socket（唤醒对象 `key`）的队列里取一个报文，`peek` 时不取走。
pub(crate) fn take(tap: &Rc<RefCell<Tap>>, key: usize, peek: bool) -> Option<Item> {
    let mut t = tap.borrow_mut();
    let r = t.receivers.iter_mut().find(|r| r.key == key)?;
    if peek {
        return r.queue.front().cloned();
    }
    let it = r.queue.pop_front()?;
    r.bytes -= it.icmp.len();
    Some(it)
}

/// 从 raw socket（唤醒对象 `key`）的错误队列里取一条；要报告的错误换成下一条的（Linux 的
/// `sock_dequeue_err_skb`），没有了是 0。
pub(crate) fn take_err(tap: &Rc<RefCell<Tap>>, sock: &RefCell<Socket>) -> Option<ErrItem> {
    let key = wake::of(sock);
    let mut t = tap.borrow_mut();
    let r = t.receivers.iter_mut().find(|r| r.key == key)?;
    let e = r.errors.pop_front()?;
    if let Socket::Inet(s) = &*sock.borrow() {
        s.icmp_err.set(r.errors.front().map_or(0, |n| n.errno));
    }
    Some(e)
}

/// raw socket（唤醒对象 `key`）的错误队列里有没有东西。
pub(crate) fn has_errors(tap: &RefCell<Tap>, key: usize) -> bool {
    let t = tap.borrow();
    t.receivers
        .iter()
        .any(|r| r.key == key && !r.errors.is_empty())
}

/// raw socket（唤醒对象 `key`）的队列：(有没有报文, 放进去过的报文数)。
pub(crate) fn queued(tap: &RefCell<Tap>, key: usize) -> (bool, u64) {
    let t = tap.borrow();
    t.receivers
        .iter()
        .find(|r| r.key == key)
        .map_or((false, 0), |r| (!r.queue.is_empty(), r.arrivals))
}

/// 把汇集点收下的 ping socket 上的报文记进流量（[`crate::traffic`]）。
pub(crate) fn account(k: &mut Kernel, arrivals: Vec<Arrival>) {
    for a in arrivals {
        if let Socket::Inet(s) = &mut *a.sock.borrow_mut()
            && s.net.is_none()
        {
            k.traffic.account(s, true, a.bytes, &a.peer);
        }
    }
}

/// 定时器线程：替 poll、epoll 的等待者取下各个汇集点上到了的东西。返回要一起等的宿主 fd。
pub(crate) fn check_taps(k: &mut Kernel) -> Vec<(i32, i16, i16)> {
    const POLLIN: i16 = 1;
    k.icmp_taps.retain(|t| t.strong_count() > 0);
    let taps: Vec<_> = k.icmp_taps.iter().filter_map(Weak::upgrade).collect();
    let mut host = Vec::new();
    for tap in taps {
        let arrivals = dispatch(&tap, None);
        account(k, arrivals);
        host.push((tap.borrow().fd(), POLLIN, 0));
    }
    host
}

/// 宿主上替 guest 打开的 `IP_RECVERR` 让收发得到的错误（`icmp_err_convert`、
/// `icmpv6_err_convert` 的结果）：得到它们时先取走错误再试一次。
pub(crate) fn from_icmp(e: i32) -> bool {
    const EHOSTDOWN: i32 = 112;
    const ENONET: i32 = 64;
    const EPROTO: i32 = 71;
    [
        Errno::ENETUNREACH,
        Errno::EHOSTUNREACH,
        Errno::ENOPROTOOPT,
        Errno::ECONNREFUSED,
        Errno::EMSGSIZE,
        Errno::EOPNOTSUPP,
        Errno::EACCES,
    ]
    .iter()
    .any(|x| x.0 == e)
        || [EHOSTDOWN, ENONET, EPROTO].contains(&e)
}

/// 宿主 socket `sock` 就要收发：raw socket 返回它的汇集点；UDP socket 设过 TTL、guest
/// 没有自己打开 `IP_RECVERR`，命名空间里又有同一个协议族的 raw socket 时，
/// 交给汇集点看着（在宿主上打开 `IP_RECVERR`），返回汇集点。返回 Some 时，收发之前要先
/// 取走这个 socket 上的错误（[`dispatch`] 只取它）。
pub(crate) fn before_io(k: &Kernel, sock: &Rc<RefCell<Socket>>) -> Option<Rc<RefCell<Tap>>> {
    let (fd, family, tapped, wanted) = match &*sock.borrow() {
        Socket::Inet(InetSocket { icmp: Some(r), .. }) => return Some(r.tap.clone()),
        Socket::Inet(s) if s.ty == SOCK_DGRAM && s.proto == IPPROTO_UDP => (
            s.fd,
            s.family,
            s.udp_tap.clone(),
            s.ttl_set && !s.guest_recverr,
        ),
        _ => return None,
    };
    if let Some(t) = tapped {
        if t.borrow_mut().has_receivers(family) {
            return Some(t);
        }
        // raw socket 都关了
        untap(sock);
        return None;
    }
    if !wanted {
        return None;
    }
    let net = match &*sock.borrow() {
        Socket::Inet(s) => s.net.clone(),
        _ => None,
    };
    let tap = find_tap(k, &net).filter(|t| t.borrow_mut().has_receivers(family))?;
    let (level, name) = recverr_option(family);
    appine_hal::net::setsockopt(fd, level, name, &1i32.to_le_bytes()).ok()?;
    tap.borrow_mut().add_source(sock, fd, family, true);
    if let Socket::Inet(s) = &mut *sock.borrow_mut() {
        s.udp_tap = Some(tap.clone());
    }
    Some(tap)
}

fn recverr_option(family: i32) -> (i32, i32) {
    match family {
        AF_INET => (SOL_IP, IP_RECVERR),
        _ => (SOL_IPV6, IPV6_RECVERR),
    }
}

/// 汇集点不再看 UDP socket `sock`（guest 自己要 `IP_RECVERR`、raw socket 都关了）：已经到了的错误先分出去，宿主上的 `IP_RECVERR` 恢复成 guest 设的。调用方不能借着
/// 任何 socket。
pub(crate) fn untap(sock: &Rc<RefCell<Socket>>) {
    let (tap, fd, family, guest) = match &*sock.borrow() {
        Socket::Inet(s) => match &s.udp_tap {
            Some(t) => (t.clone(), s.fd, s.family, s.guest_recverr),
            None => return,
        },
        _ => return,
    };
    let _ = dispatch(&tap, Some(fd));
    tap.borrow_mut().remove_source(fd);
    let (level, name) = recverr_option(family);
    let _ = appine_hal::net::setsockopt(fd, level, name, &i32::from(guest).to_le_bytes());
    if let Socket::Inet(s) = &mut *sock.borrow_mut() {
        s.udp_tap = None;
    }
}

/// UDP socket 的选项：记下 guest 设过 TTL、自己打开的 `IP_RECVERR`。返回 Some 时不交给
/// 宿主（汇集点看着它时，宿主上的 `IP_RECVERR` 总是开着），值是 guest 要的结果。
pub(crate) fn udp_setsockopt(s: &mut InetSocket, level: i32, name: i32, v: &[u8]) -> Option<bool> {
    if s.ty != SOCK_DGRAM || s.proto != IPPROTO_UDP {
        return None;
    }
    let on = v
        .first_chunk::<4>()
        .map_or(v.first().is_some_and(|&b| b != 0), |b| {
            i32::from_le_bytes(*b) != 0
        });
    match (level, name) {
        (SOL_IP, IP_TTL) | (SOL_IPV6, IPV6_UNICAST_HOPS) => {
            s.ttl_set = true;
            None
        }
        (SOL_IP, IP_RECVERR) | (SOL_IPV6, IPV6_RECVERR) => {
            s.guest_recverr = on;
            // 看着时 guest 关掉它：只记下来；打开它：之后不再看着（[`untap`]）
            s.udp_tap.as_ref().map(|_| on)
        }
        _ => None,
    }
}

/// UDP socket 的选项的值：汇集点看着它时 `IP_RECVERR` 报告 guest 设的。
pub(crate) fn udp_getsockopt(s: &InetSocket, level: i32, name: i32) -> Option<Vec<u8>> {
    match (level, name) {
        (SOL_IP, IP_RECVERR) | (SOL_IPV6, IPV6_RECVERR) if s.udp_tap.is_some() => {
            Some(i32::from(s.guest_recverr).to_le_bytes().to_vec())
        }
        _ => None,
    }
}

/// ping socket 收到的报文：IPv4 的把宿主替 guest 打开的 `IP_TTL`、`IP_PKTINFO` 拿出来，
/// 交给 guest 时按它自己的选项再给（[`RawIcmp::present`]）。
fn data_item(family: i32, icmp: Vec<u8>, src: Vec<u8>, control: &[u8], key: usize) -> Item {
    let mut it = Item {
        icmp,
        src,
        ttl: None,
        pktinfo: None,
        other: Vec::new(),
        origin: key,
    };
    for (level, ty, whole, data) in cmsgs(control) {
        match (family, level, ty) {
            (AF_INET, SOL_IP, IP_TTL) => it.ttl = int_cmsg(data),
            (AF_INET, SOL_IP, IP_PKTINFO) => it.pktinfo = data.first_chunk::<12>().copied(),
            _ => it.other.extend_from_slice(whole),
        }
    }
    it
}

fn int_cmsg(data: &[u8]) -> Option<u8> {
    data.first_chunk::<4>()
        .map(|b| i32::from_le_bytes(*b) as u8)
}

/// 错误队列里的一条（`struct sock_extended_err` 和报告者的地址），不是 ICMP 报告的返回 None。
fn extended_err(control: &[u8], level: i32, name: i32, origin: u8) -> Option<(&[u8], &[u8])> {
    let (_, _, _, data) = cmsgs(control).find(|&(l, t, _, _)| (l, t) == (level, name))?;
    (data.len() >= 16 && data[4] == origin).then(|| (&data[..16], &data[16..]))
}

/// 本端地址、原来的目的地址里的端口（网络字节序），取不到的是 0。
fn ports(local: &[u8], dst: &[u8]) -> ([u8; 2], [u8; 2]) {
    let port = |a: &[u8]| a.get(2..4).map_or([0; 2], |p| [p[0], p[1]]);
    (port(local), port(dst))
}

/// 被引用的原来的报文：ICMP 的 echo 请求换回 guest 的 id；UDP 的补上 UDP 头（错误队列里只
/// 给载荷，端口取自本端地址和原来的目的地址）。
fn quoted(
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    echo: u8,
    data: &[u8],
    sport: &[u8],
    dport: &[u8],
) -> Vec<u8> {
    if udp {
        let mut u = Vec::with_capacity(8 + data.len());
        u.extend_from_slice(sport);
        u.extend_from_slice(dport);
        u.extend_from_slice(&((8 + data.len()).min(0xffff) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(data);
        u
    } else {
        let mut d = data.to_vec();
        if d.first() == Some(&echo) {
            restore_id(&mut d, ids);
        }
        d
    }
}

/// IPv4 的错误队列里的一条拼回 ICMP 错误报文：类型、代码、附带的值（需要分片时的 MTU、
/// 参数错误的位置），引用原来的 IP 头（本端是源地址、错误队列给的地址是目的地址，TTL
/// 记作 1）和原来报文的开头。报告者是对端地址。
fn error_v4(
    fd: i32,
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    key: usize,
    data: &[u8],
    r: &appine_hal::net::Received,
) -> Option<(Item, i32, Vec<u8>)> {
    let (ee, offender) = extended_err(&r.control, SOL_IP, IP_RECVERR, SO_EE_ORIGIN_ICMP)?;
    let (ty, code) = (ee[5], ee[6]);
    let info = u32::from_le_bytes(ee[8..12].try_into().unwrap());
    let mut it = data_item(
        AF_INET,
        Vec::new(),
        offender.get(..16)?.to_vec(),
        &r.control,
        key,
    );
    it.other.clear();
    let local = appine_hal::net::sockname(fd, false).unwrap_or_default();
    // 原来的报文的源地址：错误报文的目的地址（IP_PKTINFO），或者本端地址；本端是通配地址
    // （没有连接的 UDP socket）时按路由取
    let ours: [u8; 4] = match it.pktinfo {
        Some(p) => p[8..12].try_into().unwrap(),
        None => local.get(4..8).map_or([0; 4], |a| a.try_into().unwrap()),
    };
    let ours = match (ours, r.addr.get(4..8)) {
        ([0, 0, 0, 0], Some(dst)) => appine_hal::net::source_for(dst)
            .ok()
            .and_then(|a| a.try_into().ok())
            .unwrap_or(ours),
        _ => ours,
    };
    let (sport, dport) = ports(&local, &r.addr);
    let inner = quoted(udp, ids, ICMP_ECHO, data, &sport, &dport);
    let original = quoted(false, ids, ICMP_ECHO, data, &[], &[]);
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + inner.len()).min(0xffff) as u16).to_be_bytes());
    ip[8] = 1;
    ip[9] = if udp {
        IPPROTO_UDP as u8
    } else {
        IPPROTO_ICMP as u8
    };
    ip[12..16].copy_from_slice(&ours);
    ip[16..20].copy_from_slice(r.addr.get(4..8)?);
    let sum = checksum(&ip);
    ip[10..12].copy_from_slice(&sum);
    let mut icmp = vec![ty, code, 0, 0, 0, 0, 0, 0];
    match (ty, code) {
        (ICMP_DEST_UNREACH, ICMP_FRAG_NEEDED) => {
            icmp[6..8].copy_from_slice(&(info as u16).to_be_bytes())
        }
        (ICMP_PARAMETERPROB, _) => icmp[4] = info as u8,
        _ => {}
    }
    icmp.extend_from_slice(&ip);
    icmp.extend_from_slice(&inner);
    let sum = checksum(&icmp);
    icmp[2..4].copy_from_slice(&sum);
    it.icmp = icmp;
    Some((
        it,
        i32::from_le_bytes(ee[..4].try_into().unwrap()),
        original,
    ))
}

/// IPv6 的错误队列里的一条拼回 ICMPv6 错误报文（与 [`error_v4`] 相同，引用的是 IPv6 头，
/// 校验和带伪头部：报告者到本端）。
fn error_v6(
    fd: i32,
    udp: bool,
    ids: &[([u8; 2], [u8; 2])],
    key: usize,
    data: &[u8],
    r: &appine_hal::net::Received,
) -> Option<(Item, i32, Vec<u8>)> {
    let (ee, offender) = extended_err(&r.control, SOL_IPV6, IPV6_RECVERR, SO_EE_ORIGIN_ICMP6)?;
    let (ty, code) = (ee[5], ee[6]);
    let info = u32::from_le_bytes(ee[8..12].try_into().unwrap());
    let offender = offender.get(..28)?.to_vec();
    let local = appine_hal::net::sockname(fd, false).unwrap_or_default();
    let mut ours: [u8; 16] = local.get(8..24).map_or([0; 16], |a| a.try_into().unwrap());
    if ours == [0; 16]
        && let Some(src) = r
            .addr
            .get(8..24)
            .and_then(|d| appine_hal::net::source_for(d).ok())
        && let Ok(a) = src.try_into()
    {
        ours = a;
    }
    let ours = &ours[..];
    let (sport, dport) = ports(&local, &r.addr);
    let inner = quoted(udp, ids, ICMPV6_ECHO_REQUEST, data, &sport, &dport);
    let original = quoted(false, ids, ICMPV6_ECHO_REQUEST, data, &[], &[]);
    let mut icmp = vec![ty, code, 0, 0, 0, 0, 0, 0];
    if matches!(ty, ICMPV6_PKT_TOOBIG | ICMPV6_PARAMPROB) {
        icmp[4..8].copy_from_slice(&info.to_be_bytes());
    }
    icmp.extend_from_slice(&[0x60, 0, 0, 0]);
    icmp.extend_from_slice(&(inner.len().min(0xffff) as u16).to_be_bytes());
    icmp.push(if udp {
        IPPROTO_UDP as u8
    } else {
        NEXTHDR_ICMPV6
    });
    icmp.push(1);
    icmp.extend_from_slice(ours);
    icmp.extend_from_slice(r.addr.get(8..24)?);
    icmp.extend_from_slice(&inner);
    let mut pseudo = offender[8..24].to_vec();
    pseudo.extend_from_slice(ours);
    pseudo.extend_from_slice(&(icmp.len() as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0, 0, 0, NEXTHDR_ICMPV6]);
    pseudo.extend_from_slice(&icmp);
    let sum = checksum(&pseudo);
    icmp[2..4].copy_from_slice(&sum);
    let ttl = cmsgs(&r.control)
        .find(|&(l, t, _, _)| (l, t) == (SOL_IPV6, IPV6_HOPLIMIT))
        .and_then(|(_, _, _, d)| int_cmsg(d));
    let errno = i32::from_le_bytes(ee[..4].try_into().unwrap());
    Some((
        Item {
            icmp,
            src: offender,
            ttl,
            pktinfo: None,
            other: Vec::new(),
            origin: key,
        },
        errno,
        original,
    ))
}

/// Linux 报告给没有打开 `IP_RECVERR` 的已连接 socket 的 ICMP 错误（`icmp_err_convert`、
/// `icmpv6_err_convert` 里 fatal 的，加上需要分片、参数错误）；超时之类的软错误不报告。
fn hard(family: i32, ty: u8, code: u8) -> bool {
    match family {
        AF_INET => match ty {
            ICMP_DEST_UNREACH => matches!(code, 2..=4 | 6..=10 | 13..=15),
            ICMP_PARAMETERPROB => true,
            _ => false,
        },
        _ => match ty {
            1 => matches!(code, 1 | 4..=6),
            ICMPV6_PKT_TOOBIG | ICMPV6_PARAMPROB => true,
            _ => false,
        },
    }
}

/// 出错的报文的目的地址 `dst` 就是 socket 连的对端 `peer`：协议族、地址相同，UDP 的端口
/// 也相同（raw socket 没有端口）。
fn same_peer(peer: &[u8], dst: &[u8], udp: bool) -> bool {
    let range = match peer
        .get(..2)
        .map(|f| i32::from(u16::from_le_bytes([f[0], f[1]])))
    {
        Some(AF_INET) => 4..8,
        Some(AF_INET6) => 8..24,
        _ => return false,
    };
    peer.get(..2) == dst.get(..2)
        && peer.get(range.clone()).is_some()
        && peer.get(range.clone()) == dst.get(range)
        && (!udp || peer.get(2..4) == dst.get(2..4))
}

/// 汇集点替 `sock` 记下的要报告给 guest 的 ICMP 错误：取走它（Linux 的 `sock_error`）。
pub(crate) fn take_error(sock: &RefCell<Socket>) -> Option<Errno> {
    match &*sock.borrow() {
        Socket::Inet(s) => Some(Errno(s.icmp_err.replace(0))).filter(|e| e.0 != 0),
        _ => None,
    }
}

/// echo 报文的 id 是宿主上某个 ping socket 的编号时，换成 guest 从那个 socket 发出的 id
/// （[`guest_ids`]），校验和跟着增量更新。
fn restore_id(icmp: &mut [u8], ids: &[([u8; 2], [u8; 2])]) {
    if icmp.len() >= 6
        && let Some(&(_, id)) = ids.iter().find(|(host, _)| icmp[4..6] == *host)
    {
        let sum = csum_replace([icmp[2], icmp[3]], [icmp[4], icmp[5]], id);
        icmp[2..4].copy_from_slice(&sum);
        icmp[4..6].copy_from_slice(&id);
    }
}

/// 网际校验和（RFC 1071），网络字节序。
fn checksum(b: &[u8]) -> [u8; 2] {
    let mut acc: u32 = b
        .chunks(2)
        .map(|w| u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    (!(acc as u16)).to_be_bytes()
}

/// 校验和覆盖的一个 16 位字从 `old` 改成 `new` 之后的校验和（RFC 1624 的式 3）：不用知道
/// 别的内容（ICMPv6 的伪头部）。
fn csum_replace(sum: [u8; 2], old: [u8; 2], new: [u8; 2]) -> [u8; 2] {
    let mut acc = !u16::from_be_bytes(sum) as u32
        + !u16::from_be_bytes(old) as u32
        + u16::from_be_bytes(new) as u32;
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    (!(acc as u16)).to_be_bytes()
}

/// 一条控制消息（连同对齐的空白）。
fn cmsg(level: i32, ty: i32, data: &[u8]) -> Vec<u8> {
    let mut c = ((CMSG_HDR + data.len()) as u64).to_le_bytes().to_vec();
    c.extend_from_slice(&level.to_le_bytes());
    c.extend_from_slice(&ty.to_le_bytes());
    c.extend_from_slice(data);
    c.resize(c.len().next_multiple_of(8), 0);
    c
}

/// 逐条取宿主的控制消息：(level, type, 整条连同对齐的空白, 数据)。
fn cmsgs(control: &[u8]) -> impl Iterator<Item = (i32, i32, &[u8], &[u8])> {
    let mut off = 0;
    std::iter::from_fn(move || {
        let h = control.get(off..off + CMSG_HDR)?;
        let len = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        if len < CMSG_HDR || off + len > control.len() {
            return None;
        }
        let level = i32::from_le_bytes(h[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(h[12..16].try_into().unwrap());
        let space = (len + 7) & !7;
        let whole = &control[off..(off + space).min(control.len())];
        let data = &control[off + CMSG_HDR..off + len];
        off += space;
        Some((level, ty, whole, data))
    })
}

/// 控制消息放进 guest 给的 `room` 字节（Linux 的 `put_cmsg`）：放不下的那一条截短
/// （`cmsg_len` 写成剩下的长度），之后的不再放，返回的标志带 `MSG_CTRUNC`。
pub(crate) fn fit_control(control: &[u8], room: usize) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut flags = 0;
    for (_, _, whole, _) in cmsgs(control) {
        let left = room - out.len();
        let len = u64::from_le_bytes(whole[..8].try_into().unwrap()) as usize;
        if left < CMSG_HDR {
            flags |= MSG_CTRUNC;
            break;
        }
        if left < len {
            flags |= MSG_CTRUNC;
            let start = out.len();
            out.extend_from_slice(&whole[..left]);
            out[start..start + 8].copy_from_slice(&(left as u64).to_le_bytes());
            break;
        }
        out.extend_from_slice(&whole[..whole.len().min(left)]);
    }
    (out, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn received(addr: Vec<u8>, control: Vec<u8>) -> appine_hal::net::Received {
        appine_hal::net::Received {
            len: 0,
            addr,
            control,
            flags: 0,
        }
    }

    fn sockaddr_in(ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut a = vec![0u8; 16];
        a[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&ip);
        a
    }

    /// echo 报文：类型、id（宿主的编号 0x0597）、序号 7，56 字节的载荷。
    fn echo(ty: u8) -> Vec<u8> {
        let mut e = vec![ty, 0, 0, 0, 0x05, 0x97, 0, 7];
        e.extend(8u8..64);
        let sum = checksum(&e);
        e[2..4].copy_from_slice(&sum);
        e
    }

    const IDS: [([u8; 2], [u8; 2]); 1] = [([0x05, 0x97], [0x12, 0x34])];

    #[test]
    fn replies_get_the_guest_id_and_an_ip_header() {
        let mut reply = echo(ICMP_ECHOREPLY);
        restore_id(&mut reply, &IDS);
        assert_eq!(reply[4..6], [0x12, 0x34]);
        assert_eq!(checksum(&reply), [0, 0], "校验和跟着改了");
        let mut other = echo(ICMP_ECHOREPLY);
        restore_id(&mut other, &[([9, 9], [1, 1])]);
        assert_eq!(
            other,
            echo(ICMP_ECHOREPLY),
            "不是宿主上 raw socket 的编号就不换"
        );

        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[1, 0, 0, 0, 0, 0, 0, 0, 10, 0, 0, 2]);
        control.extend(cmsg(SOL_IP, IP_TTL, &57i32.to_le_bytes()));
        let src = sockaddr_in([10, 0, 0, 1], 0);
        let it = data_item(AF_INET, reply.clone(), src.clone(), &control, 7);
        let tap = Rc::new(RefCell::new(Tap {
            watch: Watch::new().unwrap(),
            net: None,
            sources: Vec::new(),
            receivers: Vec::new(),
        }));
        let mut r = RawIcmp::new(AF_INET, tap);
        r.setsockopt(SOL_IP, IP_PKTINFO, &[1]).unwrap().unwrap();
        let (packet, kept) = r.present(&it, 7, &src, |a| a);
        assert_eq!(packet.len(), 84);
        assert_eq!(packet[..4], [0x45, 0, 0, 84]);
        assert_eq!((packet[8], packet[9]), (57, 1));
        assert_eq!(packet[12..20], [10, 0, 0, 1, 10, 0, 0, 2]);
        assert_eq!(checksum(&packet[..20]), [0, 0]);
        assert_eq!(packet[20..], reply);
        // 没打开 IP_RECVTTL：只给 PKTINFO，ipi_spec_dst 填上目的地址
        assert_eq!(kept.len(), 32);
        assert_eq!(
            kept[CMSG_HDR + 4..CMSG_HDR + 12],
            [10, 0, 0, 2, 10, 0, 0, 2]
        );

        // 过滤器挡掉 echo 回答
        assert!(r.passes(ICMP_ECHOREPLY));
        r.setsockopt(SOL_RAW, ICMP_FILTER, &1u32.to_le_bytes())
            .unwrap()
            .unwrap();
        assert!(!r.passes(ICMP_ECHOREPLY));
        assert!(r.passes(ICMP_ECHO));
    }

    #[test]
    fn errors_become_whole_icmp_messages() {
        // 10.0.0.254 报告超时，引用的是 guest 发往 1.1.1.1 的 echo 请求
        let mut ee = 113u32.to_le_bytes().to_vec();
        ee.extend([SO_EE_ORIGIN_ICMP, 11, 0, 0]);
        ee.extend([0; 8]);
        ee.extend(sockaddr_in([10, 0, 0, 254], 0));
        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[1, 0, 0, 0, 0, 0, 0, 0, 172, 17, 0, 2]);
        control.extend(cmsg(SOL_IP, IP_TTL, &63i32.to_le_bytes()));
        control.extend(cmsg(SOL_IP, IP_RECVERR, &ee));
        let r = received(sockaddr_in([1, 1, 1, 1], 0), control);
        let request = echo(ICMP_ECHO);
        let (it, errno, original) = error_v4(-1, false, &IDS, 3, &request, &r).unwrap();
        assert_eq!(errno, 113);
        assert_eq!(original[4..6], [0x12, 0x34]);
        assert_eq!(it.src, sockaddr_in([10, 0, 0, 254], 0));
        assert_eq!(it.ttl, Some(63));
        assert!(it.other.is_empty());
        let icmp = &it.icmp;
        assert_eq!(icmp[..2], [11, 0]);
        assert_eq!(checksum(icmp), [0, 0]);
        let inner = &icmp[8..28];
        assert_eq!((inner[0], inner[9]), (0x45, IPPROTO_ICMP as u8));
        assert_eq!(inner[12..20], [172, 17, 0, 2, 1, 1, 1, 1]);
        assert_eq!(checksum(inner), [0, 0]);
        assert_eq!(icmp[28..], original);
        // 超时是软错误，端口不可达是硬错误
        assert!(!hard(AF_INET, 11, 0));
        assert!(hard(AF_INET, ICMP_DEST_UNREACH, 3));
        assert!(!hard(AF_INET, ICMP_DEST_UNREACH, 1));
        assert!(hard(AF_INET6, 1, 4));
        assert!(!hard(AF_INET6, 3, 0));

        // UDP 的：错误队列里只有载荷，补上 UDP 头（目的端口取自原来的目的地址）
        let r = received(sockaddr_in([127, 0, 0, 1], 33434), r.control.clone());
        let (it, ..) = error_v4(-1, true, &[], 3, b"abcd", &r).unwrap();
        let udp = &it.icmp[28..];
        assert_eq!(it.icmp[8 + 9], IPPROTO_UDP as u8);
        assert_eq!(udp[2..6], [0x82, 0x9a, 0, 12]);
        assert_eq!(&udp[8..], b"abcd");

        // 不是 ICMP 报告的错误（本机的）不要
        let mut local = ee.clone();
        local[4] = 1;
        let r = received(
            sockaddr_in([1, 1, 1, 1], 0),
            cmsg(SOL_IP, IP_RECVERR, &local),
        );
        assert!(error_v4(-1, false, &[], 3, &request, &r).is_none());
    }

    #[test]
    fn control_messages_are_cut_like_put_cmsg() {
        let mut control = cmsg(SOL_IP, IP_PKTINFO, &[0; 12]);
        control.extend(cmsg(SOL_IP, IP_TTL, &64i32.to_le_bytes()));
        // 放不下：第一条整条，第二条截到剩下的长度；最后一条的对齐空白可以不要
        let (fit, flags) = fit_control(&control, 32 + 18);
        assert_eq!(flags, MSG_CTRUNC);
        assert_eq!(fit.len(), 50);
        assert_eq!(fit[32..40], 18u64.to_le_bytes());
        assert_eq!(
            fit_control(&control, 32 + 12),
            (control[..32].to_vec(), MSG_CTRUNC)
        );
        assert_eq!(fit_control(&control, 32 + 20), (control[..52].to_vec(), 0));
        assert_eq!(fit_control(&control, 8), (Vec::new(), MSG_CTRUNC));
        assert_eq!(fit_control(&control, 64), (control.clone(), 0));
    }
}
