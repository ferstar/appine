//! I/O 多路复用（设计文档 §10）：ppoll、pselect6、epoll，以及 eventfd。
//!
//! 三者共用 [`wait_ready`]：先检查进程内对象（管道、PTY、eventfd……）的就绪状态，
//! 宿主 fd（宿主 socket、宿主 stdio）用一次不等待的宿主 poll 检查；都没有就绪时，
//! 把宿主 fd 和自己的唤醒事件放在一起等（§6 的统一等待机制）。epoll 在进程内实现，
//! 默认水平触发。
//!
//! `EPOLLET`：Linux 每次唤醒文件的等待队列（来了数据、新的连接、eventfd 被写、timerfd
//! 到期……）都让边沿触发的关注再报告一次，即使文件本来就已经就绪；mio（tokio）的唤醒器
//! 就靠这一点，它从来不读走 eventfd 的计数。这里按"数据代数"判断（[`arrivals`]）：
//! 进程内的对象每来一次事件记一次；宿主 socket 上的数据不经过内核层，由一个只关注它的
//! 边沿触发的宿主 epoll 记录（[`HostEdges`]，有边沿触发的关注时才建立）。某一位从未
//! 就绪变成就绪，或者那个方向又来了事件时触发，触发时与 Linux 一样报告当前全部就绪的
//! 位。程序在一个方向上用完数据（读写得到 EAGAIN，或者没有读写满要求的长度，见
//! [`drained`]）之后，这个方向上次报告过的位也全部忘掉：建不了 [`HostEdges`] 时（宿主
//! 上的普通文件）只能靠它，而且读到 EAGAIN 和新数据到达之间没有调用 epoll_wait 时
//! （Go 的 netpoller 就会这样）也不会丢事件。
//!
//! 等待时，边沿触发的关注不等报告过、仍然就绪的位：宿主 poll 会立刻返回，epoll_wait
//! 就变成反复释放、获取 BKL 的忙等，把其他线程饿死。所以改为等宿主 fd 上的新事件
//! （[`HostEdges`] 的 fd），进程内的对象来了数据（管道不空时只叫醒这些等待者，见
//! [`arrived`]）、程序用完数据时叫醒这些 epoll_wait 重新计算
//! （[`Kernel::edge_waiters`](crate::kernel::Kernel)）。
//!
//! `EPOLLEXCLUSIVE`：Linux 每次唤醒只叫醒一个独占的等待者。这里一个边沿（数据代数）
//! 只报告给一个 epoll（[`OpenFile::exclusive_claim`]）：水平触发的关注由报告过它的
//! epoll 接着报告，别的 epoll 等下一个边沿，所以几个进程各用一个 epoll 等同一个监听
//! socket 时，一个新连接只让一个进程从 epoll_wait 返回。等待的线程不自己等这个文件，
//! 而是排在它的独占门上（[`ExGate`]），新的边沿只叫醒排在最前面的一个。
//!
//! 等待时登记关注的进程内对象（[`wake_objects`]）：只影响别的对象的修改（另一个管道的
//! 读写、别的 eventfd、别的 AF_UNIX 连接……）不叫醒它（§6）。

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{SysResult, get, read_timespec, tty};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, HostEdges, OpenFile, oflags};
use crate::kernel::{Cx, request_wake_object, wake};
use crate::pipe::PIPE_BUF;
use crate::process::RLIMIT_NOFILE;
use crate::signal;
use crate::uaccess::UserSlice;
use crate::vfs::dev::Device;

const POLLIN: i16 = 0x001;
const POLLPRI: i16 = 0x002;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;

/// 写方向的就绪位，其余的都算读方向。
const WRITE_BITS: u32 = (POLLOUT | POLLWRNORM) as u32 | 0x200; // POLLWRBAND

/// 嵌套 epoll 的最大深度，与 Linux 的 `EP_MAX_NESTS` 相同。
const MAX_NESTS: u32 = 4;

/// 一个打开文件现在的就绪状态：(已经知道的事件, 需要问宿主的 fd)。
fn readiness(f: &OpenFile, depth: u32) -> (i16, Option<i32>) {
    match &f.kind {
        FileKind::Host(fd) => (0, Some(*fd)),
        FileKind::Socket(s) => super::net::readiness(&s.borrow()),
        FileKind::Pipe(end) => {
            let p = end.pipe.borrow();
            let mut ev = 0;
            if end.write {
                if p.readers == 0 {
                    ev |= POLLERR;
                } else if p.space() >= PIPE_BUF {
                    ev |= POLLOUT | POLLWRNORM;
                }
            }
            if end.read {
                if !p.buf.is_empty() {
                    ev |= POLLIN | POLLRDNORM;
                }
                if p.writers == 0 && end.hup_after != Some(p.writer_opens) {
                    ev |= POLLHUP;
                }
            }
            (ev, None)
        }
        FileKind::Pty(end) => (tty::readiness(&end.tty.borrow(), end.master), None),
        FileKind::EventFd(e) => {
            let e = e.borrow();
            let mut ev = 0;
            if e.count > 0 {
                ev |= POLLIN | POLLRDNORM;
            }
            if e.count < u64::MAX - 1 {
                ev |= POLLOUT | POLLWRNORM;
            }
            (ev, None)
        }
        FileKind::TimerFd(t) => {
            let mut t = t.borrow_mut();
            t.update(Instant::now());
            (
                if t.expirations > 0 {
                    POLLIN | POLLRDNORM
                } else {
                    0
                },
                None,
            )
        }
        FileKind::Inotify(i) => (
            if i.borrow().ready() {
                POLLIN | POLLRDNORM
            } else {
                0
            },
            None,
        ),
        FileKind::SignalFd(s) => (
            if s.borrow().ready() {
                POLLIN | POLLRDNORM
            } else {
                0
            },
            None,
        ),
        FileKind::Epoll(ep) if depth < MAX_NESTS => {
            let ready = ep.borrow().ready(depth + 1, 1);
            (if ready.is_empty() { 0 } else { POLLIN }, None)
        }
        FileKind::Epoll(_) => (0, None),
        // 6.9 起进程被回收之后还有 POLLHUP；这里与 6.1 相同
        FileKind::Pidfd(p) => (if p.exited() { POLLIN | POLLRDNORM } else { 0 }, None),
        FileKind::MessageQueue(q) => (q.borrow().readiness(), None),
        FileKind::SeccompNotify(l) => (l.readiness(), None),
        FileKind::Path(_) => (POLLNVAL, None),
        _ => (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM, None),
    }
}

/// AIO 的 `IOCB_CMD_POLL`：现在就绪的、关心的事件（加上总会报告的 POLLERR、POLLHUP）。
/// `all` 时有关心的事件就绪就报告全部就绪的位：Linux 挂起之后被唤醒完成的请求就是这样。
pub(super) fn poll_once(f: &OpenFile, events: i16, all: bool) -> i16 {
    let wanted = poll_now(f, events, 0);
    if all && wanted != 0 {
        poll_now(f, !0, 0)
    } else {
        wanted
    }
}

/// 打开文件上读、写两个方向各来过几次事件：进程内的对象每次来数据（管道、socket 的
/// 写入，eventfd 的写，timerfd 到期，新的连接……）都记一次，与 Linux 每次唤醒等待队列
/// 对应；宿主 fd 上的由 [`HostEdges`] 记录，没有建立它时是 0。`EPOLLET` 据此在已经就绪
/// 时又来了数据的情况下再触发一次。
fn arrivals(f: &OpenFile) -> [u64; 2] {
    arrivals_with(f, HostEdges::counts, 0)
}

/// 与 [`arrivals`] 相同，但宿主 fd 刚刚经过 [`poll_now`]，宿主 epoll 那时不可读就不必
/// 再问宿主。
fn arrivals_polled(f: &OpenFile) -> [u64; 2] {
    arrivals_with(f, HostEdges::counts_polled, 0)
}

/// `depth` 是嵌套的 epoll 的层数（[`epoll_arrivals`]）。
fn arrivals_with(f: &OpenFile, host: fn(&HostEdges) -> [u64; 2], depth: u32) -> [u64; 2] {
    let local = match &f.kind {
        FileKind::Epoll(ep) => [epoll_arrivals(&ep.borrow(), depth), 0],
        FileKind::Pipe(end) if end.read => [end.pipe.borrow().writes, 0],
        FileKind::Pty(end) => [end.tty.borrow().arrivals[end.master as usize], 0],
        FileKind::Socket(s) => super::net::arrivals(&s.borrow()),
        FileKind::EventFd(e) => {
            let e = e.borrow();
            [e.writes, e.reads]
        }
        FileKind::TimerFd(t) => {
            let mut t = t.borrow_mut();
            t.update(Instant::now());
            [t.fired, 0]
        }
        FileKind::Inotify(i) => [i.borrow().queued(), 0],
        FileKind::SignalFd(s) => [s.borrow().arrivals(), 0],
        FileKind::MessageQueue(q) => q.borrow().arrivals(),
        FileKind::SeccompNotify(l) => l.arrivals(),
        _ => [0, 0],
    };
    let host = f.host_edges.as_ref().map_or([0, 0], host);
    [local[0] + host[0], local[1] + host[1]]
}

