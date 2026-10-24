//! guest 进程与运行循环。
//!
//! M1b：单进程、单线程，还没有信号投递。guest 遇到同步异常、或者给自己发送
//! 默认动作为终止的信号时直接终止，退出状态与 Linux 上"被信号杀死"一致。

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use appine_engine::{Cpu, Dbt, DbtStats, Exit, FaultKind};

use crate::errno::Errno;
use crate::fd::FdTable;
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::syscall::{self, Flow};
use crate::vfs::Vfs;
use crate::vfs::store::{Ino, ROOT, mode};

/// 一个信号编号（1–64）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal(i32);

impl Signal {
    /// 挂起。
    pub const HUP: Signal = Signal(1);
    /// 终端中断。
    pub const INT: Signal = Signal(2);
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
    /// 非法内存访问。
    pub const SEGV: Signal = Signal(11);
    /// 管道断开。
    pub const PIPE: Signal = Signal(13);
    /// 终止。
    pub const TERM: Signal = Signal(15);
    /// 停止（不能捕获）。
    pub const STOP: Signal = Signal(19);

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

    /// 默认动作是否是忽略（含 M1b 还不支持的"停止"）。
    pub(crate) fn ignored_by_default(self) -> bool {
        // SIGCHLD、SIGCONT、SIGSTOP、SIGTSTP、SIGTTIN、SIGTTOU、SIGURG、SIGWINCH
        matches!(self.0, 17..=23 | 28)
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

/// guest 的结束方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// guest 调用了 `exit` 或 `exit_group`。
    Exited(i32),
    /// guest 被信号终止。
    Signaled {
        /// 信号。
        signal: Signal,
        /// 终止时的 pc（同步异常时是出错指令的地址）。
        pc: u64,
        /// 访存出错时的 guest 地址。
        fault_addr: Option<u64>,
    },
}

/// 执行引擎（设计文档 §4.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Tier 0：解释器。
    #[default]
    Interpreter,
    /// Tier 1：同 ISA 直通式 DBT（M2 原型，只支持 aarch64 Linux 宿主）。
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
}

/// 一次运行的结果。
#[derive(Debug, Clone, Copy)]
pub struct RunReport {
    /// guest 的结束方式。
    pub status: ExitStatus,
    /// 使用 Tier 1 时的统计。
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
    /// 当前宿主不支持所选的执行引擎。
    TierUnavailable,
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
            RunError::TierUnavailable => {
                f.write_str("the selected tier is not supported on this host")
            }
        }
    }
}

impl std::error::Error for RunError {}

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

