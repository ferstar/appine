//! 信号（设计文档 §7）：产生、投递、信号帧与 rt_sigreturn。
//!
//! 每个线程有自己的挂起集合和屏蔽字，发给整个进程的信号挂在线程组上，由第一个
//! 没有屏蔽它的线程处理。处理方式（[`SigHandlers`]）属于线程组。信号在返回
//! guest 之前投递：syscall 之后、被 kick 打断之后、同步异常之后。
//!
//! 信号帧逐字节照抄 Linux arm64 的 `struct rt_sigframe`：siginfo、ucontext
//! （sigcontext 后面是 fpsimd_context，同步异常时再加 esr_context，以空记录结尾），
//! 帧上方是 `{fp, lr}` 帧记录。syscall 重启的规则照搬 arm64 的 `do_signal`。

use std::cell::RefCell;
use std::rc::Rc;

use appine_engine::Cpu;

use crate::errno::Errno;
use crate::kernel::{Cx, Kernel, Tid, request_wake};
use crate::process::{ExitStatus, Signal, ThreadGroup};
use crate::syscall::Flow;
use crate::task::Task;
use crate::uaccess::UserSlice;

/// sigaction 的 flags。
pub(crate) mod sa {
    /// 子进程停止和继续时不发 SIGCHLD。
    pub const NOCLDSTOP: u64 = 0x1;
    /// 子进程退出时不变成僵尸。
    pub const NOCLDWAIT: u64 = 0x2;
    /// 处理函数接收 siginfo 和 ucontext。
    pub const SIGINFO: u64 = 0x4;
    /// `sa_restorer` 有效。
    pub const RESTORER: u64 = 0x0400_0000;
    /// 在备用栈上运行处理函数。
    pub const ONSTACK: u64 = 0x0800_0000;
    /// 被打断的 syscall 自动重新执行。
    pub const RESTART: u64 = 0x1000_0000;
    /// 处理函数运行期间不屏蔽本信号。
    pub const NODEFER: u64 = 0x4000_0000;
    /// 投递一次之后恢复默认处理。
    pub const RESETHAND: u64 = 0x8000_0000;
}

/// 一个信号的处理方式（`struct k_sigaction` 在 arm64 上的布局）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    /// `SIG_DFL`。
    pub const DEFAULT: u64 = 0;
    /// `SIG_IGN`。
    pub const IGNORE: u64 = 1;
}

/// 一个线程组的信号处理方式（`CLONE_SIGHAND` 可以让几个线程组共用）。
#[derive(Debug, Clone)]
pub(crate) struct SigHandlers(pub [SigAction; Signal::MAX as usize]);

impl Default for SigHandlers {
    fn default() -> Self {
        SigHandlers([SigAction::default(); Signal::MAX as usize])
    }
}

impl SigHandlers {
    /// 信号的处理方式。
    pub fn get(&self, sig: Signal) -> SigAction {
        self.0[sig.index()]
    }

    /// 修改信号的处理方式。
    pub fn set(&mut self, sig: Signal, act: SigAction) {
        self.0[sig.index()] = act;
    }

    /// 信号当前是否会被丢弃。
    pub fn ignores(&self, sig: Signal) -> bool {
        match self.get(sig).handler {
            SigAction::IGNORE => true,
            SigAction::DEFAULT => sig.ignored_by_default(),
            _ => false,
        }
    }

    /// execve：装了处理函数的信号恢复默认，忽略的信号保持忽略。
    pub fn reset_on_exec(&mut self) {
        for act in &mut self.0 {
            let ignored = act.handler == SigAction::IGNORE;
            *act = SigAction::default();
            if ignored {
                act.handler = SigAction::IGNORE;
            }
        }
    }
}

/// siginfo 的 si_code。
pub(crate) mod code {
    /// kill(2)。
    pub const SI_USER: i32 = 0;
    /// 内核产生。
    pub const SI_KERNEL: i32 = 0x80;
    /// tkill(2)、tgkill(2)。
    pub const SI_TKILL: i32 = -6;
    /// 子进程正常退出。
    pub const CLD_EXITED: i32 = 1;
    /// 子进程被信号终止。
    pub const CLD_KILLED: i32 = 2;
    /// 子进程停止。
    pub const CLD_STOPPED: i32 = 5;
    /// 停止的子进程继续。
    pub const CLD_CONTINUED: i32 = 6;
    /// 地址没有映射。
    pub const SEGV_MAPERR: i32 = 1;
    /// 地址没有相应的权限。
    pub const SEGV_ACCERR: i32 = 2;
    /// 地址未对齐。
    pub const BUS_ADRALN: i32 = 1;
    /// 非法指令。
    pub const ILL_ILLOPC: i32 = 1;
    /// 断点。
    pub const TRAP_BRKPT: i32 = 1;
}

