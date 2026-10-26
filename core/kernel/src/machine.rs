//! 宿主眼中的 guest 内核（设计文档 §6、§11.2）。
//!
//! [`Machine`] 持有一个内核实例：可以先后启动多个进程（[`Machine::spawn`]），每个进程
//! 可以有自己的控制台（宿主持有主设备的进程内 PTY，[`Machine::open_console`]）。Android
//! 的前台服务为每个终端标签页开一个控制台和一个登录 shell；`appine-run` 只启动一个
//! 进程（[`run`]）。方法可以在任意宿主线程上调用，阻塞的方法和 guest 线程一样走统一的
//! 等待机制。间隔定时器由一个宿主线程负责（[`crate::itimer`]）。
//!
//! 方法里的内核代码撞上内核层的 bug（panic）时，只让这次调用失败
//! （[`crate::bug::host_call`]）：返回 EIO 或者 [`RunError::Internal`]，出错的是一个控制台上
//! 的调用时还挂断那个控制台，免得终端标签页卡在坏掉的控制台上。只读一张表的方法
//! （[`Machine::wait`] 等）不需要。

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;

use appine_engine::{Cpu, Dbt, DbtStats};
use appine_hal::Event;

use crate::cred::Cred;
use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{INIT, Kernel, Shared, block, request_wake, wake};
use crate::loader::{self, Args, Executable};
use crate::mm::Mm;
use crate::process::{
    ExitStatus, RunError, RunOptions, RunReport, Signal, ThreadGroup, Tier, comm_of,
    read_executable,
};
use crate::signal::{self, SigHandlers};
use crate::task::{self, FsContext, Task, TaskState};
use crate::terminal;
use crate::tty::Winsize;
use crate::vfs::{Node, Vfs};

/// 内核实例的选项。
#[derive(Debug, Clone, Default)]
pub struct MachineOptions {
    /// 执行引擎。
    pub tier: Tier,
    /// inode 仓库所在的宿主目录。设置后程序路径按 guest 路径在仓库里解析，否则按
    /// 宿主路径读取，guest 也看不到任何文件系统。
    pub rootfs: Option<PathBuf>,
    /// App 进程匿名内存的上限（字节）：超出时结束一个 guest 进程，免得 Android 的 LMK
    /// 杀掉整个 App（设计文档 §12）。None 时取宿主物理内存的一半，`u64::MAX` 表示不限制。
    pub memory_limit: Option<u64>,
}

/// 新进程的 stdin/stdout/stderr。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// 宿主进程自己的 0、1、2。
    Host,
    /// 控制台（PTY 编号）的从设备：进程成为新会话的组长，控制台是它的控制终端。
    Console(u32),
}

/// 一个 guest 内核实例。析构时结束所有 guest 进程。
pub struct Machine {
    shared: Arc<Shared>,
    timers: Option<JoinHandle<()>>,
}

thread_local! {
    /// 宿主线程在 Machine 的阻塞方法里等待用的事件。
    static EVENT: Arc<Event> = Arc::new(Event::new().expect("cannot create an eventfd"));
}

impl Machine {
    /// 建立内核实例：打开 inode 仓库，建好内置的 init（pid 1）。
    pub fn start(options: &MachineOptions) -> Result<Machine, RunError> {
        crate::bug::host_call("Machine::start", || Self::start_kernel(options))
            .unwrap_or(Err(RunError::Internal))
    }