/// epoll 实例里来过的事件：Linux 的 `ep_poll_callback` 每次被唤醒（关注的文件在关注的
/// 方向上来了事件）都唤醒等这个 epoll 的 `poll_wait`，外面套着它的 epoll 的边沿触发的
/// 关注因此再触发一次。这里是各个关注从加入起在关注的方向上来过事件的次数之和，加上
/// 去掉了的关注原来的份额和加入、修改时已经就绪的次数（[`Epoll::base`]），只增不减。
fn epoll_arrivals(ep: &Epoll, depth: u32) -> u64 {
    let mut total = ep.base.get();
    for int in &ep.interests {
        // 停用的（EPOLLONESHOT 报告过）不唤醒，份额停在原来的值
        if int.armed
            && depth < MAX_NESTS
            && let Some(file) = int.file.upgrade()
        {
            let now = interest_arrivals(int.events, &file.borrow(), depth + 1);
            int.share.set(now.saturating_sub(int.origin));
        }
        total += int.share.get();
    }
    total
}

/// 关注 `events` 的方向上，文件 `f` 来过事件的次数（读写两个方向分开算，与 Linux 按唤醒的
/// 事件过滤一样）。
fn interest_arrivals(events: u32, f: &OpenFile, depth: u32) -> u64 {
    directional(events, arrivals_with(f, HostEdges::counts, depth))
}

/// 两个方向的事件次数里，`events` 关注的方向上的。
fn directional(events: u32, a: [u64; 2]) -> u64 {
    let read = events & !WRITE_BITS & 0xffff != 0;
    let write = events & WRITE_BITS != 0;
    (if read { a[0] } else { 0 }) + (if write { a[1] } else { 0 })
}

/// 打开文件现在的"数据代数"：读、写方向用完数据的次数和来过事件的次数。代数不变就
/// 还是同一个边沿（`EPOLLEXCLUSIVE`）。
fn edge(f: &OpenFile) -> [u64; 4] {
    let a = arrivals(f);
    [f.drained[0], f.drained[1], a[0], a[1]]
}

/// 打开文件背后的宿主 fd（宿主 stdio、TCP/UDP socket）。
fn host_fd(f: &OpenFile) -> Option<i32> {
    match &f.kind {
        FileKind::Host(fd) => Some(*fd),
        FileKind::Socket(s) => match &*s.borrow() {
            crate::net::Socket::Inet(i) => Some(i.fd),
            _ => None,
        },
        _ => None,
    }
}

/// `EPOLLEXCLUSIVE` 的关注 `int`（属于地址为 `owner` 的 epoll）现在的边沿已经被别的
/// epoll 报告过了：Linux 每次唤醒只叫醒一个独占的等待者，没轮到的不报告。边沿触发的
/// 关注自己报告过的也不再报告（与没有 `EPOLLEXCLUSIVE` 时一样）。
fn claimed(int: &Interest, f: &OpenFile, owner: usize) -> bool {
    int.events & EPOLLEXCLUSIVE != 0
        && f.exclusive_claim.get().is_some_and(|(edge_then, by)| {
            edge_then == edge(f) && (by != owner || int.events & EPOLLET != 0)
        })
}

/// 立即检查一个文件就绪的事件（宿主 fd 用一次不等待的 poll），只保留 `wanted` 和
/// 总会报告的 POLLERR、POLLHUP、POLLNVAL。
fn poll_now(f: &OpenFile, wanted: i16, depth: u32) -> i16 {
    let (mut ev, host) = readiness(f, depth);
    if let Some(fd) = host {
        // 记录新事件的宿主 epoll 一起问：不可读时 arrivals_polled 不必再问
        let edges = f.host_edges.as_ref();
        let mut p = [
            (fd, wanted, 0),
            (edges.map_or(-1, HostEdges::fd), POLLIN, 0),
        ];
        let n = if edges.is_some() { 2 } else { 1 };
        if appine_hal::poll(&mut p[..n], Some(Duration::ZERO)).is_ok() {
            ev |= p[0].2;
            if let Some(e) = edges {
                e.polled(p[1].2);
            }
        }
    }
    ev & (wanted | POLLERR | POLLHUP | POLLNVAL)
}

/// 等打开文件 `f` 的就绪状态变化时要登记的唤醒对象（[`wake::of`]）。改变进程内对象
/// 就绪状态的修改要么请求唤醒这里列出的对象之一，要么请求一个通道（挂起的信号请求
/// [`wake::SIGNAL`]，pidfd 的子进程请求 [`wake::CHILD`]，关注了它们才听）；宿主 fd 上的
/// 数据由宿主 poll 等，宿主 socket 列出它自己（关闭、DNS 存根的回答）。signalfd 只列出
/// 它自己（改掩码），epoll 只列出它自己（epoll_ctl），里面的关注由 [`Waits`] 逐个加上。
fn wake_objects(f: &OpenFile, out: &mut Vec<wake::Object>) {
    match &f.kind {
        FileKind::Pipe(end) => out.push(crate::pipe::object(&end.pipe)),
        FileKind::Pty(end) => out.push(wake::of(&end.tty)),
        FileKind::EventFd(e) => out.push(wake::of(e)),
        FileKind::TimerFd(t) => out.push(wake::of(t)),
        FileKind::Inotify(i) => out.push(wake::of(i)),
        FileKind::SignalFd(s) => out.push(wake::of(s)),
        FileKind::Epoll(ep) => out.push(wake::of(ep)),
        FileKind::Socket(s) => super::net::wake_objects(s, out),
        FileKind::MessageQueue(q) => out.push(wake::of(q)),
        FileKind::SeccompNotify(l) => out.push(l.object()),
        _ => {}
    }
}

/// 等待时要一起等的东西：宿主 fd，以及最早的 timerfd 到期时刻（含嵌套 epoll 里的）。
#[derive(Default)]
struct Waits {
    host: Vec<(i32, i16, i16)>,
    /// 进程内对象的唤醒对象（[`wake_objects`]）。
    objects: Vec<wake::Object>,
    /// 有 pidfd：子进程状态的变化（[`wake::CHILD`]）也要叫醒它。别的文件都不关心这个
    /// 通道，原来每个进程结束都把所有 poll、epoll 的等待者叫醒一遍。
    pidfd: bool,
    /// 有 signalfd：被屏蔽的信号挂起（[`wake::SIGNAL`]）也要叫醒它。
    signalfd: bool,
    timer: Option<Instant>,
    /// 有边沿触发的关注少等了报告过的位。
    edge: bool,
    /// 有要靠内核叫醒的进程内对象（管道、eventfd、AF_UNIX……）：要登记在文件类的唤醒里。
    files: bool,
    /// `EPOLLEXCLUSIVE` 关注的文件：不自己等，登记在它们的独占门上（[`ExGate`]）。
    gated: Vec<FileRef>,
}

impl Waits {
    fn add(&mut self, f: &OpenFile, wanted: i16, depth: u32) {
        let (_, host) = readiness(f, depth);
        self.files |= host.is_none();
        self.pidfd |= matches!(f.kind, FileKind::Pidfd(_));
        self.signalfd |= matches!(f.kind, FileKind::SignalFd(_));
        wake_objects(f, &mut self.objects);
        match (&f.kind, host) {
            (_, Some(fd)) => self.host.push((fd, wanted, 0)),
            (FileKind::TimerFd(t), None) => {
                if let Some(d) = t.borrow().deadline {
                    self.timer = Some(self.timer.map_or(d, |e| e.min(d)));
                }
            }
            // 只等新事件（外层边沿触发的关注报告过它可读）：等它里面的宿主 socket 新到的
            // 数据和定时器，进程内的对象来了事件时内核叫醒边沿触发的等待者
            (FileKind::Epoll(ep), None) if depth < MAX_NESTS && wanted & POLLIN == 0 => {
                self.edge = true;
                self.add_epoll_edges(&ep.borrow(), depth + 1);
            }
            (FileKind::Epoll(ep), None) if depth < MAX_NESTS => {
                self.add_epoll(&ep.borrow(), depth + 1);
            }
            _ => {}
        }
    }