/// siginfo 里随 si_code 变化的字段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum InfoFields {
    /// 没有额外字段。
    #[default]
    None,
    /// kill、tkill：发送者。
    Kill { pid: Tid, uid: u32 },
    /// SIGCHLD：子进程和它的状态。
    Child { pid: Tid, uid: u32, status: i32 },
    /// 同步异常：出错地址。
    Fault { addr: u64 },
}

/// 一个挂起信号附带的 siginfo。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SigInfo {
    pub code: i32,
    pub fields: InfoFields,
    /// 同步异常的 ESR，放进信号帧的 esr_context；0 表示不是同步异常。
    pub esr: u64,
}

impl SigInfo {
    /// kill(2) 或 tkill(2) 发出的信号。
    pub fn kill(code: i32, pid: Tid) -> SigInfo {
        SigInfo {
            code,
            fields: InfoFields::Kill { pid, uid: 0 },
            esr: 0,
        }
    }

    /// 内核自己产生的信号（比如 SIGPIPE）。
    pub fn kernel() -> SigInfo {
        SigInfo {
            code: code::SI_KERNEL,
            ..SigInfo::default()
        }
    }

    /// 同步异常。
    pub fn fault(code: i32, addr: u64, esr: u64) -> SigInfo {
        SigInfo {
            code,
            fields: InfoFields::Fault { addr },
            esr,
        }
    }

    /// 子进程退出时发给父进程的 SIGCHLD。
    pub fn child(pid: Tid, status: ExitStatus) -> SigInfo {
        let (code, status) = match status {
            ExitStatus::Exited(c) => (code::CLD_EXITED, c),
            ExitStatus::Signaled { signal, .. } => (code::CLD_KILLED, signal.number()),
        };
        Self::child_event(pid, code, status)
    }

    /// 子进程状态改变（`CLD_*`）时发给父进程的 SIGCHLD。
    pub fn child_event(pid: Tid, code: i32, status: i32) -> SigInfo {
        SigInfo {
            code,
            fields: InfoFields::Child {
                pid,
                uid: 0,
                status,
            },
            esr: 0,
        }
    }

    /// 按 Linux 的 `siginfo_t`（128 字节）编码。
    pub fn encode(&self, sig: Signal) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[..4].copy_from_slice(&sig.number().to_le_bytes());
        b[8..12].copy_from_slice(&self.code.to_le_bytes());
        match self.fields {
            InfoFields::None => {}
            InfoFields::Kill { pid, uid } => {
                b[16..20].copy_from_slice(&pid.to_le_bytes());
                b[20..24].copy_from_slice(&uid.to_le_bytes());
            }
            InfoFields::Child { pid, uid, status } => {
                b[16..20].copy_from_slice(&pid.to_le_bytes());
                b[20..24].copy_from_slice(&uid.to_le_bytes());
                b[24..28].copy_from_slice(&status.to_le_bytes());
            }
            InfoFields::Fault { addr } => b[16..24].copy_from_slice(&addr.to_le_bytes()),
        }
        b
    }

    fn fault_addr(&self) -> Option<u64> {
        match self.fields {
            InfoFields::Fault { addr } => Some(addr),
            _ => None,
        }
    }
}

/// sigaltstack 的 ss_flags。
pub(crate) mod ss {
    /// 正在备用栈上运行。
    pub const ONSTACK: u32 = 1;
    /// 备用栈未启用。
    pub const DISABLE: u32 = 2;
    /// 切换到备用栈时自动停用它。
    pub const AUTODISARM: u32 = 1 << 31;
}

/// arm64 uapi 的 `MINSIGSTKSZ`：sigaltstack 接受的最小备用栈。
pub(crate) const MINSIGSTKSZ: u64 = 5120;

/// 一个线程的备用信号栈。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AltStack {
    pub sp: u64,
    pub size: u64,
    /// 只含 `SS_DISABLE` 和 `SS_AUTODISARM`。
    pub flags: u32,
}

