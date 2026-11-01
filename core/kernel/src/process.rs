//! guest 进程（线程组）（设计文档 §6）。宿主启动的进程都是内置 init（pid 1）的子进程
//! （[`crate::Machine`]）。

use std::fmt;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use appine_engine::DbtStats;

use crate::cell::RefCell;
use crate::cred::{Cred, MAY_EXEC};
use crate::errno::Errno;
use crate::kernel::{INIT, Kernel, Tid, request_wake_on, wake};
use crate::loader::{self, Executable, LoadError};
use crate::signal::{self, Pending, SigAction, SigHandlers, SigInfo, sa};
use crate::terminal::Terminal;
use crate::tty::Tty;
use crate::vfs::store::{Ino, Stat, mode};
use crate::vfs::{Dentry, MountId, Node, Pseudo, Vfs};

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
    /// 错误的系统调用（seccomp）。
    pub const SYS: Signal = Signal(31);

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
        /// 写了 core 文件（`WCOREDUMP`）。
        core_dumped: bool,
    },
}

impl ExitStatus {
    /// wait4 报告的状态字（`WIFEXITED` 等宏解码的格式）。
    pub(crate) fn wait_status(self) -> i32 {
        match self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled {
                signal,
                core_dumped,
                ..
            } => signal.number() | if core_dumped { 0x80 } else { 0 },
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
    /// 低端口映射的偏移，见 [`crate::MachineOptions::low_port_offset`]。
    pub low_port_offset: Option<u16>,
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
    /// 父进程里建立它的线程（Linux 的 `real_parent`；`CLONE_PARENT` 时是调用者的父线程）：
    /// 那个线程退出之后换成同一个进程里还活着的线程，过继时换成新的父进程里的线程。
    /// `/proc/<pid>/task/<tid>/children` 按它列出线程的子进程。
    pub parent_thread: Tid,
    /// 进程组。
    pub pgid: Tid,
    /// 会话。
    pub sid: Tid,
    /// 活着的线程，第一个是主线程（如果它还活着）。
    pub threads: Vec<Tid>,
    /// 子进程（含僵尸进程）。
    pub children: Vec<Tid>,
    /// 所在的 pid 命名空间（线程都在这里）。
    pub pid_ns: Rc<crate::pidns::PidNs>,
    /// 按默认方式处理的同步信号被强制投递过：是 pid 命名空间的 1 号进程也照常结束（Linux 的
    /// `force_sig_info_to_task` 去掉 `SIGNAL_UNKILLABLE`）。
    pub killable: bool,
    /// 发给整个进程、尚未处理的信号。
    pub pending: Pending,
    /// 信号处理方式。
    pub sighand: Rc<RefCell<SigHandlers>>,
    /// 资源限制：(软限制, 硬限制)。
    pub rlimits: [(u64, u64); RLIM_NLIMITS],
    /// 正在整体退出（exit_group、致命信号）时的结束方式。
    pub exiting: Option<ExitStatus>,
    /// 主线程自己的结束方式，异常清理的备用值；正常退出由最后一个进入退出流程的线程
    /// 确定 `exiting`（task::Task::begin_exit）。
    pub leader_status: ExitStatus,
    /// 所有线程都已退出、等待父进程回收时的结束方式。
    pub zombie: Option<ExitStatus>,
    /// 退出时发给父进程的信号（clone 的低 8 位，通常是 SIGCHLD），0 表示不发。
    pub exit_signal: i32,
    /// 控制终端。
    pub ctty: Option<Rc<RefCell<Tty>>>,
    /// 控制终端挂断时的前台进程组（Linux 的 `tty_old_pgrp`），只记在会话组长上：它退出时
    /// 已经没有控制终端，改为给这个进程组发 SIGHUP 和 SIGCONT（[`release_ctty`]）。重新
    /// 得到控制终端时清掉。
    pub tty_old_pgrp: Option<Tid>,
    /// 整个线程组被停止信号停住了。
    pub stopped: bool,
    /// 停住线程组的停止信号（Linux 的 `JOBCTL_STOP_SIGMASK`），停着时有意义：父进程的
    /// wait 取走停止事件之后仍然记着，停着的时候被跟踪的线程按它报告 group-stop。
    pub stop_signal: Signal,
    /// 还没有被父进程的 wait 取走的"停止"事件。
    pub stop_report: Option<Signal>,
    /// 还没有被父进程的 wait 取走的"继续"事件。
    pub cont_report: bool,
    /// 正在执行的程序文件（`/proc/<pid>/exe`）；还没执行过程序的（内置的 init）是 None。
    pub exe: Option<Rc<ExeFile>>,
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
    /// `PR_SET_CHILD_SUBREAPER`：后代成了孤儿时过继给最近的这样的祖先，而不是 init
    /// （[`finish_group`]）；fork 不继承，execve 保留。
    pub child_subreaper: bool,
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
    /// `/proc/<pid>/coredump_filter`（`mm::dump_filter`），fork 继承。
    pub coredump_filter: u32,
    /// 正在写 core 文件：其他线程退出时把寄存器留在这里（`crate::coredump`）。
    pub core: Option<Vec<crate::coredump::ThreadCore>>,
    /// POSIX 定时器（timer_create），按 id；fork 不继承，execve 时删除。
    pub posix_timers: std::collections::BTreeMap<i32, crate::itimer::PosixTimer>,
    /// AIO 上下文，按 id（`syscall::aio`）。
    pub aio: std::collections::BTreeMap<u64, Rc<RefCell<crate::syscall::AioContext>>>,
    /// 进程的编号，不重复：pid 被新进程重新使用之后用它区分新旧进程（`/proc/<pid>` 的
    /// inode）。
    pub serial: u64,
    /// 主线程退出时跟踪着它的进程：整个进程结束时也报告给它（[`crate::ptrace`]）。
    pub leader_tracer: Option<Tid>,
    /// 结束时跟踪着主线程、又不是父进程的跟踪者：它用 wait 取走这次结束（或者它自己退出）
    /// 之前，这个僵尸进程只有它看得见，父进程等不到、也不收到通知（Linux 的僵尸进程
    /// 还被跟踪着时）。
    pub exit_tracer: Option<Tid>,
    /// 主线程结束时的 seccomp 过滤器链：与 Linux 一样，进程被回收时才放下（[`crate::seccomp`]）。
    pub leader_seccomp: Option<crate::seccomp::Chain>,
}

impl ThreadGroup {
    pub fn new(tgid: Tid, ppid: Tid, sighand: Rc<RefCell<SigHandlers>>) -> ThreadGroup {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        ThreadGroup {
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            tgid,
            ppid,
            parent_thread: ppid,
            pgid: tgid,
            sid: tgid,
            threads: Vec::new(),
            children: Vec::new(),
            pid_ns: crate::pidns::PidNs::init(),
            killable: false,
            pending: Pending::default(),
            sighand,
            rlimits: default_rlimits(),
            exiting: None,
            leader_status: ExitStatus::Exited(0),
            zombie: None,
            exit_signal: Signal::CHLD.number(),
            ctty: None,
            tty_old_pgrp: None,
            stopped: false,
            stop_signal: Signal::STOP,
            stop_report: None,
            cont_report: false,
            exe: None,
            start_ticks: crate::pseudo::uptime_ns() / (1_000_000_000 / loader::CLOCK_TICKS),
            cpu: CpuTime::default(),
            child_cpu: CpuTime::default(),
            maxrss_kb: 0,
            child_maxrss_kb: 0,
            rss_sampled: None,
            itimer: None,
            cpu_itimers: [None; 2],
            child_subreaper: false,
            dumpable: true,
            no_new_privs: false,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            cred: Cred::root(),
            personality: 0,
            thp_disable: false,
            coredump_filter: crate::mm::dump_filter::DEFAULT,
            core: None,
            execed: false,
            posix_timers: std::collections::BTreeMap::new(),
            aio: std::collections::BTreeMap::new(),
            leader_tracer: None,
            exit_tracer: None,
            leader_seccomp: None,
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
        self.maxrss_kb = self.maxrss_kb.max(mm.peak_resident_pages(false) * 4);
    }

    /// execve 换掉地址空间之前的取样：fork 出来、还没执行过 execve、创建不超过一个时钟
    /// 滴答的进程不量它自己的匿名内存（[`crate::mm::Mm::peak_resident_pages`]）。这次取样
    /// 在 4 个进程同时起进程时占了不少时间：mincore 持着 BKL 等宿主的 mmap_lock，别的进程
    /// 正在锁外 munmap。
    pub fn sample_rss_at_exec(&mut self, mm: &crate::mm::Mm) {
        let now = crate::pseudo::uptime_ns() / (1_000_000_000 / loader::CLOCK_TICKS);
        let fresh = !self.execed && now <= self.start_ticks + 1;
        self.maxrss_kb = self.maxrss_kb.max(mm.peak_resident_pages(fresh) * 4);
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

/// 进程 `g` 里还活着、没有正在被结束的第一个线程（Linux 的 `find_alive_thread`，主线程在
/// 最前面），都在被结束时是第一个线程，没有线程时是 `tgid`。孤儿过继给一个进程时由它当父
/// 线程。
pub(crate) fn alive_thread(k: &Kernel, g: &ThreadGroup) -> Tid {
    g.threads
        .iter()
        .copied()
        .find(|t| k.tasks.get(t).is_some_and(|t| !t.killed.get()))
        .or_else(|| g.threads.first().copied())
        .unwrap_or(g.tgid)
}

/// 子进程 `children` 里设了 `PR_SET_PDEATHSIG` 的线程各要收的信号：它们的父线程退出了
/// （Linux 的 `forget_original_parent`：每个这样的线程一个，发给它所在的进程）。
pub(crate) fn pdeath_signals(k: &Kernel, children: &[Tid]) -> Vec<(Tid, Signal)> {
    let mut out = Vec::new();
    for c in children {
        let Some(g) = k.groups.get(c) else {
            continue;
        };
        for t in &g.borrow().threads {
            if let Some(sig) = k.tasks.get(t).and_then(|t| Signal::new(t.pdeathsig.get())) {
                out.push((*c, sig));
            }
        }
    }
    out
}

/// 发 [`pdeath_signals`] 的信号：与 Linux 一样是 `SI_USER`，发送者是父进程 `parent`（真实
/// 用户 ID 是 `ruid`），子进程的 pid 命名空间里看不到它时编号是 0、按强制的信号处理。
pub(crate) fn send_pdeath(k: &Kernel, signals: Vec<(Tid, Signal)>, parent: Tid, ruid: u32) {
    for (c, sig) in signals {
        let Some(g) = k.groups.get(&c) else {
            continue;
        };
        let uid = g.borrow().cred.uid_out(ruid);
        let info = SigInfo::kill(signal::code::SI_USER, k.pid_seen_by(c, parent), uid).from(parent);
        signal::send_to_group(k, c, sig, info);
    }
}

/// 进程 `parent` 里的线程 `from` 退出了：它建的子进程改由同一个进程里的线程 `to` 当父线程，
/// 排到子进程列表的最后（Linux 的 `forget_original_parent` 在本进程里找到新的父线程时
/// `list_splice_tail_init`）。
pub(crate) fn hand_thread_children(k: &Kernel, parent: &RefCell<ThreadGroup>, from: Tid, to: Tid) {
    let mut p = parent.borrow_mut();
    let (moved, kept): (Vec<Tid>, Vec<Tid>) = p.children.iter().partition(|c| {
        k.groups
            .get(c)
            .is_some_and(|g| g.borrow().parent_thread == from)
    });
    for c in &moved {
        k.groups[c].borrow_mut().parent_thread = to;
    }
    p.children = kept;
    p.children.extend(moved);
}

/// 线程组的最后一个线程退出之后：把子进程交给 init，通知父进程或者直接回收。
///
/// 每一步分别兜住内核层的 bug（[`crate::bug`]）：出错时报告，后面的步骤照样做。出错的那一步
/// 做的事丢了，别的不受牵连：父进程和宿主总能等到它结束，它的子进程、跟踪的进程、孤儿进程组
/// 和 SysV IPC 的清理不会因为前面的一步出错而卡住。
pub(crate) fn finish_group(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (tgid, pgid, sid) = {
        let g = group.borrow();
        (g.tgid, g.pgid, g.sid)
    };
    step(k, tgid, 1, "releasing AIO contexts", |k| {
        let contexts = std::mem::take(&mut group.borrow_mut().aio);
        crate::syscall::aio_released(k, contexts);
    });
    // 这个进程退出之后可能变成孤儿的进程组（Linux 的 kill_orphaned_pgrp）：它把同一会话里
    // 另一个进程组和这个进程组连在一起时，是它自己的进程组；以及同一会话里、另一个进程组的子进程的
    let mut pgrps = Vec::new();
    let mut orphaned = Vec::new();
    // pid 命名空间的 1 号进程退出：命名空间里的进程都被结束，之后不能再建进程，它等到
    // 它们都被回收了才算结束（Linux 的 zap_pid_ns_processes）
    let ns = group.borrow().pid_ns.clone();
    let zap = ns.level > 0 && ns.reaper.get() == Some(tgid);
    step(k, tgid, 2, "handing its children to init", |k| {
        if zap {
            ns.dead.set(true);
            for g in ns.members() {
                if g != tgid && k.groups.contains_key(&g) {
                    signal::send_to_group(k, g, Signal::KILL, SigInfo::kernel());
                }
            }
        }
        // 孤儿先过继给最近的 subreaper（Linux 的 find_new_reaper），没有就给它所在的 pid
        // 命名空间的 1 号进程（find_child_reaper），开机时的命名空间里、或者 1 号进程自己
        // 退出时给 init
        let subreaper = if zap {
            None
        } else {
            find_subreaper(k, &group.borrow())
        };
        let reaper = subreaper
            .or_else(|| {
                ns.reaper
                    .get()
                    .filter(|&r| ns.level > 0 && !zap && k.groups.contains_key(&r))
            })
            .unwrap_or(INIT);
        let reaper_thread = k
            .groups
            .get(&reaper)
            .map_or(reaper, |g| alive_thread(k, &g.borrow()));
        // 最后一个线程建的（别的线程退出时转给了它的）子进程要收的 PR_SET_PDEATHSIG
        orphaned = pdeath_signals(k, &group.borrow().children);
        let children = std::mem::take(&mut group.borrow_mut().children);
        for c in children {
            let Some(child) = k.groups.get(&c).cloned() else {
                continue;
            };
            let zombie = {
                let mut cg = child.borrow_mut();
                cg.ppid = reaper;
                cg.parent_thread = reaper_thread;
                cg.exit_signal = Signal::CHLD.number();
                if cg.pgid != pgid && cg.sid == sid {
                    pgrps.push(cg.pgid);
                }
                // 跟踪者还没取走的僵尸进程交给新的父进程，跟踪者取走之后由它回收
                cg.zombie.filter(|_| cg.exit_tracer.is_none())
            };
            if reaper != INIT {
                k.groups[&reaper].borrow_mut().children.push(c);
            }
            match zombie {
                Some(_) if reaper == INIT => {
                    k.groups.remove(&c);
                    k.release_pid(c);
                }
                // 新的父进程要回收它：与 Linux 的 reparent_leader 一样再通知一次
                Some(status) => {
                    hand_to_parent(k, &child, (c, reaper, Signal::CHLD.number()), status)
                }
                None if reaper == INIT => k.groups[&INIT].borrow_mut().children.push(c),
                None => {}
            }
        }
    });
    // PR_SET_PDEATHSIG：通知失去父进程的子进程
    let ruid = group.borrow().cred.ruid;
    step(k, tgid, 3, "sending parent-death signals", |k| {
        send_pdeath(k, orphaned, tgid, ruid)
    });
    step(k, tgid, 4, "releasing SysV IPC", |k| {
        crate::ipc::exit(k, tgid)
    });
    step(k, tgid, 5, "releasing the controlling terminal", |k| {
        release_ctty(k, group)
    });
    if zap && ns.members().iter().any(|&g| g != tgid) {
        k.hold_reaper(ns, group.clone(), pgrps);
    } else {
        finish_reported(k, group, pgrps);
    }
}

/// 退出的进程 `group` 的孤儿要过继给的 subreaper：从父进程往上，同一层 pid 命名空间里最近
/// 的设了 `PR_SET_CHILD_SUBREAPER`、还有活着的线程的进程（Linux 的 find_new_reaper 比 pid
/// 的层数，不跨出命名空间；整个进程都在退出的跳过）。
fn find_subreaper(k: &Kernel, group: &ThreadGroup) -> Option<Tid> {
    let level = group.pid_ns.level;
    let mut p = group.ppid;
    while p != INIT {
        let a = k.groups.get(&p)?.borrow();
        if a.pid_ns.level != level {
            return None;
        }
        if a.child_subreaper && !a.threads.is_empty() {
            return Some(p);
        }
        p = a.ppid;
    }
    None
}

/// [`finish_group`] 的后一半：报告给跟踪者、父进程。pid 命名空间的 1 号进程等到命名空间里
/// 别的进程都被回收了才做这一半（[`Kernel::release_pid`]）。
pub(crate) fn finish_reported(
    k: &mut Kernel,
    group: &Rc<RefCell<ThreadGroup>>,
    mut pgrps: Vec<Tid>,
) {
    let (tgid, ppid, pgid, sid, status, exit_signal) = {
        let g = group.borrow();
        let status = g.exiting.unwrap_or(g.leader_status);
        (g.tgid, g.ppid, g.pgid, g.sid, status, g.exit_signal)
    };
    if let Some(slot) = k.watched.get_mut(&tgid) {
        *slot = Some(status);
    }
    // 被父进程以外的进程跟踪着的先报告给跟踪者，它取走之后再交给父进程
    let traced = step(k, tgid, 6, "reporting to its tracer", |k| {
        crate::ptrace::group_finished(k, group, status)
    });
    if traced != Some(true) {
        step(k, tgid, 7, "handing it to its parent", |k| {
            // 报告给跟踪者时出错：不再等跟踪者取走
            group.borrow_mut().exit_tracer = None;
            hand_to_parent(k, group, (tgid, ppid, exit_signal), status);
        });
    }
    // 它跟踪着的不再被跟踪
    step(k, tgid, 8, "releasing its tracees", |k| {
        crate::ptrace::tracer_exited(k, tgid)
    });
    // POSIX：进程组因为这次退出变成孤儿时，如果组里有停住的进程，先发 SIGHUP 再发
    // SIGCONT，否则它们再也没有机会继续
    step(k, tgid, 9, "signalling orphaned process groups", |k| {
        if let Some(p) = k.groups.get(&ppid) {
            let p = p.borrow();
            if p.pgid != pgid && p.sid == sid {
                pgrps.push(pgid);
            }
        }
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
    });
    finished(k, tgid);
}

/// [`finish_group`] 的第 `n` 步 `what`：兜住内核层的 bug，出错时报告，返回 None。
fn step<R>(
    k: &mut Kernel,
    tgid: Tid,
    n: u32,
    what: &str,
    f: impl FnOnce(&mut Kernel) -> R,
) -> Option<R> {
    let r = crate::bug::catch(|| {
        crate::bug::trap_finish(tgid, n);
        f(k)
    });
    if r.is_none() {
        crate::bug::report(format_args!(
            "kernel panic while pid {tgid} was exiting ({what})"
        ));
    }
    r
}

/// 进程 `tgid` 结束了：叫醒等子进程的线程，宿主在等它时也叫醒宿主。其余的等待者不必
/// 醒：它们关心的修改（关闭文件、信号量的 undo、发信号）各自请求唤醒。
fn finished(k: &Kernel, tgid: Tid) {
    let watched = k.watched.contains_key(&tgid);
    request_wake_on(wake::CHILD | if watched { wake::WATCHED } else { 0 });
}

/// 跟踪者取走了结束了的进程 `group`（或者没取走就退出了）：交给父进程，与 Linux 的
/// `wait_task_zombie`、`__ptrace_detach` 对僵尸进程的处理一样（[`crate::ptrace`]）。
pub(crate) fn tracer_released(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (ids, status) = {
        let mut g = group.borrow_mut();
        g.exit_tracer = None;
        let Some(status) = g.zombie else {
            return;
        };
        ((g.tgid, g.ppid, g.exit_signal), status)
    };
    hand_to_parent(k, group, ids, status);
    request_wake_on(wake::CHILD);
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
        k.release_pid(tgid);
        if let Some(p) = &parent {
            p.borrow_mut().children.retain(|&c| c != tgid);
        }
    } else {
        group.borrow_mut().zombie = Some(status);
        if let (Some(sig), Some(p)) = (Signal::new(exit_signal), &parent) {
            let pid = k.pid_seen_by(ppid, tgid);
            let info = signal::exit_info(&group.borrow().cred, &p.borrow().cred, pid, status);
            signal::send_to_group(k, ppid, sig, info);
        }
    }
}

/// 会话组长退出：控制终端不再属于这个会话，前台进程组收到 SIGHUP 和 SIGCONT
/// （Linux 的 `disassociate_ctty`）。控制终端已经挂断了的，发给挂断时的前台进程组：
/// tmux 的窗口关掉时，`sh -c` 起的命令和不 exec 的 sh（dash）在同一个进程组里，组长 sh
/// 收到 SIGHUP 退出时它们收到 SIGHUP。
fn release_ctty(k: &Kernel, group: &Rc<RefCell<ThreadGroup>>) {
    let (tgid, sid, ctty, old_pgrp) = {
        let mut g = group.borrow_mut();
        (g.tgid, g.sid, g.ctty.clone(), g.tty_old_pgrp.take())
    };
    if sid != tgid {
        return;
    }
    let Some(tty) = ctty else {
        if let Some(pgrp) = old_pgrp {
            signal::send_to_pgrp(k, pgrp, Signal::HUP, SigInfo::kernel());
            signal::send_to_pgrp(k, pgrp, Signal::CONT, SigInfo::kernel());
        }
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

/// 进程正在执行的程序文件（Linux 的 `mm->exe_file`）：`/proc/<pid>/exe` 读出来是它的路径，
/// 跟随时直接到这个文件，不按路径重新解析。程序文件改了名、被删除或者被换掉（dpkg 升级
/// 时用 rename 换掉正在运行的程序）之后，经由 exe 打开、执行的仍是原来那个。fork 共用，
/// execve 换掉。
pub struct ExeFile {
    /// 执行时的绝对路径，符号链接全部解析：musl 按它展开主程序的 `$ORIGIN`，经由
    /// /usr/bin/java 这种链接启动的程序靠它找到自己的库。
    pub path: Rc<[u8]>,
    /// 经由的挂载和仓库里的 inode；开发用的宿主文件（没有仓库）时是 None，按路径跟随。
    pub node: Option<(MountId, Ino)>,
    /// 最后经过的目录和名字：经由 exe 执行的程序的路径按它算（[`Pseudo::follow_dentry`]）。
    pub dentry: Option<Dentry>,
    /// 打开着的宿主文件（仓库里的 object）：程序文件被删除之后 object 也删掉了，重新打开、
    /// 执行靠它（[`open_exe_file`]）。
    pub host: Option<Arc<File>>,
}

impl ExeFile {
    /// 刚执行的程序 `file`（仓库里的 inode `ino`）。`walked` 是按执行时的路径重新走一遍
    /// 得到的节点和最后经过的目录项（[`Vfs::walk`]）；走不通时（经由 fd 执行已经删除的
    /// 文件）用打开的节点 `opened`（执行的是脚本时没有：程序是解释器）。经由别的进程的
    /// exe 执行已经删除的程序时（[`crate::pseudo::PseudoNode::OpenExe`]），挂载和 inode
    /// 沿用那个进程的 `from`。
    pub(crate) fn new(
        path: Rc<[u8]>,
        walked: Option<(Node, Option<Dentry>)>,
        opened: Option<&Node>,
        from: Option<&ExeFile>,
        ino: Option<Ino>,
        file: &Executable,
    ) -> ExeFile {
        let (node, dentry) = match (walked, opened, from) {
            (Some((Node::Store(mnt, i), found)), ..) if Some(i) == ino => (Some((mnt, i)), found),
            (Some((Node::Pseudo(..), found)), _, Some(from)) => (from.node, found),
            (_, Some(&Node::Store(mnt, i)), _) if Some(i) == ino => (Some((mnt, i)), None),
            (_, Some(Node::Pseudo(..)), Some(from)) => (from.node, from.dentry.clone()),
            _ => (None, None),
        };
        ExeFile {
            path,
            node,
            dentry,
            host: file.host(),
        }
    }
}

/// 已经删除的程序文件（`/proc/<pid>/exe` 跟随之后得到的
/// [`crate::pseudo::PseudoNode::OpenExe`]）：经由记着的宿主文件打开，检查与 [`open_node`]
/// 相同。
pub(crate) fn open_exe_file(
    vfs: &Vfs,
    cred: &Cred,
    exe: &ExeFile,
) -> Result<(Executable, Stat, bool), Errno> {
    let (Some((mnt, ino)), Some(host)) = (exe.node, &exe.host) else {
        return Err(Errno::ENOENT);
    };
    let st = vfs.store().stat_open(ino, host)?;
    if st.kind() != mode::S_IFREG {
        return Err(Errno::EACCES);
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = vfs.mount_flags(&Node::Store(mnt, ino)).unwrap_or(0);
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    let file = host
        .try_clone()
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok((Executable::open(file)?, st, nosuid))
}

/// 经由打开着的文件执行（fexecve、execveat 的 `AT_EMPTY_PATH`、`/proc/<pid>/fd/<n>`）：用
/// 它的宿主文件，已经删除的文件和 memfd 的 object 都不在了，按 inode 打不开。检查与
/// [`open_node`] 相同，noexec、nosuid 按打开它时经由的挂载（memfd 不经由挂载）。不是
/// 普通文件是 EACCES。
pub(crate) fn open_open_file(
    vfs: &Vfs,
    cred: &Cred,
    f: &crate::fd::OpenFile,
) -> Result<(Executable, Stat, bool), Errno> {
    let crate::fd::FileKind::Regular { ino, file } = &f.kind else {
        return Err(Errno::EACCES);
    };
    let st = vfs.store().stat_open(*ino, file)?;
    if st.kind() != mode::S_IFREG {
        return Err(Errno::EACCES);
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = if f.pseudo {
        0
    } else {
        vfs.mount_flags(&Node::Store(f.mnt, *ino)).unwrap_or(0)
    };
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    let file = file
        .try_clone()
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
    Ok((Executable::open(file)?, st, nosuid))
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
) -> Result<(Executable, Stat, bool), Errno> {
    open_node(vfs, cred, &vfs.resolve(ps, cred, root, cwd, path, true)?)
}

/// 打开要执行的文件 `node`（execveat 已经解析好的路径或者 fd）。没有跟随的符号链接
/// 是 ELOOP（`AT_SYMLINK_NOFOLLOW`），noexec 的挂载里的是 EACCES（Linux 的 `path_noexec`）。
/// 第三项是它在 nosuid 的挂载里：setuid、setgid 位和文件能力不起作用。
pub(crate) fn open_node(
    vfs: &Vfs,
    cred: &Cred,
    node: &Node,
) -> Result<(Executable, Stat, bool), Errno> {
    let Node::Store(_, ino) = *node else {
        return Err(Errno::EACCES);
    };
    let st = vfs.store().stat(ino)?;
    match st.kind() {
        mode::S_IFREG => {}
        mode::S_IFLNK => return Err(Errno::ELOOP),
        _ => return Err(Errno::EACCES),
    }
    cred.permission(&st, MAY_EXEC)?;
    let flags = vfs.mount_flags(node).unwrap_or(0);
    if flags & crate::vfs::ms::NOEXEC != 0 {
        return Err(Errno::EACCES);
    }
    let nosuid = flags & crate::vfs::ms::NOSUID != 0;
    Ok((vfs.executable(ino)?, st, nosuid))
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
    open_executable(vfs, ps, cred, root, cwd, path).map(|(file, ..)| file)
}

/// 程序路径的最后一个组件，截到 15 字节（`comm`）。
pub(crate) fn comm_of(path: &[u8]) -> [u8; 16] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let n = name.len().min(15);
    let mut comm = [0u8; 16];
    comm[..n].copy_from_slice(&name[..n]);
    comm
}
