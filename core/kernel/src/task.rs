//! guest 线程（Linux 的 task）与它的运行循环（设计文档 §6）。
//!
//! 每个 guest 线程对应一个宿主线程（1:1）。运行循环在不持有 BKL 的情况下执行
//! guest 代码，遇到 syscall、同步异常或 kick 时获取 BKL 进入内核；少数系统调用不必
//! 取 BKL（[`crate::syscall::fast`]）。

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use appine_engine::{
    Access, Cpu, CpuState, Dbt, DbtStats, Exit, FaultKind, Kicker, PAGE_SIZE, prot,
};
use appine_hal::Event;

use crate::bug::Trap;
use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{Cx, Kernel, Shared, Tid, request_wake_on, wake};
use crate::mm::{CodeSpace, Generations, Mm};
use crate::process::{CpuTime, ExitStatus, Signal, ThreadGroup, Tier};
use crate::signal::{self, AltStack, Pending, SigInfo, code};
use crate::syscall::fast::Fast;
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

/// 根目录、当前目录和文件创建掩码（`CLONE_FS` 共享的部分），以及挂载命名空间。Linux 的
/// 挂载命名空间在 nsproxy 里，但 `CLONE_NEWNS` 不能和 `CLONE_FS` 同用、setns 要求 fs 没有
/// 共用，共用一个 fs 的任务总在同一个挂载命名空间里，放在一起是一样的。
pub(crate) struct FsContext {
    /// 根目录（chroot）。
    pub root: Node,
    pub cwd: Node,
    pub umask: u32,
    pub mnt_ns: Rc<crate::vfs::MntNs>,
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
    /// 新建的子进程所在的 pid 命名空间，None 是与自己的相同（Linux 的
    /// `nsproxy->pid_ns_for_children`；unshare、setns 的 `CLONE_NEWPID` 改它）。放在这里
    /// 而不是 TaskState 里，`/proc/<pid>/ns/pid_for_children` 要读它。
    pub pid_ns_children: RefCell<Option<Rc<crate::pidns::PidNs>>>,
    /// 所在的 UTS、IPC 命名空间（[`crate::nsproxy`]）。
    pub ns: RefCell<crate::nsproxy::NsProxy>,
    /// sigsuspend、ppoll 临时替换屏蔽字时原来的屏蔽字（Linux 的 `saved_sigmask` 加上
    /// `TIF_RESTORE_SIGMASK`）：处理函数的帧里保存它，没有处理函数时恢复它。放在这里而不是
    /// TaskState 里，跟踪者要读写它（PTRACE_GETSIGMASK、SETSIGMASK）。
    pub saved_sigmask: Cell<Option<u64>>,
    /// 线程被要求退出（线程组整体退出、execve 结束其他线程）。
    pub killed: Cell<bool>,
    /// `PR_SET_PDEATHSIG`：父进程里建立这个进程的线程退出时发给这个进程的信号，0 表示不发
    /// （[`crate::process::pdeath_signals`]）。与 Linux 一样按线程记，新的线程、进程都从 0
    /// 开始，execve 保留。
    pub pdeathsig: Cell<i32>,
    /// 线程正在内核里等待（[`Task::asleep`]、平均负载）。在释放 BKL 之前设置，重新拿到
    /// BKL 之后清除；不持 BKL 的等待（`syscall::fast`）也设置。
    pub sleeping: AtomicBool,
    /// 线程名（`prctl(PR_SET_NAME)`）。
    pub comm: Cell<[u8; 16]>,
    /// nice 值（-20–19）：与 Linux 一样属于线程，新线程继承创建者的。宿主线程跟着
    /// 设置（`syscall::sched`）。
    pub nice: Cell<i32>,
    /// 调度策略（含 `SCHED_RESET_ON_FORK`）和实时优先级：只记录，由宿主调度。
    pub sched: Cell<(i32, i32)>,
    /// IO 优先级（ioprio_set 设置的原始值，Linux 的 `io_context->ioprio`）。None 是还没有
    /// io_context（ioprio_get 读到 0）；`CLONE_IO` 建的线程与创建者共用一份。宿主线程
    /// 跟着设置（`syscall::sched`）。
    pub ioprio: RefCell<Option<Rc<Cell<u16>>>>,
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
    /// 线程开始时宿主线程已经用掉的 CPU 时间和其中的内核态时间（纳秒）：宿主线程可能
    /// 是线程池留下来的，跑过别的 guest 线程（[`pool`]）。
    pub cpu_base: Cell<(u64, u64)>,
    /// 上一次报告的 CPU 时间（[`Task::cpu`]）：用户态、内核态的时间都不比它少。
    cpu_prev: Cell<CpuTime>,
    /// 线程开始时宿主线程自愿、非自愿的上下文切换次数（宿主线程会被复用）：
    /// `/proc/<pid>/status` 报告的是之后的。
    pub ctxt_base: Cell<(u64, u64)>,
    /// 宿主线程的 tid（读宿主记的内核态时间）。
    pub host_tid: Cell<i32>,
    /// ptrace 的状态（[`crate::ptrace`]）。
    pub trace: RefCell<crate::ptrace::Trace>,
    /// 跟踪者用 PTRACE_SYSCALL 让它继续了：系统调用都交给持有 BKL 的路径，好在入口和
    /// 出口停下（`syscall::fast` 不持 BKL 时读它）。
    pub syscall_traced: AtomicBool,
    /// 线程自己结束时的结束方式：exit(2) 的退出码，或者 seccomp 让它以信号结束（被跟踪的
    /// 线程退出时报告给跟踪者）。
    pub exit_status: Cell<ExitStatus>,
    /// 已进入退出流程（可能还停在 PTRACE_EVENT_EXIT）。
    exit_started: Cell<bool>,
    /// seccomp 的状态（[`crate::seccomp`]），只在持有 BKL 时访问。
    pub seccomp: RefCell<crate::seccomp::Seccomp>,
    /// 线程有 seccomp：系统调用要先检查（`syscall::fast` 不持 BKL 时读它）。由
    /// [`crate::seccomp::publish`] 随 `seccomp` 更新。
    pub seccomp_on: AtomicBool,
    /// 过滤器对哪些编号总是返回 ALLOW（[`crate::seccomp::always_allowed`]）：这些系统调用照常
    /// 走不持 BKL 的路径。由 [`crate::seccomp::publish`] 随 `seccomp` 更新。
    pub seccomp_allow: [AtomicU64; crate::seccomp::CACHE_WORDS],
    /// 跟踪者用 PTRACE_SINGLESTEP 让它继续了：运行循环每次只让解释器执行一条指令
    /// （[`crate::ptrace::step_done`]），不持 BKL 时读它。
    pub stepping: AtomicBool,
    /// 单步执行了 svc：下一次单步不执行指令，原地再报告一次（[`crate::ptrace::syscall_stepped`]）。
    pub step_again: AtomicBool,
    /// 只有线程自己访问的状态。
    pub state: std::cell::RefCell<TaskState>,
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
    /// vfork 的子进程：execve 或退出时置位，并直接叫醒父进程（它的事件）让它继续。
    pub vfork_done: Option<(Rc<Cell<bool>>, Arc<Event>)>,
    /// 被信号打断的 nanosleep 剩下的部分（`restart_syscall`）：到期时刻和 `rem` 的地址。
    pub restart: Option<(std::time::Instant, u64)>,
    /// 不持 BKL 的 nanosleep（`syscall::fast`）还没睡够就被叫醒，交给持锁的路径：原来的
    /// 到期时刻。持锁的路径接着睡到它，被信号打断时也按它算剩下多少。
    pub fast_sleep: Option<std::time::Instant>,
}