impl Default for AltStack {
    fn default() -> Self {
        AltStack {
            sp: 0,
            size: 0,
            flags: ss::DISABLE,
        }
    }
}

impl AltStack {
    /// `sp` 是否在备用栈上（Linux 的 `on_sig_stack`）。
    pub fn on_stack(&self, sp: u64) -> bool {
        if self.flags & ss::AUTODISARM != 0 {
            return false;
        }
        sp > self.sp && sp - self.sp <= self.size
    }

    /// 报告给 guest 的 ss_flags（Linux 的 `sas_ss_flags` 加上 `SS_AUTODISARM`）。
    pub fn reported_flags(&self, sp: u64) -> u32 {
        let state = if self.size == 0 {
            ss::DISABLE
        } else if self.on_stack(sp) {
            ss::ONSTACK
        } else {
            0
        };
        state | (self.flags & ss::AUTODISARM)
    }

    /// 按 sigaltstack(2) 的规则修改：返回 Err 时保持不变。
    pub fn change(&mut self, cur_sp: u64, sp: u64, flags: u32, size: u64) -> Result<(), Errno> {
        if self.on_stack(cur_sp) {
            return Err(Errno::EPERM);
        }
        let mode = flags & !ss::AUTODISARM;
        match mode {
            ss::DISABLE => *self = AltStack::default(),
            0 | ss::ONSTACK => {
                if size < MINSIGSTKSZ {
                    return Err(Errno::ENOMEM);
                }
                *self = AltStack {
                    sp,
                    size,
                    flags: flags & ss::AUTODISARM,
                };
            }
            _ => return Err(Errno::EINVAL),
        }
        Ok(())
    }
}

// ---- 信号的产生 ----

/// 让线程尽快进入内核：打断正在执行的 guest 代码，唤醒正在等待的线程。
pub(crate) fn wake(task: &Task) {
    // SAFETY: 线程在销毁 Cpu 之前（持有 BKL）从 kernel.tasks 里移除，这里也持有 BKL
    unsafe { task.sync.kicker.kick() };
    task.event.signal();
}

/// 结束整个线程组（exit_group、致命信号、execve 之外的 SIGKILL）：记录退出状态，
/// 让每个线程在下一次进入内核时退出。
pub(crate) fn group_exit(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>, status: ExitStatus) {
    let mut g = group.borrow_mut();
    if g.exiting.is_none() {
        g.exiting = Some(status);
    }
    for tid in &g.threads {
        if let Some(t) = k.tasks.get(tid) {
            t.killed.set(true);
            wake(t);
        }
    }
}

fn killed_by(sig: Signal) -> ExitStatus {
    ExitStatus::Signaled {
        signal: sig,
        pc: None,
        fault_addr: None,
    }
}

/// 停止信号：SIGSTOP、SIGTSTP、SIGTTIN、SIGTTOU。
const STOP_SIGNALS: u64 = (1 << (19 - 1)) | (1 << (20 - 1)) | (1 << (21 - 1)) | (1 << (22 - 1));

/// 产生信号时立即生效的部分（Linux 的 `prepare_signal`）：停止信号丢掉挂起的 SIGCONT；
/// SIGCONT 丢掉挂起的停止信号，并让停住的线程组继续，不管它是否被屏蔽或忽略。
fn prepare(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>, sig: Signal) {
    let clear = |mask: u64| {
        let mut g = group.borrow_mut();
        g.pending &= !mask;
        for t in g.threads.iter().filter_map(|t| k.tasks.get(t)) {
            t.pending.set(t.pending.get() & !mask);
        }
    };
    if sig.is_stop() {
        clear(Signal::CONT.bit());
    } else if sig == Signal::CONT {
        clear(STOP_SIGNALS);
        let resumed = {
            let mut g = group.borrow_mut();
            let was = std::mem::take(&mut g.stopped);
            if was {
                g.stop_report = None;
                g.cont_report = true;
            }
            was
        };
        if resumed {
            notify_parent(k, group, code::CLD_CONTINUED, Signal::CONT.number());
            request_wake();
        }
    }
}