    /// 嵌套的 epoll 里只等新事件：各个关注的宿主 fd 新到的数据（[`HostEdges`]，外层以边沿
    /// 触发关注它时建立，[`mark_edge_watched`]）、timerfd 的到期，再往里嵌套的照此。
    fn add_epoll_edges(&mut self, ep: &Epoll, depth: u32) {
        self.files = true;
        for int in ep.interests.iter().filter(|i| i.armed) {
            let Some(file) = int.file.upgrade() else {
                continue;
            };
            let f = file.borrow();
            if let Some(e) = &f.host_edges {
                self.host.push((e.fd(), POLLIN, 0));
            }
            wake_objects(&f, &mut self.objects);
            self.pidfd |= matches!(f.kind, FileKind::Pidfd(_));
            self.signalfd |= matches!(f.kind, FileKind::SignalFd(_));
            match &f.kind {
                FileKind::TimerFd(t) => {
                    if let Some(d) = t.borrow().deadline {
                        self.timer = Some(self.timer.map_or(d, |e| e.min(d)));
                    }
                }
                FileKind::Epoll(inner) if depth < MAX_NESTS => {
                    self.add_epoll_edges(&inner.borrow(), depth + 1);
                }
                _ => {}
            }
        }
    }

    /// 一个 epoll 活着、没有停用的关注。边沿触发的关注不等报告过、仍然就绪的位，独占
    /// 的关注不等轮到了别的 epoll 的边沿，改为等宿主 fd 上的新事件（见模块文档）；
    /// 报告过 POLLHUP、POLLERR 的不等宿主 fd 本身，因为宿主 poll 总会报告这两位。
    fn add_epoll(&mut self, ep: &Epoll, depth: u32) {
        for int in ep.interests.iter().filter(|i| i.armed) {
            let Some(file) = int.file.upgrade() else {
                continue;
            };
            // 独占的关注：不自己等它（宿主 poll 会把所有等它的线程都叫醒），由独占门在新的
            // 边沿上只叫醒一个等待者。宿主 fd 上新到的数据要从 HostEdges 看，建不了的仍然
            // 自己等
            let gate = {
                let f = file.borrow();
                int.events & EPOLLEXCLUSIVE != 0
                    && (host_fd(&f).is_none() || f.host_edges.is_some())
            };
            if gate {
                self.gated.push(file);
                continue;
            }
            let mut wanted = int.events as i16;
            let seen = int.seen.get() as i16;
            let f = file.borrow();
            // 宿主 fd 上新到的数据只能从 HostEdges 的宿主 epoll 看到：等它
            let mut edges = false;
            let mut hung_up = false;
            if int.events & EPOLLET != 0 && seen != 0 {
                self.edge = true;
                edges = true;
                hung_up = seen & (POLLHUP | POLLERR) != 0;
                wanted &= !seen;
            }
            // 自己等的独占关注（见上）：现在的边沿轮到了别的 epoll 时，现在就绪的位不等
            if claimed(int, &f, ep as *const Epoll as usize) {
                self.edge = true;
                wanted &= !poll_now(&f, wanted, depth);
            }
            if edges && let Some(e) = &f.host_edges {
                self.host.push((e.fd(), POLLIN, 0));
            }
            if !hung_up {
                self.add(&f, wanted, depth);
            } else {
                // 不等宿主 fd 本身，但进程内的对象之后还可能来事件（PTY 重新打开了从设备）
                self.files = true;
                wake_objects(&f, &mut self.objects);
            }
        }
    }

    /// 等到 `deadline`、某个宿主 fd 就绪、timerfd 到期或者被唤醒。
    fn wait(mut self, cx: &mut Cx<'_>, deadline: Option<Instant>) -> Result<(), Errno> {
        let until = match (deadline, self.timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let event = cx.task.event.clone();
        if self.edge {
            cx.k().edge_waiters.push(event.clone());
        }
        if !self.gated.is_empty() {
            gate_enter(cx, &self.gated, &event);
        }
        // 只等独占的关注时不登记在文件类的唤醒里：独占门、信号、epoll_ctl 都直接叫醒它，
        // 别的文件上的修改不必叫醒它
        let only_gated = !self.files && self.host.is_empty() && !self.gated.is_empty();
        let interest = if only_gated {
            0
        } else {
            wake::FILE
                | if self.pidfd { wake::CHILD } else { 0 }
                | if self.signalfd { wake::SIGNAL } else { 0 }
        };
        let r = cx.wait_objects(until, &mut self.host, interest, &self.objects);
        if self.edge {
            cx.k().edge_waiters.retain(|e| !Arc::ptr_eq(e, &event));
        }
        if !self.gated.is_empty() {
            gate_leave(cx, &self.gated, &event);
        }
        r
    }
}

/// `EPOLLEXCLUSIVE` 关注的一个文件上等着的线程（Linux 的等待队列里独占的那些项）。
/// 等待者不自己等这个文件（宿主 poll、文件类的广播都会把所有等它的线程叫醒），而是登记在
/// 这里，由定时器线程替它们看（[`check_exclusive`]）：数据代数变了、文件就绪、这个边沿还
/// 没被哪个 epoll 报告过时，只叫醒排在最前面的一个，与 Linux 每次唤醒只叫醒一个独占的
/// 等待者一样。
pub(crate) struct ExGate {
    file: Weak<RefCell<OpenFile>>,
    /// 等着的线程，先来的在前。
    sleepers: Vec<Arc<appine_hal::Event>>,
    /// 上次检查时文件的数据代数（[`edge`]）。
    last: [u64; 4],
}

impl ExGate {
    /// 看一眼文件 `f`：数据代数变了、文件就绪、这个边沿还没被哪个 epoll 报告过时，取出
    /// 排在最前面的等待者（要叫醒它）。
    fn next_waiter(&mut self, f: &OpenFile) -> Option<Arc<appine_hal::Event>> {
        let now = edge(f);
        if now == self.last || self.sleepers.is_empty() {
            return None;
        }
        self.last = now;
        let ready = poll_now(f, POLLIN | POLLOUT | POLLPRI | POLLRDNORM | POLLWRNORM, 0) != 0;
        let claimed = f.exclusive_claim.get().is_some_and(|(e, _)| e == now);
        (ready && !claimed).then(|| self.sleepers.remove(0))
    }

