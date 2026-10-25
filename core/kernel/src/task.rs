//! guest 线程（Linux 的 task）与它的运行循环（设计文档 §6）。
//!
//! 每个 guest 线程对应一个宿主线程（1:1）。运行循环在不持有 BKL 的情况下执行
//! guest 代码，遇到 syscall、同步异常或 kick 时获取 BKL 进入内核。

use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use appine_engine::{Access, Cpu, Dbt, DbtStats, Exit, FaultKind, Kicker, PAGE_SIZE};
use appine_hal::Event;

use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{Cx, Kernel, Shared, Tid, request_wake};
use crate::mm::{Generations, Mm};
use crate::process::{Signal, ThreadGroup, Tier};
use crate::signal::{self, AltStack, Pending, SigInfo, code};
use crate::syscall::{self, Flow};
use crate::uaccess::UserSlice;
use crate::vfs::{Node, Vfs};

/// 运行循环与内核其他部分之间无锁共享的状态。
pub(crate) struct TaskSync {
    /// 线程是否正在执行 guest 代码（TLB shootdown 要等它退出）。
    pub in_guest: AtomicBool,
    /// 打断线程的 Cpu。
    pub kicker: Kicker,
}

/// 根目录、当前目录和文件创建掩码（`CLONE_FS` 共享的部分）。
pub(crate) struct FsContext {
    /// 根目录（chroot）。
    pub root: Node,
    pub cwd: Node,
    pub umask: u32,
}

/// 线程用着的地址空间、fd 表和当前目录。它们本来在 [`TaskState`] 里，这里是一份
/// 别的线程也能读的拷贝（procfs 用），由 [`Cx::set_mm`](crate::kernel::Cx::set_mm)
/// 等在替换时同步更新。
pub(crate) struct TaskRes {
    pub mm: Rc<RefCell<Mm>>,
    pub fds: Rc<RefCell<FdTable>>,
    pub fs: Rc<RefCell<FsContext>>,
}

/// 一个 guest 线程。除 [`Task::state`] 外，其余字段也会被其他线程访问（发信号、
/// 结束线程组），都只在持有 BKL 时进行。
pub(crate) struct Task {
    /// tid。非主线程 execve 时接过主线程的 tid，所以会变。
    pub tid: Cell<Tid>,
    /// 所属线程组。
    pub tgid: Tid,
    /// 发给这个线程、尚未处理的信号。
    pub pending: RefCell<Pending>,
    /// 信号屏蔽字。
    pub sigmask: Cell<u64>,
    /// 线程被要求退出（线程组整体退出、execve 结束其他线程）。
    pub killed: Cell<bool>,
    /// 线程正在内核里等待（`/proc/<pid>/stat` 的状态 S）。在释放 BKL 之前设置，重新拿到
    /// BKL 之后清除。
    pub sleeping: Cell<bool>,
    /// 线程名（`prctl(PR_SET_NAME)`）。
    pub comm: Cell<[u8; 16]>,
    /// 调度策略（含 `SCHED_RESET_ON_FORK`）和实时优先级：只记录，由宿主调度。
    pub sched: Cell<(i32, i32)>,
    /// `SCHED_DEADLINE` 的 (runtime, deadline, period)，纳秒。
    pub deadline: Cell<(u64, u64, u64)>,
    /// `PR_SET_TIMERSLACK` 的定时器松弛量和默认值（纳秒）：只记录。新线程的两者都是
    /// 父线程当前的松弛量，设成 0 时恢复默认值。
    pub timerslack: Cell<(u64, u64)>,
    /// CPU 亲和性（最多 64 个 CPU）：只记录。
    pub affinity: Cell<u64>,
    /// set_robust_list 设置的 robust futex 链表头和长度，新线程从空开始。只记录：线程
    /// 退出时不处理链表里的 futex。
    pub robust_list: Cell<(u64, u64)>,
    pub sync: Arc<TaskSync>,
    /// 唤醒等待中的这个线程。
    pub event: Arc<Event>,
    /// 地址空间等资源的共享拷贝（procfs 用）。
    pub res: RefCell<TaskRes>,
    /// 宿主线程的 CPU 时间时钟。
    pub clock: Cell<Option<i32>>,
    /// 只有线程自己访问的状态。
    pub state: RefCell<TaskState>,
}