    fn start_kernel(options: &MachineOptions) -> Result<Machine, RunError> {
        if options.tier == Tier::Dbt && Dbt::new().is_none() {
            return Err(RunError::TierUnavailable);
        }
        let vfs = match &options.rootfs {
            Some(dir) => Some(Rc::new(Vfs::open(dir).map_err(RunError::Rootfs)?)),
            None => None,
        };
        appine_hal::raise_fd_limit();
        // 引擎的 bug 只结束撞上它的 guest 进程（crate::bug）
        if crate::bug::contain() {
            appine_engine::install_fault_guard();
        }
        let shared = Arc::new(Shared::new(Kernel::new(options.tier)));
        if let Some(vfs) = &vfs {
            // 推迟提交的写事务由定时器线程到点提交
            let timers = shared.timers.clone();
            vfs.store().set_waker(Box::new(move || timers.signal()));
        }
        {
            let mut k = shared.lock();
            k.vfs = vfs;
            k.oom.limit = options
                .memory_limit
                .unwrap_or_else(crate::oom::default_limit);
            let init = ThreadGroup::new(INIT, 0, Rc::new(RefCell::new(SigHandlers::default())));
            k.groups.insert(INIT, Rc::new(RefCell::new(init)));
        }
        let timer_shared = shared.clone();
        let timers = std::thread::Builder::new()
            .name("timers".into())
            .spawn(move || timers(timer_shared))
            .map_err(RunError::Io)?;
        Ok(Machine {
            shared,
            timers: Some(timers),
        })
    }

    /// 启动一个新进程（init 的子进程），返回它的 pid。设置了仓库时 `program` 是 guest
    /// 文件系统里的路径，否则是宿主路径。
    pub fn spawn(
        &self,
        program: &[u8],
        argv: &[Vec<u8>],
        envp: &[Vec<u8>],
        stdio: Stdio,
    ) -> Result<u32, RunError> {
        crate::bug::host_call("Machine::spawn", || {
            self.spawn_process(program, argv, envp, stdio)
        })
        .unwrap_or(Err(RunError::Internal))
    }

    fn spawn_process(
        &self,
        program: &[u8],
        argv: &[Vec<u8>],
        envp: &[Vec<u8>],
        stdio: Stdio,
    ) -> Result<u32, RunError> {
        let io_err = |e: Errno| RunError::Io(io::Error::from_raw_os_error(e.0));
        let mut k = self.shared.lock();
        let vfs = k.vfs.clone();
        let offline = crate::pseudo::Offline;
        let file = match &vfs {
            Some(vfs) => read_executable(
                vfs,
                &offline,
                &Cred::root(),
                &Node::ROOT,
                &Node::ROOT,
                program,
            )
            .map_err(RunError::Exec)?,
            None => {
                let f = std::fs::File::open(OsStr::from_bytes(program)).map_err(RunError::Io)?;
                Executable::open(f).map_err(|e| RunError::Io(io::Error::from_raw_os_error(e.0)))?
            }
        };
        let args = Args {
            argv,
            envp,
            execfn: program,
            ids: [0; 4],
            stack_limit: loader::STACK_SIZE,
        };
        let mut mm = Mm::new();
        mm.share_files(&k.file_maps);
        let mut read_interp = |p: &[u8]| match &vfs {
            Some(vfs) => read_executable(vfs, &offline, &Cred::root(), &Node::ROOT, &Node::ROOT, p),
            None => Err(Errno::ENOENT),
        };
        let image =
            loader::load(&mut mm, &file, &args, &mut read_interp).map_err(RunError::Load)?;
        let real = vfs.as_ref().and_then(|vfs| {
            vfs.real_path(&offline, &Cred::root(), &Node::ROOT, &Node::ROOT, program)
                .ok()
        });
        let exe: Rc<[u8]> =
            Rc::from(real.unwrap_or_else(|| crate::vfs::normalize(&[b"/", program].concat())));
        let ino_of = |p: &[u8]| {
            vfs.as_ref()
                .and_then(|vfs| {
                    vfs.resolve(&offline, &Cred::root(), &Node::ROOT, &Node::ROOT, p, true)
                        .ok()
                })
                .and_then(|n| match n {
                    Node::Store(ino) => Some(ino),
                    Node::Pseudo(_) => None,
                })
                .unwrap_or(0)
        };
        let interp_ino = image.interp.as_ref().map_or(0, |(p, ..)| ino_of(p));
        crate::syscall::label_image(&mut mm, &image, &exe, (ino_of(program), interp_ino));

        let tid = k.alloc_tid().map_err(io_err)?;
        let mut group = ThreadGroup::new(tid, INIT, Rc::new(RefCell::new(SigHandlers::default())));
        group.exe = exe.to_vec();
        let fds = match stdio {
            Stdio::Host => FdTable::with_host_stdio(),
            Stdio::Console(id) => {
                let (slave, tty) = terminal::attach(&mut k, id, tid).map_err(RunError::Console)?;
                group.ctty = Some(tty);
                let mut table = FdTable::new();
                for fd in 0..3 {
                    table.install_at(fd, slave.clone(), false).unwrap();
                }
                table
            }
        };
        let mm = mm.into_shared();
        // SAFETY: Mm 由任务的 TaskState 持有，线程退出或 execve 之前不会释放
        let mut cpu = unsafe { Cpu::new(mm.borrow().mmu()) };
        cpu.state_mut().pc = image.entry;
        cpu.state_mut().sp = image.sp;
        let group = Rc::new(RefCell::new(group));
        let state = TaskState {
            mm,
            fds: Rc::new(RefCell::new(fds)),
            fs: Rc::new(RefCell::new(FsContext {
                root: Node::ROOT,
                cwd: Node::ROOT,
                umask: 0o022,
            })),
            group: group.clone(),
            vfs,
            altstack: Default::default(),
            clear_child_tid: 0,
            saved_sigmask: None,
            vfork_done: None,
            restart: None,
        };
        let task = Task::new(tid, tid, &cpu, state, 0, comm_of(program)).map_err(io_err)?;
        k.groups.insert(tid, group);
        k.groups[&INIT].borrow_mut().children.push(tid);
        k.forks += 1;
        k.watched.insert(tid, None);
        task::start(&self.shared, &mut k, task, cpu).map_err(io_err)?;
        Ok(tid as u32)
    }

