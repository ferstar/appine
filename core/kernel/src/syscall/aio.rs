//! Linux native AIO: io_setup, io_destroy, io_submit, io_cancel, io_getevents, io_pgetevents.
//!
//! Requests run synchronously in io_submit, and completion events go into the context's queue,
//! where io_getevents takes them; only `IOCB_CMD_POLL` stays pending while the fd is not ready,
//! and is checked again when io_getevents waits. Contexts belong to the thread group: a forked
//! child has none, and they are cleared on execve. A context id is the address of a page of
//! read-only memory in the guest: libaio reads the ring buffer header there, and when the magic
//! number is wrong it always goes through the syscall.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use appine_engine::prot;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, oflags};
use crate::kernel::{Cx, request_wake};
use crate::mm::flags;
use crate::uaccess::UserSlice;

const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;
const IOCB_CMD_POLL: u16 = 5;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
/// On completion, notify the eventfd `aio_resfd`.
const IOCB_FLAG_RESFD: u32 = 1;
/// `aio_rw_flags`: RWF_HIPRI, RWF_DSYNC and RWF_SYNC do not affect the result.
const RWF_NOWAIT: u32 = 0x8;
const RWF_APPEND: u32 = 0x10;
const RWF_KNOWN: u32 = 0x1 | 0x2 | 0x4 | RWF_NOWAIT | RWF_APPEND;

/// An `IOCB_CMD_POLL` waiting for its fd to be ready.
struct PendingPoll {
    data: u64,
    obj: u64,
    file: FileRef,
    events: i16,
    resfd: Option<i32>,
}

/// An AIO context.
pub(crate) struct AioContext {
    /// io_setup's `nr_events`: the limit on completed events not yet taken plus pending requests.
    max: usize,
    /// Completion events (`struct io_event`).
    events: VecDeque<[u8; 32]>,
    polls: Vec<PendingPoll>,
    /// io_destroy'ed: threads waiting in io_getevents return after waking (Linux's `ctx->dead`).
    dead: bool,
}

impl AioContext {
    fn room(&self) -> bool {
        self.events.len() + self.polls.len() < self.max
    }
}

fn event(data: u64, obj: u64, res: i64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&data.to_le_bytes());
    b[8..16].copy_from_slice(&obj.to_le_bytes());
    b[16..24].copy_from_slice(&res.to_le_bytes());
    b
}

fn context(cx: &Cx<'_>, ctx: u64) -> Result<Rc<RefCell<AioContext>>, Errno> {
    cx.t.group
        .borrow()
        .aio
        .get(&ctx)
        .cloned()
        .ok_or(Errno::EINVAL)
}

pub(super) fn io_setup(cx: &mut Cx<'_>, nr: u32, ctxp: u64) -> SysResult {
    let cur = u64::from_le_bytes(get(cx, ctxp)?);
    if cur != 0 || nr == 0 || nr > 0x1000_0000 / 32 {
        return Err(Errno::EINVAL);
    }
    // As on Linux, twice nr counts towards aio-nr
    let cost = u64::from(nr) * 2;
    if cx.kr().aio_nr + cost > cx.kr().aio_max_nr {
        return Err(Errno::EAGAIN);
    }
    let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
    let ring = cx.mm_mut().mmap(0, 4096, prot::READ, anon, None)?;
    if let Err(e) = put(cx, ctxp, ring.to_le_bytes()) {
        let _ = cx.mm_mut().munmap(ring, 4096);
        return Err(e);
    }
    cx.k().aio_nr += cost;
    let ctx = AioContext {
        max: nr as usize,
        events: VecDeque::new(),
        polls: Vec::new(),
        dead: false,
    };
    cx.t.group
        .borrow_mut()
        .aio
        .insert(ring, Rc::new(RefCell::new(ctx)));
    Ok(0)
}

/// Destroys a context: pending polls are cancelled (producing no completion events), and threads
/// waiting for it in io_getevents return after waking, as with Linux's `kill_ioctx`. mariadb
/// relies on this to end its event-waiting threads at shutdown.
pub(super) fn io_destroy(cx: &mut Cx<'_>, ctx: u64) -> SysResult {
    let c =
        cx.t.group
            .borrow_mut()
            .aio
            .remove(&ctx)
            .ok_or(Errno::EINVAL)?;
    {
        let mut c = c.borrow_mut();
        c.dead = true;
        c.polls.clear();
    }
    released(cx.k(), std::iter::once((ctx, c)));
    let _ = cx.mm_mut().munmap(ctx, 4096);
    request_wake();
    Ok(0)
}

/// A context is no longer used (io_destroy, execve, process exit): subtract it from aio-nr.
pub(crate) fn released(
    k: &mut crate::kernel::Kernel,
    contexts: impl IntoIterator<Item = (u64, Rc<RefCell<AioContext>>)>,
) {
    for (_, c) in contexts {
        k.aio_nr = k.aio_nr.saturating_sub(c.borrow().max as u64 * 2);
    }
}