    /// 叫醒所有等着的线程（检查出错时，宁可多叫醒）。
    pub(crate) fn wake_all(&self) {
        for e in &self.sleepers {
            crate::kernel::signal_later(e);
        }
    }
}

/// 等待之前：在 `files` 各自的独占门上登记 `event`。门是新建的时叫醒定时器线程，让它
/// 开始看这个文件。
fn gate_enter(cx: &mut Cx<'_>, files: &[FileRef], event: &Arc<appine_hal::Event>) {
    let mut created = false;
    for file in files {
        let k = cx.k();
        match k
            .ex_gates
            .iter_mut()
            .find(|g| g.file.as_ptr() == Rc::as_ptr(file))
        {
            Some(g) => g.sleepers.push(event.clone()),
            None => {
                k.ex_gates.push(ExGate {
                    file: Rc::downgrade(file),
                    sleepers: vec![event.clone()],
                    last: edge(&file.borrow()),
                });
                created = true;
            }
        }
    }
    if created {
        crate::kernel::signal_later(&cx.shared.timers);
    }
}

/// 等待之后：从 `files` 的独占门上去掉 `event`（被叫醒的已经去掉了）。
fn gate_leave(cx: &mut Cx<'_>, files: &[FileRef], event: &Arc<appine_hal::Event>) {
    for g in cx.k().ex_gates.iter_mut() {
        if files.iter().any(|f| g.file.as_ptr() == Rc::as_ptr(f)) {
            g.sleepers.retain(|e| !Arc::ptr_eq(e, event));
        }
    }
}

/// 定时器线程替 `EPOLLEXCLUSIVE` 的等待者看文件（[`ExGate`]）：任何修改都会叫醒定时器
/// 线程（有独占门时它登记在所有通道上），宿主 socket 上新到的数据从它的 [`HostEdges`]
/// 看，返回这些宿主 fd 让定时器线程一起等。没有等待者、文件已经关闭的门去掉。
pub(crate) fn check_exclusive(k: &mut crate::kernel::Kernel) -> Vec<(i32, i16, i16)> {
    let mut host = Vec::new();
    k.ex_gates.retain_mut(|g| {
        let Some(file) = g.file.upgrade().filter(|_| !g.sleepers.is_empty()) else {
            return false;
        };
        let f = file.borrow();
        if let Some(first) = g.next_waiter(&f) {
            crate::kernel::signal_later(&first);
        }
        if let Some(e) = &f.host_edges {
            host.push((e.fd(), POLLIN, 0));
        }
        true
    });
    host
}

/// 程序在 `file` 的一个方向（0 读，1 写）上用完了数据：读写返回 EAGAIN，或者没有读写
/// 满要求的长度（tokio 这类运行时据此认为数据已经读完、不再读到 EAGAIN）。边沿触发的
/// 关注之后重新报告这个方向上的事件，等待中少等了这些事件的 epoll_wait 被叫醒重新计算。
pub(super) fn drained(cx: &mut Cx<'_>, file: &FileRef, dir: usize) {
    file.borrow_mut().drained[dir] += 1;
    for event in &cx.k().edge_waiters {
        crate::kernel::signal_later(event);
    }
    // 信号驱动的 I/O 也按用完数据重新开始计算，见 check_fasync：只叫醒定时器线程
    if file.borrow().flags & oflags::O_ASYNC != 0 {
        crate::kernel::signal_later(&cx.shared.timers);
    }
}

/// 一个已经就绪的进程内对象又来了数据，而写入的一方没有请求唤醒（管道不空时）：叫醒
/// 边沿触发、少等了这些事件的 epoll_wait 重新计算。
pub(super) fn arrived(cx: &mut Cx<'_>) {
    for event in &cx.k().edge_waiters {
        crate::kernel::signal_later(event);
    }
}

// ---- 信号驱动的 I/O ----

/// 一个设置了 `O_ASYNC` 的打开文件（Linux 的 fasync）。
pub(crate) struct Fasync {
    file: Weak<RefCell<OpenFile>>,
    /// 设置 `O_ASYNC` 时用的 fd：`F_SETSIG` 的信号把它放在 si_fd 里。
    fd: i32,
    /// 上次检查时就绪的事件。
    last: i16,
    /// 上次检查时打开文件的 [`OpenFile::drained`]、[`OpenFile::reads`] 和 [`arrivals`]。
    drained: [u64; 2],
    reads: u64,
    arrivals: [u64; 2],
}

/// 打开文件刚刚设置了 `O_ASYNC`：登记它。已经就绪的事件不发信号，与 Linux 一样。
/// 叫醒定时器线程：原来没有 `O_ASYNC` 的文件时它只听"其余"通道，要等它因为别的事
/// 醒来才开始检查这个文件。
pub(super) fn register_fasync(cx: &mut Cx<'_>, file: &FileRef, fd: i32) {
    let k = cx.k();
    k.fasync.retain(|e| e.file.strong_count() > 0);
    if k.fasync.iter().any(|e| e.file.as_ptr() == Rc::as_ptr(file)) {
        return;
    }
    let f = file.borrow();
    k.fasync.push(Fasync {
        file: Rc::downgrade(file),
        fd,
        last: poll_now(&f, POLLIN | POLLOUT, 0),
        drained: f.drained,
        reads: f.reads,
        arrivals: arrivals(&f),
    });
    crate::kernel::signal_later(&cx.shared.timers);
}

/// 检查设置了 `O_ASYNC` 的文件，新出现的就绪事件给属主发信号。返回等待时要一起 poll
/// 的宿主 fd 和事件：宿主 socket 上的数据不经过内核，没有人会请求唤醒。定时器线程每次
/// 醒来调用；任何可能让文件就绪的修改都会叫醒它。
pub(crate) fn check_fasync(k: &mut crate::kernel::Kernel) -> Vec<(i32, i16, i16)> {
    const POLL_IN: i32 = 1;
    const POLL_OUT: i32 = 2;
    const POLL_ERR: i32 = 4;
    const POLL_HUP: i32 = 6;
    if k.fasync.is_empty() {
        return Vec::new();
    }
    let mut host = Vec::new();
    let mut signals = Vec::new();
    k.fasync.retain_mut(|e| {
        let Some(file) = e.file.upgrade() else {
            return false;
        };
        let f = file.borrow();
        if f.flags & oflags::O_ASYNC == 0 {
            return false;
        }
        // 程序在一个方向上用完了数据，或者这个方向又来了事件：这个方向重新算作没有
        // 就绪，仍然就绪的话再通知，Linux 每来一次数据都通知。读过数据之后仍然可读也再
        // 通知一次：宿主 socket 上看不到新到的数据（没有 HostEdges 时），两次检查之间
        // 可能读空了又来了新数据
        let came = arrivals(&f);
        if f.drained[0] != e.drained[0] || f.reads != e.reads || came[0] != e.arrivals[0] {
            e.last &= !(POLLIN | POLLRDNORM);
        }
        if f.drained[1] != e.drained[1] || came[1] != e.arrivals[1] {
            e.last &= !(POLLOUT | POLLWRNORM);
        }
        e.drained = f.drained;
        e.reads = f.reads;
        e.arrivals = came;
        let (_, host_fd) = readiness(&f, 0);
        let ev = poll_now(&f, POLLIN | POLLOUT, 0);
        let new = ev & !e.last;
        e.last = ev;
        if let Some(fd) = host_fd {
            let wanted = (POLLIN | POLLOUT) & !ev;
            if wanted != 0 {
                host.push((fd, wanted, 0));
            }
        }
        for (bit, code, band) in [
            // Linux 的 band_table
            (POLLIN, POLL_IN, POLLIN | POLLRDNORM),
            (POLLOUT, POLL_OUT, POLLOUT | POLLWRNORM | 0x200),
            (POLLERR, POLL_ERR, POLLERR),
            (POLLHUP, POLL_HUP, POLLHUP | POLLERR),
        ] {
            if new & bit != 0 {
                signals.push((f.owner, f.sig, code, band, e.fd));
            }
        }
        true
    });
    for ((ty, pid), sig, code, band, fd) in signals {
        let Some(signo) = crate::process::Signal::new(if sig == 0 { 29 } else { sig }) else {
            continue;
        };
        // F_SETSIG 设置了信号时带上事件和 fd，默认的 SIGIO 不带
        let info = if sig == 0 {
            signal::SigInfo::kernel()
        } else {
            signal::SigInfo {
                code,
                fields: signal::InfoFields::Poll {
                    band: band as i64,
                    fd,
                },
                esr: 0,
                sender: 0,
            }
        };
        send_to_owner(k, ty, pid, signo, info);
    }
    host
}

/// 给 `F_SETOWN_EX` 记下的属主发信号；没有属主（pid 为 0）时不发。
pub(crate) fn send_to_owner(
    k: &crate::kernel::Kernel,
    ty: i32,
    pid: i32,
    sig: crate::process::Signal,
    info: signal::SigInfo,
) {
    const F_OWNER_TID: i32 = 0;
    const F_OWNER_PGRP: i32 = 2;
    if pid <= 0 {
        return;
    }
    match ty {
        F_OWNER_TID => {
            if let Some(t) = k.tasks.get(&pid) {
                signal::send_to_task(k, t, sig, info);
            }
        }
        F_OWNER_PGRP => signal::send_to_pgrp(k, pid, sig, info),
        _ => signal::send_to_group(k, pid, sig, info),
    }
}

/// 等到 `items` 里至少一个文件就绪、超时或者被信号打断，返回每一项就绪的事件。
/// `None` 表示无效的 fd，报告 POLLNVAL。
fn wait_ready(
    cx: &mut Cx<'_>,
    items: &[(Option<FileRef>, i16)],
    deadline: Option<Instant>,
) -> Result<Vec<i16>, Errno> {
    loop {
        let ready: Vec<i16> = items
            .iter()
            .map(|(f, wanted)| match f {
                Some(f) => poll_now(&f.borrow(), *wanted, 0),
                None => POLLNVAL,
            })
            .collect();
        if ready.iter().any(|&e| e != 0) || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(ready);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTNOHAND);
        }
        let mut waits = Waits::default();
        for (f, wanted) in items {
            if let Some(f) = f {
                waits.add(&f.borrow(), *wanted, 0);
            }
        }
        waits.wait(cx, deadline)?;
    }
}

/// 换上 guest 给的临时屏蔽字；被信号打断时由信号投递恢复，否则由 [`restore_mask`] 恢复。
pub(super) fn set_temp_mask(cx: &mut Cx<'_>, sigmask: u64, size: usize) -> Result<(), Errno> {
    if sigmask == 0 {
        return Ok(());
    }
    if size != 8 {
        return Err(Errno::EINVAL);
    }
    let mask = u64::from_le_bytes(get(cx, sigmask)?);
    cx.task.saved_sigmask.set(Some(cx.task.sigmask.get()));
    cx.task.sigmask.set(mask & !signal::UNBLOCKABLE);
    Ok(())
}

pub(super) fn restore_mask<T>(cx: &mut Cx<'_>, r: &Result<T, Errno>) {
    let interrupted = matches!(r, Err(e) if *e == Errno::ERESTARTNOHAND);
    if !interrupted && let Some(saved) = cx.task.saved_sigmask.take() {
        cx.task.sigmask.set(saved);
    }
}

