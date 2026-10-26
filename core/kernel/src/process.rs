//! guest 进程（线程组）（设计文档 §6）。宿主启动的进程都是内置 init（pid 1）的子进程
//! （[`crate::Machine`]）。

use std::cell::RefCell;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use appine_engine::DbtStats;

use crate::cred::{Cred, MAY_EXEC};
use crate::errno::Errno;
use crate::kernel::{INIT, Kernel, Tid, request_wake_on, wake};
use crate::loader::{self, Executable, LoadError};
use crate::signal::{self, Pending, SigAction, SigHandlers, SigInfo, sa};
use crate::terminal::Terminal;
use crate::tty::Tty;
use crate::vfs::store::{Stat, mode};
use crate::vfs::{Node, Pseudo, Vfs};

/// 一个信号编号（1–64）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal(i32);

impl Signal {
    /// 挂起。
    pub const HUP: Signal = Signal(1);
    /// 终端中断。
    pub const INT: Signal = Signal(2);
    /// 终端退出（^\）。
    pub const QUIT: Signal = Signal(3);
    /// 未定义指令。
    pub const ILL: Signal = Signal(4);
    /// 断点。
    pub const TRAP: Signal = Signal(5);
    /// abort。
    pub const ABRT: Signal = Signal(6);
    /// 总线错误、未对齐访问。
    pub const BUS: Signal = Signal(7);
    /// 强制终止。
    pub const KILL: Signal = Signal(9);
    /// 用户自定义信号 1。
    pub const USR1: Signal = Signal(10);
    /// 非法内存访问。
    pub const SEGV: Signal = Signal(11);
    /// 管道断开。
    pub const PIPE: Signal = Signal(13);
    /// 定时器到期。
    pub const ALRM: Signal = Signal(14);
    /// 终止。
    pub const TERM: Signal = Signal(15);
    /// 子进程状态改变。
    pub const CHLD: Signal = Signal(17);
    /// 让停止的进程继续。
    pub const CONT: Signal = Signal(18);
    /// 停止（不能捕获）。
    pub const STOP: Signal = Signal(19);
    /// 终端停止（^Z）。
    pub const TSTP: Signal = Signal(20);
    /// 后台进程读终端。
    pub const TTIN: Signal = Signal(21);
    /// 后台进程写终端或修改终端设置。
    pub const TTOU: Signal = Signal(22);
    /// CPU 时间超过 `RLIMIT_CPU`。
    pub const XCPU: Signal = Signal(24);
    /// 文件大小超过 `RLIMIT_FSIZE`。
    pub const XFSZ: Signal = Signal(25);
    /// `ITIMER_VIRTUAL` 到期。
    pub const VTALRM: Signal = Signal(26);
    /// `ITIMER_PROF` 到期。
    pub const PROF: Signal = Signal(27);
    /// 终端窗口大小改变。
    pub const WINCH: Signal = Signal(28);

    /// 最大的信号编号。
    pub const MAX: i32 = 64;

    /// 由编号构造，编号必须在 1–64 之内。
    pub fn new(n: i32) -> Option<Signal> {
        (1..=Self::MAX).contains(&n).then_some(Signal(n))
    }

    /// Linux 信号编号。
    pub fn number(self) -> i32 {
        self.0
    }

    /// 在信号集位图里对应的位。
    pub(crate) fn bit(self) -> u64 {
        1 << (self.0 - 1)
    }

    /// 在按信号编号排列的表里的下标。
    pub(crate) fn index(self) -> usize {
        self.0 as usize - 1
    }

    /// 是不是实时信号（32 号起；libc 自己用掉前几个，它的 SIGRTMIN 更大）。
    pub(crate) fn is_realtime(self) -> bool {
        self.0 >= 32
    }

    /// 是不是停止信号（SIGSTOP、SIGTSTP、SIGTTIN、SIGTTOU）。
    pub(crate) fn is_stop(self) -> bool {
        matches!(self.0, 19..=22)
    }