    /// 在持有 BKL 时反复检查 `cond`，直到它给出结果。
    fn wait_for<T>(&self, cond: impl FnMut(&mut Kernel) -> Option<T>) -> T {
        self.wait_on(wake::ALL, cond)
    }

    /// 与 [`Machine::wait_for`] 相同，但只被 `interest` 里的通道（[`wake`]）叫醒。
    fn wait_on<T>(&self, interest: u32, mut cond: impl FnMut(&mut Kernel) -> Option<T>) -> T {
        EVENT.with(|event| {
            let mut k = self.shared.lock();
            loop {
                if let Some(v) = cond(&mut k) {
                    return v;
                }
                let (k2, _) = block(&self.shared, k, event, None, &mut [], interest);
                k = k2;
            }
        })
    }

    /// 等 [`Machine::spawn`] 启动的进程结束。
    pub fn wait(&self, pid: u32) -> ExitStatus {
        // 只有宿主在等的进程结束时才要叫醒这里，别的进程结束、关闭文件都不必
        self.wait_on(wake::WATCHED, |k| {
            k.watched.get(&(pid as i32)).copied().flatten()
        })
    }

    /// [`Machine::spawn`] 启动的进程已经结束时返回它的结束方式。
    pub fn try_wait(&self, pid: u32) -> Option<ExitStatus> {
        self.shared
            .lock()
            .watched
            .get(&(pid as i32))
            .copied()
            .flatten()
    }

    /// 新建一个控制台，返回它的 PTY 编号（`/dev/pts/<编号>`）。
    pub fn open_console(&self, rows: u16, cols: u16) -> Result<u32, Errno> {
        self.open_console_with(Winsize {
            rows,
            cols,
            ..Winsize::default()
        })
    }

    fn open_console_with(&self, size: Winsize) -> Result<u32, Errno> {
        crate::bug::host_call("Machine::open_console", || {
            terminal::open(&mut self.shared.lock(), size)
        })
        .ok_or(Errno::EIO)
    }