/// 线程私有的状态，以及它和其他线程共享的资源。
pub(crate) struct TaskState {
    /// 地址空间（`CLONE_VM` 共享）。
    pub mm: Rc<RefCell<Mm>>,
    /// fd 表（`CLONE_FILES` 共享）。
    pub fds: Rc<RefCell<FdTable>>,
    /// 当前目录、umask（`CLONE_FS` 共享）。
    pub fs: Rc<RefCell<FsContext>>,
    /// 所属线程组。
    pub group: Rc<RefCell<ThreadGroup>>,
    /// 根文件系统。
    pub vfs: Option<Rc<Vfs>>,
    /// 备用信号栈。
    pub altstack: AltStack,
    /// 线程退出时清零并 futex 唤醒的地址（`set_tid_address`、`CLONE_CHILD_CLEARTID`）。
    pub clear_child_tid: u64,
    /// sigsuspend、ppoll 临时替换屏蔽字时原来的屏蔽字。
    pub saved_sigmask: Option<u64>,
    /// vfork 的子进程：execve 或退出时置位，让父进程继续。
    pub vfork_done: Option<Rc<Cell<bool>>>,
    /// 被信号打断的 nanosleep 剩下的部分（`restart_syscall`）：到期时刻和 `rem` 的地址。
    pub restart: Option<(std::time::Instant, u64)>,
}

impl Task {
    /// 为 `cpu` 新建线程。
    pub fn new(
        tid: Tid,
        tgid: Tid,
        cpu: &Cpu,
        state: TaskState,
        sigmask: u64,
        comm: [u8; 16],
    ) -> Result<Rc<Task>, Errno> {
        let event = Event::new().map_err(|_| Errno::EAGAIN)?;
        let res = TaskRes {
            mm: state.mm.clone(),
            fds: state.fds.clone(),
            fs: state.fs.clone(),
        };
        Ok(Rc::new(Task {
            tid: Cell::new(tid),
            tgid,
            pending: RefCell::default(),
            sigmask: Cell::new(sigmask),
            killed: Cell::new(false),
            sleeping: Cell::new(false),
            comm: Cell::new(comm),
            sched: Cell::new((0, 0)),
            deadline: Cell::new((0, 0, 0)),
            timerslack: Cell::new((50_000, 50_000)),
            affinity: Cell::new(u64::MAX),
            robust_list: Cell::new((0, 0)),
            sync: Arc::new(TaskSync {
                in_guest: AtomicBool::new(false),
                kicker: cpu.kicker(),
            }),
            event: Arc::new(event),
            res: RefCell::new(res),
            clock: Cell::new(None),
            state: RefCell::new(state),
        }))
    }

    /// 线程用掉的 CPU 时间（纳秒），以宿主线程的 CPU 时间计。
    pub fn cpu_ns(&self) -> u64 {
        self.clock
            .get()
            .and_then(|c| appine_hal::clock_gettime(c).ok())
            .map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64)
    }
}

/// 为 `task` 启动宿主线程，并把它登记到 kernel、线程组和地址空间里。调用方持有 BKL，
/// 线程要等调用方释放 BKL 之后才开始运行。
pub(crate) fn start(
    shared: &Arc<Shared>,
    k: &mut Kernel,
    task: Rc<Task>,
    cpu: Cpu,
) -> Result<(), Errno> {
    let tid = task.tid.get();
    let thread_shared = shared.clone();
    // 不保留 JoinHandle：线程退出时它的宿主资源就释放了。会话结束时等的是
    // kernel.tasks 变空，线程在那之后只剩释放自己的 Cpu 和翻译缓存
    std::thread::Builder::new()
        .name(format!("guest-{tid}"))
        .spawn(move || thread_main(thread_shared, tid, cpu))
        .map_err(|_| Errno::EAGAIN)?;
    {
        let st = task.state.borrow();
        st.group.borrow_mut().threads.push(tid);
        st.mm.borrow_mut().add_user(task.sync.clone());
    }
    k.tasks.insert(tid, task);
    Ok(())
}

fn thread_main(shared: Arc<Shared>, tid: Tid, mut cpu: Cpu) {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| run(&shared, tid, &mut cpu)));
    if r.is_err() {
        // 内核层的 bug：BKL 保护的状态可能已经不一致，不能让其他线程继续
        eprintln!("appine: internal error in guest thread {tid}, aborting");
        std::process::abort();
    }
}