    /// 默认动作是否是忽略：SIGCHLD、SIGCONT（它的"继续"在产生时就生效了）、SIGURG、SIGWINCH。
    pub(crate) fn ignored_by_default(self) -> bool {
        matches!(self.0, 17 | 18 | 23 | 28)
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [&str; 31] = [
            "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV",
            "USR2", "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN",
            "TTOU", "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
        ];
        match self.0 {
            n @ 1..=31 => write!(f, "SIG{}", NAMES[n as usize - 1]),
            n => write!(f, "SIGRT{}", n - 32),
        }
    }
}

/// guest 进程的结束方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// 调用了 `exit` 或 `exit_group`。
    Exited(i32),
    /// 被信号终止。
    Signaled {
        /// 信号。
        signal: Signal,
        /// 同步异常（访存出错、未定义指令、断点）时出错指令的地址。
        pc: Option<u64>,
        /// 访存出错时的 guest 地址。
        fault_addr: Option<u64>,
    },
}

impl ExitStatus {
    /// wait4 报告的状态字（`WIFEXITED` 等宏解码的格式）。
    pub(crate) fn wait_status(self) -> i32 {
        match self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled { signal, .. } => signal.number(),
        }
    }
}

/// 执行引擎（设计文档 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Tier 0：解释器。
    #[default]
    Interpreter,
    /// Tier 1：同 ISA 直通式 DBT（只支持 aarch64 Linux 宿主）。
    Dbt,
}

/// 运行选项。
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// 执行引擎。
    pub tier: Tier,
    /// inode 仓库所在的宿主目录。设置后程序路径按 guest 路径在仓库里解析，
    /// 否则按宿主路径读取，guest 也看不到任何文件系统。
    pub rootfs: Option<PathBuf>,
    /// 设置时，第一个进程以一个进程内 PTY 为 stdin/stdout/stderr 和控制终端，PTY 接到
    /// 这些宿主 fd 上；否则 guest 直接使用宿主的 stdin/stdout/stderr。
    pub terminal: Option<Terminal>,
    /// 宿主进程匿名内存的上限，见 [`crate::MachineOptions::memory_limit`]。
    pub memory_limit: Option<u64>,
}

/// 一次运行的结果。
#[derive(Debug, Clone, Copy)]
pub struct RunReport {
    /// 第一个 guest 进程的结束方式。
    pub status: ExitStatus,
    /// 使用 Tier 1 时所有线程的统计之和。
    pub dbt_stats: Option<DbtStats>,
}

/// guest 没能开始运行的原因。
#[derive(Debug)]
pub enum RunError {
    /// 读取宿主上的程序文件失败。
    Io(io::Error),
    /// 打开 inode 仓库失败。
    Rootfs(Errno),
    /// 在 guest 文件系统里找不到或不能执行程序。
    Exec(Errno),
    /// 加载失败。
    Load(LoadError),
    /// 控制台不存在，或者已经属于一个会话。
    Console(Errno),
    /// 当前宿主不支持所选的执行引擎。
    TierUnavailable,
    /// 内核层的 bug（panic），报告在宿主的 stderr 上（设计文档 §12 第 5 条）。
    Internal,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(err) => err.fmt(f),
            RunError::Rootfs(errno) => write!(f, "cannot open the rootfs: {errno}"),
            RunError::Exec(errno) if *errno == Errno::ENOENT => {
                f.write_str("no such file or directory")
            }
            RunError::Exec(errno) if *errno == Errno::EACCES => f.write_str("permission denied"),
            RunError::Exec(errno) => write!(f, "cannot execute: {errno}"),
            RunError::Load(err) => err.fmt(f),
            RunError::Console(errno) => write!(f, "cannot use the console: {errno}"),
            RunError::TierUnavailable => {
                f.write_str("the selected tier is not supported on this host")
            }
            RunError::Internal => f.write_str("internal error"),
        }
    }
}

impl std::error::Error for RunError {}