    /// 读控制台的输出，没有输出时等待。返回 0 表示会话结束：所有从设备都关闭了，
    /// 输出也已经读完。
    pub fn read_console(&self, id: u32, buf: &mut [u8]) -> Result<usize, Errno> {
        let out = self.wait_for(|k| {
            on_console(k, id, "Machine::read_console", |k| {
                terminal::take_output(k, id, buf.len())
            })
            .transpose()
        })?;
        buf[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }

    /// 向控制台输入（经过行规程），输入队列满时等待。返回写入的字节数；会话已经结束时
    /// 返回 EIO。
    pub fn write_console(&self, id: u32, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0);
        }
        self.wait_for(|k| {
            on_console(k, id, "Machine::write_console", |k| {
                let room = terminal::input_room(k, id)?;
                if room > 0 {
                    let n = room.min(data.len());
                    return terminal::feed(k, id, &data[..n]).map(|()| Some(n));
                }
                // 输入队列满了，但已经没有进程会读它
                match terminal::take_output(k, id, 0) {
                    Ok(Some(_)) => Err(Errno::EIO),
                    _ => Ok(None),
                }
            })
            .transpose()
        })
    }

    /// 控制台的窗口大小变了：前台进程组收到 SIGWINCH。
    pub fn resize_console(&self, id: u32, rows: u16, cols: u16) -> Result<(), Errno> {
        let w = Winsize {
            rows,
            cols,
            ..Winsize::default()
        };
        on_console(
            &mut self.shared.lock(),
            id,
            "Machine::resize_console",
            |k| terminal::resize(k, id, w),
        )
    }

    /// 关闭控制台（终端标签页关闭）：会话组长收到 SIGHUP。
    pub fn close_console(&self, id: u32) -> Result<(), Errno> {
        crate::bug::host_call("Machine::close_console", || {
            terminal::close(&mut self.shared.lock(), id)
        })
        .unwrap_or(Err(Errno::EIO))
    }

    /// 结束所有 guest 进程，等所有 guest 线程退出。
    pub fn kill_all(&self) {
        crate::bug::host_call("Machine::kill_all", || {
            let k = self.shared.lock();
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
        });
        self.wait_for(|k| k.tasks.is_empty().then_some(()));
    }

    /// 宿主内存紧张（Android 的 `onTrimMemory`）：丢掉可以重建的缓存。现在是池里保留的
    /// Tier 1 翻译（运行中的进程的翻译不动）。App 这时多半在后台，随时可能被杀掉，顺便
    /// 提交推迟的写事务。
    pub fn trim_memory(&self) {
        appine_engine::trim_dbt_pool();
        let k = self.shared.lock();
        k.file_maps.clear();
        if let Some(vfs) = &k.vfs {
            vfs.trim();
        }
        drop(k);
        self.sync();
    }

    /// 提交 inode 仓库推迟提交的写事务（App 退到后台时，[`crate::vfs::store`]）。
    pub fn sync(&self) {
        crate::bug::host_call("Machine::sync", || {
            if let Some(vfs) = &self.shared.lock().vfs {
                let _ = vfs.store().commit();
            }
        });
    }

    /// 使用 Tier 1 时已退出线程的统计之和。
    pub fn dbt_stats(&self) -> Option<DbtStats> {
        self.shared.lock().dbt_stats
    }
}

/// 执行控制台 `id` 上的宿主调用。内核层 panic 时像终端被挂断一样关闭这个控制台（会话组长
/// 收到 SIGHUP），返回 EIO。
fn on_console<T>(
    k: &mut Kernel,
    id: u32,
    what: &str,
    f: impl FnOnce(&mut Kernel) -> Result<T, Errno>,
) -> Result<T, Errno> {
    crate::bug::host_call(what, || f(k)).unwrap_or_else(|| {
        let _ = crate::bug::host_call("Machine::close_console", || terminal::close(k, id));
        Err(Errno::EIO)
    })
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.kill_all();
        self.sync();
        {
            let mut k = self.shared.lock();
            k.shutting_down = true;
            request_wake();
        }
        if let Some(t) = self.timers.take() {
            let _ = t.join();
        }
    }
}

