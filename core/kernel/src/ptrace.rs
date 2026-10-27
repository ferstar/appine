//! ptrace 里被跟踪的一方（设计文档 §7.6）：停在 ptrace-stop 里，等跟踪者读写它的寄存器和
//! 内存，再让它继续。跟踪者一方的请求在 `syscall::ptrace`。
//!
//! 被跟踪的是线程，跟踪者记的是进程（tgid）。Linux 只接受跟踪者线程自己发的请求，wait
//! 则线程组里的哪个线程都可以；appine 不记 fork 的是哪个线程（PTRACE_TRACEME 的跟踪者是
//! 父进程），所以放宽到跟踪者进程里的任何线程。
//!
//! 被跟踪的线程在这些地方停下来，由 [`stop`] 通知跟踪者：
//!
//! - 投递信号之前（信号投递停止，[`signal_stop`]）：跟踪者可以换掉信号、改它的 siginfo，
//!   或者不投递；
//! - 线程组被停止信号停住时（group-stop，[`group_stop`]）。被跟踪的线程不在 group-stop 里
//!   一直停着，跟踪者让它继续它就继续执行，即使线程组仍然停着，与 Linux 一样；
//! - PTRACE_SYSCALL 让它继续之后，系统调用的入口和出口（[`syscall_entry`]、
//!   [`syscall_exit`]）；
//! - `PTRACE_EVENT_*`（[`event`]）：fork、vfork、clone、exec、vfork 结束、退出，以及
//!   PTRACE_INTERRUPT 和 PTRACE_SEIZE 跟进来的新线程的 `PTRACE_EVENT_STOP`（[`trap`]）。
//!
//! 停住时把寄存器存一份在 [`Trace::regs`]，跟踪者读写的是这一份，继续时写回 Cpu。停住期间
//! 不处理信号，只有 SIGKILL 让它离开（Linux 的 TASK_TRACED）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use appine_engine::{Cpu, CpuState};

use crate::kernel::{Cx, Kernel, Tid, request_wake_on, wake};
use crate::process::{ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, SigAction, SigInfo, code, sa};
use crate::task::Task;

/// `PTRACE_EVENT_*`。
pub(crate) mod event {
    pub const FORK: u32 = 1;
    pub const VFORK: u32 = 2;
    pub const CLONE: u32 = 3;
    pub const EXEC: u32 = 4;
    pub const VFORK_DONE: u32 = 5;
    pub const EXIT: u32 = 6;
    pub const SECCOMP: u32 = 7;
    pub const STOP: u32 = 128;
}

/// `PTRACE_O_*`。`PTRACE_EVENT_x` 对应的选项是 `1 << x`。
pub(crate) mod opt {
    pub const TRACESYSGOOD: u32 = 1;
    pub const EXITKILL: u32 = 1 << 20;
    pub const SUSPEND_SECCOMP: u32 = 1 << 21;
    /// 认得的选项（Linux 的 `PTRACE_O_MASK`）。
    pub const MASK: u32 = 0xff | EXITKILL | SUSPEND_SECCOMP;
}

/// PTRACE_GETEVENTMSG 在系统调用入口、出口停住时的值。
pub(crate) const EVENTMSG_SYSCALL_ENTRY: u64 = 1;
pub(crate) const EVENTMSG_SYSCALL_EXIT: u64 = 2;

/// 停住的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// 投递信号之前。
    Signal(Signal),
    /// group-stop（不是 PTRACE_SEIZE 跟踪的；SEIZE 的报告成 `PTRACE_EVENT_STOP`）。
    Group(Signal),
    /// 系统调用的入口（true）或出口。
    Syscall(bool),
    /// `PTRACE_EVENT_*` 和 WSTOPSIG：`PTRACE_EVENT_STOP` 的是 SIGTRAP 或者停止信号，其余
    /// 的是 SIGTRAP。
    Event(u32, Signal),
}

/// 跟踪者让线程怎样继续。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    /// PTRACE_CONT：一直执行到下一次停下。
    #[default]
    Cont,
    /// PTRACE_SYSCALL：在系统调用的入口和出口停下。
    Syscall,
    /// PTRACE_SINGLESTEP：执行一条指令就停下（[`step_done`]）。
    Step,
}

