//! 宿主 socket（设计文档 §10）。
//!
//! guest 的 TCP/UDP socket 直接对应一个宿主 socket。guest 和宿主都是 arm64 Linux 的
//! ABI，所以 sockaddr、socket 选项和控制消息的字节布局完全相同，这里按字节原样传递。
//! 宿主 socket 一律是非阻塞的，阻塞语义由内核层在统一的等待机制里模拟。

use std::time::Duration;

use super::last_errno;

/// 新建非阻塞、close-on-exec 的宿主 socket。
pub fn socket(domain: i32, ty: i32, protocol: i32) -> Result<i32, i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let fd = unsafe {
        libc::socket(
            domain,
            ty | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            protocol,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let fd = set_flags(unsafe { libc::socket(domain, ty, protocol) });
    if fd < 0 { Err(last_errno()) } else { Ok(fd) }
}

/// 没有 `SOCK_NONBLOCK`、accept4 的宿主：创建之后再设成非阻塞、close-on-exec。
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_flags(fd: i32) -> i32 {
    if fd >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    fd
}

/// 关闭宿主 fd。
pub fn close(fd: i32) {
    unsafe { libc::close(fd) };
}

fn check(rc: libc::c_int) -> Result<(), i32> {
    if rc < 0 { Err(last_errno()) } else { Ok(()) }
}

/// bind(2)。
pub fn bind(fd: i32, addr: &[u8]) -> Result<(), i32> {
    check(unsafe { libc::bind(fd, addr.as_ptr().cast(), addr.len() as libc::socklen_t) })
}

/// connect(2)。非阻塞 socket 上返回 `EINPROGRESS` 表示连接正在进行。
pub fn connect(fd: i32, addr: &[u8]) -> Result<(), i32> {
    check(unsafe { libc::connect(fd, addr.as_ptr().cast(), addr.len() as libc::socklen_t) })
}

/// listen(2)。
pub fn listen(fd: i32, backlog: i32) -> Result<(), i32> {
    check(unsafe { libc::listen(fd, backlog) })
}

/// accept4(2)：新的 socket 同样是非阻塞、close-on-exec 的。返回 (fd, 对端地址)。
pub fn accept(fd: i32) -> Result<(i32, Vec<u8>), i32> {
    let mut addr = [0u8; 128];
    let mut len = addr.len() as libc::socklen_t;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let new = unsafe {
        libc::accept4(
            fd,
            addr.as_mut_ptr().cast(),
            &mut len,
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let new = set_flags(unsafe { libc::accept(fd, addr.as_mut_ptr().cast(), &mut len) });
    if new < 0 {
        return Err(last_errno());
    }
    Ok((new, addr[..len as usize].to_vec()))
}

/// getsockname(2) 或 getpeername(2)。
pub fn sockname(fd: i32, peer: bool) -> Result<Vec<u8>, i32> {
    let mut addr = [0u8; 128];
    let mut len = addr.len() as libc::socklen_t;
    let rc = unsafe {
        if peer {
            libc::getpeername(fd, addr.as_mut_ptr().cast(), &mut len)
        } else {
            libc::getsockname(fd, addr.as_mut_ptr().cast(), &mut len)
        }
    };
    check(rc)?;
    Ok(addr[..len as usize].to_vec())
}

/// setsockopt(2)。
pub fn setsockopt(fd: i32, level: i32, name: i32, value: &[u8]) -> Result<(), i32> {
    check(unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            value.as_ptr().cast(),
            value.len() as libc::socklen_t,
        )
    })
}

/// getsockopt(2)：最多取 `len` 字节。
pub fn getsockopt(fd: i32, level: i32, name: i32, len: usize) -> Result<Vec<u8>, i32> {
    let mut buf = vec![0u8; len];
    let mut l = len as libc::socklen_t;
    check(unsafe { libc::getsockopt(fd, level, name, buf.as_mut_ptr().cast(), &mut l) })?;
    buf.truncate(l as usize);
    Ok(buf)
}

/// shutdown(2)。
pub fn shutdown(fd: i32, how: i32) -> Result<(), i32> {
    check(unsafe { libc::shutdown(fd, how) })
}

/// sendmsg(2)：`addr` 为空时发给已连接的对端。总是带上 `MSG_NOSIGNAL`，连接断开时
/// 返回 EPIPE 而不是让宿主收到 SIGPIPE。
pub fn sendmsg(
    fd: i32,
    data: &[u8],
    addr: &[u8],
    control: &[u8],
    flags: i32,
) -> Result<usize, i32> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    if !addr.is_empty() {
        msg.msg_name = addr.as_ptr() as *mut libc::c_void;
        msg.msg_namelen = addr.len() as libc::socklen_t;
    }
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !control.is_empty() {
        msg.msg_control = control.as_ptr() as *mut libc::c_void;
        msg.msg_controllen = control.len() as _;
    }
    let n = unsafe { libc::sendmsg(fd, &msg, flags | libc::MSG_NOSIGNAL) };
    if n < 0 {
        Err(last_errno())
    } else {
        Ok(n as usize)
    }
}

