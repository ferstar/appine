//! guest 进程（线程组）与一次运行会话（设计文档 §6）。
//!
//! [`run`] 建立内核、加载第一个 guest 进程并为它启动线程，然后等待它结束。
//! 第一个进程是内置 init（pid 1）的子进程；它结束时会话结束，其余进程被强制终止。

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use appine_engine::{Cpu, Dbt, DbtStats};
use appine_hal::Event;

use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{INIT, Kernel, Shared, Tid, block, request_wake};
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::signal::{self, SigAction, SigHandlers, SigInfo, sa};
use crate::task::{self, FsContext, Task, TaskState};
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
    /// 用户自定义信号 1。
    pub const USR1: Signal = Signal(10);
    /// 非法内存访问。
    pub const SEGV: Signal = Signal(11);
    /// 管道断开。
    pub const PIPE: Signal = Signal(13);
    /// 终止。
    pub const TERM: Signal = Signal(15);
    /// 子进程状态改变。
    pub const CHLD: Signal = Signal(17);
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

    /// 在按信号编号排列的表里的下标。
    pub(crate) fn index(self) -> usize {
        self.0 as usize - 1
    }

    /// 是不是停止信号（SIGSTOP、SIGTSTP、SIGTTIN、SIGTTOU）。
    pub(crate) fn is_stop(self) -> bool {
        matches!(self.0, 19..=22)
    }

    /// 默认动作是否是忽略。作业控制在 M3b 实现之前，停止信号也按忽略处理。
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

/// 第一个 guest 进程的 pid：pid 1 是内置的 init。
pub(crate) const FIRST_PID: Tid = INIT + 1;