/// 停住时的寄存器。跟踪者读写这一份，继续时写回 Cpu。
#[derive(Clone)]
pub(crate) struct Regs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    /// 只有 NZCV。
    pub nzcv: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub tpidr: u64,
    pub v: [[u64; 2]; 32],
    /// 系统调用的原始 x0（PTRACE_GET_SYSCALL_INFO 报告的第一个参数），只读。
    pub orig_x0: u64,
}

impl Regs {
    pub(crate) fn save(s: &CpuState) -> Regs {
        Regs {
            x: s.x,
            sp: s.sp,
            pc: s.pc,
            nzcv: s.nzcv,
            fpcr: s.fpcr,
            fpsr: s.fpsr,
            tpidr: s.tpidr_el0,
            v: s.v,
            orig_x0: s.orig_x0,
        }
    }

    fn restore(&self, s: &mut CpuState) {
        s.x = self.x;
        s.sp = self.sp;
        s.pc = self.pc;
        s.nzcv = self.nzcv & PSTATE_NZCV;
        s.fpcr = self.fpcr;
        s.fpsr = self.fpsr;
        s.tpidr_el0 = self.tpidr;
        s.v = self.v;
    }
}

/// pstate 里跟踪者能改的位：NZCV。
pub(crate) const PSTATE_NZCV: u64 = 0xf000_0000;

/// 线程的 ptrace 状态（[`Task::trace`]），只在持有 BKL 时访问。
#[derive(Default)]
pub(crate) struct Trace {
    /// 跟踪者进程；None 表示没有被跟踪。
    pub tracer: Option<Tid>,
    /// PTRACE_SEIZE 跟踪的：group-stop 和 PTRACE_INTERRUPT 报告成 `PTRACE_EVENT_STOP`，
    /// 跟进来的新线程开始时也是；不会因为 execve 收到 SIGTRAP。
    pub seized: bool,
    /// PTRACE_SETOPTIONS 的选项。
    pub options: u32,
    /// 继续之后怎样执行。
    pub mode: Mode,
    /// 停住的原因；None 表示在运行。
    pub stop: Option<Stop>,
    /// 这次停住已经被跟踪者的 wait 取走。
    pub reported: bool,
    /// 跟踪者让它继续了：要注入的信号（0 表示不注入）。
    pub resume: Option<i32>,
    /// PTRACE_LISTEN：仍然停着，但 group-stop 结束或者来了 PTRACE_INTERRUPT 时重新报告一次。
    pub listening: bool,
    /// 要停一次 `PTRACE_EVENT_STOP`（PTRACE_INTERRUPT、SEIZE 跟进来的新线程）。
    pub trap: bool,
    /// 要参加的 group-stop 的停止信号：线程组被停住时置上，停下来之后清掉。
    pub group_stop: Option<Signal>,
    /// 停住时的寄存器。
    pub regs: Option<Box<Regs>>,
    /// 停住时的 siginfo（Linux 的 `last_siginfo`），按 `siginfo_t` 编码；不是 SEIZE 的
    /// group-stop 没有，PTRACE_GETSIGINFO 是 EINVAL。
    pub siginfo: Option<[u8; 128]>,
    /// PTRACE_GETEVENTMSG。
    pub message: u64,
    /// 系统调用的编号：入口停住时跟踪者可以改（`NT_ARM_SYSTEM_CALL`），改成 -1 时跳过。
    pub syscallno: i32,
    /// 跟踪者设置的硬件断点和观察点；`hw_changed` 时继续之后交给 CPU。
    pub hw: HwDebug,
    pub hw_changed: bool,
}

/// 硬件断点、观察点的槽数，与常见的 Cortex-A 核相同（`ID_AA64DFR0_EL1` 的 BRPs、WRPs）。
pub(crate) const BRK_SLOTS: usize = 6;
pub(crate) const WATCH_SLOTS: usize = 4;

/// 硬件断点、观察点（`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 的 16 组地址和控制字，前
/// [`BRK_SLOTS`]、[`WATCH_SLOTS`] 组有用）。启用的已经按 Linux 规范化：观察点的地址按 8
/// 字节对齐，偏移挪进字节选择（BAS），特权级是 EL0（`syscall::ptrace`）。与 Linux 一样
/// 属于线程：fork、clone 出来的线程没有，execve 时清掉，PTRACE_DETACH 不清。
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HwDebug {
    pub brk: [(u64, u32); 16],
    pub watch: [(u64, u32); 16],
}

