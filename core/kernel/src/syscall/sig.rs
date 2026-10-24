//! 信号类 syscall：kill 系列、sigaction、屏蔽字、sigsuspend、sigaltstack。

use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT, Tid};
use crate::process::Signal;
use crate::signal::{self, SigAction, SigInfo, UNBLOCKABLE, code};

const SIG_BLOCK: u32 = 0;
const SIG_UNBLOCK: u32 = 1;
const SIG_SETMASK: u32 = 2;
/// guest 的 sigset_t 在内核 ABI 里的大小（64 个信号）。
const SIGSET_SIZE: usize = 8;

/// 信号编号参数：0 只检查目标是否存在。
fn signal_arg(sig: i32) -> Result<Option<Signal>, Errno> {
    if sig == 0 {
        return Ok(None);
    }
    Signal::new(sig).map(Some).ok_or(Errno::EINVAL)
}

pub(super) fn kill(cx: &mut Cx<'_>, pid: i32, sig: i32) -> SysResult {
    let sig = signal_arg(sig)?;
    let me = cx.tgid();
    let my_pgid = cx.t.group.borrow().pgid;
    // 活着的目标线程组：pid > 0 是一个进程，0 是本进程组，-1 是除 init 和自己以外的
    // 所有进程，< -1 是进程组 -pid
    let targets: Vec<Tid> = cx
        .kr()
        .groups
        .values()
        .filter_map(|g| {
            let g = g.borrow();
            let hit = match pid {
                p if p > 0 => g.tgid == p,
                0 => g.pgid == my_pgid,
                -1 => g.tgid != INIT && g.tgid != me,
                p => g.pgid == -p,
            };
            (hit && g.tgid != INIT).then_some(g.tgid)
        })
        .collect();
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    if let Some(sig) = sig {
        for tgid in targets {
            signal::send_to_group(cx.kr(), tgid, sig, SigInfo::kill(code::SI_USER, me));
        }
    }
    Ok(0)
}

pub(super) fn tkill(cx: &mut Cx<'_>, tid: i32, sig: i32) -> SysResult {
    tgkill(cx, -1, tid, sig)
}

/// `tgid` 为 -1 时不检查线程组（tkill）。
pub(super) fn tgkill(cx: &mut Cx<'_>, tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tid <= 0 || tgid == 0 || tgid < -1 {
        return Err(Errno::EINVAL);
    }
    let sig = signal_arg(sig)?;
    let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ESRCH)?;
    if tgid != -1 && task.tgid != tgid {
        return Err(Errno::ESRCH);
    }
    if let Some(sig) = sig {
        let info = SigInfo::kill(code::SI_TKILL, cx.tgid());
        signal::send_to_task(cx.kr(), &task, sig, info);
    }
    Ok(0)
}

pub(super) fn rt_sigaction(
    cx: &mut Cx<'_>,
    sig: i32,
    act: u64,
    oact: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let sig = Signal::new(sig).ok_or(Errno::EINVAL)?;
    if act != 0 && (sig == Signal::KILL || sig == Signal::STOP) {
        return Err(Errno::EINVAL);
    }
    let new = if act != 0 {
        let b: [u8; 32] = get(cx, act)?;
        let word = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        Some(SigAction {
            handler: word(0),
            flags: word(1),
            restorer: word(2),
            mask: word(3) & !UNBLOCKABLE,
        })
    } else {
        None
    };
    let sighand = cx.t.group.borrow().sighand.clone();
    if oact != 0 {
        let old = sighand.borrow().get(sig);
        let mut b = [0u8; 32];
        for (i, w) in [old.handler, old.flags, old.restorer, old.mask]
            .into_iter()
            .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        put(cx, oact, b)?;
    }
    if let Some(new) = new {
        sighand.borrow_mut().set(sig, new);
        // 改成忽略之后，已经挂起的同一信号被丢弃（线程组和每个线程上的）
        if sighand.borrow().ignores(sig) {
            let threads = {
                let mut g = cx.t.group.borrow_mut();
                g.pending &= !sig.bit();
                g.threads.clone()
            };
            for tid in threads {
                if let Some(t) = cx.kr().tasks.get(&tid) {
                    t.pending.set(t.pending.get() & !sig.bit());
                }
            }
        }
    }
    Ok(0)
}

pub(super) fn rt_sigprocmask(
    cx: &mut Cx<'_>,
    how: u32,
    set: u64,
    oset: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let cur = cx.task.sigmask.get();
    let new = if set != 0 {
        let s = u64::from_le_bytes(get(cx, set)?);
        Some(match how {
            SIG_BLOCK => cur | s,
            SIG_UNBLOCK => cur & !s,
            SIG_SETMASK => s,
            _ => return Err(Errno::EINVAL),
        })
    } else {
        None
    };
    if oset != 0 {
        put(cx, oset, cur.to_le_bytes())?;
    }
    if let Some(mask) = new {
        cx.task.sigmask.set(mask & !UNBLOCKABLE);
    }
    Ok(0)
}

pub(super) fn rt_sigpending(cx: &mut Cx<'_>, set: u64, size: usize) -> SysResult {
    if size > SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let pending = (cx.task.pending.get() | cx.t.group.borrow().pending) & cx.task.sigmask.get();
    cx.mm().write(
        crate::uaccess::UserSlice::new(set, size),
        &pending.to_le_bytes()[..size],
    )?;
    Ok(0)
}

pub(super) fn rt_sigsuspend(cx: &mut Cx<'_>, set: u64, size: usize) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let mask = u64::from_le_bytes(get(cx, set)?);
    // 投递信号时帧里保存原来的屏蔽字，处理函数返回后恢复（signal::deliver）
    cx.t.saved_sigmask = Some(cx.task.sigmask.get());
    cx.task.sigmask.set(mask & !UNBLOCKABLE);
    while !cx.interrupted() {
        if let Err(e) = cx.wait(None, &mut []) {
            cx.task.sigmask.set(cx.t.saved_sigmask.take().unwrap());
            return Err(e);
        }
    }
    Err(Errno::ERESTARTNOHAND)
}

pub(super) fn sigaltstack(cx: &mut Cx<'_>, sp: u64, uss: u64, uoss: u64) -> SysResult {
    let old = cx.t.altstack;
    if uss != 0 {
        let b: [u8; 24] = get(cx, uss)?;
        let ss_sp = u64::from_le_bytes(b[..8].try_into().unwrap());
        let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..].try_into().unwrap());
        let mut alt = old;
        alt.change(sp, ss_sp, flags, size)?;
        cx.t.altstack = alt;
    }
    if uoss != 0 {
        let mut b = [0u8; 24];
        b[..8].copy_from_slice(&old.sp.to_le_bytes());
        b[8..12].copy_from_slice(&old.reported_flags(sp).to_le_bytes());
        b[16..].copy_from_slice(&old.size.to_le_bytes());
        put(cx, uoss, b)?;
    }
    Ok(0)
}