/// 子进程停止或继续时通知父进程：唤醒在 wait 的父进程，没有设置 `SA_NOCLDSTOP` 时发 SIGCHLD。
fn notify_parent(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>, code: i32, status: i32) {
    let (tgid, ppid) = {
        let g = group.borrow();
        (g.tgid, g.ppid)
    };
    request_wake();
    let Some(parent) = k.groups.get(&ppid) else {
        return;
    };
    let act = parent.borrow().sighand.borrow().get(Signal::CHLD);
    if act.flags & sa::NOCLDSTOP == 0 {
        send_to_group(
            k,
            ppid,
            Signal::CHLD,
            SigInfo::child_event(tgid, code, status),
        );
    }
}

/// 给一个线程发信号（tkill、SIGPIPE 等）。
pub(crate) fn send_to_task(k: &Kernel, task: &Task, sig: Signal, info: SigInfo) {
    let Some(group) = k.groups.get(&task.tgid).cloned() else {
        return;
    };
    if sig == Signal::KILL {
        group_exit(k, &group, killed_by(sig));
        return;
    }
    prepare(k, &group, sig);
    let blocked = task.sigmask.get() & sig.bit() != 0;
    // 被屏蔽的信号即使当前被忽略也要挂起：解除屏蔽之前处理方式可能会变
    if !blocked && group.borrow().sighand.borrow().ignores(sig) {
        return;
    }
    task.pending.set(task.pending.get() | sig.bit());
    task.info.borrow_mut()[sig.index()] = info;
    if !blocked {
        wake(task);
    }
}

/// 给整个线程组发信号（kill）。目标是僵尸进程时什么都不做。
pub(crate) fn send_to_group(k: &Kernel, tgid: Tid, sig: Signal, info: SigInfo) {
    let Some(group) = k.groups.get(&tgid).cloned() else {
        return;
    };
    let threads: Vec<Rc<Task>> = group
        .borrow()
        .threads
        .iter()
        .filter_map(|t| k.tasks.get(t).cloned())
        .collect();
    let Some(leader) = threads.first() else {
        return;
    };
    if sig == Signal::KILL {
        group_exit(k, &group, killed_by(sig));
        return;
    }
    prepare(k, &group, sig);
    let ignored = group.borrow().sighand.borrow().ignores(sig);
    if ignored && leader.sigmask.get() & sig.bit() == 0 {
        return;
    }
    {
        let mut g = group.borrow_mut();
        g.pending |= sig.bit();
        g.info[sig.index()] = info;
    }
    if let Some(t) = threads.iter().find(|t| t.sigmask.get() & sig.bit() == 0) {
        wake(t);
    }
}

/// 给进程组里的每个进程发信号（终端产生的信号、kill 负数 pid）。
pub(crate) fn send_to_pgrp(k: &Kernel, pgid: Tid, sig: Signal, info: SigInfo) {
    let members: Vec<Tid> = k
        .groups
        .values()
        .filter(|g| g.borrow().pgid == pgid)
        .map(|g| g.borrow().tgid)
        .collect();
    for tgid in members {
        send_to_group(k, tgid, sig, info);
    }
}

/// 孤儿进程组：组里没有一个进程的父进程在同一会话的另一个进程组里。这样的进程组
/// 被停住之后没有人能让它继续，所以终端产生的停止信号对它无效。
pub(crate) fn pgrp_orphaned(k: &Kernel, pgid: Tid) -> bool {
    !k.groups.values().any(|g| {
        let g = g.borrow();
        g.pgid == pgid
            && g.zombie.is_none()
            && k.groups.get(&g.ppid).is_some_and(|p| {
                let p = p.borrow();
                p.pgid != pgid && p.sid == g.sid
            })
    })
}

/// 同步异常：信号被屏蔽或被忽略时恢复默认处理并解除屏蔽（Linux 的 `force_sig_info`），
/// 然后挂到当前线程上。
pub(crate) fn force(cx: &mut Cx<'_>, sig: Signal, info: SigInfo) {
    let blocked = cx.task.sigmask.get() & sig.bit() != 0;
    {
        let group = cx.t.group.borrow();
        let mut sh = group.sighand.borrow_mut();
        let mut act = sh.get(sig);
        if blocked || act.handler == SigAction::IGNORE {
            act.handler = SigAction::DEFAULT;
            sh.set(sig, act);
        }
    }
    if blocked {
        cx.task.sigmask.set(cx.task.sigmask.get() & !sig.bit());
    }
    cx.task.pending.set(cx.task.pending.get() | sig.bit());
    cx.task.info.borrow_mut()[sig.index()] = info;
}