impl HwDebug {
    /// 交给引擎（`CpuState::set_hw_debug`）：启用的断点的地址，启用的观察点的
    /// [起点, 终点) 和种类（控制字的 LSC 位就是引擎的 `WATCH_LOAD`、`WATCH_STORE`）。
    pub fn apply(&self, s: &mut CpuState) {
        s.set_hw_debug(&self.breakpoints(), &self.watchpoints());
    }

    fn breakpoints(&self) -> Vec<u64> {
        self.brk[..BRK_SLOTS]
            .iter()
            .filter(|(_, ctrl)| ctrl & 1 != 0)
            .map(|&(addr, _)| addr)
            .collect()
    }

    fn watchpoints(&self) -> Vec<(u64, u64, u32)> {
        self.watch[..WATCH_SLOTS]
            .iter()
            .filter(|(_, ctrl)| ctrl & 1 != 0)
            .map(|&(addr, ctrl)| {
                let bas = ctrl >> 5 & 0xff;
                let lo = addr + u64::from(bas.trailing_zeros());
                (lo, lo + u64::from(bas.count_ones()), ctrl >> 3 & 3)
            })
            .collect()
    }
}

impl Trace {
    /// 停住时报告给跟踪者的状态（Linux 的 `exit_code`）：wait4 的状态字是它左移 8 位再
    /// 或上 0x7f，waitid 的 `si_status` 就是它。
    pub fn exit_code(&self) -> Option<i32> {
        let trap = Signal::TRAP.number();
        Some(match self.stop? {
            Stop::Signal(s) | Stop::Group(s) => s.number(),
            Stop::Syscall(_) if self.options & opt::TRACESYSGOOD != 0 => trap | 0x80,
            Stop::Syscall(_) => trap,
            Stop::Event(e, s) => s.number() | (e as i32) << 8,
        })
    }

    /// 跟踪者打开了事件 `e`。
    pub fn enabled(&self, e: u32) -> bool {
        self.tracer.is_some() && self.options & (1 << e) != 0
    }

    /// 停在 ptrace-stop 里，而且不是 PTRACE_LISTEN：跟踪者可以对它发请求。
    pub fn stopped(&self) -> bool {
        self.stop.is_some() && !self.listening
    }
}

/// 线程被跟踪着。
pub(crate) fn traced(task: &Task) -> bool {
    task.trace.borrow().tracer.is_some()
}

/// 跟踪者发出的 siginfo：用 kill 发信号时那样带上发送者（Linux 的 `ptrace_do_notify`
/// 填的是被跟踪的线程自己）。
fn notify_info(signo: Signal, si_code: i32, pid: Tid, uid: u32) -> [u8; 128] {
    SigInfo::kill(si_code, pid, uid).encode(signo)
}

/// 停在 ptrace-stop 里：通知跟踪者，等它让线程继续。返回跟踪者要注入的信号（0 表示
/// 没有）；线程被结束（SIGKILL）时返回 None。`info` 是这次停住的 siginfo，`message` 是
/// PTRACE_GETEVENTMSG 的值。
pub(crate) fn stop(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    why: Stop,
    info: Option<[u8; 128]>,
    message: u64,
) -> Option<i32> {
    {
        let mut t = cx.task.trace.borrow_mut();
        if t.tracer.is_none() {
            return Some(0);
        }
        t.stop = Some(why);
        t.reported = false;
        t.resume = None;
        t.listening = false;
        t.regs = Some(Box::new(Regs::save(cpu.state())));
        t.siginfo = info;
        t.message = message;
    }
    notify(cx.kr(), cx.task);
    let (tid, uid) = (cx.tid(), cx.cred().ruid);
    let resumed = loop {
        if cx.task.killed.get() {
            break None;
        }
        let group_stopped = cx.t.group.borrow().stopped;
        let retrap = {
            let mut t = cx.task.trace.borrow_mut();
            if let Some(sig) = t.resume.take() {
                break Some(sig);
            }
            // 跟踪者没有让它继续就退出了：要投递的信号照常投递
            if t.tracer.is_none() {
                break Some(match t.stop {
                    Some(Stop::Signal(sig)) => sig.number(),
                    _ => 0,
                });
            }
            // PTRACE_LISTEN 之后 group-stop 结束了（SIGCONT），或者来了 PTRACE_INTERRUPT：
            // 重新报告一次 PTRACE_EVENT_STOP
            if t.listening && (t.trap || !group_stopped) {
                let sig = match t.stop {
                    Some(Stop::Event(_, sig)) if group_stopped => sig,
                    _ => Signal::TRAP,
                };
                t.listening = false;
                t.trap = false;
                t.stop = Some(Stop::Event(event::STOP, sig));
                t.reported = false;
                let si_code = sig.number() | (event::STOP as i32) << 8;
                t.siginfo = Some(notify_info(sig, si_code, tid, uid));
                true
            } else {
                false
            }
        };
        if retrap {
            notify(cx.kr(), cx.task);
        }
        let _ = cx.wait_alone(None);
    };
    let regs = {
        let mut t = cx.task.trace.borrow_mut();
        t.stop = None;
        t.listening = false;
        t.regs.take()
    };
    if let Some(regs) = regs {
        regs.restore(cpu.state_mut());
    }
    // 跟踪者改了硬件断点、观察点（PTRACE_SETREGSET）
    let hw = {
        let mut t = cx.task.trace.borrow_mut();
        std::mem::take(&mut t.hw_changed).then_some(t.hw)
    };
    if let Some(hw) = hw {
        hw.apply(cpu.state_mut());
    }
    resumed
}