/// `RLIMIT_CPU`（秒）。
pub(crate) const RLIMIT_CPU: u32 = 0;
/// `RLIMIT_FSIZE`。
pub(crate) const RLIMIT_FSIZE: u32 = 1;
/// `RLIMIT_STACK`。
pub(crate) const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_CORE`。
pub(crate) const RLIMIT_CORE: u32 = 4;
/// `RLIMIT_NPROC`：一个真实用户 ID 最多有多少个线程。
pub(crate) const RLIMIT_NPROC: u32 = 6;
/// `RLIMIT_NOFILE`。
pub(crate) const RLIMIT_NOFILE: u32 = 7;
/// `RLIMIT_MEMLOCK`。
pub(crate) const RLIMIT_MEMLOCK: u32 = 8;
/// `RLIMIT_SIGPENDING`。
pub(crate) const RLIMIT_SIGPENDING: u32 = 11;
/// `RLIMIT_NICE`：不用 `CAP_SYS_NICE` 就能调到的最低 nice 值，记作 20 - nice。
pub(crate) const RLIMIT_NICE: u32 = 13;
/// `RLIMIT_RTPRIO`。
const RLIMIT_RTPRIO: u32 = 14;
/// 资源种类数（`RLIM_NLIMITS`）。
pub(crate) const RLIM_NLIMITS: usize = 16;

/// `threads-max` 能写入的最小值（Linux 的 `MIN_THREADS`）。
pub(crate) const MIN_THREADS: usize = 20;

/// `/proc/sys/kernel/threads-max` 的默认值：同时存在的线程数的上限，挡住 fork 炸弹，root
/// 也受它限制（`RLIMIT_NPROC` 管不了 root）。每个 guest 线程是一个宿主线程，还占一个
/// 宿主 fd（等待用的 eventfd），所以按物理内存每 MB 一个（Linux 是每 128 KB 一个），
/// 并且不超过宿主 fd 上限的一半，给打开的文件留出余地。
pub(crate) fn default_threads_max() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        let by_memory = match appine_hal::memory_info().0 {
            0 => 4096,
            total => (total >> 20) as usize,
        };
        let by_fds = usize::try_from(appine_hal::fd_limit() / 2).unwrap_or(usize::MAX);
        by_memory.min(by_fds).max(MIN_THREADS)
    })
}

fn default_rlimits() -> [(u64, u64); RLIM_NLIMITS] {
    let mut r = [(u64::MAX, u64::MAX); RLIM_NLIMITS];
    r[RLIMIT_STACK as usize] = (loader::STACK_SIZE, u64::MAX);
    r[RLIMIT_CORE as usize] = (0, u64::MAX);
    let (soft, hard) = crate::fd::DEFAULT_NOFILE;
    r[RLIMIT_NOFILE as usize] = (soft as u64, hard as u64);
    // 与 Linux 5.16 起的默认值相同
    r[RLIMIT_MEMLOCK as usize] = (8 << 20, 8 << 20);
    // 与 Linux 一样是 threads-max 的一半；root 不受它限制
    let nproc = default_threads_max() as u64 / 2;
    r[RLIMIT_NPROC as usize] = (nproc, nproc);
    // Linux 按内存大小算（max_threads / 2），4 GB 内存时是这个数
    r[RLIMIT_SIGPENDING as usize] = (16384, 16384);
    r[RLIMIT_NICE as usize] = (0, 0);
    r[RLIMIT_RTPRIO as usize] = (0, 0);
    r
}

/// 一个 guest 进程，即线程组。线程退出之后线程组可能作为僵尸进程留下来，直到父进程回收。
pub(crate) struct ThreadGroup {
    /// pid。
    pub tgid: Tid,
    /// 父进程。
    pub ppid: Tid,
    /// 进程组。
    pub pgid: Tid,
    /// 会话。
    pub sid: Tid,
    /// 活着的线程，第一个是主线程（如果它还活着）。
    pub threads: Vec<Tid>,
    /// 子进程（含僵尸进程）。
    pub children: Vec<Tid>,
    /// 发给整个进程、尚未处理的信号。
    pub pending: Pending,
    /// 信号处理方式。
    pub sighand: Rc<RefCell<SigHandlers>>,
    /// 资源限制：(软限制, 硬限制)。
    pub rlimits: [(u64, u64); RLIM_NLIMITS],
    /// 正在整体退出（exit_group、致命信号）时的结束方式。
    pub exiting: Option<ExitStatus>,
    /// 主线程用 exit(2) 退出时的退出码。
    pub leader_code: i32,
    /// 所有线程都已退出、等待父进程回收时的结束方式。
    pub zombie: Option<ExitStatus>,
    /// 退出时发给父进程的信号（clone 的低 8 位，通常是 SIGCHLD），0 表示不发。
    pub exit_signal: i32,
    /// 控制终端。
    pub ctty: Option<Rc<RefCell<Tty>>>,
    /// 整个线程组被停止信号停住了。
    pub stopped: bool,
    /// 还没有被父进程的 wait 取走的"停止"事件。
    pub stop_report: Option<Signal>,
    /// 还没有被父进程的 wait 取走的"继续"事件。
    pub cont_report: bool,
    /// 当前程序的路径（`/proc/<pid>/exe`）。
    pub exe: Vec<u8>,
    /// 创建时刻，开机以来的时钟滴答数（`/proc/<pid>/stat`）。
    pub start_ticks: u64,
    /// 已退出线程用掉的 CPU 时间。
    pub cpu: CpuTime,
    /// 已回收的子进程用掉的 CPU 时间。
    pub child_cpu: CpuTime,
    /// 驻留内存的峰值（kB，`ru_maxrss`）：getrusage、execve、退出时取样，跨 execve 保留。
    pub maxrss_kb: u64,
    /// 已回收的子进程（包括它们回收的）里最大的 `maxrss_kb`。
    pub child_maxrss_kb: u64,
    /// getrusage 上次取样的时刻（[`ThreadGroup::sample_rss_throttled`]）。
    pub rss_sampled: Option<std::time::Instant>,
    /// `ITIMER_REAL`：fork 不继承，execve 保留。
    pub itimer: Option<crate::itimer::ITimer>,
    /// `ITIMER_VIRTUAL`、`ITIMER_PROF`，按进程用掉的 CPU 时间计时。
    pub cpu_itimers: [Option<crate::itimer::CpuTimer>; 2],
    /// `PR_SET_PDEATHSIG`：父进程退出时收到的信号，0 表示不发；fork 不继承。
    pub pdeathsig: i32,
    /// `PR_SET_DUMPABLE`，fork 继承。
    pub dumpable: bool,
    /// `PR_SET_NO_NEW_PRIVS`，fork 和 execve 都保留。
    pub no_new_privs: bool,
    /// `/proc/<pid>/oom_score_adj`（-1000–1000），fork 继承。OOM killer 挑进程时用
    /// （[`crate::oom`]），-1000 表示不挑它。
    pub oom_score_adj: i32,
    /// 没有 `CAP_SYS_RESOURCE` 时 `oom_score_adj` 能调到的最低值：有这个能力的写入者
    /// 写的值，fork 继承。
    pub oom_score_adj_min: i32,
    /// 凭据，fork 继承。
    pub cred: Cred,
    /// personality(2) 的执行域和标志：只记录，fork 和 execve 都保留。
    pub personality: u32,
    /// 执行过 execve（父进程不能再 setpgid 它）；fork 出来的进程从 false 开始。
    pub execed: bool,
    /// `PR_SET_THP_DISABLE`（没有透明大页，只记录），fork 继承。
    pub thp_disable: bool,
    /// POSIX 定时器（timer_create），按 id；fork 不继承，execve 时删除。
    pub posix_timers: std::collections::BTreeMap<i32, crate::itimer::PosixTimer>,
    /// AIO 上下文，按 id（`syscall::aio`）。
    pub aio: std::collections::BTreeMap<u64, Rc<RefCell<crate::syscall::AioContext>>>,
    /// 进程的编号，不重复：pid 被新进程重新使用之后用它区分新旧进程（`/proc/<pid>` 的
    /// inode）。
    pub serial: u64,
}

impl ThreadGroup {
    pub fn new(tgid: Tid, ppid: Tid, sighand: Rc<RefCell<SigHandlers>>) -> ThreadGroup {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        ThreadGroup {
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            tgid,
            ppid,
            pgid: tgid,
            sid: tgid,
            threads: Vec::new(),
            children: Vec::new(),
            pending: Pending::default(),
            sighand,
            rlimits: default_rlimits(),
            exiting: None,
            leader_code: 0,
            zombie: None,
            exit_signal: Signal::CHLD.number(),
            ctty: None,
            stopped: false,
            stop_report: None,
            cont_report: false,
            exe: Vec::new(),
            start_ticks: crate::pseudo::uptime_ns() / (1_000_000_000 / loader::CLOCK_TICKS),
            cpu: CpuTime::default(),
            child_cpu: CpuTime::default(),
            maxrss_kb: 0,
            child_maxrss_kb: 0,
            rss_sampled: None,
            itimer: None,
            cpu_itimers: [None; 2],
            pdeathsig: 0,
            dumpable: true,
            no_new_privs: false,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            cred: Cred::root(),
            personality: 0,
            thp_disable: false,
            execed: false,
            posix_timers: std::collections::BTreeMap::new(),
            aio: std::collections::BTreeMap::new(),
        }
    }
}

impl ThreadGroup {
    /// 用掉的 CPU 时间：已退出的线程加上活着的线程。
    pub fn cpu(&self, k: &Kernel) -> CpuTime {
        let threads = self.threads.iter().filter_map(|t| k.tasks.get(t));
        threads.fold(self.cpu, |sum, t| sum + t.cpu())
    }

    /// [`ThreadGroup::cpu`] 的总数。
    pub fn cpu_ns(&self, k: &Kernel) -> u64 {
        self.cpu(k).total_ns
    }

    /// 取一次驻留内存的样，更新峰值。
    pub fn sample_rss(&mut self, mm: &crate::mm::Mm) {
        self.maxrss_kb = self.maxrss_kb.max(mm.resident_pages() * 4);
    }

    /// getrusage 的取样：距上次不到 10 毫秒时沿用上次的峰值。取样要用 mincore 扫一遍写过
    /// 的映射，JVM 频繁调用 getrusage、堆又很大，每次都取样时它占了 Maven 一成多的时间。
    /// execve、退出时的取样不受限制。
    pub fn sample_rss_throttled(&mut self, mm: &crate::mm::Mm) {
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
        let now = std::time::Instant::now();
        if self.rss_sampled.is_some_and(|t| now - t < INTERVAL) {
            return;
        }
        self.rss_sampled = Some(now);
        self.sample_rss(mm);
    }
}

/// 用掉的 CPU 时间，其中 `sys_ns` 是内核态的部分（不超过 `total_ns`，见 [`Task::cpu`](crate::task::Task::cpu)）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuTime {
    pub total_ns: u64,
    pub sys_ns: u64,
}

impl CpuTime {
    pub fn user_ns(self) -> u64 {
        self.total_ns - self.sys_ns
    }
}

impl std::ops::Add for CpuTime {
    type Output = CpuTime;
    fn add(self, o: CpuTime) -> CpuTime {
        CpuTime {
            total_ns: self.total_ns + o.total_ns,
            sys_ns: self.sys_ns + o.sys_ns,
        }
    }
}

impl std::ops::AddAssign for CpuTime {
    fn add_assign(&mut self, o: CpuTime) {
        *self = *self + o;
    }
}

/// getrusage、wait4 报告的资源用量。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Rusage {
    pub cpu: CpuTime,
    /// 驻留内存的峰值（kB）。
    pub maxrss_kb: u64,
}

impl Rusage {
    /// `struct rusage`：用户态、内核态的 CPU 时间和 `ru_maxrss`，其余字段为 0。
    pub fn encode(&self) -> [u8; 144] {
        let mut b = [0u8; 144];
        for (at, ns) in [(0, self.cpu.user_ns()), (16, self.cpu.sys_ns)] {
            b[at..at + 8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        }
        b[32..40].copy_from_slice(&self.maxrss_kb.to_le_bytes());
        b
    }
}

/// 线程组的最后一个线程退出之后：把子进程交给 init，通知父进程或者直接回收。
pub(crate) fn finish_group(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let contexts = std::mem::take(&mut group.borrow_mut().aio);
    crate::syscall::aio_released(k, contexts);
    let (tgid, ppid, status, children, exit_signal) = {
        let mut g = group.borrow_mut();
        let status = g.exiting.unwrap_or(ExitStatus::Exited(g.leader_code));
        (
            g.tgid,
            g.ppid,
            status,
            std::mem::take(&mut g.children),
            g.exit_signal,
        )
    };
    let init = k.groups[&INIT].clone();
    // 这个进程退出之后可能变成孤儿的进程组（Linux 的 kill_orphaned_pgrp）：它把同一会话里
    // 另一个进程组和这个进程组连在一起时，是它自己的进程组；以及同一会话里、另一个进程组的子进程的
    let (pgid, sid) = {
        let g = group.borrow();
        (g.pgid, g.sid)
    };
    let mut pgrps = Vec::new();
    if let Some(p) = k.groups.get(&ppid) {
        let p = p.borrow();
        if p.pgid != pgid && p.sid == sid {
            pgrps.push(pgid);
        }
    }
    let mut orphaned = Vec::new();
    for c in children {
        let Some(child) = k.groups.get(&c).cloned() else {
            continue;
        };
        let zombie = {
            let mut cg = child.borrow_mut();
            cg.ppid = INIT;
            if cg.pgid != pgid && cg.sid == sid {
                pgrps.push(cg.pgid);
            }
            if let Some(sig) = Signal::new(cg.pdeathsig) {
                orphaned.push((c, sig));
            }
            cg.zombie.is_some()
        };
        if zombie {
            k.groups.remove(&c);
        } else {
            init.borrow_mut().children.push(c);
        }
    }
    // PR_SET_PDEATHSIG：通知失去父进程的子进程
    for (c, sig) in orphaned {
        signal::send_to_group(k, c, sig, SigInfo::kernel());
    }
    if let Some(slot) = k.watched.get_mut(&tgid) {
        *slot = Some(status);
    }
    k.ipc.exit(tgid);
    release_ctty(k, group);
    hand_to_parent(k, group, (tgid, ppid, exit_signal), status);
    // POSIX：进程组因为这次退出变成孤儿时，如果组里有停住的进程，先发 SIGHUP 再发
    // SIGCONT，否则它们再也没有机会继续
    pgrps.sort_unstable();
    pgrps.dedup();
    for pgid in pgrps {
        let stopped = k
            .groups
            .values()
            .any(|g| g.borrow().pgid == pgid && g.borrow().stopped);
        if stopped && signal::pgrp_orphaned(k, pgid) {
            signal::send_to_pgrp(k, pgid, Signal::HUP, SigInfo::kernel());
            signal::send_to_pgrp(k, pgid, Signal::CONT, SigInfo::kernel());
        }
    }
    finished(k, tgid);
}

/// 进程 `tgid` 结束了：叫醒等子进程的线程，宿主在等它时也叫醒宿主。其余的等待者不必
/// 醒：它们关心的修改（关闭文件、信号量的 undo、发信号）各自请求唤醒。
fn finished(k: &Kernel, tgid: Tid) {
    let watched = k.watched.contains_key(&tgid);
    request_wake_on(wake::CHILD | if watched { wake::WATCHED } else { 0 });
}

/// [`finish_group`] 中途撞上内核层的 bug 之后（[`crate::bug`]）：至少让宿主等得到它结束、
/// 父进程能回收它。做到一半的其余部分（子进程交给 init、孤儿进程组的信号等）只能放弃。
pub(crate) fn finish_group_after_bug(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (ids, status, handed) = {
        let g = group.borrow();
        let status = g.exiting.unwrap_or(ExitStatus::Exited(g.leader_code));
        let handed = g.zombie.is_some() || !k.groups.contains_key(&g.tgid);
        ((g.tgid, g.ppid, g.exit_signal), status, handed)
    };
    if let Some(slot) = k.watched.get_mut(&ids.0) {
        *slot = Some(status);
    }
    if !handed {
        hand_to_parent(k, group, ids, status);
    }
    finished(k, ids.0);
}

/// 退出的进程（`tgid`、`ppid`、`exit_signal`）交给父进程：父进程是 init、已经不在了，
/// 或者忽略 SIGCHLD（SIG_IGN、SA_NOCLDWAIT）时直接回收，否则变成僵尸进程并通知父进程。
fn hand_to_parent(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    (tgid, ppid, exit_signal): (Tid, Tid, i32),
    status: ExitStatus,
) {
    let parent = k.groups.get(&ppid).cloned();
    let reap = ppid == INIT
        || parent.as_ref().is_none_or(|p| {
            let act = p.borrow().sighand.borrow().get(Signal::CHLD);
            act.handler == SigAction::IGNORE || act.flags & sa::NOCLDWAIT != 0
        });
    if reap {
        k.groups.remove(&tgid);
        if let Some(p) = &parent {
            p.borrow_mut().children.retain(|&c| c != tgid);
        }
    } else {
        group.borrow_mut().zombie = Some(status);
        if let Some(sig) = Signal::new(exit_signal) {
            signal::send_to_group(k, ppid, sig, SigInfo::child(tgid, status));
        }
    }
}

/// 会话组长退出：控制终端不再属于这个会话，前台进程组收到 SIGHUP 和 SIGCONT
/// （Linux 的 `disassociate_ctty`）。
fn release_ctty(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (tgid, sid, ctty) = {
        let g = group.borrow();
        (g.tgid, g.sid, g.ctty.clone())
    };
    let Some(tty) = ctty.filter(|_| sid == tgid) else {
        return;
    };
    let fg = {
        let mut t = tty.borrow_mut();
        if t.session != Some(sid) {
            return;
        }
        t.session = None;
        t.fg.take()
    };
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.sid == sid {
            g.ctty = None;
        }
    }
    if let Some(fg) = fg {
        signal::send_to_pgrp(k, fg, Signal::HUP, SigInfo::kernel());
        signal::send_to_pgrp(k, fg, Signal::CONT, SigInfo::kernel());
    }
}

/// 按 execve 的规则打开 guest 文件：必须是仓库里的普通文件，凭据为 `cred` 的进程
/// 有执行权限。同时返回文件的元数据（setuid、setgid 位）。
pub(crate) fn open_executable(
    vfs: &Vfs,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    path: &[u8],
) -> Result<(Executable, Stat), Errno> {
    open_node(vfs, cred, &vfs.resolve(ps, cred, root, cwd, path, true)?)
}

/// 打开要执行的文件 `node`（execveat 已经解析好的路径或者 fd）。没有跟随的符号链接
/// 是 ELOOP（`AT_SYMLINK_NOFOLLOW`）。
pub(crate) fn open_node(vfs: &Vfs, cred: &Cred, node: &Node) -> Result<(Executable, Stat), Errno> {
    let Node::Store(ino) = *node else {
        return Err(Errno::EACCES);
    };
    let st = vfs.store().stat(ino)?;
    match st.kind() {
        mode::S_IFREG => {}
        mode::S_IFLNK => return Err(Errno::ELOOP),
        _ => return Err(Errno::EACCES),
    }
    cred.permission(&st, MAY_EXEC)?;
    Ok((Executable::open(vfs.store().open_object(ino, false)?)?, st))
}

/// [`open_executable`]，只要文件本身。
pub(crate) fn read_executable(
    vfs: &Vfs,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    path: &[u8],
) -> Result<Executable, Errno> {
    open_executable(vfs, ps, cred, root, cwd, path).map(|(file, _)| file)
}

/// 程序路径的最后一个组件，截到 15 字节（`comm`）。
pub(crate) fn comm_of(path: &[u8]) -> [u8; 16] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let n = name.len().min(15);
    let mut comm = [0u8; 16];
    comm[..n].copy_from_slice(&name[..n]);
    comm
}