/// `RLIMIT_STACK`。
pub(crate) const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_CORE`。
pub(crate) const RLIMIT_CORE: u32 = 4;
/// `RLIMIT_NOFILE`。
pub(crate) const RLIMIT_NOFILE: u32 = 7;
/// 资源种类数（`RLIM_NLIMITS`）。
pub(crate) const RLIM_NLIMITS: usize = 16;

fn default_rlimits() -> [(u64, u64); RLIM_NLIMITS] {
    let mut r = [(u64::MAX, u64::MAX); RLIM_NLIMITS];
    r[RLIMIT_STACK as usize] = (loader::STACK_SIZE, u64::MAX);
    r[RLIMIT_CORE as usize] = (0, u64::MAX);
    r[RLIMIT_NOFILE as usize] = (crate::fd::MAX_FDS as u64, crate::fd::MAX_FDS as u64);
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
    pub pending: u64,
    /// 挂起信号附带的 siginfo。
    pub info: [SigInfo; Signal::MAX as usize],
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
}

impl ThreadGroup {
    pub fn new(tgid: Tid, ppid: Tid, sighand: Rc<RefCell<SigHandlers>>) -> ThreadGroup {
        ThreadGroup {
            tgid,
            ppid,
            pgid: tgid,
            sid: tgid,
            threads: Vec::new(),
            children: Vec::new(),
            pending: 0,
            info: [SigInfo::default(); Signal::MAX as usize],
            sighand,
            rlimits: default_rlimits(),
            exiting: None,
            leader_code: 0,
            zombie: None,
            exit_signal: Signal::CHLD.number(),
        }
    }
}

/// 线程组的最后一个线程退出之后：把子进程交给 init，通知父进程或者直接回收。
pub(crate) fn finish_group(k: &mut Kernel, group: &Rc<RefCell<ThreadGroup>>) {
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
    for c in children {
        let Some(child) = k.groups.get(&c).cloned() else {
            continue;
        };
        let zombie = {
            let mut cg = child.borrow_mut();
            cg.ppid = INIT;
            cg.zombie.is_some()
        };
        if zombie {
            k.groups.remove(&c);
        } else {
            init.borrow_mut().children.push(c);
        }
    }
    if tgid == FIRST_PID {
        k.session = Some(status);
    }

    // 父进程是 init、已经不在了，或者忽略 SIGCHLD（SIG_IGN、SA_NOCLDWAIT）时直接回收
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
    request_wake();
}

/// 按 execve 的规则读取 guest 文件：必须是带执行权限的普通文件。
pub(crate) fn read_executable(vfs: &Vfs, cwd: Ino, path: &[u8]) -> Result<Vec<u8>, Errno> {
    let ino = vfs.resolve(cwd, path, true)?;
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

/// 程序路径的最后一个组件，截到 15 字节（`comm`）。
pub(crate) fn comm_of(path: &[u8]) -> [u8; 16] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let n = name.len().min(15);
    let mut comm = [0u8; 16];
    comm[..n].copy_from_slice(&name[..n]);
    comm
}

/// 加载 guest 程序并运行到第一个进程结束。
///
/// 设置了 [`RunOptions::rootfs`] 时，`program` 是 guest 文件系统里的路径；
/// 否则是宿主路径。
pub fn run(
    program: &OsStr,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    if options.tier == Tier::Dbt && Dbt::new().is_none() {
        return Err(RunError::TierUnavailable);
    }
    let vfs = match &options.rootfs {
        Some(dir) => {
            let vfs = Vfs::open(dir).map_err(RunError::Rootfs)?;
            if let Err(e) = crate::vfs::dev::ensure_dev_nodes(vfs.store()) {
                crate::debug::log(format_args!("cannot create the device nodes: {e}"));
            }
            Some(Rc::new(vfs))
        }
        None => None,
    };
    let path = program.as_bytes();
    let file = match &vfs {
        Some(vfs) => read_executable(vfs, ROOT, path).map_err(RunError::Exec)?,
        None => std::fs::read(Path::new(program)).map_err(RunError::Io)?,
    };
    let args = Args {
        argv,
        envp,
        execfn: path,
    };
    let mut mm = Mm::new();
    let mut read_interp = |p: &[u8]| match &vfs {
        Some(vfs) => read_executable(vfs, ROOT, p),
        None => Err(Errno::ENOENT),
    };
    let image = loader::load(&mut mm, &file, &args, &mut read_interp).map_err(RunError::Load)?;

    let event = Arc::new(Event::new().map_err(|e| RunError::Io(io::Error::from_raw_os_error(e)))?);
    let shared = Arc::new(Shared::new(Kernel::new(options.tier)));
    {
        let mut k = shared.lock();
        let init = ThreadGroup::new(INIT, 0, Rc::new(RefCell::new(SigHandlers::default())));
        k.groups.insert(INIT, Rc::new(RefCell::new(init)));
        let tid = k.alloc_tid().expect("the first tid is free");
        debug_assert_eq!(tid, FIRST_PID);
        let mm = Rc::new(RefCell::new(mm));
        // SAFETY: Mm 由任务的 TaskState 持有，线程退出或 execve 之前不会释放
        let mut cpu = unsafe { Cpu::new(mm.borrow().mmu()) };
        cpu.state_mut().pc = image.entry;
        cpu.state_mut().sp = image.sp;
        let group = Rc::new(RefCell::new(ThreadGroup::new(
            tid,
            INIT,
            Rc::new(RefCell::new(SigHandlers::default())),
        )));
        let state = TaskState {
            mm,
            fds: Rc::new(RefCell::new(FdTable::with_host_stdio())),
            fs: Rc::new(RefCell::new(FsContext {
                cwd: ROOT,
                umask: 0o022,
            })),
            group: group.clone(),
            vfs,
            altstack: Default::default(),
            clear_child_tid: 0,
            saved_sigmask: None,
            vfork_done: None,
        };
        let task = Task::new(tid, tid, &cpu, state, 0, comm_of(path))
            .map_err(|e| RunError::Io(io::Error::from_raw_os_error(e.0)))?;
        k.groups.insert(tid, group);
        k.groups[&INIT].borrow_mut().children.push(tid);
        task::start(&shared, &mut k, task, cpu)
            .map_err(|e| RunError::Io(io::Error::from_raw_os_error(e.0)))?;
    }

    // 等第一个进程结束，然后结束其余的进程，等所有线程退出
    let status = wait_until(&shared, &event, |k| k.session);
    {
        let k = shared.lock();
        let groups: Vec<_> = k.groups.values().cloned().collect();
        for g in groups.iter().filter(|g| !g.borrow().threads.is_empty()) {
            signal::group_exit(
                &k,
                g,
                ExitStatus::Signaled {
                    signal: Signal::KILL,
                    pc: None,
                    fault_addr: None,
                },
            );
        }
    }
    wait_until(&shared, &event, |k| k.tasks.is_empty().then_some(()));
    let dbt_stats = shared.lock().dbt_stats;
    Ok(RunReport { status, dbt_stats })
}

/// 宿主主线程等待内核状态满足条件。
fn wait_until<T>(shared: &Shared, event: &Arc<Event>, cond: impl Fn(&Kernel) -> Option<T>) -> T {
    let mut k = shared.lock();
    loop {
        if let Some(v) = cond(&k) {
            return v;
        }
        let (k2, _) = block(shared, k, event, None, &mut []);
        k = k2;
    }
}
