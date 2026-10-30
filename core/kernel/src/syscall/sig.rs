//! 信号类 syscall：kill 系列、sigaction、屏蔽字、sigsuspend、sigaltstack。

use std::rc::{Rc, Weak};
use std::time::Instant;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::cred::Cred;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::{Cx, INIT, Tid, wake};
use crate::process::Signal;
use crate::pseudo::PseudoNode;
use crate::signal::{self, SigAction, SigInfo, UNBLOCKABLE, code};
use crate::vfs::Node;

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
    let (my_pgid, my_sid) = {
        let g = cx.t.group.borrow();
        (g.pgid, g.sid)
    };
    let cred = cx.cred();
    // 内置的 init 忽略所有信号；没有权限给它发信号时与 Linux 一样报 EPERM
    if pid == INIT {
        return if cred.may_signal(&crate::cred::Cred::root()) {
            Ok(0)
        } else {
            Err(Errno::EPERM)
        };
    }
    // 活着的目标线程组：pid > 0 是一个进程，0 是本进程组，-1 是除 init 和自己以外的
    // 所有进程（自己的 pid 命名空间里看得到、编号大于 1 的），< -1 是进程组 -pid
    let ns = cx.pid_ns();
    let targets: Vec<Tid> = cx
        .kr()
        .groups
        .values()
        .filter_map(|g| {
            let g = g.borrow();
            let hit = match pid {
                p if p > 0 => g.tgid == p,
                0 => g.pgid == my_pgid,
                -1 => g.tgid != me && ns.vnr(g.tgid).is_some_and(|v| v > 1),
                p => g.pgid == -p,
            };
            (hit && g.tgid != INIT).then_some(g.tgid)
        })
        .collect();
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    // 没有权限的目标跳过；一个也发不出去时报 EPERM（pid 为 -1 时不报）
    let allowed: Vec<Tid> = targets
        .into_iter()
        .filter(|&t| may_kill(cx, &cred, my_sid, t, sig))
        .collect();
    if allowed.is_empty() && pid != -1 {
        return Err(Errno::EPERM);
    }
    if let Some(sig) = sig {
        for tgid in allowed {
            let info = SigInfo::kill(code::SI_USER, cx.pid_out(me), cred.ruid).from(me);
            signal::send_to_group(cx.kr(), tgid, sig, info);
        }
    }
    Ok(0)
}

/// 能不能给进程 `tgid` 发信号 `sig`（`check_kill_permission`）：凭据允许，或者是同一个
/// 进程，或者是发给同一个会话的 SIGCONT。
fn may_kill(cx: &Cx<'_>, cred: &Cred, sid: Tid, tgid: Tid, sig: Option<Signal>) -> bool {
    let Some(target) = cx.kr().groups.get(&tgid) else {
        return false;
    };
    let t = target.borrow();
    t.tgid == cx.tgid() || cred.may_signal(&t.cred) || (sig == Some(Signal::CONT) && t.sid == sid)
}

pub(super) fn tkill(cx: &mut Cx<'_>, tid: i32, sig: i32) -> SysResult {
    kill_thread(cx, None, tid, sig)
}

pub(super) fn tgkill(cx: &mut Cx<'_>, tgid: i32, tid: i32, sig: i32) -> SysResult {
    if tgid <= 0 {
        return Err(Errno::EINVAL);
    }
    kill_thread(cx, Some(tgid), tid, sig)
}

/// 给一个线程发信号；`tgid` 为 None 时不检查它属于哪个线程组（tkill）。
fn kill_thread(cx: &mut Cx<'_>, tgid: Option<i32>, tid: i32, sig: i32) -> SysResult {
    if tid <= 0 {
        return Err(Errno::EINVAL);
    }
    let sig = signal_arg(sig)?;
    let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ESRCH)?;
    if tgid.is_some_and(|g| task.tgid != g) {
        return Err(Errno::ESRCH);
    }
    let cred = cx.cred();
    let sid = cx.t.group.borrow().sid;
    if !may_kill(cx, &cred, sid, task.tgid, sig) {
        return Err(Errno::EPERM);
    }
    if let Some(sig) = sig {
        // tkill 的 si_code 不是 SI_USER：实时信号排不上队时与 sigqueue 一样是 EAGAIN
        if !signal::can_queue(cx.kr(), task.tgid, sig) {
            return Err(Errno::EAGAIN);
        }
        let me = cx.tgid();
        let info = SigInfo::kill(code::SI_TKILL, cx.pid_out(me), cred.ruid).from(me);
        signal::send_to_task(cx.kr(), &task, sig, info);
    }
    Ok(0)
}