impl Task {
    /// 为 `cpu` 新建线程。
    pub fn new(
        tid: Tid,
        tgid: Tid,
        cpu: &Cpu,
        state: TaskState,
        ns: crate::nsproxy::NsProxy,
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
            pid_ns_children: RefCell::default(),
            ns: RefCell::new(ns),
            saved_sigmask: Cell::new(None),
            killed: Cell::new(false),
            pdeathsig: Cell::new(0),
            sleeping: AtomicBool::new(false),
            comm: Cell::new(comm),
            nice: Cell::new(0),
            sched: Cell::new((0, 0)),
            ioprio: RefCell::new(None),
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
            cpu_base: Cell::new((0, 0)),
            cpu_prev: Cell::new(CpuTime::default()),
            ctxt_base: Cell::new((0, 0)),
            host_tid: Cell::new(0),
            trace: RefCell::default(),
            syscall_traced: AtomicBool::new(false),
            exit_status: Cell::new(ExitStatus::Exited(0)),
            exit_started: Cell::new(false),
            seccomp: RefCell::default(),
            seccomp_on: AtomicBool::new(false),
            seccomp_allow: [const { AtomicU64::new(0) }; crate::seccomp::CACHE_WORDS],
            stepping: AtomicBool::new(false),
            step_again: AtomicBool::new(false),
            state: std::cell::RefCell::new(state),
        }))
    }

    /// Linux 的 synchronize_group_exit：最后一个进入退出流程的线程确定进程的退出
    /// 状态，已有 exit_group/致命信号的状态不覆盖。必须在 PTRACE_EVENT_EXIT 之前做，
    /// 不能按跟踪者放行之后的资源释放顺序决定。
    pub(crate) fn begin_exit(&self, k: &Kernel, group: &RefCell<ThreadGroup>, status: ExitStatus) {
        if self.exit_started.replace(true) {
            return;
        }
        self.exit_status.set(status);
        let mut g = group.borrow_mut();
        if g.threads
            .iter()
            .all(|tid| k.tasks.get(tid).is_none_or(|t| t.exit_started.get()))
        {
            g.exiting.get_or_insert(status);
        }
    }

    /// 线程在内核里等待、还没被叫醒（`/proc/<pid>/stat` 的状态 S）。被叫醒之后到重新拿到
    /// BKL 之前 [`Task::sleeping`] 还在，但它的事件已经触发了：与 Linux 的
    /// `try_to_wake_up` 当场把状态改成可运行一样，算作 R。LTP 靠这个同步，比如 shmctl01
    /// 给停在 pause() 里的子进程发信号，等它的状态回到 S 就当它做完了下一步。
    pub fn asleep(&self) -> bool {
        self.sleeping.load(Ordering::Relaxed) && !self.event.is_signaled()
    }

    /// 线程用掉的 CPU 时间（纳秒），以宿主线程的 CPU 时间计。
    pub fn cpu_ns(&self) -> u64 {
        self.host_cpu_ns().saturating_sub(self.cpu_base.get().0)
    }

    /// 宿主线程总共用掉的 CPU 时间（纳秒）。
    fn host_cpu_ns(&self) -> u64 {
        self.clock
            .get()
            .and_then(|c| appine_hal::clock_gettime(c).ok())
            .map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64)
    }

    /// 由线程自己调用的 [`Task::cpu`]：内核态时间用 getrusage 取，不必读 /proc（线程
    /// 结束时原来占了五分之一）。
    pub fn cpu_self(&self) -> CpuTime {
        let total_ns = self.cpu_ns();
        let sys_ns = appine_hal::thread_system_ns_self().saturating_sub(self.cpu_base.get().1);
        self.cpu_adjust(total_ns, sys_ns)
    }

    /// 线程用掉的 CPU 时间，分出内核态：宿主内核替这个宿主线程干活的时间（转发的
    /// I/O、缺页等）。内核层自己在宿主用户态做的事情算不进去，所以偏少。
    pub fn cpu(&self) -> CpuTime {
        let total_ns = self.cpu_ns();
        let sys_ns = appine_hal::thread_system_ns(self.host_tid.get())
            .unwrap_or(0)
            .saturating_sub(self.cpu_base.get().1);
        self.cpu_adjust(total_ns, sys_ns)
    }

    /// 让用户态、内核态的时间各自不倒退（Linux 的 `cputime_adjust`）。用户态的时间是总数
    /// 减去内核态的，两者来源不同：总数是宿主线程的 CPU 时钟，精确到纳秒；内核态的时间
    /// 读别的线程时只有 `/proc` 里按时钟滴答取整的值，线程自己读的 getrusage 也和总数不在
    /// 同一刻。内核态的时间一下子多出一个滴答时，用户态的时间就会倒退（LTP 的 times03）。
    fn cpu_adjust(&self, total_ns: u64, sys_ns: u64) -> CpuTime {
        let prev = self.cpu_prev.get();
        if total_ns <= prev.total_ns {
            return prev;
        }
        // 之前的内核态时间不超过之前的总数，所以这里不超过 total_ns；用户态时间也不倒退的
        // 话，内核态时间只多出总数增加的部分
        let mut sys_ns = sys_ns.min(total_ns).max(prev.sys_ns);
        if total_ns - sys_ns < prev.user_ns() {
            sys_ns = total_ns - prev.user_ns();
        }
        let now = CpuTime { total_ns, sys_ns };
        self.cpu_prev.set(now);
        now
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
    // 不保留 JoinHandle：会话结束时等的是 kernel.tasks 变空，线程在那之后只剩释放自己的
    // Cpu 和翻译缓存
    pool::spawn(Box::new(move |reused| {
        appine_hal::set_thread_name(&format!("guest-{tid}"));
        thread_main(thread_shared, tid, cpu, reused)
    }))
    .map_err(|_| Errno::EAGAIN)?;
    {
        let st = task.state.borrow();
        st.group.borrow_mut().threads.push(tid);
        st.mm.borrow_mut().add_user(task.sync.clone());
    }
    k.tasks.insert(tid, task);
    Ok(())
}