/// On completion, notifies the eventfd (`IOCB_FLAG_RESFD`): its count goes up by one.
fn notify_resfd(cx: &mut Cx<'_>, fd: Option<i32>) {
    let Some(fd) = fd else {
        return;
    };
    if let Ok(file) = cx.fds().get(fd)
        && let FileKind::EventFd(e) = &file.borrow().kind
    {
        let mut e = e.borrow_mut();
        e.count = e.count.saturating_add(1).min(u64::MAX - 1);
        // Counts as one arrival, like a write to the eventfd (EPOLLET fires again)
        e.writes += 1;
    }
    request_wake();
}

/// Runs a read or write request, returning the io_event's res (negative is an errno).
fn read_write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    op: u16,
    (buf, len, offset): (u64, u64, i64),
    rw_flags: u32,
) -> i64 {
    // Files that cannot seek (pipes, sockets) ignore the offset; with RWF_NOWAIT, do not wait
    let at = super::fs::seekable(&file.borrow()).then_some(offset as u64);
    let saved = file.borrow().flags;
    let mut f = saved;
    if rw_flags & RWF_NOWAIT != 0 {
        f |= oflags::O_NONBLOCK;
    }
    if rw_flags & RWF_APPEND != 0 {
        f |= oflags::O_APPEND;
    }
    file.borrow_mut().flags = f;
    let len = len as usize;
    let r = match op {
        IOCB_CMD_PREAD => super::fs::read_file(cx, file, buf, len, at),
        IOCB_CMD_PWRITE => super::fs::write_file(cx, file, buf, len, at),
        IOCB_CMD_PREADV => super::fs::vectored(cx, fd, buf, len, at, false),
        _ => super::fs::vectored(cx, fd, buf, len, at, true),
    };
    // Restore only the two flags added temporarily: other threads may have changed other flags
    // while the request ran
    let temp = oflags::O_NONBLOCK | oflags::O_APPEND;
    let restored = (file.borrow().flags & !temp) | (saved & temp);
    file.borrow_mut().flags = restored;
    match r {
        Ok(n) => n as i64,
        Err(e) => -(e.0 as i64),
    }
}

/// Submits one request: reads, writes and fsync complete on the spot, and a poll that is not ready
/// stays pending.
fn submit_one(cx: &mut Cx<'_>, c: &Rc<RefCell<AioContext>>, obj: u64) -> Result<(), Errno> {
    let b: [u8; 64] = get(cx, obj)?;
    let u64_at = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let data = u64_at(0);
    let rw_flags = u32_at(12);
    let op = u16::from_le_bytes([b[16], b[17]]);
    let fd = u32_at(20) as i32;
    let (buf, len, offset) = (u64_at(24), u64_at(32), u64_at(40) as i64);
    let (reserved, aio_flags, resfd) = (u64_at(48), u32_at(56), u32_at(60) as i32);
    if reserved != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(fd)?;
    let resfd = if aio_flags & IOCB_FLAG_RESFD != 0 {
        match &cx.fds().get(resfd)?.borrow().kind {
            FileKind::EventFd(_) => Some(resfd),
            _ => return Err(Errno::EINVAL),
        }
    } else {
        None
    };
    match op {
        IOCB_CMD_PREAD | IOCB_CMD_PREADV if !file.borrow().readable() => return Err(Errno::EBADF),
        IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV if !file.borrow().writable() => {
            return Err(Errno::EBADF);
        }
        IOCB_CMD_PREAD | IOCB_CMD_PREADV | IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV
            if rw_flags & !RWF_KNOWN != 0 =>
        {
            return Err(Errno::EOPNOTSUPP);
        }
        IOCB_CMD_PREAD | IOCB_CMD_PREADV | IOCB_CMD_PWRITE | IOCB_CMD_PWRITEV | IOCB_CMD_FSYNC
        | IOCB_CMD_FDSYNC | IOCB_CMD_POLL => {}
        _ => return Err(Errno::EINVAL),
    }
    if !c.borrow().room() {
        return Err(Errno::EAGAIN);
    }
    // The kernel writes aio_key as 0 (KIOCB_KEY), and io_cancel checks against it
    put(cx, obj + 8, 0u32.to_le_bytes())?;
    let res = match op {
        IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => match super::fs::fsync(cx, fd, op == IOCB_CMD_FDSYNC) {
            Ok(_) => 0,
            Err(e) => -(e.0 as i64),
        },
        IOCB_CMD_POLL => {
            let events = buf as u16 as i16;
            let ready = super::poll::poll_once(&file.borrow(), events, false);
            if ready == 0 {
                c.borrow_mut().polls.push(PendingPoll {
                    data,
                    obj,
                    file,
                    events,
                    resfd,
                });
                return Ok(());
            }
            ready as i64
        }
        _ => read_write(cx, &file, fd, op, (buf, len, offset), rw_flags),
    };
    c.borrow_mut().events.push_back(event(data, obj, res));
    notify_resfd(cx, resfd);
    Ok(())
}