// ---- 信号的投递 ----

/// 同步异常产生的信号优先投递（Linux 的 `SYNCHRONOUS_MASK`）。
const SYNCHRONOUS: u64 = (1 << (11 - 1)) // SIGSEGV
    | (1 << (7 - 1)) // SIGBUS
    | (1 << (4 - 1)) // SIGILL
    | (1 << (5 - 1)) // SIGTRAP
    | (1 << (8 - 1)) // SIGFPE
    | (1 << (31 - 1)); // SIGSYS

fn pick(set: u64) -> Signal {
    let sync = set & SYNCHRONOUS;
    let set = if sync != 0 { sync } else { set };
    Signal::new(set.trailing_zeros() as i32 + 1).unwrap()
}

/// 取出下一个没有被屏蔽的挂起信号：先看线程自己的，再看线程组的。
fn dequeue(cx: &mut Cx<'_>) -> Option<(Signal, SigInfo)> {
    let mask = cx.task.sigmask.get();
    let own = cx.task.pending.get() & !mask;
    if own != 0 {
        let sig = pick(own);
        cx.task.pending.set(cx.task.pending.get() & !sig.bit());
        return Some((sig, cx.task.info.borrow()[sig.index()]));
    }
    let mut g = cx.t.group.borrow_mut();
    let shared = g.pending & !mask;
    if shared != 0 {
        let sig = pick(shared);
        g.pending &= !sig.bit();
        return Some((sig, g.info[sig.index()]));
    }
    None
}

/// 有没有可以投递的信号，线程组是否被停住，或者线程已经被要求退出（等待中的 syscall
/// 据此提前返回）。
pub(crate) fn interrupted(cx: &Cx<'_>) -> bool {
    let mask = cx.task.sigmask.get();
    let g = cx.t.group.borrow();
    cx.task.killed.get() || g.stopped || (cx.task.pending.get() | g.pending) & !mask != 0
}

/// 停住整个线程组（停止信号的默认动作）：通知父进程，打断其他线程，让它们也停下来。
fn group_stop(cx: &mut Cx<'_>, sig: Signal) {
    let group = cx.t.group.clone();
    let threads = {
        let mut g = group.borrow_mut();
        g.stopped = true;
        g.stop_report = Some(sig);
        g.cont_report = false;
        g.threads.clone()
    };
    for t in threads.iter().filter_map(|t| cx.kr().tasks.get(t)) {
        if !Rc::ptr_eq(t, cx.task) {
            wake(t);
        }
    }
    notify_parent(cx.kr(), &group, code::CLD_STOPPED, sig.number());
}

/// 线程组停住期间，在这里等到 SIGCONT 或者被结束。
fn wait_while_stopped(cx: &mut Cx<'_>) -> Flow {
    loop {
        if cx.task.killed.get() {
            return Flow::Exit;
        }
        if !cx.t.group.borrow().stopped {
            return Flow::Continue;
        }
        // 只有 SIGCONT、SIGKILL 能让它离开这里，所以不检查其他信号
        let _ = cx.wait(None, &mut []);
    }
}