fn run(shared: &Arc<Shared>, tid: Tid, cpu: &mut Cpu) {
    let (task, sync, mut generations, tier) = {
        let k = shared.lock();
        let task = k.tasks[&tid].clone();
        task.clock.set(appine_hal::thread_cpu_clock());
        let generations = task.state.borrow().mm.borrow().generations();
        let sync = task.sync.clone();
        (task, sync, generations, k.tier)
    };
    let mut dbt = match tier {
        Tier::Interpreter => None,
        Tier::Dbt => Dbt::new(),
    };
    let load = |g: &Generations| (g.tlb.load(Ordering::SeqCst), g.code.load(Ordering::SeqCst));
    let mut seen = load(&generations);
    loop {
        // 先标记、再读代数，与 Mm::sync 的"先改代数、再看标记"配对
        sync.in_guest.store(true, Ordering::SeqCst);
        let now = load(&generations);
        if now.0 != seen.0 {
            cpu.tlb_flush();
        }
        // 只有代码变了才需要作废翻译：按错过的范围逐页作废，记录不全时整体清空
        if now.1 != seen.1
            && let Some(d) = &mut dbt
        {
            match generations.log.lock().unwrap().since(seen.1, now.1) {
                Some(ranges) => ranges
                    .iter()
                    .for_each(|&(start, len)| d.invalidate(start, len)),
                None => d.flush(),
            }
        }
        seen = now;
        let exit = match &mut dbt {
            Some(d) => d.run(cpu),
            None => cpu.run(),
        };
        sync.in_guest.store(false, Ordering::SeqCst);

        let k = shared.lock();
        let mut state = task.state.borrow_mut();
        let mut cx = Cx::new(shared, k, &task, &mut state);
        match handle(&mut cx, cpu, exit) {
            Flow::Continue => {}
            Flow::Exec => {
                generations = cx.mm().generations();
                seen = load(&generations);
                // 又一次 exec 的多半是同一个程序（shell 脚本里的 busybox）：保留翻译，核对后继续用
                if let Some(d) = &mut dbt {
                    d.recheck();
                }
            }
            Flow::Exit => break,
        }
    }
    exit(shared, task, dbt.map(|d| d.stats()));
}

// 合成 esr_context 用的 ESR 字段（ARM ARM D17.2.37）
const ESR_IL: u64 = 1 << 25;
const ESR_WNR: u64 = 1 << 6;
const EC_IABT_LOW: u64 = 0x20;
const EC_PC_ALIGN: u64 = 0x22;
const EC_DABT_LOW: u64 = 0x24;
const EC_SP_ALIGN: u64 = 0x26;
/// 第 3 级转换错误（页没有映射）。
const FSC_TRANSLATION: u64 = 0x07;
/// 第 3 级权限错误。
const FSC_PERMISSION: u64 = 0x0f;
/// 对齐错误。
const FSC_ALIGNMENT: u64 = 0x21;

/// 处理一次 guest 退出。
fn handle(cx: &mut Cx<'_>, cpu: &mut Cpu, exit: Exit) -> Flow {
    if cx.task.killed.get() {
        return Flow::Exit;
    }
    let pc = cpu.state().pc;
    let (sig, info) = match exit {
        Exit::Svc { .. } => return syscall::dispatch(cx, cpu),
        Exit::Kick => return signal::deliver(cx, cpu, false),
        Exit::InvalidateCode { addr } => {
            // 作废所在一页的翻译，并等共用地址空间的其他线程都看到
            let sync = cx.task.sync.clone();
            {
                let mut mm = cx.mm_mut();
                mm.invalidate_code(addr & !(PAGE_SIZE - 1), PAGE_SIZE);
                mm.sync(&sync);
            }
            return signal::deliver(cx, cpu, false);
        }
        Exit::Undefined { insn } => {
            crate::debug::log(format_args!(
                "undefined instruction {insn:#010x} at {pc:#x}"
            ));
            (Signal::ILL, SigInfo::fault(code::ILL_ILLOPC, pc, ESR_IL))
        }
        // Linux 发断点的 SIGTRAP 时信号帧里没有 esr_context
        Exit::Breakpoint { .. } => (Signal::TRAP, SigInfo::fault(code::TRAP_BRKPT, pc, 0)),
        Exit::Fault {
            addr,
            access,
            kind: FaultKind::Page,
        } => {
            let (ec, wnr) = match access {
                Access::Exec => (EC_IABT_LOW, 0),
                Access::Read => (EC_DABT_LOW, 0),
                Access::Write => (EC_DABT_LOW, ESR_WNR),
            };
            let past_eof = cx.mm_mut().fault_past_eof(addr);
            if past_eof == Some(false) {
                // 共享映射的文件变长了，新内容已经映射进来：重新执行
                return signal::deliver(cx, cpu, false);
            }
            if past_eof == Some(true) {
                let esr = ec << 26 | ESR_IL | wnr | FSC_TRANSLATION;
                let info = SigInfo::fault(code::BUS_ADRERR, addr, esr);
                signal::force(cx, Signal::BUS, info);
                return signal::deliver(cx, cpu, false);
            }
            let mapped = cx.mm().is_mapped(addr);
            let (code, fsc) = if mapped {
                (code::SEGV_ACCERR, FSC_PERMISSION)
            } else {
                (code::SEGV_MAPERR, FSC_TRANSLATION)
            };
            let esr = ec << 26 | ESR_IL | wnr | fsc;
            (Signal::SEGV, SigInfo::fault(code, addr, esr))
        }
        Exit::Fault {
            addr,
            access,
            kind: FaultKind::Alignment,
        } => {
            let esr = match access {
                Access::Exec => EC_PC_ALIGN << 26 | ESR_IL,
                Access::Read => EC_DABT_LOW << 26 | ESR_IL | FSC_ALIGNMENT,
                Access::Write => EC_DABT_LOW << 26 | ESR_IL | ESR_WNR | FSC_ALIGNMENT,
            };
            (Signal::BUS, SigInfo::fault(code::BUS_ADRALN, addr, esr))
        }
        Exit::Fault {
            addr,
            kind: FaultKind::StackAlignment,
            ..
        } => {
            let info = SigInfo::fault(code::BUS_ADRALN, addr, EC_SP_ALIGN << 26 | ESR_IL);
            (Signal::BUS, info)
        }
    };
    signal::force(cx, sig, info);
    signal::deliver(cx, cpu, false)
}