/// `RLIMIT_STACK`。
pub(crate) const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_CORE`。
pub(crate) const RLIMIT_CORE: u32 = 4;
/// `RLIMIT_NOFILE`。
pub(crate) const RLIMIT_NOFILE: u32 = 7;
/// 资源种类数（`RLIM_NLIMITS`）。
const RLIM_NLIMITS: usize = 16;

/// 一个 guest 进程（M1b 只有一个线程）。
pub(crate) struct Process {
    pub mm: Mm,
    pub fds: FdTable,
    pub vfs: Option<Rc<Vfs>>,
    pub cwd: Ino,
    pub umask: u32,
    pub pid: i32,
    pub clear_child_tid: u64,
    pub sigactions: [SigAction; Signal::MAX as usize],
    pub sigmask: u64,
    /// 已产生、尚未处理的信号。
    pub pending: u64,
    /// 资源限制：(软限制, 硬限制)。
    pub rlimits: [(u64, u64); RLIM_NLIMITS],
    pub comm: [u8; 16],
}

impl Process {
    /// guest 的 pid：pid 1 留给内核层内置的 init（设计文档 §6）。
    const FIRST_PID: i32 = 2;

    fn new(vfs: Option<Rc<Vfs>>) -> Self {
        Process {
            mm: Mm::new(),
            fds: FdTable::with_host_stdio(),
            vfs,
            cwd: ROOT,
            umask: 0o022,
            pid: Self::FIRST_PID,
            clear_child_tid: 0,
            sigactions: [SigAction::default(); Signal::MAX as usize],
            sigmask: 0,
            pending: 0,
            rlimits: Self::default_rlimits(),
            comm: [0; 16],
        }
    }

    fn default_rlimits() -> [(u64, u64); RLIM_NLIMITS] {
        let mut r = [(u64::MAX, u64::MAX); RLIM_NLIMITS];
        r[RLIMIT_STACK as usize] = (loader::STACK_SIZE, u64::MAX);
        r[RLIMIT_CORE as usize] = (0, u64::MAX);
        r[RLIMIT_NOFILE as usize] = (crate::fd::MAX_FDS as u64, crate::fd::MAX_FDS as u64);
        r
    }

    /// 信号当前是否会被丢弃。
    pub fn signal_ignored(&self, sig: Signal) -> bool {
        match self.sigactions[sig.number() as usize - 1].handler {
            SigAction::IGNORE => true,
            SigAction::DEFAULT => sig.ignored_by_default(),
            _ => false,
        }
    }

    /// 取出下一个应当处理的信号，返回导致进程终止的那一个。
    ///
    /// M1b 还没有信号帧：装了处理函数的信号也按默认动作终止进程，并记录日志。
    pub fn next_signal(&mut self) -> Option<Signal> {
        loop {
            let ready = self.pending & !self.sigmask;
            if ready == 0 {
                return None;
            }
            let sig = Signal(ready.trailing_zeros() as i32 + 1);
            self.pending &= !sig.bit();
            if self.signal_ignored(sig) {
                continue;
            }
            let handler = self.sigactions[sig.number() as usize - 1].handler;
            if handler != SigAction::DEFAULT {
                crate::debug::log(format_args!(
                    "signal handlers are not supported yet; {sig} terminates the process"
                ));
            }
            return Some(sig);
        }
    }

    /// 根文件系统；没有 rootfs 时 guest 看不到任何文件。
    pub fn vfs(&self) -> Result<&Rc<Vfs>, Errno> {
        self.vfs.as_ref().ok_or(Errno::ENOENT)
    }

    /// 按 execve 的规则读取 guest 文件：必须是带执行权限的普通文件。
    fn read_executable(&self, path: &[u8]) -> Result<Vec<u8>, Errno> {
        let vfs = self.vfs()?;
        let ino = vfs.resolve(self.cwd, path, true)?;
        let st = vfs.store().stat(ino)?;
        if st.kind() != mode::S_IFREG || st.mode & 0o111 == 0 {
            return Err(Errno::EACCES);
        }
        let mut data = Vec::with_capacity(st.size as usize);
        vfs.store()
            .open_object(ino, false)?
            .read_to_end(&mut data)
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
        Ok(data)
    }

    fn set_comm(&mut self, path: &[u8]) {
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
        let n = name.len().min(15);
        self.comm = [0; 16];
        self.comm[..n].copy_from_slice(&name[..n]);
    }
}

/// 加载 guest 程序并运行到结束。
///
/// 设置了 [`RunOptions::rootfs`] 时，`program` 是 guest 文件系统里的路径；
/// 否则是宿主路径。
pub fn run(
    program: &OsStr,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    let mut dbt = match options.tier {
        Tier::Interpreter => None,
        Tier::Dbt => Some(Dbt::new().ok_or(RunError::TierUnavailable)?),
    };
    let vfs = match &options.rootfs {
        Some(dir) => Some(Rc::new(Vfs::open(dir).map_err(RunError::Rootfs)?)),
        None => None,
    };
    let mut proc = Process::new(vfs);
    let path = program.as_bytes();
    let file = match proc.vfs {
        Some(_) => proc.read_executable(path).map_err(RunError::Exec)?,
        None => std::fs::read(Path::new(program)).map_err(RunError::Io)?,
    };
    let args = Args {
        argv,
        envp,
        execfn: path,
    };
    let mut read_interp = |p: &[u8]| proc.read_executable(p);
    let mut mm = Mm::new();
    let image = loader::load(&mut mm, &file, &args, &mut read_interp).map_err(RunError::Load)?;
    proc.mm = mm;
    proc.set_comm(path);

    // SAFETY: cpu 比 proc 后声明、先析构；mm 的页表由 Mm 内部的 Mmu 持有，
    // 在 proc 存续期间地址不变
    let mut cpu = unsafe { Cpu::new(proc.mm.mmu()) };
    cpu.state_mut().pc = image.entry;
    cpu.state_mut().sp = image.sp;

    let status = loop {
        let exit = match &mut dbt {
            Some(dbt) => dbt.run(&mut cpu),
            None => cpu.run(),
        };
        let (signal, fault_addr) = match exit {
            Exit::Svc { .. } => {
                let flow = syscall::dispatch(&mut cpu, &mut proc);
                if proc.mm.take_dirty() {
                    cpu.tlb_flush();
                    if let Some(dbt) = &mut dbt {
                        dbt.flush();
                    }
                }
                match flow {
                    Flow::Continue => continue,
                    Flow::Exit(code) => break ExitStatus::Exited(code),
                    Flow::Kill(signal) => (signal, None),
                }
            }
            Exit::Kick => continue,
            Exit::Undefined { insn } => {
                crate::debug::log(format_args!(
                    "undefined instruction {insn:#010x} at {:#x}",
                    cpu.state().pc
                ));
                (Signal::ILL, None)
            }
            Exit::Breakpoint { .. } => (Signal::TRAP, None),
            Exit::Fault {
                addr,
                kind: FaultKind::Page,
                ..
            } => (Signal::SEGV, Some(addr)),
            Exit::Fault {
                addr,
                kind: FaultKind::Alignment,
                ..
            } => (Signal::BUS, Some(addr)),
        };
        break ExitStatus::Signaled {
            signal,
            pc: cpu.state().pc,
            fault_addr,
        };
    };
    Ok(RunReport {
        status,
        dbt_stats: dbt.map(|d| d.stats()),
    })
}