/// 返回 guest 之前：处理 syscall 重启，投递所有可以投递的信号。
///
/// `syscall` 为 true 表示刚处理完一个 syscall，x0 是它的返回值。
pub(crate) fn deliver(cx: &mut Cx<'_>, cpu: &mut Cpu, syscall: bool) -> Flow {
    if cx.task.killed.get() {
        return Flow::Exit;
    }
    // (重启码, syscall 之后的 pc)：先按"重新执行"准备好，有处理函数时再按规则改回 EINTR
    let mut restart = None;
    if syscall {
        let st = cpu.state_mut();
        let ret = st.x[0] as i64;
        if [
            Errno::ERESTARTSYS,
            Errno::ERESTARTNOINTR,
            Errno::ERESTARTNOHAND,
        ]
        .iter()
        .any(|e| ret == -(e.0 as i64))
        {
            restart = Some((Errno(-ret as i32), st.pc));
            st.x[0] = st.orig_x0;
            st.pc -= 4;
        }
    }
    // 线程组被别的线程停住了：在返回 guest 之前一起停下
    if let Flow::Exit = wait_while_stopped(cx) {
        return Flow::Exit;
    }

    let mut handled = false;
    while let Some((sig, info)) = dequeue(cx) {
        let act = cx.t.group.borrow().sighand.borrow().get(sig);
        match act.handler {
            SigAction::IGNORE => continue,
            SigAction::DEFAULT if sig.ignored_by_default() => continue,
            SigAction::DEFAULT if sig.is_stop() => {
                // 孤儿进程组不响应终端产生的停止信号（SIGSTOP 除外）
                let pgid = cx.t.group.borrow().pgid;
                if sig != Signal::STOP && pgrp_orphaned(cx.kr(), pgid) {
                    continue;
                }
                group_stop(cx, sig);
                if let Flow::Exit = wait_while_stopped(cx) {
                    return Flow::Exit;
                }
                continue;
            }
            SigAction::DEFAULT => {
                let status = ExitStatus::Signaled {
                    signal: sig,
                    pc: (info.esr != 0).then_some(cpu.state().pc),
                    fault_addr: info
                        .fault_addr()
                        .filter(|_| sig == Signal::SEGV || sig == Signal::BUS),
                };
                let group = cx.t.group.clone();
                group_exit(cx.k(), &group, status);
                return Flow::Exit;
            }
            _ => {}
        }
        if let (false, Some((code, cont))) = (handled, restart) {
            let to_eintr = code == Errno::ERESTARTNOHAND
                || (code == Errno::ERESTARTSYS && act.flags & sa::RESTART == 0);
            if to_eintr {
                let st = cpu.state_mut();
                st.x[0] = Errno::EINTR.to_syscall_return();
                st.pc = cont;
            }
        }
        // sigsuspend、ppoll 临时换掉的屏蔽字：帧里保存原来的，处理函数返回后恢复
        let saved = cx.t.saved_sigmask.take().unwrap_or(cx.task.sigmask.get());
        if setup_frame(cx, cpu, sig, &info, &act, saved).is_err() {
            crate::debug::log(format_args!("cannot set up the frame for {sig}"));
            let group = cx.t.group.clone();
            group_exit(cx.k(), &group, killed_by(Signal::SEGV));
            return Flow::Exit;
        }
        let mut mask = cx.task.sigmask.get() | act.mask;
        if act.flags & sa::NODEFER == 0 {
            mask |= sig.bit();
        }
        cx.task.sigmask.set(mask & !UNBLOCKABLE);
        if act.flags & sa::RESETHAND != 0 {
            let group = cx.t.group.borrow();
            group.sighand.borrow_mut().set(sig, SigAction::default());
        }
        handled = true;
    }
    if !handled && let Some(saved) = cx.t.saved_sigmask.take() {
        cx.task.sigmask.set(saved);
    }
    Flow::Continue
}

/// SIGKILL 和 SIGSTOP 不能被屏蔽。
pub(crate) const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

// ---- 信号帧 ----

const UC: usize = 128;
const UC_STACK: usize = UC + 16;
const UC_SIGMASK: usize = UC + 40;
const MCONTEXT: usize = UC + 176;
const FAULT_ADDRESS: usize = MCONTEXT;
const REGS: usize = MCONTEXT + 8;
const SP: usize = MCONTEXT + 256;
const PC: usize = MCONTEXT + 264;
const PSTATE: usize = MCONTEXT + 272;
const RESERVED: usize = MCONTEXT + 288;
const RESERVED_SIZE: usize = 4096;
/// `sizeof(struct rt_sigframe)`。
const FRAME_SIZE: usize = RESERVED + RESERVED_SIZE;
/// 帧上方的 `struct frame_record`。
const FRAME_RECORD: usize = 16;
/// 宣告给 guest 的 `AT_MINSIGSTKSZ`：帧、帧记录，再加上最多 16 字节的对齐，
/// 与没有 SVE/SME 的 arm64 Linux 算出来的值相同。
pub(crate) const MIN_SIGSTKSZ: u64 = (FRAME_SIZE + FRAME_RECORD + 16) as u64;

const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: usize = 528;
const ESR_MAGIC: u32 = 0x4553_5201;
const ESR_SIZE: usize = 16;
/// pstate 里 guest 可以看到和恢复的位：NZCV。
const PSTATE_NZCV: u64 = 0xf000_0000;
/// pstate 的异常级别和 AArch32 位：信号帧里必须是 EL0t、AArch64。
const PSTATE_MODE: u64 = 0x1f;

