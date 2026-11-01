//! 1024 以下的端口映射（设计文档 §10）。
//!
//! Android 上的 App 没有 `CAP_NET_BIND_SERVICE`，宿主上绑不了 1024 以下的端口；guest 里的
//! root 照样要绑 22、80 这些（sshd、nginx 的默认配置）。guest 的 socket 绑这些端口时，宿主
//! socket 绑到另一个端口上，guest 一侧看不出来：getsockname、getpeername、accept、recvfrom、
//! `/proc/net` 报告的都是原来的端口，连本机的这个端口时换成宿主上的。别的设备、手机上别的
//! App 要连宿主上的端口，App 第一次看到在非回环地址上监听时发通知告诉用户
//! （[`PortMap::take_listens`]）。
//!
//! 宿主端口依次试 `端口 + 偏移`、`端口 + 2 × 偏移`、`端口 + 3 × 偏移`（不超过 65535），
//! 都被占了就让宿主分配一个。后三位始终是原来的端口（偏移是 10000 时 22 是 10022、20022、
//! 30022），一眼认得出来，也不会占掉别的端口的位置。一个（网络命名空间、协议、端口）只要
//! 还有 socket 绑着，就一直用同一个宿主端口（`SO_REUSEPORT`、IPv4 和 IPv6 分开绑），guest
//! 自己的冲突照常是 EADDRINUSE；同一个端口的 TCP 和 UDP 尽量用同一个宿主端口（DNS 的 53）。
//! 全部关掉之后 [`STICKY`] 之内再绑，仍先用原来的宿主端口、绑不上就是 EADDRINUSE：TCP 连接
//! 关掉之后宿主端口处在 TIME_WAIT，Linux 上不带 `SO_REUSEADDR` 再绑也是 EADDRINUSE，不能
//! 把它当作别的程序占着、换到别的端口去。

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::cell::RefCell;
use crate::errno::Errno;

/// 默认的偏移。
pub const DEFAULT_OFFSET: u16 = 10000;

/// 最多攒着多少个还没被 App 取走的监听事件（appine-run 不取）。
const MAX_EVENTS: usize = 32;

/// 映射全部释放之后多久之内再绑仍用原来的宿主端口：比 TIME_WAIT（Linux、Android 都是
/// 60 秒）长。
pub const STICKY: Duration = Duration::from_secs(120);

/// 映射全部释放之后，宿主端口还换回 guest 端口多久。Linux 在数据包到达时换算，这里在对方
/// recvfrom、accept 时才换：服务回完一个数据报就关掉 socket（`srv.py` 那样），回答还在
/// 对方的接收队列里，对方读到时来源照样是 guest 的端口。宿主在这段时间里把这个端口分给
/// 同一个命名空间里别的、没有映射的 socket 的可能很小（宿主随机选临时端口）。
pub const LINGER: Duration = Duration::from_secs(10);

/// 登记表的键：网络命名空间（开机时的是 0，别的是 `NetNs` 的地址）、协议、guest 的端口。
type Key = (usize, i32, u16);

struct Entry {
    host: u16,
    refs: u32,
}

/// guest 第一次在非回环地址上监听一个端口（设计文档 §12 #7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenEvent {
    /// 监听的进程的名字（comm）。
    pub comm: String,
    /// guest 看到的端口。
    pub port: u16,
    /// 宿主上的端口：别的设备连它（没有映射时与 `port` 相同）。
    pub host_port: u16,
}

/// 端口映射的登记表，整个内核一份（[`crate::kernel::Kernel::portmap`]）。
pub struct PortMap {
    /// 0 表示不映射：绑低端口照旧交给宿主（Android 上是 EACCES）。
    pub offset: u16,
    entries: HashMap<Key, Entry>,
    /// 宿主端口 → guest 端口（同样的键，端口是宿主上的），全部释放了的带着释放的时刻
    /// （[`LINGER`]）。
    by_host: HashMap<Key, (u16, Option<Instant>)>,
    /// 全部释放了的映射：宿主端口和释放的时刻（[`STICKY`]）。
    released: HashMap<Key, (u16, Instant)>,
    /// 报告过的（guest 端口, 宿主端口）。
    reported: HashSet<(u16, u16)>,
    events: VecDeque<ListenEvent>,
}