/// execve：与 Linux 的 `flush_ptrace_hw_breakpoint` 一样清掉硬件断点和观察点。
pub(crate) fn exec_hw_flush(cx: &Cx<'_>, cpu: &mut Cpu) {
    let mut t = cx.task.trace.borrow_mut();
    if t.hw != HwDebug::default() {
        t.hw = HwDebug::default();
        t.hw_changed = false;
        cpu.state_mut().set_hw_debug(&[], &[]);
    }
}

/// 通知跟踪者线程停住了：叫醒它的 wait；它没有忽略 SIGCHLD、也没有设 SA_NOCLDSTOP 时
/// 发 SIGCHLD（`CLD_TRAPPED`，group-stop 是 `CLD_STOPPED`）。
fn notify(k: &Kernel, task: &Task) {
    let (tracer, why, exit_code) = {
        let t = task.trace.borrow();
        match (t.tracer, t.stop, t.exit_code()) {
            (Some(tracer), Some(why), Some(c)) => (tracer, why, c),
            _ => return,
        }
    };
    request_wake_on(wake::CHILD);
    let Some(g) = k.groups.get(&tracer) else {
        return;
    };
    let act = g.borrow().sighand.borrow().get(Signal::CHLD);
    if act.handler == SigAction::IGNORE || act.flags & sa::NOCLDSTOP != 0 {
        return;
    }
    let why = match why {
        Stop::Group(_) | Stop::Event(event::STOP, _) => code::CLD_STOPPED,
        _ => code::CLD_TRAPPED,
    };
    let info = SigInfo::child_event(task.tid.get(), why, exit_code & 0x7f);
    signal::send_to_group(k, tracer, Signal::CHLD, info);
}

/// 投递信号 `sig` 之前（信号投递停止）。返回跟踪者让投递的信号和它的 siginfo，跟踪者
/// 不让投递时返回 `Ok(None)`，线程被结束时返回 `Err(())`。
pub(crate) fn signal_stop(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    sig: Signal,
    info: SigInfo,
) -> Result<Option<(Signal, SigInfo)>, ()> {
    if !traced(cx.task) {
        return Ok(Some((sig, info)));
    }
    let raw = info.encode(sig);
    let data = stop(cx, cpu, Stop::Signal(sig), Some(raw), 0).ok_or(())?;
    let Some(new) = Signal::new(data) else {
        return Ok(None);
    };
    // PTRACE_SETSIGINFO 改过的 siginfo（Linux 的 ptrace_signal）
    let now = cx.task.trace.borrow_mut().siginfo.take().unwrap_or(raw);
    let signo = i32::from_le_bytes(now[..4].try_into().unwrap());
    let info = if new.number() != signo {
        // 换了信号又没有给 siginfo：当作跟踪者用 kill 发的
        let tracer = cx.task.trace.borrow().tracer.unwrap_or(0);
        let uid = cx
            .kr()
            .groups
            .get(&tracer)
            .map_or(0, |g| g.borrow().cred.ruid);
        SigInfo::kill(code::SI_USER, tracer, uid)
    } else if now == raw {
        info
    } else {
        SigInfo::from_raw(&now)
    };
    // 换成的信号被屏蔽着：重新挂起，解除屏蔽之后再投递
    if cx.task.sigmask.get() & new.bit() != 0 {
        signal::send_to_task(cx.kr(), cx.task, new, info);
        return Ok(None);
    }
    Ok(Some((new, info)))
}

