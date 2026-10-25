//! I/O 多路复用（设计文档 §10）：ppoll、pselect6、epoll，以及 eventfd。
//!
//! 三者共用 [`wait_ready`]：先检查进程内对象（管道、PTY、eventfd……）的就绪状态，
//! 宿主 fd（宿主 socket、宿主 stdio）用一次不等待的宿主 poll 检查；都没有就绪时，
//! 把宿主 fd 和自己的唤醒事件放在一起等（§6 的统一等待机制）。epoll 在进程内实现，
//! 默认水平触发。`EPOLLET` 近似为边沿触发：某一位从未就绪变成就绪时触发，触发时与
//! Linux 一样报告当前全部就绪的位。程序在一个方向上用完数据（读写得到 EAGAIN，或者
//! 没有读写满要求的长度，见 [`drained`]）之后，这个方向上次报告过的位全部忘掉，所以
//! 之后来的数据一定会再触发一次：只靠 epoll_wait 看到的状态判断会丢事件，比如读到
//! EAGAIN 和新数据到达之间没有调用 epoll_wait（Go 的 netpoller 就会这样）。已经可读时
//! 又来了数据、而程序没有用完数据，不会再触发。
//!
//! 等待时，边沿触发的关注不等报告过、仍然就绪的位：宿主 poll 会立刻返回，epoll_wait
//! 就变成反复释放、获取 BKL 的忙等，把其他线程饿死。所以程序用完数据时要叫醒这些
//! epoll_wait 重新计算（[`Kernel::edge_waiters`](crate::kernel::Kernel)）。

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{SysResult, get, read_timespec, tty};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Cx, request_wake};
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
                if p.writers == 0 {
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

/// 打开文件现在的"数据代数"：读、写方向用完数据的次数和读到数据的次数。代数不变就
/// 还是同一个边沿（`EPOLLEXCLUSIVE`）。
fn edge(f: &OpenFile) -> [u64; 3] {
    [f.drained[0], f.drained[1], f.reads]
}

/// 立即检查一个文件就绪的事件（宿主 fd 用一次不等待的 poll），只保留 `wanted` 和
/// 总会报告的 POLLERR、POLLHUP、POLLNVAL。
fn poll_now(f: &OpenFile, wanted: i16, depth: u32) -> i16 {
    let (mut ev, host) = readiness(f, depth);
    if let Some(fd) = host {
        let mut p = [(fd, wanted, 0)];
        if appine_hal::poll(&mut p, Some(Duration::ZERO)).is_ok() {
            ev |= p[0].2;
        }
    }
    ev & (wanted | POLLERR | POLLHUP | POLLNVAL)
}

/// 等待时要一起等的东西：宿主 fd，以及最早的 timerfd 到期时刻（含嵌套 epoll 里的）。
#[derive(Default)]
struct Waits {
    host: Vec<(i32, i16, i16)>,
    timer: Option<Instant>,
    /// 有边沿触发的关注少等了报告过的位。
    edge: bool,
}

impl Waits {
    fn add(&mut self, f: &OpenFile, wanted: i16, depth: u32) {
        match (&f.kind, readiness(f, depth).1) {
            (_, Some(fd)) => self.host.push((fd, wanted, 0)),
            (FileKind::TimerFd(t), None) => {
                if let Some(d) = t.borrow().deadline {
                    self.timer = Some(self.timer.map_or(d, |e| e.min(d)));
                }
            }
            (FileKind::Epoll(ep), None) if depth < MAX_NESTS => {
                self.add_epoll(&ep.borrow(), depth + 1);
            }
            _ => {}
        }
    }

    /// 一个 epoll 活着、没有停用的关注。边沿触发的关注不等报告过、仍然就绪的位（见
    /// 模块文档）；报告过 POLLHUP、POLLERR 的整个不等，因为宿主 poll 总会报告这两位，
    /// 而挂断、出错之后也不会再有新的事件。
    fn add_epoll(&mut self, ep: &Epoll, depth: u32) {
        for int in ep.interests.iter().filter(|i| i.armed) {
            let Some(file) = int.file.upgrade() else {
                continue;
            };
            let mut wanted = int.events as i16;
            let seen = int.seen.get() as i16;
            if int.events & EPOLLET != 0 && seen != 0 {
                self.edge = true;
                if seen & (POLLHUP | POLLERR) != 0 {
                    continue;
                }
                wanted &= !seen;
            }
            self.add(&file.borrow(), wanted, depth);
        }
    }

    /// 等到 `deadline`、某个宿主 fd 就绪、timerfd 到期或者被唤醒。
    fn wait(mut self, cx: &mut Cx<'_>, deadline: Option<Instant>) -> Result<(), Errno> {
        let until = match (deadline, self.timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if !self.edge {
            return cx.wait(until, &mut self.host);
        }
        let event = cx.task.event.clone();
        cx.k().edge_waiters.push(event.clone());
        let r = cx.wait(until, &mut self.host);
        cx.k().edge_waiters.retain(|e| !Arc::ptr_eq(e, &event));
        r
    }
}

/// 程序在 `file` 的一个方向（0 读，1 写）上用完了数据：读写返回 EAGAIN，或者没有读写
/// 满要求的长度（tokio 这类运行时据此认为数据已经读完、不再读到 EAGAIN）。边沿触发的
/// 关注之后重新报告这个方向上的事件，等待中少等了这些事件的 epoll_wait 被叫醒重新计算。
pub(super) fn drained(cx: &mut Cx<'_>, file: &FileRef, dir: usize) {
    file.borrow_mut().drained[dir] += 1;
    for event in &cx.k().edge_waiters {
        event.signal();
    }
    // 信号驱动的 I/O 也按用完数据重新开始计算，见 check_fasync
    if file.borrow().flags & oflags::O_ASYNC != 0 {
        request_wake();
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
    /// 上次检查时打开文件的 [`OpenFile::drained`] 和 [`OpenFile::reads`]。
    drained: [u64; 2],
    reads: u64,
}

/// 打开文件刚刚设置了 `O_ASYNC`：登记它。已经就绪的事件不发信号，与 Linux 一样。
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
    });
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
        // 程序在一个方向上用完了数据：这个方向重新算作没有就绪，之后再来数据还会通知。
        // 读过数据之后仍然可读也再通知一次：两次检查之间可能读空了又来了新数据，
        // Linux 每来一次数据都通知
        if f.drained[0] != e.drained[0] || f.reads != e.reads {
            e.last &= !(POLLIN | POLLRDNORM);
        }
        if f.drained[1] != e.drained[1] {
            e.last &= !(POLLOUT | POLLWRNORM);
        }
        e.drained = f.drained;
        e.reads = f.reads;
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
    cx.t.saved_sigmask = Some(cx.task.sigmask.get());
    cx.task.sigmask.set(mask & !signal::UNBLOCKABLE);
    Ok(())
}