/// 线程离开它的地址空间（退出、execve）：处理 robust futex 链表，清零 `clear_child_tid`
/// 并唤醒等它的线程（pthread_join），让 vfork 的父进程继续，从使用者里注销。
pub(crate) fn release_mm(k: &mut Kernel, task: &Task, st: &mut TaskState) {
    let (head, _) = task.robust_list.replace((0, 0));
    crate::futex::exit_robust_list(&mut k.futexes, &st.mm.borrow(), head, task.tid.get() as u32);
    let addr = std::mem::take(&mut st.clear_child_tid);
    {
        let mm = st.mm.borrow();
        // 与 Linux 的 mm_release 一样，地址空间没有别人在用时不必通知
        if addr != 0
            && mm.shared_with_others()
            && mm.write(UserSlice::new(addr, 4), &[0; 4]).is_ok()
            && let Some(key) = mm.futex_key(addr)
        {
            k.futexes.wake(key, 1, u32::MAX);
        }
    }
    if let Some(done) = st.vfork_done.take() {
        done.set(true);
        request_wake();
    }
    st.mm.borrow_mut().remove_user(&task.sync);
}

/// 线程退出：释放它的资源；它是线程组的最后一个线程时结束整个进程。
fn exit(shared: &Arc<Shared>, task: Rc<Task>, stats: Option<DbtStats>) {
    let mut k = shared.lock();
    if let Some(s) = stats {
        let total = k.dbt_stats.get_or_insert_with(DbtStats::default);
        add_stats(total, &s);
    }
    let tid = task.tid.get();
    let cpu = task.cpu_ns();
    k.cpu_ns += cpu;
    let group = {
        let mut st = task.state.borrow_mut();
        release_mm(&mut k, &task, &mut st);
        st.group.clone()
    };
    group.borrow_mut().cpu_ns += cpu;
    k.tasks.remove(&tid);
    let last = {
        let mut g = group.borrow_mut();
        g.threads.retain(|&t| t != tid);
        g.threads.is_empty()
    };
    if last {
        crate::process::finish_group(&mut k, &group);
    }
    // 在持有 BKL 时释放线程的资源：地址空间、fd 表（关闭管道会唤醒另一端）……
    drop(group);
    drop(task);
    request_wake();
}

fn add_stats(total: &mut DbtStats, s: &DbtStats) {
    total.blocks += s.blocks;
    total.insns += s.insns;
    total.insns_copied += s.insns_copied;
    total.insns_remapped += s.insns_remapped;
    total.insns_memory += s.insns_memory;
    total.insns_fallback += s.insns_fallback;
    total.exits_chain += s.exits_chain;
    total.exits_lookup += s.exits_lookup;
    total.exits_interp += s.exits_interp;
    total.exits_svc += s.exits_svc;
    total.exits_kick += s.exits_kick;
    total.flushes += s.flushes;
    total.invalidated += s.invalidated;
    total.blocks_shared += s.blocks_shared;
}