/// 参加 group-stop（线程组被停止信号停住了）：停在 ptrace-stop 里报告给跟踪者。没有
/// 要参加的 group-stop 时什么也不做。线程被结束时返回 false。
pub(crate) fn group_stop(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    let (sig, seized) = {
        let mut t = cx.task.trace.borrow_mut();
        match t.group_stop.take() {
            Some(sig) if t.tracer.is_some() => (sig, t.seized),
            _ => return true,
        }
    };
    let (why, info) = if seized {
        let si_code = sig.number() | (event::STOP as i32) << 8;
        let info = notify_info(sig, si_code, cx.tid(), cx.cred().ruid);
        (Stop::Event(event::STOP, sig), Some(info))
    } else {
        (Stop::Group(sig), None)
    };
    stop(cx, cpu, why, info, 0).is_some()
}

/// PTRACE_INTERRUPT、SEIZE 跟进来的新线程：停一次 `PTRACE_EVENT_STOP`。没有要停的时
/// 什么也不做。线程被结束时返回 false。
pub(crate) fn trap(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    {
        let mut t = cx.task.trace.borrow_mut();
        if !std::mem::take(&mut t.trap) || t.tracer.is_none() {
            return true;
        }
    }
    let sig = Signal::TRAP;
    let si_code = sig.number() | (event::STOP as i32) << 8;
    let info = notify_info(sig, si_code, cx.tid(), cx.cred().ruid);
    stop(cx, cpu, Stop::Event(event::STOP, sig), Some(info), 0).is_some()
}

/// 单步时信号的处理函数刚设置好（还没执行第一条指令）：与 Linux 的 `ptrace_notify(SIGTRAP)`
/// 一样停一次，跟踪者看到的 siginfo 是 SIGTRAP、`si_code` 也是 SIGTRAP、发送者是线程
/// 自己。停住时的信号不投递。线程被结束时返回 false。
pub(crate) fn handler_entered(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    if !traced(cx.task) {
        return true;
    }
    let sig = Signal::TRAP;
    let info = notify_info(sig, sig.number(), cx.tid(), cx.cred().ruid);
    stop(cx, cpu, Stop::Event(0, sig), Some(info), 0).is_some()
}

/// PTRACE_SINGLESTEP 让线程执行完了一条指令（不是 svc）：与 arm64 Linux 的单步异常一样
/// 投递 SIGTRAP（`TRAP_TRACE`，地址是下一条指令），停在信号投递停止里报告给跟踪者。
pub(crate) fn step_done(cx: &mut Cx<'_>, cpu: &Cpu) {
    let info = SigInfo::fault(code::TRAP_TRACE, cpu.state().pc, 0);
    signal::force(cx, Signal::TRAP, info);
}

/// 单步执行的是 svc：系统调用做完之后报告。与 arm64 Linux 一样，这次的 SIGTRAP 是
/// `SI_USER`，没有发送者；硬件的单步状态还挂着，下一次 PTRACE_SINGLESTEP 不执行指令，
/// 在原地再报告一次 `TRAP_TRACE`（`Task::step_again`）。
pub(crate) fn syscall_stepped(cx: &mut Cx<'_>) {
    if cx.task.stepping.load(Ordering::Relaxed) {
        signal::force(cx, Signal::TRAP, SigInfo::kill(code::SI_USER, 0, 0));
        cx.task.step_again.store(true, Ordering::Relaxed);
    }
}

/// 系统调用停住时跟踪者注入的信号：与 Linux 一样当作内核发给自己的新信号。
fn inject(cx: &Cx<'_>, data: i32) {
    if let Some(sig) = Signal::new(data) {
        signal::send_to_task(cx.kr(), cx.task, sig, SigInfo::kernel());
    }
}