/// 调用者给出的 siginfo（rt_sigqueueinfo、pidfd_send_signal）。
fn queue_info(b: &[u8; 128]) -> SigInfo {
    let word = |off: usize| i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    SigInfo {
        code: word(8),
        fields: signal::InfoFields::Queue {
            pid: word(16),
            uid: word(20) as u32,
            value: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        },
        esr: 0,
        sender: 0,
    }
}

/// pidfd_send_signal：pidfd 也可以是打开的 `/proc/<pid>` 目录。`uinfo` 为空时与 kill
/// 相同，否则与 rt_sigqueueinfo 相同（si_signo 必须等于 `sig`）。
pub(super) fn pidfd_send_signal(
    cx: &mut Cx<'_>,
    pidfd: i32,
    sig: i32,
    uinfo: u64,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(pidfd)?;
    let tgid = match &file.borrow().kind {
        FileKind::Pidfd(p) => p.group(cx.kr()).map(|_| p.tgid).ok_or(Errno::ESRCH)?,
        // 打开的那个进程已经回收了：pid 可能已经给了新进程
        FileKind::Directory {
            node: Node::Pseudo(_, PseudoNode::Pid(pid, None)),
            process,
            ..
        } => {
            let same = |g: &Rc<RefCell<crate::process::ThreadGroup>>| {
                process
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|p| Rc::ptr_eq(&p, g))
            };
            match cx.kr().groups.get(pid) {
                Some(g) if same(g) => *pid,
                _ => return Err(Errno::ESRCH),
            }
        }
        _ => return Err(Errno::EBADF),
    };
    let sig = signal_arg(sig)?;
    let me = cx.tgid();
    let cred = cx.cred();
    let info = if uinfo != 0 {
        let b: [u8; 128] = get(cx, uinfo)?;
        if i32::from_le_bytes(b[..4].try_into().unwrap()) != sig.map_or(0, Signal::number) {
            return Err(Errno::EINVAL);
        }
        let info = queue_info(&b);
        if (info.code >= 0 || info.code == code::SI_TKILL) && tgid != me {
            return Err(Errno::EPERM);
        }
        info
    } else {
        SigInfo::kill(code::SI_USER, cx.pid_out(me), cred.ruid)
    }
    .from(me);
    if !cx.kr().groups.contains_key(&tgid) {
        return Err(Errno::ESRCH);
    }
    let sid = cx.t.group.borrow().sid;
    if !may_kill(cx, &cred, sid, tgid, sig) {
        return Err(Errno::EPERM);
    }
    let Some(sig) = sig else {
        return Ok(0);
    };
    if uinfo != 0 && info.code != code::SI_USER && !signal::can_queue(cx.kr(), tgid, sig) {
        return Err(Errno::EAGAIN);
    }
    // 内置的 init 忽略所有信号
    if tgid != INIT {
        signal::send_to_group(cx.kr(), tgid, sig, info);
    }
    Ok(0)
}