/// recvmsg(2) 的结果。
pub struct Received {
    /// 数据长度（`MSG_TRUNC` 时可能大于缓冲区）。
    pub len: usize,
    /// 对端地址。
    pub addr: Vec<u8>,
    /// 控制消息。
    pub control: Vec<u8>,
    /// `msg_flags`。
    pub flags: i32,
}

/// recvmsg(2)：数据读进 `buf`，控制消息最多取 `control_len` 字节。
pub fn recvmsg(fd: i32, buf: &mut [u8], control_len: usize, flags: i32) -> Result<Received, i32> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut addr = [0u8; 128];
    let mut control = vec![0u8; control_len];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = addr.as_mut_ptr().cast();
    msg.msg_namelen = addr.len() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if control_len > 0 {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control_len as _;
    }
    let n = unsafe { libc::recvmsg(fd, &mut msg, flags) };
    if n < 0 {
        return Err(last_errno());
    }
    // msg_controllen 在 Linux 上是 size_t，在 macOS 上是 socklen_t
    #[allow(clippy::unnecessary_cast)]
    control.truncate(msg.msg_controllen as usize);
    Ok(Received {
        len: n as usize,
        addr: addr[..(msg.msg_namelen as usize).min(addr.len())].to_vec(),
        control,
        flags: msg.msg_flags,
    })
}

/// 宿主上的一个网络接口（netlink 模拟用）。
#[derive(Debug, Clone, Default)]
pub struct Interface {
    /// 名字。
    pub name: String,
    /// 接口编号。
    pub index: u32,
    /// `IFF_*` 标志。
    pub flags: u32,
    /// MTU。
    pub mtu: u32,
    /// 硬件地址。
    pub mac: Option<[u8; 6]>,
    /// 地址：(地址族, 地址, 前缀长度)。
    pub addrs: Vec<(i32, Vec<u8>, u8)>,
}

fn prefix_len(mask: &[u8]) -> u8 {
    mask.iter().map(|b| b.count_ones() as u8).sum()
}