/// PTRACE_SYSCALL 让线程继续之后，这个系统调用（编号 `nr`）要不要在入口和出口停下。vDSO
/// 里读时钟的存根不算：Linux 上它们不进内核，跟踪者看不到。
pub(crate) fn syscall_traced(cx: &Cx<'_>, cpu: &Cpu, nr: u64) -> bool {
    {
        let t = cx.task.trace.borrow();
        if t.tracer.is_none() || t.mode != Mode::Syscall {
            return false;
        }
    }
    !crate::vdso::clock_stub(cx, cpu.state().pc, nr)
}

/// 系统调用的停住：与 arm64 Linux 一样，停住期间 x7 是 0（入口）或 1（出口），跟踪者
/// 看不到也改不了它原来的值。线程被结束时返回 false。
fn syscall_stop(cx: &mut Cx<'_>, cpu: &mut Cpu, entry: bool) -> bool {
    let options = cx.task.trace.borrow().options;
    let sig = Signal::TRAP;
    let si_code = sig.number()
        | if options & opt::TRACESYSGOOD != 0 {
            0x80
        } else {
            0
        };
    let info = notify_info(sig, si_code, cx.tid(), cx.cred().ruid);
    let message = if entry {
        EVENTMSG_SYSCALL_ENTRY
    } else {
        EVENTMSG_SYSCALL_EXIT
    };
    let saved = std::mem::replace(&mut cpu.state_mut().x[7], u64::from(!entry));
    let data = stop(cx, cpu, Stop::Syscall(entry), Some(info), message);
    cpu.state_mut().x[7] = saved;
    match data {
        Some(data) => {
            inject(cx, data);
            true
        }
        None => false,
    }
}

/// 系统调用入口停下。返回跟踪者改过之后的编号（-1 是跳过这个系统调用）；线程被结束时
/// 返回 None。
pub(crate) fn syscall_entry(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64) -> Option<u64> {
    cx.task.trace.borrow_mut().syscallno = nr as i32;
    syscall_stop(cx, cpu, true).then(|| cx.task.trace.borrow().syscallno as i64 as u64)
}

/// 系统调用出口停下（PTRACE_SYSCALL 让线程继续的话）。线程被结束时返回 false。
pub(crate) fn syscall_exit(cx: &mut Cx<'_>, cpu: &mut Cpu) -> bool {
    let nr = cx.task.trace.borrow().syscallno as i64 as u64;
    !syscall_traced(cx, cpu, nr) || syscall_stop(cx, cpu, false)
}

/// seccomp 的过滤器返回 TRACE（数据是 `data`）：停一次 `PTRACE_EVENT_SECCOMP`，跟踪者
/// 可以改编号（`NT_ARM_SYSTEM_CALL`，-1 是跳过）和寄存器。返回之后的编号；线程被结束时
/// 返回 None。
pub(crate) fn seccomp_stop(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64, data: u32) -> Option<u64> {
    cx.task.trace.borrow_mut().syscallno = nr as i32;
    event(cx, cpu, event::SECCOMP, u64::from(data))
        .then(|| cx.task.trace.borrow().syscallno as i64 as u64)
}

/// `PTRACE_EVENT_*` 停下（跟踪者打开了这个事件时）。线程被结束时返回 false。
pub(crate) fn event(cx: &mut Cx<'_>, cpu: &mut Cpu, e: u32, message: u64) -> bool {
    if !cx.task.trace.borrow().enabled(e) {
        return true;
    }
    let sig = Signal::TRAP;
    let si_code = sig.number() | (e as i32) << 8;
    let info = notify_info(sig, si_code, cx.tid(), cx.cred().ruid);
    stop(cx, cpu, Stop::Event(e, sig), Some(info), message).is_some()
}

/// execve 成功之后：`PTRACE_EVENT_EXEC`（消息是 execve 之前的 tid），没有打开这个事件、
/// 又不是 SEIZE 跟踪的给自己发 SIGTRAP。线程被结束时返回 false。
pub(crate) fn exec(cx: &mut Cx<'_>, cpu: &mut Cpu, old_tid: Tid) -> bool {
    let (traced, seized, enabled) = {
        let t = cx.task.trace.borrow();
        (t.tracer.is_some(), t.seized, t.enabled(event::EXEC))
    };
    if enabled {
        return event(cx, cpu, event::EXEC, old_tid as u64);
    }
    if traced && !seized {
        let info = SigInfo::kill(code::SI_USER, cx.tgid(), cx.cred().ruid);
        signal::send_to_task(cx.kr(), cx.task, Signal::TRAP, info);
    }
    true
}