pub(super) fn io_submit(cx: &mut Cx<'_>, ctx: u64, nr: i64, iocbpp: u64) -> SysResult {
    let c = context(cx, ctx)?;
    if nr < 0 {
        return Err(Errno::EINVAL);
    }
    let mut done = 0;
    for i in 0..nr as u64 {
        let r = get::<8>(cx, iocbpp.wrapping_add(i * 8))
            .map(u64::from_le_bytes)
            .and_then(|obj| {
                if obj == 0 {
                    Err(Errno::EFAULT)
                } else {
                    Ok(obj)
                }
            })
            .and_then(|obj| submit_one(cx, &c, obj));
        match r {
            Ok(()) => done += 1,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    request_wake();
    Ok(done)
}

pub(super) fn io_cancel(cx: &mut Cx<'_>, ctx: u64, iocb: u64, _result: u64) -> SysResult {
    let key = u32::from_le_bytes(get(cx, iocb.wrapping_add(8))?);
    let c = context(cx, ctx)?;
    if key != 0 {
        return Err(Errno::EINVAL);
    }
    // Only a pending poll can be cancelled: its completion event still goes into the queue (with
    // no ready events), and EINPROGRESS is returned, as on Linux
    let mut c = c.borrow_mut();
    let i = c
        .polls
        .iter()
        .position(|p| p.obj == iocb)
        .ok_or(Errno::EINVAL)?;
    let p = c.polls.remove(i);
    c.events.push_back(event(p.data, p.obj, 0));
    request_wake();
    Err(Errno::EINPROGRESS)
}

/// Completes the pending polls that have become ready.
fn complete_polls(cx: &mut Cx<'_>, c: &Rc<RefCell<AioContext>>) {
    let mut done = Vec::new();
    {
        let mut c = c.borrow_mut();
        let mut i = 0;
        while i < c.polls.len() {
            let p = &c.polls[i];
            let ready = super::poll::poll_once(&p.file.borrow(), p.events, true);
            if ready != 0 {
                let p = c.polls.remove(i);
                c.events.push_back(event(p.data, p.obj, ready as i64));
                done.push(p.resfd);
            } else {
                i += 1;
            }
        }
    }
    for resfd in done {
        notify_resfd(cx, resfd);
    }
}

pub(super) fn io_getevents(
    cx: &mut Cx<'_>,
    ctx: u64,
    min_nr: i64,
    nr: i64,
    events: u64,
    timeout: u64,
    sigmask: Option<(u64, usize)>,
) -> SysResult {
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(Errno::EINVAL);
    }
    let c = context(cx, ctx)?;
    let deadline = match timeout {
        0 => None,
        t => Some(Instant::now() + read_timespec(cx, t)?),
    };
    if let Some((mask, size)) = sigmask {
        super::poll::set_temp_mask(cx, mask, size)?;
    }
    let r = loop {
        complete_polls(cx, &c);
        let have = c.borrow().events.len() as i64;
        if have >= min_nr.max(1) || min_nr == 0 || deadline.is_some_and(|d| Instant::now() >= d) {
            break Ok(());
        }
        // The context was destroyed while waiting: events already completed are taken all the
        // same, and when there are none it is EINVAL (Linux's aio_read_events)
        if c.borrow().dead {
            break if have > 0 { Ok(()) } else { Err(Errno::EINVAL) };
        }
        if cx.interrupted() {
            break Err(Errno::EINTR);
        }
        if let Err(e) = cx.wait(deadline, &mut []) {
            break Err(e);
        }
    };
    if sigmask.is_some() {
        super::poll::restore_mask(cx, &r);
    }
    r?;
    let take = (c.borrow().events.len() as i64).min(nr) as usize;
    let out: Vec<[u8; 32]> = c.borrow_mut().events.drain(..take).collect();
    let bytes: Vec<u8> = out.concat();
    cx.mm().write(UserSlice::new(events, bytes.len()), &bytes)?;
    Ok(take as u64)
}

/// The last argument of io_pgetevents: `struct { const sigset_t *sigmask; size_t sigsetsize; }`.
pub(super) fn sigmask_arg(cx: &Cx<'_>, usig: u64) -> Result<Option<(u64, usize)>, Errno> {
    if usig == 0 {
        return Ok(None);
    }
    let b: [u8; 16] = get(cx, usig)?;
    let mask = u64::from_le_bytes(b[..8].try_into().unwrap());
    let size = u64::from_le_bytes(b[8..].try_into().unwrap()) as usize;
    Ok((mask != 0).then_some((mask, size)))
}