/// 一个 socket 绑着的映射：析构时归还（最后一个归还的去掉登记）。
pub struct Lease {
    map: Rc<RefCell<PortMap>>,
    key: Key,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut m = self.map.borrow_mut();
        let Some(e) = m.entries.get_mut(&self.key) else {
            return;
        };
        e.refs -= 1;
        if e.refs == 0 {
            let host = e.host;
            m.entries.remove(&self.key);
            let (ns, proto, port) = self.key;
            let now = Instant::now();
            m.by_host
                .retain(|_, (_, t)| t.is_none_or(|t| now.duration_since(t) < LINGER));
            if let Some(v) = m.by_host.get_mut(&(ns, proto, host))
                && v.0 == port
            {
                v.1 = Some(now);
            }
            m.released
                .retain(|_, (_, t)| now.duration_since(*t) < STICKY);
            m.released.insert(self.key, (host, now));
        }
    }
}

impl PortMap {
    pub fn new() -> PortMap {
        PortMap {
            offset: DEFAULT_OFFSET,
            entries: HashMap::new(),
            by_host: HashMap::new(),
            released: HashMap::new(),
            reported: HashSet::new(),
            events: VecDeque::new(),
        }
    }

    /// 这个端口要不要映射。
    pub fn maps(&self, port: u16) -> bool {
        self.offset != 0 && (1..1024).contains(&port)
    }

    /// 已经映射了的 guest 端口在宿主上是哪个（连本机的这个端口时换成它）。
    pub fn host_port(&self, ns: usize, proto: i32, port: u16) -> Option<u16> {
        self.entries.get(&(ns, proto, port)).map(|e| e.host)
    }

    /// 宿主端口是映射来的时（包括刚释放的，[`LINGER`]），guest 看到的端口。
    pub fn guest_port(&self, ns: usize, proto: i32, host: u16) -> Option<u16> {
        self.by_host
            .get(&(ns, proto, host))
            .filter(|(_, t)| t.is_none_or(|t| t.elapsed() < LINGER))
            .map(|&(g, _)| g)
    }

    /// 记下一次在非回环地址上的监听：（guest 端口, 宿主端口）第一次出现时排进事件队列。
    pub fn listened(&mut self, comm: String, port: u16, host_port: u16) {
        if self.reported.insert((port, host_port)) && self.events.len() < MAX_EVENTS {
            self.events.push_back(ListenEvent {
                comm,
                port,
                host_port,
            });
        }
    }

    /// 取走攒着的监听事件。
    pub fn take_listens(&mut self) -> Vec<ListenEvent> {
        self.events.drain(..).collect()
    }
}

impl Default for PortMap {
    fn default() -> Self {
        PortMap::new()
    }
}

/// 把 guest 的端口 `port` 绑到宿主上：`bind` 按给的宿主端口绑（0 是让宿主分配），返回
/// 宿主 errno；`bound_port` 读出宿主实际绑到的端口。返回宿主端口和租约，以及这个映射
/// 是不是新建的（新建的写一条内核日志）。
pub fn bind(
    map: &Rc<RefCell<PortMap>>,
    ns: usize,
    proto: i32,
    port: u16,
    mut bind: impl FnMut(u16) -> Result<(), i32>,
    bound_port: impl Fn() -> Option<u16>,
) -> Result<(u16, Lease, bool), Errno> {
    let key = (ns, proto, port);
    let existing = map.borrow().host_port(ns, proto, port).or_else(|| {
        // 刚释放的：仍用原来的宿主端口（STICKY）
        let m = map.borrow();
        m.released
            .get(&key)
            .filter(|(_, t)| t.elapsed() < STICKY)
            .map(|&(h, _)| h)
    });
    let lease = |map: &Rc<RefCell<PortMap>>, host: u16| {
        let mut m = map.borrow_mut();
        m.released.remove(&key);
        m.entries.entry(key).or_insert(Entry { host, refs: 0 }).refs += 1;
        m.by_host.insert((ns, proto, host), (port, None));
        Lease {
            map: map.clone(),
            key,
        }
    };
    // 已经有 socket 绑着这个端口、刚释放过：用同一个宿主端口，冲突就是 guest 自己的
    if let Some(host) = existing {
        bind(host).map_err(Errno)?;
        return Ok((host, lease(map, host), false));
    }
    let offset = u32::from(map.borrow().offset);
    let mut candidates = Vec::new();
    // 同一个端口的另一个协议已经映射了：先试它的宿主端口
    let other = if proto == crate::net::IPPROTO_TCP {
        crate::net::IPPROTO_UDP
    } else {
        crate::net::IPPROTO_TCP
    };
    if let Some(h) = map.borrow().host_port(ns, other, port) {
        candidates.push(h);
    }
    for k in 1..=3u32 {
        let h = u32::from(port) + k * offset;
        if h <= u32::from(u16::MAX) && !candidates.contains(&(h as u16)) {
            candidates.push(h as u16);
        }
    }
    candidates.push(0);
    for h in candidates {
        match bind(h) {
            Ok(()) => {
                let host = if h == 0 {
                    bound_port().ok_or(Errno::EADDRINUSE)?
                } else {
                    h
                };
                return Ok((host, lease(map, host), true));
            }
            // 被 guest 以外的程序占着（或者 guest 直接绑了这个高端口）：换下一个
            Err(e) if e == Errno::EADDRINUSE.0 && h != 0 => {}
            Err(e) => return Err(Errno(e)),
        }
    }
    Err(Errno::EADDRINUSE)
}