/// 线程要退出了（exit、exit_group、致命信号的默认动作）：`PTRACE_EVENT_EXIT`，消息是
/// wait 的状态字。exit_group 已经要求所有线程退出，包括自己；调用者自己仍然停下来报告，
/// 停住期间被 SIGKILL 结束时不再等。
pub(crate) fn exit(cx: &mut Cx<'_>, cpu: &mut Cpu, status: ExitStatus) {
    cx.task.begin_exit(cx.kr(), &cx.t.group, status);
    if !cx.task.trace.borrow().enabled(event::EXIT) {
        return;
    }
    let killed = cx.task.killed.replace(false);
    if event(cx, cpu, event::EXIT, status.wait_status() as u64) {
        cx.task.killed.set(killed);
    }
}

/// clone 出来的线程 `child`（`flags`、`exit_signal` 是 clone 的参数）要不要被跟踪：父
/// 线程被跟踪、打开了对应的事件（或者用了 `CLONE_PTRACE`）、没有 `CLONE_UNTRACED` 时，
/// 新线程由同一个跟踪者跟踪，执行第一条指令之前先停下来（不是 SEIZE 的挂一个 SIGSTOP）。
/// 返回父线程要报告的事件。
pub(crate) fn clone(parent: &Task, child: &Task, flags: u64, exit_signal: i32) -> Option<u32> {
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_PTRACE: u64 = 0x2000;
    const CLONE_UNTRACED: u64 = 0x0080_0000;
    let p = parent.trace.borrow();
    if p.tracer.is_none() || flags & CLONE_UNTRACED != 0 {
        return None;
    }
    let e = if flags & CLONE_VFORK != 0 {
        event::VFORK
    } else if exit_signal != Signal::CHLD.number() {
        event::CLONE
    } else {
        event::FORK
    };
    let report = p.enabled(e).then_some(e);
    if report.is_none() && flags & CLONE_PTRACE == 0 {
        return None;
    }
    {
        let mut c = child.trace.borrow_mut();
        c.tracer = p.tracer;
        c.seized = p.seized;
        c.options = p.options;
        if p.seized {
            c.trap = true;
        } else {
            let stop = SigInfo::default();
            child.pending.borrow_mut().add(Signal::STOP, stop, false);
        }
    }
    // SAFETY: 新线程的 Cpu 还在，调用方持有 BKL
    unsafe { child.sync.kicker.kick() };
    report
}

/// 跟踪者不再跟踪 `task`（PTRACE_DETACH、跟踪者退出）：停着的让它继续，写回跟踪者改过的
/// 寄存器。`sig` 是 PTRACE_DETACH 要投递的信号；跟踪者退出时是 None，信号投递停止里的
/// 信号照常投递（[`stop`]）。线程组被停住时它回到 group-stop 里。
pub(crate) fn detach(task: &Task, sig: Option<i32>) {
    {
        let mut t = task.trace.borrow_mut();
        let (stop, regs) = (t.stop, t.regs.take());
        *t = Trace::default();
        if stop.is_some() {
            t.stop = stop;
            t.regs = regs;
            t.resume = sig;
        }
    }
    task.syscall_traced.store(false, Ordering::Relaxed);
    task.stepping.store(false, Ordering::Relaxed);
    signal::wake(task);
}

/// 跟踪者 `tgid` 退出了：设了 `PTRACE_O_EXITKILL` 的被跟踪线程被结束，其余的不再被跟踪。
pub(crate) fn tracer_exited(k: &mut Kernel, tgid: Tid) {
    let tracees: Vec<_> = k
        .tasks
        .values()
        .filter(|t| t.trace.borrow().tracer == Some(tgid))
        .cloned()
        .collect();
    for task in tracees {
        if task.trace.borrow().options & opt::EXITKILL != 0 {
            signal::send_to_task(k, &task, Signal::KILL, SigInfo::kernel());
        }
        detach(&task, None);
    }
    // 还没取走的结束：被跟踪的进程交给父进程（Linux 的 `__ptrace_detach`）
    let (gone, kept) = std::mem::take(&mut k.trace_exits)
        .into_iter()
        .partition(|e| e.tracer == tgid);
    k.trace_exits = kept;
    for e in gone {
        released(k, &e);
    }
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.leader_tracer == Some(tgid) {
            g.leader_tracer = None;
        }
    }
}

