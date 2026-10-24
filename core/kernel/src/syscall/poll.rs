//! I/O 多路复用（设计文档 §10）：ppoll、pselect6、epoll，以及 eventfd。
//!
//! 三者共用 [`wait_ready`]：先检查进程内对象（管道、PTY、eventfd……）的就绪状态，
//! 宿主 fd（宿主 socket、宿主 stdio）用一次不等待的宿主 poll 检查；都没有就绪时，
//! 把宿主 fd 和自己的唤醒事件放在一起等（§6 的统一等待机制）。epoll 在进程内实现，
//! 按水平触发报告（`EPOLLET` 按水平触发处理，读到 EAGAIN 为止的程序不受影响）。

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::{SysResult, get, read_timespec, tty};
use crate::errno::Errno;
use crate::fd::{EventFd, FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Cx, request_wake};
use crate::pipe::PIPE_BUF;
use crate::process::RLIMIT_NOFILE;
use crate::signal;
use crate::uaccess::UserSlice;

const POLLIN: i16 = 0x001;
const POLLPRI: i16 = 0x002;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;
const POLLRDNORM: i16 = 0x040;
const POLLWRNORM: i16 = 0x100;

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
            } else {
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
        FileKind::Epoll(ep) if depth < MAX_NESTS => {
            let ready = ep.borrow().ready(depth + 1, 1);
            (if ready.is_empty() { 0 } else { POLLIN }, None)
        }
        FileKind::Epoll(_) => (0, None),
        FileKind::Path(_) => (POLLNVAL, None),
        _ => (POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM, None),
    }
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

/// 需要等的宿主 fd（含嵌套 epoll 里的）。
fn collect_host(f: &OpenFile, wanted: i16, depth: u32, out: &mut Vec<(i32, i16, i16)>) {
    match (&f.kind, readiness(f, depth).1) {
        (_, Some(fd)) => out.push((fd, wanted, 0)),
        (FileKind::Epoll(ep), None) if depth < MAX_NESTS => {
            for (file, events) in ep.borrow().live() {
                collect_host(&file.borrow(), events, depth + 1, out);
            }
        }
        _ => {}
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
        let mut host = Vec::new();
        for (f, wanted) in items {
            if let Some(f) = f {
                collect_host(&f.borrow(), *wanted, 0, &mut host);
            }
        }
        cx.wait(deadline, &mut host)?;
    }
}

/// 换上 guest 给的临时屏蔽字；被信号打断时由信号投递恢复，否则由 [`restore_mask`] 恢复。
fn set_temp_mask(cx: &mut Cx<'_>, sigmask: u64, size: usize) -> Result<(), Errno> {
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

fn restore_mask<T>(cx: &mut Cx<'_>, r: &Result<T, Errno>) {
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

/// epoll 关注的一个文件。
struct Interest {
    fd: i32,
    /// 打开文件关闭之后，关注自动失效（与 Linux 一样不阻止它关闭）。
    file: Weak<RefCell<OpenFile>>,
    events: u32,
    data: u64,
    /// `EPOLLONESHOT` 报告过一次之后停用，直到 `EPOLL_CTL_MOD`。
    armed: bool,
}

/// 一个 epoll 实例。
#[derive(Default)]
pub(crate) struct Epoll {
    interests: Vec<Interest>,
}

impl Epoll {
    /// 活着、没有停用的关注：(文件, 关注的 poll 事件)。
    fn live(&self) -> Vec<(FileRef, i16)> {
        self.interests
            .iter()
            .filter(|i| i.armed)
            .filter_map(|i| Some((i.file.upgrade()?, i.events as i16)))
            .collect()
    }

    /// 立即检查哪些关注就绪，最多 `max` 个：(下标, 事件)。
    fn ready(&self, depth: u32, max: usize) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        for (i, int) in self.interests.iter().enumerate() {
            if out.len() == max {
                break;
            }
            let Some(file) = int.file.upgrade().filter(|_| int.armed) else {
                continue;
            };
            let ev = poll_now(&file.borrow(), int.events as i16, depth) as u16 as u32;
            if ev != 0 {
                out.push((i, ev));
            }
        }
        out
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

pub(super) fn epoll_ctl(cx: &mut Cx<'_>, epfd: i32, op: u32, fd: i32, event: u64) -> SysResult {
    let epf = cx.fds().get(epfd)?;
    let FileKind::Epoll(ep) = &epf.borrow().kind else {
        return Err(Errno::EINVAL);
    };
    let ep = ep.clone();
    let file = cx.fds().get(fd)?;
    if Rc::ptr_eq(&epf, &file) {
        return Err(Errno::EINVAL);
    }
    // 普通文件和目录总是就绪，Linux 不允许把它们加进 epoll
    if matches!(
        file.borrow().kind,
        FileKind::Regular { .. } | FileKind::Directory { .. }
    ) {
        return Err(Errno::EPERM);
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
    let mut ep = ep.borrow_mut();
    ep.interests.retain(|i| i.file.strong_count() > 0);
    let pos = ep
        .interests
        .iter()
        .position(|i| i.fd == fd && i.file.as_ptr() == Rc::as_ptr(&file));
    match (op, pos) {
        (EPOLL_CTL_ADD, Some(_)) => Err(Errno::EEXIST),
        (EPOLL_CTL_ADD, None) => {
            ep.interests.push(Interest {
                fd,
                file: Rc::downgrade(&file),
                events: events & !EPOLLET,
                data,
                armed: true,
            });
            Ok(0)
        }
        (EPOLL_CTL_MOD, Some(i)) => {
            let int = &mut ep.interests[i];
            int.events = events & !EPOLLET;
            int.data = data;
            int.armed = true;
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
        let mut host = Vec::new();
        for (file, wanted) in ep.borrow().live() {
            collect_host(&file.borrow(), wanted, 0, &mut host);
        }
        cx.wait(deadline, &mut host)?;
    })();
    restore_mask(cx, &r);
    let ready = r?;
    let mut out = Vec::with_capacity(ready.len() * 16);
    {
        let mut ep = ep.borrow_mut();
        for &(i, ev) in &ready {
            let int = &mut ep.interests[i];
            out.extend_from_slice(&ev.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&int.data.to_le_bytes());
            if int.events & EPOLLONESHOT != 0 {
                int.armed = false;
            }
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
    fn eventfd_readiness_and_epoll_oneshot() {
        let efd = Rc::new(RefCell::new(EventFd {
            count: 0,
            semaphore: false,
        }));
        let file = OpenFile::new(FileKind::EventFd(efd.clone()), oflags::O_RDWR);
        assert_eq!(poll_now(&file.borrow(), POLLIN | POLLOUT, 0), POLLOUT);
        let mut ep = Epoll::default();
        ep.interests.push(Interest {
            fd: 3,
            file: Rc::downgrade(&file),
            events: (POLLIN as u32) | EPOLLONESHOT,
            data: 7,
            armed: true,
        });
        assert!(ep.ready(0, 8).is_empty());
        efd.borrow_mut().count = 2;
        assert_eq!(ep.ready(0, 8), vec![(0, POLLIN as u32)]);
        ep.interests[0].armed = false;
        assert!(ep.ready(0, 8).is_empty());
        // 文件关闭之后关注自动失效
        ep.interests[0].armed = true;
        drop(file);
        assert!(ep.ready(0, 8).is_empty() && ep.live().is_empty());
    }
}