fn w32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn w64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn r64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// 在 guest 栈上构造信号帧，并让 CPU 从处理函数开始执行。
fn setup_frame(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    sig: Signal,
    info: &SigInfo,
    act: &SigAction,
    saved_mask: u64,
) -> Result<(), Errno> {
    let alt = cx.t.altstack;
    let st = cpu.state_mut();
    let mut top = st.sp;
    if act.flags & sa::ONSTACK != 0 && alt.size != 0 && !alt.on_stack(st.sp) {
        top = alt.sp.wrapping_add(alt.size);
    }
    let record = top.checked_sub(FRAME_RECORD as u64).ok_or(Errno::EFAULT)? & !15;
    let frame = record.checked_sub(FRAME_SIZE as u64).ok_or(Errno::EFAULT)?;

    let mut b = vec![0u8; FRAME_SIZE + FRAME_RECORD];
    b[..128].copy_from_slice(&info.encode(sig));
    // uc_flags、uc_link 为 0
    w64(&mut b, UC_STACK, alt.sp);
    w32(&mut b, UC_STACK + 8, alt.reported_flags(st.sp));
    w64(&mut b, UC_STACK + 16, alt.size);
    w64(&mut b, UC_SIGMASK, saved_mask);
    w64(&mut b, FAULT_ADDRESS, info.fault_addr().unwrap_or(0));
    for (i, &x) in st.x.iter().enumerate() {
        w64(&mut b, REGS + 8 * i, x);
    }
    w64(&mut b, SP, st.sp);
    w64(&mut b, PC, st.pc);
    w64(&mut b, PSTATE, st.nzcv & PSTATE_NZCV);
    // __reserved：fpsimd_context，可选的 esr_context，最后是空记录
    let mut off = RESERVED;
    w32(&mut b, off, FPSIMD_MAGIC);
    w32(&mut b, off + 4, FPSIMD_SIZE as u32);
    w32(&mut b, off + 8, st.fpsr as u32);
    w32(&mut b, off + 12, st.fpcr as u32);
    for (i, v) in st.v.iter().enumerate() {
        w64(&mut b, off + 16 + 16 * i, v[0]);
        w64(&mut b, off + 24 + 16 * i, v[1]);
    }
    off += FPSIMD_SIZE;
    if info.esr != 0 {
        w32(&mut b, off, ESR_MAGIC);
        w32(&mut b, off + 4, ESR_SIZE as u32);
        w64(&mut b, off + 8, info.esr);
    }
    // 帧记录：让栈回溯能越过信号帧
    w64(&mut b, FRAME_SIZE, st.x[29]);
    w64(&mut b, FRAME_SIZE + 8, st.x[30]);
    cx.mm().write(UserSlice::new(frame, b.len()), &b)?;

    if alt.flags & ss::AUTODISARM != 0 {
        cx.t.altstack = AltStack::default();
    }
    let restorer = if act.flags & sa::RESTORER != 0 {
        act.restorer
    } else {
        cx.mm().vdso + crate::vdso::sigreturn_offset()
    };
    st.x[0] = sig.number() as u64;
    if act.flags & sa::SIGINFO != 0 {
        st.x[1] = frame;
        st.x[2] = frame + UC as u64;
    }
    st.sp = frame;
    st.x[29] = record;
    st.x[30] = restorer;
    st.pc = act.handler;
    Ok(())
}