/// 被跟踪的线程退出了，等跟踪者 wait 取走（[`Kernel::trace_exits`]）。
pub(crate) struct TraceExit {
    pub tracer: Tid,
    pub pid: Tid,
    /// 退出时所在的进程组（wait 按进程组等）。
    pub pgid: Tid,
    pub status: ExitStatus,
    /// 是整个进程的结束：跟踪者取走之后进程才交给父进程（[`group_finished`]）。
    pub process: bool,
}

/// 被跟踪的线程 `task` 退出了（已经从 `kernel.tasks` 里拿掉）：不是主线程的单独报告给
/// 跟踪者；主线程记下跟踪者，等整个进程结束时再报告（[`group_finished`]）。`status` 是
/// 线程自己结束的方式。
pub(crate) fn tracee_exited(
    k: &mut Kernel,
    task: &Task,
    group: &mut ThreadGroup,
    status: ExitStatus,
) {
    let Some(tracer) = task.trace.borrow().tracer else {
        return;
    };
    let tid = task.tid.get();
    if tid == group.tgid {
        group.leader_tracer = Some(tracer);
        return;
    }
    let status = group.exiting.unwrap_or(status);
    let exit = TraceExit {
        tracer,
        pid: tid,
        pgid: group.pgid,
        status,
        process: false,
    };
    report_exit(k, exit);
}

/// 进程 `group` 结束了：主线程被父进程以外的进程跟踪着时，与 Linux 一样先只报告给跟踪者
/// （发 SIGCHLD），它在 wait 里取走之后（[`released`]）或者自己退出时，才交给父进程。
/// 这样推迟了时返回 true。父进程本身就是跟踪者时由普通的 wait 报告。
pub(crate) fn group_finished(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    status: ExitStatus,
) -> bool {
    let exit = {
        let g = group.borrow();
        match g.leader_tracer {
            Some(tracer) if tracer != g.ppid && k.groups.contains_key(&tracer) => TraceExit {
                tracer,
                pid: g.tgid,
                pgid: g.pgid,
                status,
                process: true,
            },
            _ => return false,
        }
    };
    {
        let mut g = group.borrow_mut();
        g.zombie = Some(status);
        g.exit_tracer = Some(exit.tracer);
    }
    report_exit(k, exit);
    true
}

/// 跟踪者取走了被跟踪的线程的结束 `e`（或者没取走就退出了）：整个进程的结束交给父进程。
pub(crate) fn released(k: &mut Kernel, e: &TraceExit) {
    if !e.process {
        return;
    }
    if let Some(group) = k.groups.get(&e.pid).cloned()
        && group.borrow().exit_tracer == Some(e.tracer)
    {
        crate::process::tracer_released(k, &group);
    }
}

fn report_exit(k: &mut Kernel, exit: TraceExit) {
    if !k.groups.contains_key(&exit.tracer) {
        return;
    }
    let (tracer, info) = (exit.tracer, SigInfo::child(exit.pid, exit.status));
    k.trace_exits.push(exit);
    request_wake_on(wake::CHILD);
    signal::send_to_group(k, tracer, Signal::CHLD, info);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_debug_ranges_come_from_the_byte_selects() {
        let mut hw = HwDebug::default();
        // 8 字节对齐的地址，BAS 选第 4–7 个字节，读写都算；停用的和超出槽数的不算
        hw.watch[0] = (0x1000, 0xf0 << 5 | 3 << 3 | 4 | 1);
        hw.watch[1] = (0x2000, 0x01 << 5 | 2 << 3 | 4);
        hw.watch[WATCH_SLOTS] = (0x3000, 0x01 << 5 | 2 << 3 | 4 | 1);
        hw.brk[0] = (0x4000, 0xf << 5 | 4 | 1);
        assert_eq!(hw.watchpoints(), vec![(0x1004, 0x1008, 3)]);
        assert_eq!(hw.breakpoints(), vec![0x4000]);
    }
}