pub(super) fn ppoll(
    cx: &mut Cx<'_>,
    fds: u64,
    nfds: usize,
    timeout: u64,
    sigmask: u64,
    sigsetsize: usize,
) -> SysResult {
    if nfds as u64 > cx.t.group.borrow().rlimits[RLIMIT_NOFILE as usize].0 {
        return Err(Errno::EINVAL);
    }
    let deadline = match timeout {
        0 => None,
        addr => Some(Instant::now() + read_timespec(cx, addr)?),
    };
    let mut raw = vec![0u8; nfds * 8];
    cx.mm().read(UserSlice::new(fds, raw.len()), &mut raw)?;
    // struct pollfd：fd、events、revents；负的 fd 被忽略
    let items: Vec<(Option<FileRef>, i16, bool)> = raw
        .chunks_exact(8)
        .map(|e| {
            let fd = i32::from_le_bytes(e[..4].try_into().unwrap());
            let events = i16::from_le_bytes([e[4], e[5]]);
            (cx.fds().get(fd).ok(), events, fd < 0)
        })
        .collect();
    let wanted: Vec<(Option<FileRef>, i16)> = items
        .iter()
        .filter(|(_, _, skip)| !skip)
        .map(|(f, e, _)| (f.clone(), *e))
        .collect();
    set_temp_mask(cx, sigmask, sigsetsize)?;
    let r = wait_ready(cx, &wanted, deadline);
    restore_mask(cx, &r);
    let mut ready = r?.into_iter();
    let mut n = 0;
    for ((_, _, skip), out) in items.iter().zip(raw.chunks_exact_mut(8)) {
        let ev = if *skip { 0 } else { ready.next().unwrap() };
        n += (ev != 0) as u64;
        out[6..8].copy_from_slice(&ev.to_le_bytes());
    }
    cx.mm().write(UserSlice::new(fds, raw.len()), &raw)?;
    Ok(n)
}

pub(super) fn pselect6(
    cx: &mut Cx<'_>,
    nfds: i32,
    sets: [u64; 3],
    timeout: u64,
    sigdata: u64,
) -> SysResult {
    let limit = cx.t.group.borrow().rlimits[RLIMIT_NOFILE as usize].0 as i32;
    if nfds < 0 || nfds > limit {
        return Err(Errno::EINVAL);
    }
    let words = (nfds as usize).div_ceil(64);
    // 读、写、异常三个 fd_set
    let mut bits = [vec![0u64; words], vec![0u64; words], vec![0u64; words]];
    for (set, &addr) in bits.iter_mut().zip(&sets) {
        if addr == 0 {
            continue;
        }
        let mut b = vec![0u8; words * 8];
        cx.mm().read(UserSlice::new(addr, b.len()), &mut b)?;
        for (w, c) in set.iter_mut().zip(b.chunks_exact(8)) {
            *w = u64::from_le_bytes(c.try_into().unwrap());
        }
    }
    let is_set = |set: &[u64], fd: usize| set[fd / 64] >> (fd % 64) & 1 != 0;
    let mut items = Vec::new();
    let mut fds = Vec::new();
    for fd in 0..nfds as usize {
        let mut ev = 0;
        if is_set(&bits[0], fd) {
            ev |= POLLIN | POLLRDNORM | POLLHUP | POLLERR;
        }
        if is_set(&bits[1], fd) {
            ev |= POLLOUT | POLLWRNORM | POLLERR;
        }
        if is_set(&bits[2], fd) {
            ev |= POLLPRI;
        }
        if ev != 0 {
            items.push((Some(cx.fds().get(fd as i32)?), ev));
            fds.push(fd);
        }
    }
    let start = Instant::now();
    let deadline = match timeout {
        0 => None,
        addr => Some(start + read_timespec(cx, addr)?),
    };
    // 第六个参数指向 { const sigset_t *ss; size_t ss_len; }
    if sigdata != 0 {
        let d: [u8; 16] = get(cx, sigdata)?;
        let ss = u64::from_le_bytes(d[..8].try_into().unwrap());
        let len = u64::from_le_bytes(d[8..].try_into().unwrap()) as usize;
        set_temp_mask(cx, ss, len)?;
    }
    let r = wait_ready(cx, &items, deadline);
    restore_mask(cx, &r);
    let ready = r?;
    let mut out = [vec![0u64; words], vec![0u64; words], vec![0u64; words]];
    let mut n = 0;
    for (&fd, &ev) in fds.iter().zip(&ready) {
        let checks = [
            (POLLIN | POLLRDNORM | POLLHUP | POLLERR, 0),
            (POLLOUT | POLLWRNORM | POLLERR, 1),
            (POLLPRI, 2),
        ];
        for (mask, i) in checks {
            if is_set(&bits[i], fd) && ev & mask != 0 {
                out[i][fd / 64] |= 1 << (fd % 64);
                n += 1;
            }
        }
    }
    for (set, &addr) in out.iter().zip(&sets) {
        if addr != 0 {
            let b: Vec<u8> = set.iter().flat_map(|w| w.to_le_bytes()).collect();
            cx.mm().write(UserSlice::new(addr, b.len()), &b)?;
        }
    }
    // 与 Linux 一样把剩余时间写回 timeout
    if let (Some(d), true) = (deadline, timeout != 0) {
        let left = d.saturating_duration_since(Instant::now());
        super::write_pair(
            cx,
            timeout,
            left.as_secs() as i64,
            left.subsec_nanos() as i64,
        )?;
    }
    Ok(n)
}

// ---- epoll ----

const EPOLL_CTL_ADD: u32 = 1;
const EPOLL_CTL_DEL: u32 = 2;
const EPOLL_CTL_MOD: u32 = 3;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
/// 多个 epoll 等同一个 fd 时只唤醒其中一个（见模块文档）。
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLLWAKEUP: u32 = 1 << 29;
/// 能和 `EPOLLEXCLUSIVE` 一起用的位（Linux 的 `EPOLLEXCLUSIVE_OK_BITS`）。
const EXCLUSIVE_OK: u32 =
    (POLLIN | POLLOUT | POLLERR | POLLHUP) as u32 | EPOLLWAKEUP | EPOLLET | EPOLLEXCLUSIVE;

/// epoll 关注的一个文件。
struct Interest {
    fd: i32,
    /// 打开文件关闭之后，关注自动失效（与 Linux 一样不阻止它关闭）。
    file: Weak<RefCell<OpenFile>>,
    events: u32,
    data: u64,
    /// `EPOLLONESHOT` 报告过一次之后停用，直到 `EPOLL_CTL_MOD`。
    armed: bool,
    /// `EPOLLET`：上次报告时就绪的位里现在仍然就绪的那些。出现不在其中的就绪位才触发。
    seen: Cell<u32>,
    /// `EPOLLET`：上次检查时打开文件的 [`OpenFile::drained`] 和 [`arrivals`]。
    drained: Cell<[u64; 2]>,
    arrivals: Cell<[u64; 2]>,
    /// 加入时文件在关注的方向上来过事件的次数，和此后来过的次数（这个 epoll 被嵌套时的
    /// 数据代数，[`epoll_arrivals`]）。
    origin: u64,
    share: Cell<u64>,
}

impl Interest {
    fn new(fd: i32, file: &FileRef, events: u32, data: u64) -> Interest {
        let came = arrivals(&file.borrow());
        Interest {
            fd,
            file: Rc::downgrade(file),
            events,
            data,
            armed: true,
            seen: Cell::new(0),
            drained: Cell::new(file.borrow().drained),
            arrivals: Cell::new(came),
            origin: directional(events, came),
            share: Cell::new(0),
        }
    }
}

/// 一个 epoll 实例。
#[derive(Default)]
pub(crate) struct Epoll {
    interests: Vec<Interest>,
    /// 下一次从哪个关注开始检查：与 Linux 把报告过的放到就绪链表末尾一样，就绪的比
    /// maxevents 多时轮流报告。
    cursor: usize,
    /// 去掉了的关注原来的份额，加上加入、修改关注时它已经就绪的次数（[`epoll_arrivals`]）。
    base: Cell<u64>,
    /// 有别的 epoll 以边沿触发关注这个 epoll：它关注的宿主 fd 也要记录新到的数据
    /// （[`mark_edge_watched`]）。
    edge_watched: Cell<bool>,
    /// 正在 epoll_wait 里等着的线程：epoll_ctl 改了关注时直接叫醒它们（只等独占关注的
    /// 线程不在文件类的唤醒里）。
    waiters: Vec<Arc<appine_hal::Event>>,
}

impl Epoll {
    /// 去掉第 `i` 个关注，份额记进 `base`，嵌套时的数据代数不会变少。
    fn remove(&mut self, i: usize) -> Interest {
        let int = self.interests.remove(i);
        self.base.set(self.base.get() + int.share.get());
        int
    }