/// rt_sigreturn：从栈上的信号帧恢复寄存器、屏蔽字和备用栈。帧无效时以 SIGSEGV 终止。
pub(crate) fn sigreturn(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Flow {
    if restore_frame(cx, cpu).is_err() {
        crate::debug::log(format_args!("rt_sigreturn: bad signal frame"));
        let group = cx.t.group.clone();
        group_exit(cx.k(), &group, killed_by(Signal::SEGV));
        return Flow::Exit;
    }
    // 恢复的屏蔽字可能解除了对挂起信号的屏蔽；x0 是恢复出来的值，不按 syscall 返回值处理
    deliver(cx, cpu, false)
}

fn restore_frame(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Result<(), Errno> {
    let frame = cpu.state().sp;
    if !frame.is_multiple_of(16) {
        return Err(Errno::EFAULT);
    }
    let mut b = vec![0u8; FRAME_SIZE];
    cx.mm().read(UserSlice::new(frame, FRAME_SIZE), &mut b)?;
    let pstate = r64(&b, PSTATE);
    if pstate & PSTATE_MODE != 0 {
        return Err(Errno::EINVAL);
    }
    // 先解析 __reserved 里的记录，确认整个帧有效之后再修改任何状态
    let mut fpsimd = None;
    let mut off = 0;
    loop {
        if off + 8 > RESERVED_SIZE {
            return Err(Errno::EINVAL);
        }
        let magic = r32(&b, RESERVED + off);
        let size = r32(&b, RESERVED + off + 4) as usize;
        if magic == 0 && size == 0 {
            break;
        }
        if size < 8 || !size.is_multiple_of(16) || off + size > RESERVED_SIZE {
            return Err(Errno::EINVAL);
        }
        match magic {
            FPSIMD_MAGIC if size == FPSIMD_SIZE && fpsimd.is_none() => {
                fpsimd = Some(RESERVED + off);
            }
            ESR_MAGIC => {}
            _ => return Err(Errno::EINVAL),
        }
        off += size;
    }
    let fp = fpsimd.ok_or(Errno::EINVAL)?;

    let st = cpu.state_mut();
    for (i, x) in st.x.iter_mut().enumerate() {
        *x = r64(&b, REGS + 8 * i);
    }
    st.sp = r64(&b, SP);
    st.pc = r64(&b, PC);
    st.nzcv = pstate & PSTATE_NZCV;
    st.fpsr = r32(&b, fp + 8) as u64;
    st.fpcr = r32(&b, fp + 12) as u64;
    for (i, v) in st.v.iter_mut().enumerate() {
        v[0] = r64(&b, fp + 16 + 16 * i);
        v[1] = r64(&b, fp + 24 + 16 * i);
    }
    let sp = st.sp;
    cx.task.sigmask.set(r64(&b, UC_SIGMASK) & !UNBLOCKABLE);
    // 与 Linux 的 restore_altstack 一样，修改备用栈失败时保持原样
    let _ = cx.t.altstack.change(
        sp,
        r64(&b, UC_STACK),
        r32(&b, UC_STACK + 8),
        r64(&b, UC_STACK + 16),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_matches_linux_arm64() {
        // arch/arm64/include/uapi/asm/{sigcontext,ucontext}.h
        assert_eq!(MCONTEXT, 304);
        assert_eq!(RESERVED, 592);
        assert_eq!(FRAME_SIZE, 4688);
        assert_eq!(MIN_SIGSTKSZ, 4720);
        assert_eq!(FRAME_SIZE % 16, 0);
    }

    #[test]
    fn altstack_rules() {
        let mut alt = AltStack::default();
        assert_eq!(alt.reported_flags(0x1000), ss::DISABLE);
        assert_eq!(alt.change(0, 0x10000, 0, 4096), Err(Errno::ENOMEM));
        assert_eq!(alt.change(0, 0x10000, 7, 8192), Err(Errno::EINVAL));
        alt.change(0, 0x10000, 0, 8192).unwrap();
        assert!(alt.on_stack(0x11000));
        assert!(!alt.on_stack(0x10000));
        assert_eq!(alt.reported_flags(0x11000), ss::ONSTACK);
        assert_eq!(alt.change(0x11000, 0, ss::DISABLE, 0), Err(Errno::EPERM));
        alt.change(0x1_0000_0000, 0, ss::DISABLE, 0).unwrap();
        assert_eq!(alt, AltStack::default());
    }

    #[test]
    fn siginfo_encoding() {
        let b = SigInfo::fault(code::SEGV_MAPERR, 0x1000, 0).encode(Signal::SEGV);
        assert_eq!(&b[..4], &11i32.to_le_bytes());
        assert_eq!(&b[8..12], &1i32.to_le_bytes());
        assert_eq!(&b[16..24], &0x1000u64.to_le_bytes());
        let b = SigInfo::child(42, ExitStatus::Exited(3)).encode(Signal::CHLD);
        assert_eq!(&b[16..20], &42i32.to_le_bytes());
        assert_eq!(&b[24..28], &3i32.to_le_bytes());
    }

    #[test]
    fn synchronous_signals_come_first() {
        let set = Signal::INT.bit() | Signal::SEGV.bit() | Signal::HUP.bit();
        assert_eq!(pick(set), Signal::SEGV);
        assert_eq!(pick(Signal::INT.bit() | Signal::TERM.bit()), Signal::INT);
    }
}