/// 宿主线程池：guest 线程结束之后，它的宿主线程留一会儿给下一个 guest 线程用。fork 的
/// 时间原来有四分之一花在新建宿主线程上（内核栈、线程栈、TLS），线程结束时还要拆掉。
pub(crate) mod pool {
    use std::collections::VecDeque;
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// 参数是这个宿主线程有没有跑过别的 guest 线程（亲和性这类宿主线程的设置要重新设置）。
    type Job = Box<dyn FnOnce(bool) + Send>;

    struct Pool {
        jobs: VecDeque<Job>,
        idle: usize,
    }

    static POOL: Mutex<Pool> = Mutex::new(Pool {
        jobs: VecDeque::new(),
        idle: 0,
    });
    static READY: Condvar = Condvar::new();

    thread_local! {
        /// 持有 BKL 时交给空闲线程的 job 数：放开 BKL 之后再叫醒它们（[`notify_pending`]），
        /// 否则空闲线程醒来马上又要睡下去等 BKL。
        #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
        static PENDING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// 最多留着几个空闲的宿主线程。
    const MAX_IDLE: usize = 8;
    /// 空闲的宿主线程留多久。
    const LINGER: Duration = Duration::from_secs(5);

    /// 在一个宿主线程上运行 `job`：有空闲的就交给它，没有就新建一个。`job` 不能 panic。
    pub fn spawn(job: Job) -> std::io::Result<()> {
        let mut p = POOL.lock().unwrap_or_else(PoisonError::into_inner);
        if p.idle > p.jobs.len() {
            p.jobs.push_back(job);
            if crate::kernel::bkl_held() {
                PENDING.set(PENDING.get() + 1);
            } else {
                READY.notify_one();
            }
            return Ok(());
        }
        drop(p);
        std::thread::Builder::new()
            .name("guest".into())
            .spawn(move || worker(job))
            .map(drop)
    }

    /// 叫醒持有 BKL 时交了 job 的空闲线程。放开 BKL 之后调用。
    pub(crate) fn notify_pending() {
        for _ in 0..PENDING.replace(0) {
            READY.notify_one();
        }
    }

    fn worker(mut job: Job) {
        let mut reused = false;
        loop {
            job(reused);
            let mut p = POOL.lock().unwrap_or_else(PoisonError::into_inner);
            if p.idle >= MAX_IDLE {
                return;
            }
            p.idle += 1;
            let deadline = Instant::now() + LINGER;
            job = loop {
                if let Some(j) = p.jobs.pop_front() {
                    p.idle -= 1;
                    break j;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    p.idle -= 1;
                    return;
                }
                p = READY
                    .wait_timeout(p, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            };
            reused = true;
        }
    }
}

/// 一个 guest 线程和它的线程组（弱引用）。
type Current = (Weak<Task>, Weak<RefCell<ThreadGroup>>);

thread_local! {
    /// 这个宿主线程正在执行的 guest 线程和它的线程组（[`current`]）。只在持有 BKL 时
    /// 设置和清掉（[`exit`]）。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static CURRENT: std::cell::RefCell<Option<Current>> = const { std::cell::RefCell::new(None) };
}

/// 调用者：这个宿主线程正在执行的 guest 线程和它的线程组。不在 guest 线程上（定时器线程、
/// 宿主调用 Machine 的线程）时是 None。系统调用里 [`Task::state`] 已经被借走，拿不到
/// 线程组，所以线程开始时记下。
pub(crate) fn current() -> Option<(Rc<Task>, Rc<RefCell<ThreadGroup>>)> {
    // upgrade 改弱引用指向的对象的计数
    debug_assert!(crate::kernel::bkl_held(), "task::current without the BKL");
    CURRENT.with_borrow(|c| {
        let (task, group) = c.as_ref()?;
        Some((task.upgrade()?, group.upgrade()?))
    })
}

fn thread_main(shared: Arc<Shared>, tid: Tid, mut cpu: Cpu, reused: bool) {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| run(&shared, tid, &mut cpu, reused)));
    // CURRENT 已经由 exit 在持有 BKL 时清掉了
    if r.is_err() {
        // 内核层的 bug，而且不在兜住的地方（crate::bug）：线程找不到自己，或者出错之后的
        // 收拾本身又出了错。BKL 保护的状态可能已经不一致，不能让其他线程继续
        eprintln!("appine: internal error in guest thread {tid}, aborting");
        std::process::abort();
    }
}

