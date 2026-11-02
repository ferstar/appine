//! Linux 原生 AIO：io_setup、io_destroy、io_submit、io_cancel、io_getevents、
//! io_pgetevents。
//!
//! 请求在 io_submit 里同步执行，完成事件放进上下文的队列，由 io_getevents 取走；只有
//! `IOCB_CMD_POLL` 在 fd 还没就绪时挂起，等 io_getevents 时再检查。上下文属于线程组：
//! fork 出来的子进程没有，execve 时清掉。上下文 id 是 guest 里一页只读内存的地址：
//! libaio 会读那里的环形缓冲区头部，魔数不对就总是走 syscall。

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
/// 完成时通知 `aio_resfd` 这个 eventfd。
const IOCB_FLAG_RESFD: u32 = 1;
/// `aio_rw_flags`：RWF_HIPRI、RWF_DSYNC、RWF_SYNC 不影响结果。
const RWF_NOWAIT: u32 = 0x8;
const RWF_APPEND: u32 = 0x10;
const RWF_KNOWN: u32 = 0x1 | 0x2 | 0x4 | RWF_NOWAIT | RWF_APPEND;

/// 一个等着 fd 就绪的 `IOCB_CMD_POLL`。
struct PendingPoll {
    data: u64,
    obj: u64,
    file: FileRef,
    events: i16,
    resfd: Option<i32>,
}

/// 一个 AIO 上下文。
pub(crate) struct AioContext {
    /// io_setup 的 `nr_events`：已经完成、还没取走的事件和挂起的请求加起来的上限。
    max: usize,
    /// 完成事件（`struct io_event`）。
    events: VecDeque<[u8; 32]>,
    polls: Vec<PendingPoll>,
    /// 已经 io_destroy：正在 io_getevents 里等的线程醒来之后返回（Linux 的 `ctx->dead`）。
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
    // 与 Linux 一样按 nr 的两倍计入 aio-nr
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

/// 销毁上下文：挂起的 poll 取消掉（不产生完成事件），正在 io_getevents 里等它的线程醒来
/// 之后返回，与 Linux 的 `kill_ioctx` 一样。mariadb 关闭时靠这一点结束等事件的线程。
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

/// 上下文不再使用（io_destroy、execve、进程退出）：从 aio-nr 里扣掉。
pub(crate) fn released(
    k: &mut crate::kernel::Kernel,
    contexts: impl IntoIterator<Item = (u64, Rc<RefCell<AioContext>>)>,
) {
    for (_, c) in contexts {
        k.aio_nr = k.aio_nr.saturating_sub(c.borrow().max as u64 * 2);
    }
}

/// 完成时通知 eventfd（`IOCB_FLAG_RESFD`）：计数加一。
fn notify_resfd(cx: &mut Cx<'_>, fd: Option<i32>) {
    let Some(fd) = fd else {
        return;
    };
    if let Ok(file) = cx.fds().get(fd)
        && let FileKind::EventFd(e) = &file.borrow().kind
    {
        let mut e = e.borrow_mut();
        e.count = e.count.saturating_add(1).min(u64::MAX - 1);
        // 与 eventfd 的写一样算一次到达（EPOLLET 再触发）
        e.writes += 1;
    }
    request_wake();
}

/// 执行一个读写请求，返回 io_event 的 res（负数是 errno）。
fn read_write(
    cx: &mut Cx<'_>,
    file: &FileRef,
    fd: i32,
    op: u16,
    (buf, len, offset): (u64, u64, i64),
    rw_flags: u32,
) -> i64 {
    // 不能定位的文件（管道、socket）不管偏移；RWF_NOWAIT 时不等
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
    // 只还原临时加上的两个标志：请求执行期间别的线程可能改了别的标志
    let temp = oflags::O_NONBLOCK | oflags::O_APPEND;
    let restored = (file.borrow().flags & !temp) | (saved & temp);
    file.borrow_mut().flags = restored;
    match r {
        Ok(n) => n as i64,
        Err(e) => -(e.0 as i64),
    }
}

/// 提交一个请求：读写和 fsync 当场完成，poll 没就绪时挂起。
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
    // 内核把 aio_key 写成 0（KIOCB_KEY），io_cancel 拿它核对
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
    // 只有挂起的 poll 能取消：完成事件照样放进队列（就绪的事件为空），与 Linux 一样
    // 返回 EINPROGRESS
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

/// 挂起的 poll 里已经就绪的完成掉。
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
        // 等的时候上下文被销毁了：已经完成的事件照样取走，一个也没有就是 EINVAL（Linux 的
        // aio_read_events）
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

/// io_pgetevents 的最后一个参数：`struct { const sigset_t *sigmask; size_t sigsetsize; }`。
pub(super) fn sigmask_arg(cx: &Cx<'_>, usig: u64) -> Result<Option<(u64, usize)>, Errno> {
    if usig == 0 {
        return Ok(None);
    }
    let b: [u8; 16] = get(cx, usig)?;
    let mask = u64::from_le_bytes(b[..8].try_into().unwrap());
    let size = u64::from_le_bytes(b[8..].try_into().unwrap()) as usize;
    Ok((mask != 0).then_some((mask, size)))
}