/// 宿主的网络接口（getifaddrs）。
pub fn interfaces() -> Vec<Interface> {
    let mut list: Vec<Interface> = Vec::new();
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifa) } < 0 {
        return list;
    }
    let mut cur = ifa;
    while !cur.is_null() {
        let a = unsafe { &*cur };
        cur = a.ifa_next;
        let name = unsafe { std::ffi::CStr::from_ptr(a.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let index = unsafe { libc::if_nametoindex(a.ifa_name) };
        let entry = match list.iter_mut().position(|i| i.name == name) {
            Some(i) => &mut list[i],
            None => {
                list.push(Interface {
                    name,
                    index,
                    flags: a.ifa_flags,
                    mtu: if a.ifa_flags & libc::IFF_LOOPBACK as u32 != 0 {
                        65536
                    } else {
                        1500
                    },
                    ..Default::default()
                });
                list.last_mut().unwrap()
            }
        };
        if a.ifa_addr.is_null() {
            continue;
        }
        let family = unsafe { (*a.ifa_addr).sa_family } as i32;
        match family {
            libc::AF_INET => {
                let sin = unsafe { &*(a.ifa_addr as *const libc::sockaddr_in) };
                let mask = unsafe { (a.ifa_netmask as *const libc::sockaddr_in).as_ref() };
                let prefix = mask.map_or(32, |m| prefix_len(&m.sin_addr.s_addr.to_ne_bytes()));
                entry
                    .addrs
                    .push((family, sin.sin_addr.s_addr.to_ne_bytes().to_vec(), prefix));
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*(a.ifa_addr as *const libc::sockaddr_in6) };
                let mask = unsafe { (a.ifa_netmask as *const libc::sockaddr_in6).as_ref() };
                let prefix = mask.map_or(128, |m| prefix_len(&m.sin6_addr.s6_addr));
                entry
                    .addrs
                    .push((family, sin6.sin6_addr.s6_addr.to_vec(), prefix));
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::AF_PACKET => {
                let ll = unsafe { &*(a.ifa_addr as *const libc::sockaddr_ll) };
                if ll.sll_halen == 6 {
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&ll.sll_addr[..6]);
                    entry.mac = Some(mac);
                }
            }
            _ => {}
        }
    }
    unsafe { libc::freeifaddrs(ifa) };
    list.sort_by_key(|i| i.index);
    list
}

/// 宿主 `/etc/resolv.conf` 里的名字服务器（sockaddr 的字节）。
#[cfg(not(target_os = "android"))]
fn nameservers() -> Vec<Vec<u8>> {
    let Ok(conf) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|a| a.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| sockaddr(std::net::SocketAddr::new(ip, 53)))
        .take(3)
        .collect()
}