/// 新的网络命名空间里显式绑的端口（[`bind`] 是开机时的命名空间里的低端口）：宿主上一律另选
/// 一个（让宿主分配），不占宿主上的这个端口，开机时的命名空间、手机上别的 App 照样绑得了
/// 它（宿主上别人绑了通配地址时，绑这个命名空间的回环地址本来也会冲突）。同一个命名空间里
/// 已经有 socket 绑着这个端口时用同一个宿主端口，冲突就是 guest 自己的（EADDRINUSE、
/// `SO_REUSEPORT`）。没有 [`STICKY`]：宿主端口别人不会去连，换一个就是。
pub fn bind_any(
    map: &Rc<RefCell<PortMap>>,
    ns: usize,
    proto: i32,
    port: u16,
    mut bind: impl FnMut(u16) -> Result<(), i32>,
    bound_port: impl Fn() -> Option<u16>,
) -> Result<(u16, Lease), Errno> {
    let key = (ns, proto, port);
    let lease = |map: &Rc<RefCell<PortMap>>, host: u16| {
        let mut m = map.borrow_mut();
        m.entries.entry(key).or_insert(Entry { host, refs: 0 }).refs += 1;
        m.by_host.insert((ns, proto, host), (port, None));
        Lease {
            map: map.clone(),
            key,
        }
    };
    let existing = map.borrow().host_port(ns, proto, port);
    if let Some(host) = existing {
        bind(host).map_err(Errno)?;
        return Ok((host, lease(map, host)));
    }
    bind(0).map_err(Errno)?;
    let host = bound_port().ok_or(Errno::EADDRINUSE)?;
    Ok((host, lease(map, host)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: i32 = crate::net::IPPROTO_TCP;
    const UDP: i32 = crate::net::IPPROTO_UDP;

    fn map() -> Rc<RefCell<PortMap>> {
        Rc::new(RefCell::new(PortMap::new()))
    }

    #[test]
    fn tries_the_offsets_then_lets_the_host_choose() {
        let m = map();
        let busy = [10022u16, 20022];
        let (h, _a, new) = bind(
            &m,
            0,
            TCP,
            22,
            |p| {
                if busy.contains(&p) {
                    Err(Errno::EADDRINUSE.0)
                } else {
                    Ok(())
                }
            },
            || None,
        )
        .unwrap();
        assert_eq!((h, new), (30022, true));
        let all = [30080u16, 10080, 20080];
        let (h, _b, _) = bind(
            &m,
            0,
            TCP,
            80,
            |p| {
                if all.contains(&p) {
                    Err(Errno::EADDRINUSE.0)
                } else {
                    Ok(())
                }
            },
            || Some(45678),
        )
        .unwrap();
        assert_eq!(h, 45678);
        assert_eq!(m.borrow().guest_port(0, TCP, 45678), Some(80));
        // 别的错误不往下试
        assert_eq!(
            bind(&m, 0, TCP, 23, |_| Err(Errno::EACCES.0), || None).err(),
            Some(Errno::EACCES)
        );
    }

    /// 新命名空间里的端口：宿主分配；同一个命名空间里再绑用同一个宿主端口（冲突交给宿主），
    /// 别的命名空间的同一个端口另外分配，全部释放之后再绑不必用原来的。
    #[test]
    fn namespace_ports_let_the_host_choose() {
        let m = map();
        let mut tried = Vec::new();
        let (h, a) = bind_any(
            &m,
            7,
            TCP,
            8080,
            |p| {
                tried.push(p);
                Ok(())
            },
            || Some(41000),
        )
        .unwrap();
        assert_eq!((h, tried), (41000, vec![0]));
        assert_eq!(m.borrow().host_port(7, TCP, 8080), Some(41000));
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(8080));
        // 同一个命名空间：同一个宿主端口，宿主说冲突就是 EADDRINUSE
        let r = bind_any(
            &m,
            7,
            TCP,
            8080,
            |p| {
                assert_eq!(p, 41000);
                Err(Errno::EADDRINUSE.0)
            },
            || None,
        );
        assert_eq!(r.err(), Some(Errno::EADDRINUSE));
        let (h2, b) = bind_any(&m, 7, TCP, 8080, |_| Ok(()), || None).unwrap();
        assert_eq!(h2, 41000);
        // 别的命名空间、开机时的命名空间的同一个端口互不相干
        let (h3, _c) = bind_any(
            &m,
            9,
            TCP,
            8080,
            |p| {
                assert_eq!(p, 0);
                Ok(())
            },
            || Some(42000),
        )
        .unwrap();
        assert_eq!(h3, 42000);
        assert_eq!(m.borrow().host_port(0, TCP, 8080), None);
        // 最后一个租约归还之后登记就没了，再绑重新分配
        drop((a, b));
        assert_eq!(m.borrow().host_port(7, TCP, 8080), None);
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(8080));
        let (h4, _d) = bind_any(
            &m,
            7,
            TCP,
            8080,
            |p| {
                assert_eq!(p, 0);
                Ok(())
            },
            || Some(43000),
        )
        .unwrap();
        assert_eq!(h4, 43000);
        // 宿主把刚释放的端口分给了另一个映射：换成新的
        let (_, _e) = bind_any(&m, 7, TCP, 9090, |_| Ok(()), || Some(41000)).unwrap();
        assert_eq!(m.borrow().guest_port(7, TCP, 41000), Some(9090));
    }

    #[test]
    fn a_bound_port_keeps_its_host_port_until_the_last_lease() {
        let m = map();
        let (h, a, _) = bind(&m, 0, TCP, 22, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 10022);
        // 第二个 socket（SO_REUSEPORT、另一个地址族）用同一个宿主端口，冲突照常报告
        let (h2, b, new) = bind(
            &m,
            0,
            TCP,
            22,
            |p| if p == 10022 { Ok(()) } else { Err(1) },
            || None,
        )
        .unwrap();
        assert_eq!((h2, new), (10022, false));
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        drop(a);
        assert_eq!(m.borrow().host_port(0, TCP, 22), Some(10022));
        drop(b);
        assert_eq!(m.borrow().host_port(0, TCP, 22), None);
        // 宿主端口还换得回 guest 的端口一会儿（对方还没读到的回答）
        assert_eq!(m.borrow().guest_port(0, TCP, 10022), Some(22));
        m.borrow_mut().by_host.get_mut(&(0, TCP, 10022)).unwrap().1 = Some(Instant::now() - LINGER);
        assert_eq!(m.borrow().guest_port(0, TCP, 10022), None);
        // 刚释放的端口再绑仍用原来的宿主端口，被占着（TIME_WAIT）就是 EADDRINUSE，不换
        assert_eq!(
            bind(&m, 0, TCP, 22, |_| Err(Errno::EADDRINUSE.0), || None).err(),
            Some(Errno::EADDRINUSE)
        );
        let (h, again, _) = bind(
            &m,
            0,
            TCP,
            22,
            |p| if p == 10022 { Ok(()) } else { Err(1) },
            || None,
        )
        .unwrap();
        assert_eq!(h, 10022);
        drop(again);
        // 命名空间、协议各自独立；UDP 先试 TCP 的宿主端口
        let (_, _c, _) = bind(
            &m,
            0,
            TCP,
            53,
            |p| {
                if p == 10053 {
                    Err(Errno::EADDRINUSE.0)
                } else {
                    Ok(())
                }
            },
            || None,
        )
        .unwrap();
        let (h, _d, _) = bind(&m, 0, UDP, 53, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 20053);
        let (h, _e, _) = bind(&m, 1, TCP, 53, |_| Ok(()), || None).unwrap();
        assert_eq!(h, 10053);
    }

    #[test]
    fn listens_are_reported_once() {
        let mut m = PortMap::new();
        m.listened("sshd".into(), 22, 10022);
        m.listened("sshd".into(), 22, 10022);
        m.listened("sshd".into(), 22, 20022);
        let e = m.take_listens();
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].host_port, 20022);
        assert!(m.take_listens().is_empty());
        assert!(m.maps(22) && !m.maps(0) && !m.maps(1024));
        m.offset = 0;
        assert!(!m.maps(22));
    }
}