/// rt_sigqueueinfo、rt_tgsigqueueinfo（`tid` 为 None 时发给整个进程）：siginfo 由调用者
/// 给出。不能冒充内核或 kill 发出的信号（si_code 不小于 0 或者是 SI_TKILL），除非发给
/// 自己。
pub(super) fn sigqueueinfo(
    cx: &mut Cx<'_>,
    tgid: i32,
    tid: Option<i32>,
    sig: i32,
    uinfo: u64,
) -> SysResult {
    let b: [u8; 128] = get(cx, uinfo)?;
    let info = queue_info(&b).from(cx.tgid());
    let code = info.code;
    let target = tid.unwrap_or(tgid);
    if (code >= 0 || code == code::SI_TKILL) && target != cx.tid() && tgid != cx.tgid() {
        return Err(Errno::EPERM);
    }
    let sig = signal_arg(sig)?;
    let cred = cx.cred();
    let sid = cx.t.group.borrow().sid;
    // rt_sigqueueinfo 的 pid 也可以是进程里任何一个线程的 tid
    let tgid = match tid {
        None => cx.kr().tasks.get(&tgid).map_or(tgid, |t| t.tgid),
        Some(_) => tgid,
    };
    // 实时信号排不上队：返回 EAGAIN，而不是像 kill 那样丢掉 siginfo
    let full = |cx: &Cx<'_>| {
        sig.is_some_and(|s| code != code::SI_USER && !signal::can_queue(cx.kr(), tgid, s))
    };
    match tid {
        Some(tid) => {
            if tgid <= 0 || tid <= 0 {
                return Err(Errno::EINVAL);
            }
            let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ESRCH)?;
            if task.tgid != tgid {
                return Err(Errno::ESRCH);
            }
            if !may_kill(cx, &cred, sid, tgid, sig) {
                return Err(Errno::EPERM);
            }
            if full(cx) {
                return Err(Errno::EAGAIN);
            }
            if let Some(sig) = sig {
                signal::send_to_task(cx.kr(), &task, sig, info);
            }
        }
        None => {
            let alive = cx
                .kr()
                .groups
                .get(&tgid)
                .is_some_and(|g| g.borrow().zombie.is_none() && tgid != INIT);
            if !alive {
                return Err(Errno::ESRCH);
            }
            if !may_kill(cx, &cred, sid, tgid, sig) {
                return Err(Errno::EPERM);
            }
            if full(cx) {
                return Err(Errno::EAGAIN);
            }
            if let Some(sig) = sig {
                signal::send_to_group(cx.kr(), tgid, sig, info);
            }
        }
    }
    Ok(0)
}