/// 定时器线程：等到最早的到期时刻，发出到期的 SIGALRM（[`crate::itimer::expire`]），
/// 有 guest 线程在运行时定时检查内存的上限（[`crate::oom`]），到点提交 inode 仓库推迟
/// 提交的写事务，直到 Machine 关闭。
///
/// 撞上内核层的 bug 时（[`crate::bug`]）照常运行：处理一个进程的定时器时出错，结束那个
/// 进程；内存上限的检查和信号驱动的 I/O 分不出是哪个进程的，出错时关掉这项功能。
fn timers(shared: Arc<Shared>) {
    let event = shared.timers.clone();
    let mut k = shared.lock();
    while !k.shutting_down {
        let mut next = crate::itimer::expire(&k);
        match crate::bug::catch(|| crate::oom::check(&mut k, &shared)) {
            Some(Some(check)) => next = Some(next.map_or(check, |n| n.min(check))),
            Some(None) => {}
            None => {
                crate::bug::report(format_args!(
                    "kernel panic in the OOM killer; the memory limit is off"
                ));
                k.oom.limit = u64::MAX;
            }
        }
        // inode 仓库推迟提交的写事务：开始推迟时叫醒这个线程
        if let Some(vfs) = k.vfs.clone() {
            match crate::bug::catch(|| vfs.store().commit_due()) {
                Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
                Some(None) => {}
                None => crate::bug::report(format_args!(
                    "kernel panic while committing the inode store"
                )),
            }
        }
        // 信号驱动的 I/O：任何修改都会叫醒这个线程，顺便检查；宿主 socket 一起等
        let fasync = crate::bug::catch(|| crate::syscall::check_fasync(&mut k));
        let mut host = fasync.unwrap_or_else(|| {
            crate::bug::report(format_args!(
                "kernel panic in signal-driven I/O; no more SIGIO for the files that used it"
            ));
            k.fasync.clear();
            Vec::new()
        });
        // 定时器的设置不是打开文件上的修改；有信号驱动的 I/O 时才要看每一次修改
        let interest = if k.fasync.is_empty() {
            wake::OTHER
        } else {
            wake::ALL
        };
        let (k2, _) = block(&shared, k, &event, next, &mut host, interest);
        k = k2;
    }
}

/// 加载 guest 程序并运行到它结束，然后结束其余的进程（`appine-run`）。
///
/// 设置了 [`RunOptions::rootfs`] 时，`program` 是 guest 文件系统里的路径；
/// 否则是宿主路径。
pub fn run(
    program: &OsStr,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    let machine = Machine::start(&MachineOptions {
        tier: options.tier,
        rootfs: options.rootfs.clone(),
        memory_limit: options.memory_limit,
    })?;
    // 有终端时，进程是会话组长，接到宿主终端的控制台是它的控制终端
    let console = options
        .terminal
        .map(|term| {
            let id = machine.open_console_with(terminal::initial_size(&term));
            id.map(|id| (term, id)).map_err(RunError::Console)
        })
        .transpose()?;
    let stdio = console.map_or(Stdio::Host, |(_, id)| Stdio::Console(id));
    let pid = machine.spawn(program.as_bytes(), argv, envp, stdio)?;
    let bridge = console.map(|(term, id)| {
        let shared = machine.shared.clone();
        std::thread::Builder::new()
            .name("terminal".into())
            .spawn(move || terminal::bridge(shared, term, id, pid as i32))
            .expect("cannot start the terminal thread")
    });
    let status = machine.wait(pid);
    machine.kill_all();
    // 桥接线程把剩下的输出写完才退出
    if let Some(b) = bridge {
        let _ = b.join();
    }
    Ok(RunReport {
        status,
        dbt_stats: machine.dbt_stats(),
    })
}