/// `SocketAddr` 编码成 sockaddr 的字节。
pub fn sockaddr(addr: std::net::SocketAddr) -> Vec<u8> {
    match addr {
        std::net::SocketAddr::V4(a) => {
            let mut b = vec![0u8; 16];
            b[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
            b[2..4].copy_from_slice(&a.port().to_be_bytes());
            b[4..8].copy_from_slice(&a.ip().octets());
            b
        }
        std::net::SocketAddr::V6(a) => {
            let mut b = vec![0u8; 28];
            b[..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
            b[2..4].copy_from_slice(&a.port().to_be_bytes());
            b[8..24].copy_from_slice(&a.ip().octets());
            b[24..28].copy_from_slice(&a.scope_id().to_ne_bytes());
            b
        }
    }
}

/// 等一个 fd 就绪，超时返回 `ETIMEDOUT`。
#[cfg(not(target_os = "android"))]
fn wait_fd(fd: i32, events: i16, timeout: Duration) -> Result<(), i32> {
    let mut fds = [(fd, events, 0)];
    match super::poll(&mut fds, Some(timeout))? {
        0 => Err(libc::ETIMEDOUT),
        _ => Ok(()),
    }
}

/// 经由 UDP 向一个名字服务器查询，回答被截断（TC 位）时改用 TCP。
#[cfg(not(target_os = "android"))]
fn query_server(server: &[u8], query: &[u8]) -> Result<Vec<u8>, i32> {
    const TIMEOUT: Duration = Duration::from_secs(3);
    let family = u16::from_ne_bytes([server[0], server[1]]) as i32;
    let fd = socket(family, libc::SOCK_DGRAM, 0)?;
    let result = (|| -> Result<Vec<u8>, i32> {
        connect(fd, server)?;
        sendmsg(fd, query, &[], &[], 0)?;
        let mut buf = vec![0u8; 65536];
        loop {
            wait_fd(fd, libc::POLLIN, TIMEOUT)?;
            let r = recvmsg(fd, &mut buf, 0, 0)?;
            // 只接受与查询 ID 相同的回答
            if r.len >= 12 && buf[..2] == query[..2] {
                buf.truncate(r.len);
                return Ok(buf);
            }
        }
    })();
    close(fd);
    let answer = result?;
    if answer[2] & 0x02 == 0 {
        return Ok(answer);
    }
    query_tcp(server, query, TIMEOUT)
}

#[cfg(not(target_os = "android"))]
fn query_tcp(server: &[u8], query: &[u8], timeout: Duration) -> Result<Vec<u8>, i32> {
    let family = u16::from_ne_bytes([server[0], server[1]]) as i32;
    let fd = socket(family, libc::SOCK_STREAM, 0)?;
    let result = (|| -> Result<Vec<u8>, i32> {
        match connect(fd, server) {
            Err(libc::EINPROGRESS) => wait_fd(fd, libc::POLLOUT, timeout)?,
            r => r?,
        }
        let mut msg = (query.len() as u16).to_be_bytes().to_vec();
        msg.extend_from_slice(query);
        let mut sent = 0;
        while sent < msg.len() {
            wait_fd(fd, libc::POLLOUT, timeout)?;
            sent += sendmsg(fd, &msg[sent..], &[], &[], 0)?;
        }
        let mut answer = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if answer.len() >= 2 {
                let want = u16::from_be_bytes([answer[0], answer[1]]) as usize + 2;
                if answer.len() >= want {
                    answer.truncate(want);
                    return Ok(answer.split_off(2));
                }
            }
            wait_fd(fd, libc::POLLIN, timeout)?;
            match recvmsg(fd, &mut buf, 0, 0)?.len {
                0 => return Err(libc::ECONNRESET),
                n => answer.extend_from_slice(&buf[..n]),
            }
        }
    })();
    close(fd);
    result
}

/// 把一个 DNS 查询（报文）交给宿主解析，返回回答的报文（设计文档 §10 的 DNS 存根）。
///
/// Android 上交给 `android_res_nsend`，私人 DNS、VPN 和按网络分流的设置都会生效；
/// 其他宿主上依次询问宿主 `/etc/resolv.conf` 里的名字服务器。会阻塞，调用方不能
/// 持有锁。
#[cfg(target_os = "android")]
pub fn dns_query(query: &[u8]) -> Result<Vec<u8>, i32> {
    #[link(name = "android")]
    unsafe extern "C" {
        fn android_res_nsend(network: u64, msg: *const u8, len: usize, flags: u32) -> i32;
        fn android_res_nresult(fd: i32, rcode: *mut i32, answer: *mut u8, len: usize) -> i32;
        fn android_res_cancel(fd: i32);
    }
    /// `NETWORK_UNSPECIFIED`：系统的默认网络。
    const DEFAULT_NETWORK: u64 = 0;
    if query.len() < 12 {
        return Err(libc::EINVAL);
    }
    let fd = unsafe { android_res_nsend(DEFAULT_NETWORK, query.as_ptr(), query.len(), 0) };
    if fd < 0 {
        return Err(-fd);
    }
    // 系统解析器自己会重试和超时，这里只防止它永远不回答
    let mut pfd = [(fd, libc::POLLIN, 0)];
    if !matches!(super::poll(&mut pfd, Some(Duration::from_secs(30))), Ok(1)) {
        unsafe { android_res_cancel(fd) };
        return Err(libc::ETIMEDOUT);
    }
    let mut answer = vec![0u8; 65535];
    let mut rcode = 0;
    let n = unsafe { android_res_nresult(fd, &mut rcode, answer.as_mut_ptr(), answer.len()) };
    if n < 0 {
        return Err(-n);
    }
    answer.truncate(n as usize);
    Ok(answer)
}

/// 把一个 DNS 查询（报文）交给宿主解析，返回回答的报文（设计文档 §10 的 DNS 存根）。
///
/// Android 上交给 `android_res_nsend`，私人 DNS、VPN 和按网络分流的设置都会生效；
/// 其他宿主上依次询问宿主 `/etc/resolv.conf` 里的名字服务器。会阻塞，调用方不能
/// 持有锁。
#[cfg(not(target_os = "android"))]
pub fn dns_query(query: &[u8]) -> Result<Vec<u8>, i32> {
    if query.len() < 12 {
        return Err(libc::EINVAL);
    }
    let mut last = libc::EHOSTUNREACH;
    for server in nameservers() {
        match query_server(&server, query) {
            Ok(answer) => return Ok(answer),
            Err(e) => last = e,
        }
    }
    Err(last)
}