/// signalfd4：`fd` 为 -1 时新建，否则修改已有 signalfd 的掩码。
pub(super) fn signalfd4(cx: &mut Cx<'_>, fd: i32, mask: u64, size: usize, flags: u32) -> SysResult {
    use crate::fd::{FileKind, OpenFile, SignalFd, oflags};
    if size != SIGSET_SIZE || flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let set = u64::from_le_bytes(get(cx, mask)?) & !UNBLOCKABLE;
    if fd != -1 {
        let file = cx.fds().get(fd)?;
        let FileKind::SignalFd(s) = &file.borrow().kind else {
            return Err(Errno::EINVAL);
        };
        s.borrow_mut().mask = set;
        // 等这个 signalfd 的线程按新的掩码重新检查（poll 登记了它）
        crate::kernel::request_wake_object(crate::kernel::wake::of(s));
        return Ok(fd as u64);
    }
    let s = SignalFd {
        mask: set,
        group: std::rc::Rc::downgrade(&cx.t.group),
    };
    let file = OpenFile::new(
        FileKind::SignalFd(std::rc::Rc::new(crate::cell::RefCell::new(s))),
        oflags::O_RDONLY | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

/// 读 signalfd：取走调用者挂起的、在掩码里的信号，每个编码成 128 字节的
/// `struct signalfd_siginfo`。一个都没有时阻塞（非阻塞时 EAGAIN）。
pub(super) fn signalfd_read(
    cx: &mut Cx<'_>,
    s: &std::rc::Rc<crate::cell::RefCell<crate::fd::SignalFd>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if buf.len() < 128 {
        return Err(Errno::EINVAL);
    }
    let mut n = 0;
    loop {
        let mask = s.borrow().mask;
        while n + 128 <= buf.len() {
            let Some((sig, info)) = signal::take(cx, mask) else {
                break;
            };
            buf[n..n + 128].copy_from_slice(&encode_signalfd_siginfo(sig, &info));
            n += 128;
        }
        if n > 0 {
            return Ok(n);
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // 被屏蔽的信号挂起时请求 wake::SIGNAL，改掩码时请求这个 signalfd
        cx.wait_objects(None, &mut [], wake::FILE | wake::SIGNAL, &[wake::of(s)])?;
    }
}

/// `struct signalfd_siginfo`（128 字节）。
fn encode_signalfd_siginfo(sig: Signal, info: &SigInfo) -> [u8; 128] {
    use signal::InfoFields;
    let mut b = [0u8; 128];
    let mut put32 = |off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    put32(0, sig.number() as u32);
    put32(8, info.code as u32);
    let mut value = None;
    match info.fields {
        InfoFields::None => {}
        InfoFields::Kill { pid, uid } => {
            put32(12, pid as u32);
            put32(16, uid);
        }
        InfoFields::Child { pid, uid, status } => {
            put32(12, pid as u32);
            put32(16, uid);
            put32(40, status as u32);
        }
        InfoFields::Queue { pid, uid, value: v } => {
            put32(12, pid as u32);
            put32(16, uid);
            value = Some(v);
        }
        InfoFields::Timer {
            id,
            overrun,
            value: v,
        } => {
            put32(24, id as u32);
            put32(32, overrun);
            value = Some(v);
        }
        InfoFields::Fault { addr } => b[72..80].copy_from_slice(&addr.to_le_bytes()),
        InfoFields::Poll { band, fd } => {
            put32(20, fd as u32);
            put32(28, band as u32);
        }
        InfoFields::Sys {
            errno,
            call_addr,
            syscall,
            arch,
        } => {
            put32(4, errno as u32);
            put32(84, syscall as u32);
            put32(96, arch);
            b[88..96].copy_from_slice(&call_addr.to_le_bytes());
        }
        // 跟踪者给的：多数 si_code 的开头是发送者的 pid 和 uid
        InfoFields::Raw(rest) => {
            put32(12, u32::from_le_bytes(rest[..4].try_into().unwrap()));
            put32(16, u32::from_le_bytes(rest[4..8].try_into().unwrap()));
        }
    }
    if let Some(v) = value {
        b[44..48].copy_from_slice(&(v as u32).to_le_bytes());
        b[48..56].copy_from_slice(&v.to_le_bytes());
    }
    b
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
            immutable: false,
        })
    } else {
        None
    };
    let sighand = cx.t.group.borrow().sighand.clone();
    // seccomp 要结束进程时强制的默认处理不能再改（Linux 的 SA_IMMUTABLE）
    if sighand.borrow().get(sig).immutable {
        return Err(Errno::EINVAL);
    }
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
                g.pending.discard(sig.bit());
                g.threads.clone()
            };
            for tid in threads {
                if let Some(t) = cx.kr().tasks.get(&tid) {
                    t.pending.borrow_mut().discard(sig.bit());
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
    let pending = cx.task.pending.borrow().set() | cx.t.group.borrow().pending.set();
    let pending = pending & cx.task.sigmask.get();
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
    cx.task.saved_sigmask.set(Some(cx.task.sigmask.get()));
    cx.task.sigmask.set(mask & !UNBLOCKABLE);
    while !cx.interrupted() {
        if let Err(e) = cx.wait(None, &mut []) {
            if let Some(saved) = cx.task.saved_sigmask.take() {
                cx.task.sigmask.set(saved);
            }
            return Err(e);
        }
    }
    Err(Errno::ERESTARTNOHAND)
}

/// rt_sigtimedwait（sigwait、sigwaitinfo、sigtimedwait）：等 `set` 里的信号挂起，不经过
/// 处理函数直接取走它。`set` 里的信号通常被调用方屏蔽着；别的信号打断等待时返回 EINTR，
/// 与 Linux 相同，这个 syscall 不会被重新执行。
pub(super) fn rt_sigtimedwait(
    cx: &mut Cx<'_>,
    set: u64,
    info: u64,
    timeout: u64,
    size: usize,
) -> SysResult {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let want = u64::from_le_bytes(get(cx, set)?) & !UNBLOCKABLE;
    let deadline = match timeout {
        0 => None,
        t => Some(Instant::now() + read_timespec(cx, t)?),
    };
    loop {
        if let Some((sig, si)) = signal::take(cx, want) {
            if info != 0 {
                put(cx, info, si.encode(sig))?;
            }
            return Ok(sig.number() as u64);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::EINTR);
        }
        // 被屏蔽的信号挂起时请求 wake::SIGNAL；没有屏蔽的直接叫醒这个线程
        cx.wait_on(deadline, &mut [], wake::SIGNAL)?;
    }
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