/// `reused`：宿主线程跑过别的 guest 线程（[`pool`]）。
fn run(shared: &Arc<Shared>, tid: Tid, cpu: &mut Cpu, reused: bool) {
    let k = shared.lock();
    let task = k.tasks[&tid].clone();
    let group = Rc::downgrade(&task.state.borrow().group);
    CURRENT.set(Some((Rc::downgrade(&task), group)));
    // 线程开始时的 bug 和进入内核时的一样只结束这个进程（crate::bug）
    let started = crate::bug::catch(|| {
        crate::bug::trap(Trap::Start, task.tgid);
        task.clock.set(appine_hal::thread_cpu_clock());
        task.cpu_base
            .set((task.host_cpu_ns(), appine_hal::thread_system_ns_self()));
        task.host_tid.set(appine_hal::gettid());
        task.ctxt_base
            .set(appine_hal::thread_ctxt_switches_self().unwrap_or((0, 0)));
        crate::syscall::host_started(&task, reused);
        task.state.borrow().mm.borrow().generations()
    });
    let Some(mut generations) = started else {
        let what = format_args!("kernel panic while starting the thread");
        crate::bug::kill(&k, &task, cpu.state().pc, what);
        drop(k);
        return exit(shared, task, cpu.state(), None);
    };
    let (sync, tier) = (task.sync.clone(), k.tier);
    // Tier 1 的翻译由这个地址空间里的线程共用，同一个程序的几个地址空间也可能共用
    // （crate::mm::CodeGroup）。Mm 只在持有 BKL 时访问；挂上去（清空跳转缓存等）不用 BKL
    let space_of = |k: &Kernel| match tier {
        Tier::Interpreter => None,
        Tier::Dbt => task
            .state
            .borrow()
            .mm
            .borrow_mut()
            .code_space(&k.code_groups),
    };
    let attach = |space: Option<Arc<CodeSpace>>| {
        space.and_then(|s| Dbt::attach(&s.space, s.tag).map(|d| (d, s)))
    };
    let space = space_of(&k);
    let mut fast = Fast::new(shared, &task, &task.state.borrow());
    drop(k);
    let mut dbt = attach(space);
    let load = |g: &Generations| {
        let tlb = g.tlb.load(Ordering::SeqCst);
        (
            tlb,
            g.code.load(Ordering::SeqCst),
            g.space.load(Ordering::SeqCst),
        )
    };
    let mut seen = load(&generations);
    loop {
        // 所有线程都在等待时定时器线程的定期检查会停下来（crate::oom、crate::loadavg）
        shared.resume_checks();
        // 先标记、再读代数，与 Mm::sync 的"先改代数、再看标记"配对
        sync.in_guest.store(true, Ordering::SeqCst);
        let now = load(&generations);
        // 地址空间离开了共用翻译的组（crate::mm::CodeGroup）：换用它自己的翻译。离开时做了
        // shootdown，所以这个线程不会再执行组里的翻译
        if now.2 != seen.2 {
            sync.in_guest.store(false, Ordering::SeqCst);
            if dbt.take().is_some() {
                let space = space_of(&shared.lock());
                dbt = attach(space);
            }
            seen.2 = now.2;
            continue;
        }
        if now.0 != seen.0 {
            cpu.tlb_flush();
        }
        // 只有代码变了才需要作废翻译：按错过的范围逐页作废，记录不全时整体清空。翻译是
        // 共用的，第一个看到新代数的线程做一次就够了
        if now.1 != seen.1
            && let Some((d, space)) = &mut dbt
        {
            let mut done = space.seen.lock().unwrap();
            if *done < now.1 {
                match generations.log.lock().unwrap().since(*done, now.1) {
                    Some(ranges) => ranges
                        .iter()
                        .for_each(|&(start, len)| d.invalidate(cpu, start, len)),
                    None => d.flush(),
                }
                *done = now.1;
            }
        }
        seen = now;
        // PTRACE_SINGLESTEP：解释器只执行一条指令。执行完了（ic ivau 也算）进内核报告；
        // svc 在系统调用做完之后报告，访存出错、brk 投递它们自己的信号（crate::ptrace）
        let (exit, stepped) = if task.stepping.load(Ordering::Relaxed) {
            // 单步执行了 svc 之后的这一次不执行指令（crate::ptrace::syscall_stepped）
            let r = if task.step_again.swap(false, Ordering::Relaxed) {
                None
            } else {
                cpu.step()
            };
            match r {
                None => (Exit::Kick, true),
                Some(e @ Exit::InvalidateCode { .. }) => (e, true),
                Some(e) => (e, false),
            }
        } else {
            // 有硬件断点（ptrace 设置的）时只有解释器检查它们。观察点由 Tier 1 把被观察的页
            // 上的访问交给解释器（CpuState::set_hw_debug）
            let exit = match &mut dbt {
                Some((d, _)) if !cpu.state().hw_breakpoints_active() => d.run(cpu),
                _ => cpu.run(),
            };
            (exit, false)
        };
        // 不持 BKL 就能做完的系统调用（syscall::fast）。它访问 guest 内存时仍然标记着在
        // 执行 guest 代码。内核层的 bug 与持锁的路径一样只结束这个进程（crate::bug）：
        // 这里展开经过的帧里没有 Rc，原来一直展开到 thread_main，终止整个 App
        let done = match exit {
            Exit::Svc { .. } => crate::bug::catch(|| fast.syscall(shared, &task, cpu)),
            _ => Some(false),
        };
        sync.in_guest.store(false, Ordering::SeqCst);
        match done {
            Some(true) => continue,
            Some(false) => {}
            None => {
                task.sleeping.store(false, Ordering::Relaxed);
                let nr = cpu.state().x[8];
                let what = format_args!("kernel panic in syscall {nr} without the BKL");
                crate::bug::kill(&shared.lock(), &task, cpu.state().pc, what);
                break;
            }
        }

        // 内核层的 bug 只结束这个进程（crate::bug）
        let pc = cpu.state().pc;
        let entered = crate::bug::catch(|| {
            let k = shared.lock();
            let mut state = task.state.borrow_mut();
            let mut cx = Cx::new(shared, k, &task, &mut state);
            if stepped {
                crate::ptrace::step_done(&mut cx, cpu);
            }
            let flow = handle(&mut cx, cpu, exit);
            if let Flow::Exec = flow {
                generations = cx.mm().generations();
            }
            fast.refresh(shared, cx.task, cx.t);
            flow
        });
        let flow = entered.unwrap_or_else(|| {
            task.sleeping.store(false, Ordering::Relaxed);
            let what = match exit {
                Exit::Svc { .. } => format!("kernel panic in syscall {}", cpu.state().x[8]),
                _ => format!("kernel panic while handling {exit:?}"),
            };
            crate::bug::kill(&shared.lock(), &task, pc, format_args!("{what}"));
            Flow::Exit
        });
        match flow {
            Flow::Continue => {}
            Flow::Exec => {
                seen = load(&generations);
                // 换了地址空间，换用它的翻译。先放开旧的：旧地址空间没人用了，它的翻译放回
                // 池里，新的地址空间多半就拿到它。又一次 exec 的多半是同一个程序（shell
                // 脚本里的 busybox），保留下来的翻译核对后继续用
                if dbt.take().is_some() {
                    let space = space_of(&shared.lock());
                    dbt = attach(space);
                }
            }
            Flow::Exit => break,
        }
    }
    exit(shared, task, cpu.state(), dbt.map(|(d, _)| d.stats()));
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
    // Tier 1 的 ic ivau 按页记下、到 dsb、isb 才交出来（Exit::InvalidateCode）：因为别的
    // 原因退出时在这里作废
    let ivau = std::mem::take(&mut cpu.state_mut().ivau_page);
    let pc = cpu.state().pc;
    let (sig, info) = match exit {
        Exit::InvalidateCode { addr } => {
            invalidate_code(cx, &[addr & !(PAGE_SIZE - 1), ivau]);
            return signal::deliver(cx, cpu, false);
        }
        _ if ivau != 0 => {
            invalidate_code(cx, &[ivau]);
            return handle(cx, cpu, exit);
        }
        Exit::Svc { .. } => return syscall::dispatch(cx, cpu),
        Exit::Kick => return signal::deliver(cx, cpu, false),
        Exit::Undefined { insn } => {
            crate::debug::log(format_args!(
                "undefined instruction {insn:#010x} at {pc:#x}"
            ));
            (Signal::ILL, SigInfo::fault(code::ILL_ILLOPC, pc, ESR_IL))
        }
        // Linux 发断点的 SIGTRAP 时信号帧里没有 esr_context
        Exit::Breakpoint { .. } => (Signal::TRAP, SigInfo::fault(code::TRAP_BRKPT, pc, 0)),
        // 引擎的 bug（crate::bug）
        Exit::HostFault {
            signal,
            code,
            addr,
            pc: host_pc,
        } => {
            crate::bug::kill(
                cx.kr(),
                cx.task,
                pc,
                format_args!(
                    "host signal {signal} (code {code}) in the engine at {host_pc:#x}, \
                     address {addr:#x}"
                ),
            );
            return Flow::Exit;
        }
        Exit::Fault {
            addr,
            access,
            kind: FaultKind::Page,
        } => {
            // fork 之后写时复制的页：换成可写的，重新执行
            if access == Access::Write && cx.mm().cow_break(addr) {
                return signal::deliver(cx, cpu, false);
            }
            // 等 BKL 的时候页表可能已经改好了（借用方还回了写时复制的页、别的线程
            // mprotect），与 Linux 一样重新执行。引擎报告的是出错的那一页（跨页时是后一页）
            let need = match access {
                Access::Exec => prot::EXEC,
                Access::Read => prot::READ,
                Access::Write => prot::WRITE,
            };
            if cx.mm().mmu().translate(addr, need).is_some() {
                return signal::deliver(cx, cpu, false);
            }
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
            // 向下增长的映射（主线程栈、MAP_GROWSDOWN）长到这里：重新执行
            if cx.grow_stack(addr) {
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
        // 硬件观察点（访问的起点）、断点（pc）：指令还没有执行，与 Linux 的
        // ptrace_hbptriggered 一样没有 esr_context
        Exit::Fault {
            addr,
            kind: FaultKind::Watchpoint | FaultKind::HwBreakpoint,
            ..
        } => (Signal::TRAP, SigInfo::fault(code::TRAP_HWBKPT, addr, 0)),
    };
    signal::force(cx, sig, info);
    signal::deliver(cx, cpu, false)
}

/// 作废 `pages` 里各页（0 跳过）的翻译，并让共用地址空间的其他线程知道。
fn invalidate_code(cx: &mut Cx<'_>, pages: &[u64]) {
    let sync = cx.task.sync.clone();
    let mut mm = cx.mm_mut();
    for &page in pages.iter().filter(|&&p| p != 0) {
        mm.invalidate_code(page, PAGE_SIZE);
    }
    mm.sync(&sync);
}

/// 线程离开它的地址空间（退出、execve）：处理 robust futex 链表，清零 `clear_child_tid`
/// 并唤醒等它的线程（pthread_join），让 vfork 的父进程继续，从使用者里注销。
pub(crate) fn release_mm(shared: &Shared, task: &Task, st: &mut TaskState) {
    let (head, _) = task.robust_list.replace((0, 0));
    // 锁里记的是线程自己看到的 tid（它的 pid 命名空间里的）
    let tid = st.group.borrow().pid_ns.vnr(task.tid.get()).unwrap_or(0);
    let mut futexes = shared.futexes();
    crate::futex::exit_robust_list(&mut futexes, &st.mm.borrow(), head, tid as u32);
    let addr = std::mem::take(&mut st.clear_child_tid);
    {
        let mm = st.mm.borrow();
        // 与 Linux 的 mm_release 一样，地址空间没有别人在用时不必通知
        if addr != 0
            && mm.shared_with_others()
            && mm.write(UserSlice::new(addr, 4), &[0; 4]).is_ok()
            && let Some(key) = mm.futex_key(addr)
        {
            futexes.wake(key, 1, u32::MAX);
        }
    }
    drop(futexes);
    // 父进程只等直接的通知：原来叫醒所有等待者，每次 posix_spawn（musl、glibc 都用
    // CLONE_VFORK）都把读着管道、停在 poll 里的线程叫醒一遍
    if let Some((done, parent)) = st.vfork_done.take() {
        done.set(true);
        crate::kernel::signal_later(&parent);
    }
    st.mm.borrow_mut().remove_user(&task.sync);
}

/// 线程退出：释放它的资源；它是线程组的最后一个线程时结束整个进程。`pc` 是线程最后的
/// guest pc。
///
/// 每一步分别兜住内核层的 bug（crate::bug）：出错时结束这个进程，后面的步骤照样做，所以
/// 线程总会从内核里注销，进程也总会结束，等它的父进程和宿主不会卡住。
fn exit(shared: &Arc<Shared>, task: Rc<Task>, state: &CpuState, stats: Option<DbtStats>) {
    let pc = state.pc;
    let mut k = shared.lock();
    let (tid, tgid) = (task.tid.get(), task.tgid);
    let group = task.state.borrow().group.clone();
    let vfs = task.state.borrow().vfs.clone();
    task.begin_exit(&k, &group, task.exit_status.get());
    let released = crate::bug::catch(|| {
        crate::bug::trap(Trap::Exit, tid);
        // 进程正在写 core 文件：留下寄存器（crate::coredump）
        if let Some(core) = group.borrow_mut().core.as_mut() {
            core.push(crate::coredump::ThreadCore::of(&task, state));
        }
        if let Some(s) = stats {
            let total = k.dbt_stats.get_or_insert_with(DbtStats::default);
            add_stats(total, &s);
        }
        let cpu = task.cpu_self();
        k.cpu += cpu;
        group.borrow_mut().cpu += cpu;
        let mut st = task.state.borrow_mut();
        // 进程的最后一个线程：退出之前给驻留内存的峰值取一次样
        if group.borrow().threads.len() == 1 {
            group.borrow_mut().sample_rss(&st.mm.borrow());
        }
        release_mm(shared, &task, &mut st);
        // 被跟踪的线程退出了：报告给跟踪者（crate::ptrace）
        let status = task.exit_status.get();
        crate::ptrace::tracee_exited(&mut k, &task, &mut group.borrow_mut(), status);
        crate::seccomp::exited(&task, &mut group.borrow_mut());
    });
    if released.is_none() {
        let what = format_args!("kernel panic while the thread was exiting");
        crate::bug::kill(&k, &task, pc, what);
    }
    k.tasks.remove(&tid);
    k.release_pid(tid);
    let last = {
        let mut g = group.borrow_mut();
        g.threads.retain(|&t| t != tid);
        g.threads.is_empty()
    };
    // 这个线程建的子进程交给进程里还活着的线程（Linux 的 forget_original_parent）；最后一个
    // 线程的由 finish_group 过继出去
    if !last {
        let to = crate::process::alive_thread(&k, &group.borrow());
        let mine: Vec<Tid> = group
            .borrow()
            .children
            .iter()
            .copied()
            .filter(|c| {
                k.groups
                    .get(c)
                    .is_some_and(|g| g.borrow().parent_thread == tid)
            })
            .collect();
        let signals = crate::process::pdeath_signals(&k, &mine);
        crate::process::hand_thread_children(&k, &group, tid, to);
        let ruid = group.borrow().cred.ruid;
        crate::process::send_pdeath(&k, signals, tgid, ruid);
    }
    // 后面的步骤出错时：还有别的线程就结束这个进程，否则它已经结束了，只报告
    let failed = |k: &Kernel, step: &str| {
        if last {
            crate::bug::report(format_args!(
                "kernel panic while pid {tgid} was exiting ({step})"
            ));
        } else {
            let what = format_args!("kernel panic while thread {tid} was exiting ({step})");
            crate::bug::kill_process(k, tgid, what);
        }
    };
    // finish_group 自己分步兜住，这里只剩读进程状态的那几行
    if last && crate::bug::catch(|| crate::process::finish_group(&mut k, &group)).is_none() {
        failed(&k, "finishing the process");
    }
    // 在持有 BKL 时释放线程的资源：地址空间、fd 表（关闭管道会唤醒另一端）……线程对象
    // 可能还有别处拿着，命名空间先放掉
    *task.ns.borrow_mut() = k.init_ns.clone();
    if crate::bug::catch(|| drop((group, task))).is_none() {
        failed(&k, "releasing its resources");
    }
    // fd 表里的 unix socket 关掉之后可能只剩在途的环
    if last
        && !k.unix_inflight.is_empty()
        && crate::bug::catch(|| crate::unix::collect(&mut k)).is_none()
    {
        failed(&k, "collecting unix sockets");
    }
    // 最后一个用某个 IPC 命名空间的线程退出了：丢掉里面的对象
    k.collect_ipc();
    // 最后一个用某个挂载命名空间的线程退出了：卸下它的挂载（里面的 tmpfs 随之删掉）
    if let Some(vfs) = &vfs
        && crate::bug::catch(|| crate::syscall::collect_namespaces(&k, vfs)).is_none()
    {
        failed(&k, "collecting mount namespaces");
    }
    // 子进程的状态变化由 finish_group 另外请求（wake::CHILD），vfork 的父进程由
    // release_mm 叫醒，关闭文件的各自请求；这里只叫醒等线程结束的（execve、关闭）
    request_wake_on(wake::EXITED);
    // 线程局部变量里的两个弱引用也要在持有 BKL 时丢掉：Rc 的计数不是原子的，放开之后再丢，
    // 会和同时持有 BKL 的线程（同一个进程里刚开始的线程做 Rc::downgrade）一起改同一个
    // 计数，改丢了就提前释放，之后的访问都落在释放掉的内存上
    CURRENT.set(None);
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
    total.insns_grouped += s.insns_grouped;
    total.ungrouped += s.ungrouped;
}