pub(super) fn restore_mask<T>(cx: &mut Cx<'_>, r: &Result<T, Errno>) {
    let interrupted = matches!(r, Err(e) if *e == Errno::ERESTARTNOHAND);
    if !interrupted && let Some(saved) = cx.t.saved_sigmask.take() {
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
/// 多个 epoll 等同一个 fd 时只唤醒其中一个。这里总是全部唤醒，所以只检查参数。
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
    /// `EPOLLET`：上次检查时打开文件的 [`OpenFile::drained`]。
    drained: Cell<[u64; 2]>,
}

impl Interest {
    fn new(fd: i32, file: &FileRef, events: u32, data: u64) -> Interest {
        Interest {
            fd,
            file: Rc::downgrade(file),
            events,
            data,
            armed: true,
            seen: Cell::new(0),
            drained: Cell::new(file.borrow().drained),
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
}

impl Epoll {
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
            // 边沿触发的独占关注：同一个边沿已经被别的 epoll 报告过了，当作没有就绪
            // （Linux 只叫醒一个等待者）
            if int.events & (EPOLLET | EPOLLEXCLUSIVE) == EPOLLET | EPOLLEXCLUSIVE
                && file.borrow().exclusive_claim.get() == Some(edge(&file.borrow()))
            {
                continue;
            }
            let ev = poll_now(&file.borrow(), int.events as i16, depth) as u16 as u32;
            if int.events & EPOLLET != 0 {
                // 程序在某个方向上用完了数据：这个方向上次报告的事件作废
                let drained = file.borrow().drained;
                let last = int.drained.replace(drained);
                let mut seen = int.seen.get();
                if drained[0] != last[0] {
                    seen &= WRITE_BITS;
                }
                if drained[1] != last[1] {
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
        let int = &mut self.interests[i];
        int.seen.set(ev);
        if int.events & EPOLLONESHOT != 0 {
            int.armed = false;
        }
        if int.events & (EPOLLET | EPOLLEXCLUSIVE) == EPOLLET | EPOLLEXCLUSIVE
            && let Some(file) = int.file.upgrade()
        {
            let f = file.borrow();
            f.exclusive_claim.set(Some(edge(&f)));
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
    let mut ep = ep.borrow_mut();
    ep.interests.retain(|i| i.file.strong_count() > 0);
    let pos = ep
        .interests
        .iter()
        .position(|i| i.fd == fd && i.file.as_ptr() == Rc::as_ptr(&file));
    // 别的线程可能正在 epoll_wait：叫醒它们按新的关注重新等
    match (op, pos) {
        (EPOLL_CTL_ADD, Some(_)) => Err(Errno::EEXIST),
        (EPOLL_CTL_ADD, None) => {
            ep.interests.push(Interest::new(fd, &file, events, data));
            request_wake();
            Ok(0)
        }
        // 带 EPOLLEXCLUSIVE 加入的不能修改
        (EPOLL_CTL_MOD, Some(i)) if ep.interests[i].events & EPOLLEXCLUSIVE != 0 => {
            Err(Errno::EINVAL)
        }
        // 与 Linux 相同，修改之后重新判断：已经就绪的会再报告一次
        (EPOLL_CTL_MOD, Some(i)) => {
            ep.interests[i] = Interest::new(fd, &file, events, data);
            request_wake();
            Ok(0)
        }
        (EPOLL_CTL_DEL, Some(i)) => {
            ep.interests.remove(i);
            Ok(0)
        }
        (EPOLL_CTL_MOD | EPOLL_CTL_DEL, None) => Err(Errno::ENOENT),
        _ => Err(Errno::EINVAL),
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
    let epf = cx.fds().get(epfd)?;
    let FileKind::Epoll(ep) = &epf.borrow().kind else {
        return Err(Errno::EINVAL);
    };
    let ep = ep.clone();
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
        waits.wait(cx, deadline)?;
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
                buf[..8].copy_from_slice(&v.to_le_bytes());
                request_wake();
                return Ok(8);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(None, &mut [])?;
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
                request_wake();
                return Ok(8);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(None, &mut [])?;
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
    fn eventfd_readiness_and_epoll_oneshot() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
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