    /// 去掉文件已经关闭了的关注。
    fn prune(&mut self) {
        let mut i = 0;
        while i < self.interests.len() {
            if self.interests[i].file.strong_count() == 0 {
                self.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// 关注里 fd 为 `fd` 的第 `nth` 个（从 0 数）的打开文件（kcmp 的 `KCMP_EPOLL_TFD`）。
    pub(crate) fn target(&self, fd: i32, nth: usize) -> Option<FileRef> {
        self.interests
            .iter()
            .filter(|i| i.fd == fd)
            .filter_map(|i| i.file.upgrade())
            .nth(nth)
    }

    /// `/proc/<pid>/fdinfo` 里每个关注一行。与 Linux 一样，关注的事件总是带着
    /// EPOLLERR 和 EPOLLHUP。
    pub(crate) fn fdinfo(&self) -> String {
        let mut s = String::new();
        for int in &self.interests {
            let Some(file) = int.file.upgrade() else {
                continue;
            };
            let f = file.borrow();
            let (dev, ino) = super::fs::dev_ino(&f);
            let events = int.events | (POLLERR | POLLHUP) as u32;
            s += &format!(
                "tfd: {:8} events: {events:8x} data: {:16x}  pos:{} ino:{ino:x} sdev:{dev:x}\n",
                int.fd, int.data, f.pos
            );
        }
        s
    }

    /// 立即检查哪些关注就绪，最多 `max` 个：(下标, 事件)。只检查，不消耗边沿触发的
    /// 事件（报告之后由 [`Epoll::reported`] 记下）。
    fn ready(&self, depth: u32, max: usize) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        let n = self.interests.len();
        let start = if n == 0 { 0 } else { self.cursor % n };
        for i in (start..n).chain(0..start) {
            let int = &self.interests[i];
            if out.len() == max {
                break;
            }
            let Some(file) = int.file.upgrade().filter(|_| int.armed) else {
                continue;
            };
            let f = file.borrow();
            // 独占的关注：这个边沿已经轮到了别的 epoll，当作没有就绪
            if claimed(int, &f, self as *const Epoll as usize) {
                continue;
            }
            let ev = poll_now(&f, int.events as i16, depth) as u16 as u32;
            if int.events & EPOLLET != 0 {
                // 程序在某个方向上用完了数据，或者这个方向又来了事件：这个方向上次报告的
                // 事件作废，仍然就绪的话再报告一次
                let drained = f.drained;
                let last = int.drained.replace(drained);
                let came = arrivals_polled(&f);
                let before = int.arrivals.replace(came);
                let mut seen = int.seen.get();
                if drained[0] != last[0] || came[0] != before[0] {
                    seen &= WRITE_BITS;
                }
                if drained[1] != last[1] || came[1] != before[1] {
                    seen &= !WRITE_BITS;
                }
                // 不再就绪的位忘掉，下次重新就绪时才能再触发
                seen &= ev;
                int.seen.set(seen);
                if ev & !seen == 0 {
                    continue;
                }
            }
            if ev != 0 {
                out.push((i, ev));
            }
        }
        out
    }

    /// 关注 `i` 的事件 `ev` 已经报告给 guest。
    fn reported(&mut self, i: usize, ev: u32) {
        let owner = self as *const Epoll as usize;
        let int = &mut self.interests[i];
        int.seen.set(ev);
        if int.events & EPOLLONESHOT != 0 {
            int.armed = false;
        }
        // 独占的关注：这个边沿归这个 epoll，别的 epoll 等下一个边沿
        if int.events & EPOLLEXCLUSIVE != 0
            && let Some(file) = int.file.upgrade()
        {
            let f = file.borrow();
            f.exclusive_claim.set(Some((edge(&f), owner)));
        }
        self.cursor = i + 1;
    }
}

pub(super) fn epoll_create1(cx: &mut Cx<'_>, flags: u32) -> SysResult {
    if flags & !oflags::O_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    let file = OpenFile::new(
        FileKind::Epoll(Rc::new(RefCell::new(Epoll::default()))),
        oflags::O_RDWR,
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// `file` 里面嵌套了几层 epoll（不是 epoll 时是 0，只关注普通对象的 epoll 是 1）；
/// 其中能到达 `target`（会成环）或者嵌套太深时返回 None。
fn nesting(file: &FileRef, target: &Rc<RefCell<Epoll>>, level: u32) -> Option<u32> {
    let FileKind::Epoll(ep) = &file.borrow().kind else {
        return Some(0);
    };
    if Rc::ptr_eq(ep, target) || level > MAX_NESTS {
        return None;
    }
    let children: Vec<FileRef> = ep
        .borrow()
        .interests
        .iter()
        .filter_map(|i| i.file.upgrade())
        .collect();
    let mut depth = 0;
    for child in children {
        depth = depth.max(nesting(&child, target, level + 1)?);
    }
    Some(depth + 1)
}

pub(super) fn epoll_ctl(cx: &mut Cx<'_>, epfd: i32, op: u32, fd: i32, event: u64) -> SysResult {
    let epf = cx.fds().get(epfd)?;
    let FileKind::Epoll(ep) = &epf.borrow().kind else {
        return Err(Errno::EINVAL);
    };
    let ep = ep.clone();
    let key = wake::of(&ep);
    let file = cx.fds().get(fd)?;
    if file.borrow().flags & oflags::O_PATH != 0 {
        return Err(Errno::EBADF);
    }
    if Rc::ptr_eq(&epf, &file) {
        return Err(Errno::EINVAL);
    }
    // 普通文件、目录和 /dev/null 这类设备不支持 poll，Linux 不允许把它们加进 epoll
    if matches!(
        file.borrow().kind,
        FileKind::Regular { .. }
            | FileKind::Directory { .. }
            | FileKind::Proc { .. }
            | FileKind::Device {
                dev: Device::Null | Device::Zero | Device::Full,
                ..
            }
    ) {
        return Err(Errno::EPERM);
    }
    // 加入另一个 epoll：不能成环，嵌套不能超过 EP_MAX_NESTS 层
    if op == EPOLL_CTL_ADD && nesting(&file, &ep, 0).is_none_or(|d| d > MAX_NESTS) {
        return Err(Errno::ELOOP);
    }
    let (events, data) = if op == EPOLL_CTL_DEL {
        (0, 0)
    } else {
        let b: [u8; 16] = get(cx, event)?;
        (
            u32::from_le_bytes(b[..4].try_into().unwrap()),
            u64::from_le_bytes(b[8..].try_into().unwrap()),
        )
    };
    // 与 Linux 相同：EPOLLEXCLUSIVE 只能在加入时用，不能用于 epoll，也不能配别的标志
    if events & EPOLLEXCLUSIVE != 0
        && (op == EPOLL_CTL_MOD
            || op == EPOLL_CTL_ADD
                && (matches!(file.borrow().kind, FileKind::Epoll(_))
                    || events & !EXCLUSIVE_OK != 0))
    {
        return Err(Errno::EINVAL);
    }
    if op != EPOLL_CTL_DEL {
        if events & (EPOLLET | EPOLLEXCLUSIVE) != 0 || ep.borrow().edge_watched.get() {
            watch_host_edges(&file);
        }
        // 以边沿触发关注另一个 epoll，或者往被这样关注着的 epoll 里加 epoll：里面的宿主
        // fd 也要记录新到的数据
        if let FileKind::Epoll(inner) = &file.borrow().kind
            && (events & EPOLLET != 0 || ep.borrow().edge_watched.get())
        {
            mark_edge_watched(inner, 0);
        }
    }
    // 加入、修改时已经就绪：与 Linux 一样唤醒外面等它的 epoll（嵌套时的数据代数加一）。
    // 只有外面以边沿触发等它时才用得上，不必每次都问宿主
    let ready_now = op != EPOLL_CTL_DEL
        && ep.borrow().edge_watched.get()
        && poll_now(&file.borrow(), events as i16, 0) != 0;
    let mut ep = ep.borrow_mut();
    ep.prune();
    let pos = ep
        .interests
        .iter()
        .position(|i| i.fd == fd && i.file.as_ptr() == Rc::as_ptr(&file));
    // 别的线程可能正在 epoll_wait：叫醒它们按新的关注重新等。外面套着它的 epoll、poll 等
    // 的是它的唤醒对象（下面的 request_wake_object）
    for w in &ep.waiters {
        crate::kernel::signal_later(w);
    }
    match (op, pos) {
        (EPOLL_CTL_ADD, Some(_)) => Err(Errno::EEXIST),
        (EPOLL_CTL_ADD, None) => {
            ep.interests.push(Interest::new(fd, &file, events, data));
            ep.base.set(ep.base.get() + ready_now as u64);
            request_wake_object(key);
            Ok(0)
        }
        // 带 EPOLLEXCLUSIVE 加入的不能修改
        (EPOLL_CTL_MOD, Some(i)) if ep.interests[i].events & EPOLLEXCLUSIVE != 0 => {
            Err(Errno::EINVAL)
        }
        // 与 Linux 相同，修改之后重新判断：已经就绪的会再报告一次
        (EPOLL_CTL_MOD, Some(i)) => {
            let old =
                std::mem::replace(&mut ep.interests[i], Interest::new(fd, &file, events, data));
            ep.base
                .set(ep.base.get() + old.share.get() + ready_now as u64);
            request_wake_object(key);
            Ok(0)
        }
        (EPOLL_CTL_DEL, Some(i)) => {
            ep.remove(i);
            Ok(0)
        }
        (EPOLL_CTL_MOD | EPOLL_CTL_DEL, None) => Err(Errno::ENOENT),
        _ => Err(Errno::EINVAL),
    }
}

/// 有 epoll 以边沿触发关注 `ep`（`depth` 是往里嵌套的层数）：它和它里面嵌套的 epoll 关注
/// 的宿主 fd 都建立 [`HostEdges`]，之后加进来的也是（[`Epoll::edge_watched`]）。已有的关注
/// 的数据代数重新取一次起点，刚建立的记录不算新事件。
fn mark_edge_watched(ep: &Rc<RefCell<Epoll>>, depth: u32) {
    if depth >= MAX_NESTS {
        return;
    }
    let ep = ep.borrow();
    ep.edge_watched.set(true);
    for int in &ep.interests {
        let Some(file) = int.file.upgrade() else {
            continue;
        };
        if file.borrow().host_edges.is_none() {
            watch_host_edges(&file);
            int.arrivals.set(arrivals(&file.borrow()));
        }
        if let FileKind::Epoll(inner) = &file.borrow().kind {
            mark_edge_watched(inner, depth + 1);
        }
    }
}

/// 边沿触发、独占的关注要知道宿主 fd 上新到的数据：第一次时建立 [`HostEdges`]。宿主
/// 不让 epoll 的 fd（宿主上的普通文件）建不了，仍然只看程序有没有用完数据。
fn watch_host_edges(file: &FileRef) {
    let mut f = file.borrow_mut();
    if f.host_edges.is_some() {
        return;
    }
    let Some(fd) = host_fd(&f) else {
        return;
    };
    match HostEdges::new(fd) {
        Ok(e) => f.host_edges = Some(e),
        Err(e) => crate::debug::log(format_args!("epoll: host fd {fd}: edges: errno {e}")),
    }
}

pub(super) fn epoll_pwait(
    cx: &mut Cx<'_>,
    epfd: i32,
    events: u64,
    maxevents: i32,
    deadline: Option<Instant>,
    sigmask: u64,
    sigsetsize: usize,
) -> SysResult {
    if maxevents <= 0 || maxevents > i32::MAX / 16 {
        return Err(Errno::EINVAL);
    }
    // 打开文件只借这一句：`let … else` 绑定借用出来的引用时，借用一直活到函数结束，等的
    // 时候别的线程改这个打开文件（fcntl、FIONBIO）就会 panic。epf 本身留到最后，等的时候
    // fd 被关掉也保活
    let epf = cx.fds().get(epfd)?;
    let ep = match &epf.borrow().kind {
        FileKind::Epoll(ep) => ep.clone(),
        _ => return Err(Errno::EINVAL),
    };
    set_temp_mask(cx, sigmask, sigsetsize)?;
    let r = (|| loop {
        let ready = ep.borrow().ready(0, maxevents as usize);
        if !ready.is_empty() || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(ready);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTNOHAND);
        }
        let mut waits = Waits::default();
        waits.add_epoll(&ep.borrow(), 0);
        let event = cx.task.event.clone();
        ep.borrow_mut().waiters.push(event.clone());
        let r = waits.wait(cx, deadline);
        ep.borrow_mut().waiters.retain(|e| !Arc::ptr_eq(e, &event));
        r?;
    })();
    restore_mask(cx, &r);
    let ready = r?;
    let mut out = Vec::with_capacity(ready.len() * 16);
    {
        let mut ep = ep.borrow_mut();
        for &(i, ev) in &ready {
            out.extend_from_slice(&ev.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&ep.interests[i].data.to_le_bytes());
            ep.reported(i, ev);
        }
    }
    cx.mm().write(UserSlice::new(events, out.len()), &out)?;
    Ok(ready.len() as u64)
}

/// epoll_pwait 的超时：毫秒，负数表示一直等。
pub(super) fn deadline_ms(timeout: i32) -> Option<Instant> {
    (timeout >= 0).then(|| Instant::now() + Duration::from_millis(timeout as u64))
}

/// epoll_pwait2 的超时：`struct timespec`，NULL 表示一直等。
pub(super) fn deadline_ts(cx: &Cx<'_>, timeout: u64) -> Result<Option<Instant>, Errno> {
    if timeout == 0 {
        return Ok(None);
    }
    Ok(Some(Instant::now() + read_timespec(cx, timeout)?))
}

// ---- eventfd ----

const EFD_SEMAPHORE: u32 = 1;

pub(super) fn eventfd2(cx: &mut Cx<'_>, initval: u32, flags: u32) -> SysResult {
    if flags & !(EFD_SEMAPHORE | oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let efd = EventFd {
        count: initval as u64,
        semaphore: flags & EFD_SEMAPHORE != 0,
        writes: 0,
        reads: 0,
    };
    let file = OpenFile::new(
        FileKind::EventFd(Rc::new(RefCell::new(efd))),
        oflags::O_RDWR | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// eventfd 的读：取走计数（semaphore 模式下取 1），计数为 0 时阻塞。
pub(super) fn eventfd_read(
    cx: &mut Cx<'_>,
    efd: &Rc<RefCell<EventFd>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if buf.len() < 8 {
        return Err(Errno::EINVAL);
    }
    loop {
        {
            let mut e = efd.borrow_mut();
            if e.count > 0 {
                let v = if e.semaphore { 1 } else { e.count };
                e.count -= v;
                e.reads += 1;
                buf[..8].copy_from_slice(&v.to_le_bytes());
                request_wake_object(wake::of(efd));
                return Ok(8);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(efd)])?;
    }
}

/// eventfd 的写：加到计数上，会溢出时阻塞。
pub(super) fn eventfd_write(
    cx: &mut Cx<'_>,
    efd: &Rc<RefCell<EventFd>>,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if data.len() < 8 {
        return Err(Errno::EINVAL);
    }
    let v = u64::from_le_bytes(data[..8].try_into().unwrap());
    if v == u64::MAX {
        return Err(Errno::EINVAL);
    }
    loop {
        {
            let mut e = efd.borrow_mut();
            if e.count.checked_add(v).is_some_and(|c| c < u64::MAX) {
                e.count += v;
                e.writes += 1;
                request_wake_object(wake::of(efd));
                return Ok(8);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(efd)])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_triggered_interests_fire_on_new_readiness() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        let both = (POLLIN | POLLOUT) as u32;
        let mut ep = Epoll::default();
        ep.interests
            .push(Interest::new(3, &file, both | EPOLLET, 0));
        // 一开始可写：报告一次之后，没有变化就不再报告
        assert_eq!(ep.ready(0, 8), vec![(0, POLLOUT as u32)]);
        ep.reported(0, POLLOUT as u32);
        assert!(ep.ready(0, 8).is_empty());
        // 变得可读：报告当前全部就绪的位
        efd.borrow_mut().count = 1;
        assert_eq!(ep.ready(0, 8), vec![(0, both)]);
        // 只检查（嵌套 epoll）不消耗事件
        assert_eq!(ep.ready(0, 8), vec![(0, both)]);
        ep.reported(0, both);
        assert!(ep.ready(0, 8).is_empty());
        // 读空之后再写入：重新触发
        efd.borrow_mut().count = 0;
        assert!(ep.ready(0, 8).is_empty());
        efd.borrow_mut().count = 5;
        assert_eq!(ep.ready(0, 8), vec![(0, both)]);
        ep.reported(0, both);
        // 读空、得到 EAGAIN、又来了数据，其间没有调用 epoll_wait：也要重新触发
        efd.borrow_mut().count = 0;
        file.borrow_mut().drained[0] += 1;
        efd.borrow_mut().count = 2;
        assert_eq!(ep.ready(0, 8), vec![(0, both)]);
        ep.reported(0, both);
        assert!(ep.ready(0, 8).is_empty());
        // 写方向用完只重新报告写方向的位，等待时不等报告过、仍然就绪的位
        file.borrow_mut().drained[1] += 1;
        assert_eq!(ep.ready(0, 8), vec![(0, both)]);
        ep.reported(0, both);
        assert!(ep.ready(0, 8).is_empty());
        let mut waits = Waits::default();
        waits.add_epoll(&ep, 0);
        assert!(waits.edge);
    }

    #[test]
    fn edge_triggered_interests_fire_on_every_arrival() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        let mut ep = Epoll::default();
        ep.interests
            .push(Interest::new(3, &file, POLLIN as u32 | EPOLLET, 0));
        let write = |n| {
            let mut e = efd.borrow_mut();
            e.count += n;
            e.writes += 1;
        };
        write(1);
        assert_eq!(ep.ready(0, 8), vec![(0, POLLIN as u32)]);
        ep.reported(0, POLLIN as u32);
        assert!(ep.ready(0, 8).is_empty());
        // 计数没有读走又写了一次（mio 的唤醒器）：再报告一次
        write(1);
        assert_eq!(ep.ready(0, 8), vec![(0, POLLIN as u32)]);
        ep.reported(0, POLLIN as u32);
        assert!(ep.ready(0, 8).is_empty());
        // 等待时不等仍然就绪的位，但要被叫醒（边沿触发的等待者）
        let mut waits = Waits::default();
        waits.add_epoll(&ep, 0);
        assert!(waits.edge);
        // 写零也算一次（Linux 照样唤醒）
        write(0);
        assert_eq!(ep.ready(0, 8), vec![(0, POLLIN as u32)]);
    }

    #[test]
    fn waits_register_the_objects_of_watched_files() {
        let eventfd = || {
            let e = Rc::new(RefCell::new(EventFd {
                count: 0,
                semaphore: false,
                writes: 0,
                reads: 0,
            }));
            (
                OpenFile::new(FileKind::EventFd(e.clone()), oflags::O_RDWR),
                e,
            )
        };
        let (f1, e1) = eventfd();
        let (f2, e2) = eventfd();
        let mut inner = Epoll::default();
        inner
            .interests
            .push(Interest::new(3, &f1, POLLIN as u32, 0));
        let inner = Rc::new(RefCell::new(inner));
        let inner_file = OpenFile::new(FileKind::Epoll(inner.clone()), oflags::O_RDWR);
        let cred = crate::unix::Ucred::of(1, 0, 0);
        let (a, _b) = crate::unix::UnixSocket::pair(crate::net::SOCK_STREAM, cred);
        let mut unix_objects = Vec::new();
        a.wake_objects(&mut unix_objects);
        let sock = Rc::new(RefCell::new(crate::net::Socket::Unix(a)));
        let sock_file = OpenFile::new(FileKind::Socket(sock), oflags::O_RDWR);
        let mut outer = Epoll::default();
        for (fd, f) in [(4, &inner_file), (5, &f2), (6, &sock_file)] {
            outer.interests.push(Interest::new(fd, f, POLLIN as u32, 0));
        }
        let mut waits = Waits::default();
        waits.add_epoll(&outer, 0);
        // 嵌套的 epoll 自己（epoll_ctl）和它关注的文件，AF_UNIX socket 的接收队列和连接的
        // 两个方向
        let mut want = vec![wake::of(&inner), wake::of(&e1), wake::of(&e2)];
        want.extend(&unix_objects);
        assert_eq!(unix_objects.len(), 3);
        for o in want {
            assert!(waits.objects.contains(&o));
        }
        assert!(!waits.pidfd);
    }

    #[test]
    fn nested_epolls_fire_on_every_inner_event() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        // 内层水平触发关注 eventfd，外层边沿触发关注内层
        let mut inner = Epoll::default();
        inner
            .interests
            .push(Interest::new(3, &file, POLLIN as u32, 0));
        let inner = OpenFile::new(
            FileKind::Epoll(Rc::new(RefCell::new(inner))),
            oflags::O_RDWR,
        );
        let mut outer = Epoll::default();
        outer
            .interests
            .push(Interest::new(4, &inner, POLLIN as u32 | EPOLLET, 0));
        let write = || {
            let mut e = efd.borrow_mut();
            e.count += 1;
            e.writes += 1;
        };
        assert!(outer.ready(0, 8).is_empty());
        write();
        assert_eq!(outer.ready(0, 8), vec![(0, POLLIN as u32)]);
        outer.reported(0, POLLIN as u32);
        assert!(outer.ready(0, 8).is_empty());
        // 内层仍然就绪，又来了一次：外层再报告
        write();
        assert_eq!(outer.ready(0, 8), vec![(0, POLLIN as u32)]);
        outer.reported(0, POLLIN as u32);
        // 内层去掉关注不算新事件
        if let FileKind::Epoll(ep) = &inner.borrow().kind {
            ep.borrow_mut().remove(0);
        }
        assert!(outer.ready(0, 8).is_empty());
    }

    #[test]
    fn exclusive_interests_report_each_edge_to_one_epoll() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        let new = || {
            let mut ep = Epoll::default();
            ep.interests
                .push(Interest::new(3, &file, POLLIN as u32 | EPOLLEXCLUSIVE, 0));
            ep
        };
        let (mut a, mut b) = (new(), new());
        let write = || {
            let mut e = efd.borrow_mut();
            e.count += 1;
            e.writes += 1;
        };
        write();
        assert_eq!(a.ready(0, 8), vec![(0, POLLIN as u32)]);
        a.reported(0, POLLIN as u32);
        // 水平触发：报告过的 epoll 只要仍然就绪就接着报告，别的 epoll 等下一个边沿
        assert!(b.ready(0, 8).is_empty());
        assert_eq!(a.ready(0, 8), vec![(0, POLLIN as u32)]);
        // 等待时不自己等独占关注的文件，登记在它的独占门上（定时器线程只叫醒一个）
        let mut waits = Waits::default();
        waits.add_epoll(&b, 0);
        assert_eq!(waits.gated.len(), 1);
        assert!(waits.host.is_empty() && !waits.files);
        // 又来了数据：轮到先检查的那个
        write();
        assert_eq!(b.ready(0, 8), vec![(0, POLLIN as u32)]);
        b.reported(0, POLLIN as u32);
        assert!(a.ready(0, 8).is_empty());
        // 读空之后再来：仍然只报告给一个
        efd.borrow_mut().count = 0;
        file.borrow_mut().drained[0] += 1;
        assert!(a.ready(0, 8).is_empty() && b.ready(0, 8).is_empty());
        write();
        assert_eq!(a.ready(0, 8), vec![(0, POLLIN as u32)]);
        a.reported(0, POLLIN as u32);
        assert!(b.ready(0, 8).is_empty());
    }

    #[test]
    fn exclusive_gates_pick_one_waiter_per_edge() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        let sleepers: Vec<_> = (0..3)
            .map(|_| Arc::new(appine_hal::Event::new().unwrap()))
            .collect();
        let mut gate = ExGate {
            file: Rc::downgrade(&file),
            sleepers: sleepers.clone(),
            last: edge(&file.borrow()),
        };
        let write = || {
            let mut e = efd.borrow_mut();
            e.count += 1;
            e.writes += 1;
        };
        // 没有新的边沿：谁也不叫醒
        assert!(gate.next_waiter(&file.borrow()).is_none());
        // 一个新的边沿只叫醒排在最前面的一个，同一个边沿不再叫醒别的
        write();
        let w = gate.next_waiter(&file.borrow()).unwrap();
        assert!(Arc::ptr_eq(&w, &sleepers[0]));
        assert!(gate.next_waiter(&file.borrow()).is_none());
        // 已经被某个 epoll 报告过的边沿不叫醒
        write();
        let now = edge(&file.borrow());
        file.borrow().exclusive_claim.set(Some((now, 1)));
        assert!(gate.next_waiter(&file.borrow()).is_none());
        // 下一个边沿轮到第二个
        write();
        let w = gate.next_waiter(&file.borrow()).unwrap();
        assert!(Arc::ptr_eq(&w, &sleepers[1]));
        assert_eq!(gate.sleepers.len(), 1);
    }

    #[test]
    fn eventfd_readiness_and_epoll_oneshot() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
            writes: 0,
            reads: 0,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        assert_eq!(poll_now(&file.borrow(), POLLIN | POLLOUT, 0), POLLOUT);
        let mut ep = Epoll::default();
        ep.interests
            .push(Interest::new(3, &file, (POLLIN as u32) | EPOLLONESHOT, 7));
        assert!(ep.ready(0, 8).is_empty());
        efd.borrow_mut().count = 2;
        assert_eq!(ep.ready(0, 8), vec![(0, POLLIN as u32)]);
        ep.reported(0, POLLIN as u32);
        assert!(ep.ready(0, 8).is_empty());
        // 文件关闭之后关注自动失效
        ep.interests[0].armed = true;
        drop(file);
        assert!(ep.ready(0, 8).is_empty() && ep.interests[0].file.upgrade().is_none());
    }
}
