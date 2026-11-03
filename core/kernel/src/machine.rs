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

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;

use appine_engine::{Cpu, Dbt, DbtStats};
use appine_hal::Event;

use crate::cell::RefCell;
use crate::cred::Cred;
use crate::errno::Errno;
use crate::fd::FdTable;
use crate::kernel::{INIT, Kernel, Shared, block, block_for, request_wake, wake};
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
    /// 1024 以下的端口在宿主上换成的端口的偏移（[`crate::portmap`]）：None 是默认的 10000，
    /// 0 表示不映射（Android 上绑多数低端口是 EACCES）。
    pub low_port_offset: Option<u16>,
    /// 宿主让绑原来的低端口时也换一个（默认绑原来的）：在绑得了低端口的宿主（以 root 运行、
    /// 容器里）上测映射。
    pub always_map_low_ports: bool,
}

/// 新进程的 stdin/stdout/stderr。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// 宿主进程自己的 0、1、2。
    Host,
    /// 控制台（PTY 编号）的从设备：进程成为新会话的组长，控制台是它的控制终端。
    Console(u32),
}

/// 正在运行、用着仓库的内核：宿主另外打开仓库改了东西时（[`crate::browse_changing`]）找到
/// 它们报 inotify 事件（[`store_changed`]）。仓库目录按规范化的路径比；内核停了弱引用就
/// 失效，下次登记、查找时清掉。
static RUNNING: std::sync::Mutex<Vec<(PathBuf, std::sync::Weak<Shared>)>> =
    std::sync::Mutex::new(Vec::new());

/// 宿主改了仓库 `store` 里的 `changes`：交给用着它、正在运行的内核，各自持着 BKL 照对应的
/// syscall 报 inotify 事件（[`crate::syscall::host_change`]）。
pub(crate) fn store_changed(store: &std::path::Path, changes: &[crate::vfs::browse::Change]) {
    if changes.is_empty() {
        return;
    }
    let dir = store.canonicalize().unwrap_or_else(|_| store.to_path_buf());
    let kernels: Vec<Arc<Shared>> = {
        let mut running = RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.retain(|(_, w)| w.strong_count() > 0);
        running
            .iter()
            .filter(|(d, _)| *d == dir)
            .filter_map(|(_, w)| w.upgrade())
            .collect()
    };
    for shared in kernels {
        let k = shared.lock();
        for c in changes {
            crate::syscall::host_change(&k, c);
        }
    }
}

/// 一个 guest 内核实例。析构时结束所有 guest 进程。
pub struct Machine {
    shared: Arc<Shared>,
    timers: Option<JoinHandle<()>>,
}

/// [`run`] 关机时 inittab 里关机的命令一共能用多久（OpenRC 停 PostgreSQL 最多等 15 秒）。
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        crate::bug::install_panic_hook();
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
            if let Some(vfs) = &vfs {
                vfs.store().guard_with_bkl(shared.id());
            }
            k.vfs = vfs;
            k.oom.limit = options
                .memory_limit
                .unwrap_or_else(crate::oom::default_limit);
            {
                let mut portmap = k.portmap.borrow_mut();
                portmap.offset = options
                    .low_port_offset
                    .unwrap_or(crate::portmap::DEFAULT_OFFSET);
                portmap.always_map = options.always_map_low_ports;
            }
            let init = ThreadGroup::new(INIT, 0, Rc::new(RefCell::new(SigHandlers::default())));
            k.groups.insert(INIT, Rc::new(RefCell::new(init)));
        }
        if let Some(dir) = &options.rootfs {
            let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            let mut running = RUNNING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            running.retain(|(_, w)| w.strong_count() > 0);
            running.push((dir, Arc::downgrade(&shared)));
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
            secure: false,
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
        let walked = vfs.as_ref().and_then(|vfs| {
            vfs.walk(
                &offline,
                &Cred::root(),
                &Node::ROOT,
                &Node::ROOT,
                program,
                true,
            )
            .ok()
        });
        let real = match (&vfs, &walked) {
            (Some(vfs), Some((node, found))) => vfs
                .found_path(&offline, &Node::ROOT, node, found.as_ref())
                .ok(),
            _ => None,
        };
        let exe: Rc<[u8]> =
            Rc::from(real.unwrap_or_else(|| crate::vfs::normalize(&[b"/", program].concat())));
        let ino_of = |p: &[u8]| {
            vfs.as_ref()
                .and_then(|vfs| {
                    vfs.resolve(&offline, &Cred::root(), &Node::ROOT, &Node::ROOT, p, true)
                        .ok()
                })
                .and_then(|n| n.ino())
                .unwrap_or(0)
        };
        let root = Node::ROOT;
        let interp = crate::syscall::interp_label(
            vfs.as_deref(),
            &offline,
            &Cred::root(),
            &root,
            &root,
            &image,
        );
        crate::syscall::label_image(&mut mm, &image, (&exe, ino_of(program)), interp);
        // 与 execve 一样记下程序文件（ETXTBSY、池里翻译的键、共用翻译的组）
        mm.exe = Some(ino_of(program)).filter(|&ino| ino != 0);

        let tid = k.alloc_tid().map_err(io_err)?;
        let mut group = ThreadGroup::new(tid, INIT, Rc::new(RefCell::new(SigHandlers::default())));
        group.exe = Some(Rc::new(crate::process::ExeFile::new(
            exe.clone(),
            walked,
            None,
            None,
            mm.exe,
            &file,
        )));
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
                mnt_ns: vfs.as_ref().map_or_else(
                    || {
                        Rc::new(crate::vfs::MntNs {
                            id: crate::vfs::INIT_MNT_NS,
                            owner: crate::cred::UserNs::init(),
                            anon: false,
                        })
                    },
                    |v| v.init_ns(),
                ),
            })),
            group: group.clone(),
            vfs,
            altstack: Default::default(),
            clear_child_tid: 0,
            vfork_done: None,
            restart: None,
            fast_sleep: None,
        };
        let ns = k.init_ns.clone();
        let task = Task::new(tid, tid, &cpu, state, ns, 0, comm_of(program)).map_err(io_err)?;
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
    fn wait_on<T>(&self, interest: u32, cond: impl FnMut(&mut Kernel) -> Option<T>) -> T {
        self.wait_objects(interest, None, cond)
    }

    /// 与 [`Machine::wait_on`] 相同；`objects` 不为 None 时，只影响别的对象的修改不叫醒它
    /// （[`crate::kernel::Cx::wait_objects`]）。
    fn wait_objects<T>(
        &self,
        interest: u32,
        objects: Option<&[wake::Object]>,
        cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> T {
        self.wait_deadline(interest, objects, None, cond)
            .expect("waited without a deadline")
    }

    /// 与 [`Machine::wait_on`] 相同，到了 `deadline` 还没等到时返回 None。
    fn wait_until<T>(
        &self,
        interest: u32,
        deadline: Option<std::time::Instant>,
        cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> Option<T> {
        self.wait_deadline(interest, None, deadline, cond)
    }

    fn wait_deadline<T>(
        &self,
        interest: u32,
        objects: Option<&[wake::Object]>,
        deadline: Option<std::time::Instant>,
        mut cond: impl FnMut(&mut Kernel) -> Option<T>,
    ) -> Option<T> {
        EVENT.with(|event| {
            let mut k = self.shared.lock();
            loop {
                if let Some(v) = cond(&mut k) {
                    return Some(v);
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    return None;
                }
                let (k2, _) =
                    block_for(&self.shared, k, event, deadline, &mut [], interest, objects);
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
        let objects = terminal::wake_objects(&self.shared.lock(), id);
        let out = self.wait_objects(wake::FILE, objects.as_deref(), |k| {
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
        let objects = terminal::wake_objects(&self.shared.lock(), id);
        self.wait_objects(wake::FILE, objects.as_deref(), |k| {
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
            let mut k = self.shared.lock();
            k.stopping = true;
            let groups: Vec<_> = k.groups.values().cloned().collect();
            for g in groups.iter().filter(|g| !g.borrow().threads.is_empty()) {
                signal::group_exit(
                    &k,
                    g,
                    ExitStatus::Signaled {
                        signal: Signal::KILL,
                        pc: None,
                        fault_addr: None,
                        core_dumped: false,
                    },
                );
            }
        });
        self.wait_for(|k| k.tasks.is_empty().then_some(()));
    }

    /// 按 `/etc/inittab` 开机（[`crate::inittab`]）：依次运行 sysinit、boot、bootwait、默认
    /// 运行级别的 wait（每个都等它结束）和 once（不等），输出写到 [`crate::inittab::LOG`]。
    /// 返回时 once 的命令可能还在运行。没有 inittab、没有仓库时什么也不做。之后内置的 init
    /// 接受 busybox 的 reboot、halt、poweroff 的请求（[`Machine::take_power`]）。
    pub fn boot(&self) {
        self.shared.lock().init_handlers = true;
        let entries = self.inittab();
        let mut first = true;
        for (process, wait) in crate::inittab::boot_plan(&entries) {
            if self.shared.lock().stopping {
                break;
            }
            let Some(pid) = self.run_init_process(&process, first) else {
                continue;
            };
            first = false;
            if wait {
                self.wait(pid);
            }
        }
        // 一个命令也没运行（没装 OpenRC）：上次开机的输出不留着，免得被当成这次的（App 从里面
        // 找没起来的服务）
        if first {
            crate::bug::host_call("Machine::boot", || {
                let k = self.shared.lock();
                let Some(vfs) = &k.vfs else { return };
                let log = crate::inittab::LOG.as_bytes();
                if crate::vfs::read_file(vfs.store(), log).is_ok_and(|t| !t.is_empty()) {
                    let _ = crate::vfs::write_file(vfs.store(), log, b"", 0o644, true);
                }
            });
        }
    }

    /// 关机（busybox init 的 `run_shutdown_and_kill_processes`）：开过机的先依次运行 inittab
    /// 里关机的命令（[`crate::inittab::shutdown_plan`]），然后给所有进程发 SIGTERM，最多等
    /// 1 秒，再结束剩下的（[`Machine::kill_all`]）。`timeout` 限制关机的命令一共能用多久，
    /// 到了就不再等。
    pub fn shutdown(&self, power: crate::Power, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let init = {
            let mut k = self.shared.lock();
            k.stopping = true;
            k.init_handlers
        };
        if init {
            for process in crate::inittab::shutdown_plan(&self.inittab(), power) {
                if let Some(pid) = self.run_init_process(&process, false) {
                    let exited = |k: &mut Kernel| k.watched.get(&(pid as i32)).copied().flatten();
                    if self
                        .wait_until(wake::WATCHED, Some(deadline), exited)
                        .is_none()
                    {
                        break;
                    }
                }
            }
        }
        crate::bug::host_call("Machine::shutdown", || {
            let k = self.shared.lock();
            let groups: Vec<_> = k.groups.values().cloned().collect();
            for g in groups.iter().filter(|g| !g.borrow().threads.is_empty()) {
                let tgid = g.borrow().tgid;
                let info = crate::signal::SigInfo::kill(crate::signal::code::SI_USER, INIT, 0);
                signal::send_to_group(&k, tgid, Signal::TERM, info);
            }
        });
        let second = std::time::Instant::now() + std::time::Duration::from_secs(1);
        self.wait_until(wake::ALL, Some(second), |k| {
            k.tasks.is_empty().then_some(())
        });
        self.kill_all();
    }

    /// 控制台 `id` 前台进程组的当前目录（App 被杀之后恢复标签页时，新的 shell 从这里开始，
    /// 设计文档 §11.2）。shell 已经退出时是 ESRCH。
    pub fn console_cwd(&self, id: u32) -> Result<Vec<u8>, Errno> {
        let k = self.shared.lock();
        crate::bug::host_call("Machine::console_cwd", || terminal::fg_cwd(&k, id))
            .unwrap_or(Err(Errno::EIO))
    }

    /// 还在运行的 guest 进程数（不算内置的 init 和僵尸进程）：App 关掉最后一个标签页之后据此
    /// 决定留在后台还是退出（设计文档 §11.2）。
    pub fn processes(&self) -> usize {
        let k = self.shared.lock();
        k.groups
            .values()
            .filter(|g| {
                let g = g.borrow();
                g.tgid != INIT && !g.threads.is_empty()
            })
            .count()
    }

    /// 取走 guest 发给内置 init 的关机请求（busybox 的 reboot、halt、poweroff）。只在
    /// [`Machine::boot`] 之后才有。
    pub fn take_power(&self) -> Option<crate::Power> {
        self.shared.lock().power.take()
    }

    /// 等 [`Machine::spawn`] 启动的进程结束，或者 guest 请求关机（[`Machine::take_power`]，
    /// 请求不取走）。
    pub fn wait_or_power(&self, pid: u32) -> Result<ExitStatus, crate::Power> {
        self.wait_on(wake::WATCHED, |k| match k.watched.get(&(pid as i32)) {
            Some(Some(status)) => Some(Ok(*status)),
            _ => k.power.map(Err),
        })
    }

    /// 读 `/etc/inittab`（跟随符号链接），没有时是空表。持着 BKL 读：关机时 guest 进程还在
    /// 运行，仓库（SQLite 的连接、各种缓存）只能在 BKL 下用。
    fn inittab(&self) -> Vec<crate::inittab::Entry> {
        let text = crate::bug::host_call("Machine::inittab", || {
            let k = self.shared.lock();
            let vfs = k.vfs.as_ref()?;
            let offline = crate::pseudo::Offline;
            let root = Cred::root();
            match vfs.resolve(
                &offline,
                &root,
                &Node::ROOT,
                &Node::ROOT,
                b"/etc/inittab",
                true,
            ) {
                Ok(Node::Store(_, ino)) => {
                    let mut text = Vec::new();
                    let f = vfs.store().open_object(ino, false).ok()?;
                    io::Read::read_to_end(&mut &f, &mut text).ok()?;
                    Some(text)
                }
                _ => None,
            }
        });
        text.flatten()
            .map(|t| crate::inittab::parse(&t))
            .unwrap_or_default()
    }

    /// 由 `/bin/sh -c` 运行 inittab 里的命令（[`crate::inittab::script`]），返回 pid。命令的
    /// 程序是不存在的绝对路径时（比如没装 OpenRC），或者起不来时，返回 None。
    fn run_init_process(&self, process: &[u8], first: bool) -> Option<u32> {
        if let Some(program) = crate::inittab::program(process) {
            // 持着 BKL 查：前面的 once 命令可能已经在运行（见 [`Machine::inittab`]）
            let offline = crate::pseudo::Offline;
            let found = crate::bug::host_call("Machine::boot", || {
                let k = self.shared.lock();
                k.vfs.as_ref().is_some_and(|vfs| {
                    vfs.resolve(
                        &offline,
                        &Cred::root(),
                        &Node::ROOT,
                        &Node::ROOT,
                        program,
                        true,
                    )
                    .is_ok()
                })
            });
            if found != Some(true) {
                return None;
            }
        }
        let argv = [
            b"/bin/sh".to_vec(),
            b"-c".to_vec(),
            crate::inittab::script(process, first),
        ];
        let envp: Vec<Vec<u8>> = crate::inittab::ENV.iter().map(|e| e.to_vec()).collect();
        match self.spawn(b"/bin/sh", &argv, &envp, Stdio::Host) {
            Ok(pid) => Some(pid),
            Err(e) => {
                crate::debug::log(format_args!("cannot run an inittab entry: {e}"));
                None
            }
        }
    }

    /// 宿主内存紧张、App 的界面离开了一阵子（设计文档 §11.2）：丢掉可以重建的缓存，即池里
    /// 保留的 Tier 1 翻译、共用的只读文件映射和 dentry/inode 缓存（运行中的进程用着的不动）。
    /// App 这时多半在后台，随时可能被杀掉，顺便提交推迟的写事务。
    pub fn trim_memory(&self) {
        crate::debug::log(format_args!("trimming memory"));
        appine_engine::trim_dbt_pool();
        crate::bug::host_call("Machine::trim_memory", || {
            let k = self.shared.lock();
            k.file_maps.clear();
            if let Some(vfs) = &k.vfs {
                vfs.trim();
            }
        });
        self.sync();
    }

    /// 宿主的网络变了（设计文档 §10）：主表里的路由换成 `routes`，guest 的 `ip route`、
    /// `/proc/net/route` 等看到的就是它们（[`crate::route`]）。Android 上 App 读不到宿主的
    /// 路由表，由它从默认网络的 `LinkProperties` 取了交进来；None 时改回读宿主的，读不到时
    /// 按接口的地址合成。
    pub fn set_routes(&self, routes: Option<Vec<crate::HostRoute>>) {
        crate::bug::host_call("Machine::set_routes", || {
            self.shared.lock().host_routes = routes;
        });
    }

    /// 提交 inode 仓库推迟提交的写事务（App 退到后台时，[`crate::vfs::store`]）。
    pub fn sync(&self) {
        crate::bug::host_call("Machine::sync", || {
            if let Some(vfs) = &self.shared.lock().vfs {
                let _ = vfs.store().commit();
            }
        });
    }

    /// 取走 guest 在非回环地址上第一次监听的端口（设计文档 §12 #7）：App 据此发通知，
    /// 告诉用户别的设备连哪个端口（低端口映射到了别处，[`crate::portmap`]）。
    pub fn take_listens(&self) -> Vec<crate::ListenEvent> {
        crate::bug::host_call("Machine::take_listens", || {
            self.shared.lock().portmap.borrow_mut().take_listens()
        })
        .unwrap_or_default()
    }

    /// 取走内核层结束了的 guest 进程（内核 bug、引擎的宿主故障、OOM killer）：App 在它们的
    /// 标签页里说明原因，不在标签页里的发通知（设计文档 §11.2）。
    pub fn take_kills(&self) -> Vec<crate::KillReport> {
        crate::bug::host_call("Machine::take_kills", || {
            std::mem::take(&mut *self.shared.lock().kills.borrow_mut())
        })
        .unwrap_or_default()
    }

    /// 取走 guest 改过目录项（新建、删除、改名）的目录在仓库里的路径：App 让"文件"App 刷新
    /// 它们（设计文档 §11.3）。第一次调用时才开始记；删掉了的、tmpfs 里的不给。
    pub fn take_changed_dirs(&self) -> Vec<Vec<u8>> {
        crate::bug::host_call("Machine::take_changed_dirs", || {
            let k = self.shared.lock();
            let Some(vfs) = &k.vfs else {
                return Vec::new();
            };
            let store = vfs.store();
            store
                .take_changed_dirs()
                .into_iter()
                .filter_map(|d| crate::vfs::browse::dir_path(store, d).ok())
                .collect()
        })
        .unwrap_or_default()
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
/// 有 guest 线程在运行时定时检查内存的上限、拆代码缓存在可写视图里写过的页
/// （[`crate::oom`]）、为平均负载取样（[`crate::loadavg`]），到点提交 inode 仓库推迟提交的
/// 写事务，直到 Machine 关闭。
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
        match crate::bug::catch(|| crate::oom::unmap_code_views(&mut k, &shared)) {
            Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
            Some(None) => {}
            None => crate::bug::report(format_args!(
                "kernel panic while unmapping the written code views"
            )),
        }
        match crate::bug::catch(|| crate::loadavg::sample(&mut k, &shared)) {
            Some(Some(due)) => next = Some(next.map_or(due, |n| n.min(due))),
            Some(None) => {}
            None => crate::bug::report(format_args!("kernel panic in the load average")),
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
        // EPOLLEXCLUSIVE 的独占门：这里替等着的线程看文件，来了新的边沿只叫醒一个
        match crate::bug::catch(|| crate::syscall::check_exclusive(&mut k)) {
            Some(fds) => host.extend(fds),
            None => {
                crate::bug::report(format_args!(
                    "kernel panic in EPOLLEXCLUSIVE wakeups; waking every exclusive waiter"
                ));
                for gate in std::mem::take(&mut k.ex_gates) {
                    gate.wake_all();
                }
            }
        }
        // raw ICMP socket 的汇集点：替 poll、epoll 的等待者取下到了的报文
        match crate::bug::catch(|| crate::rawicmp::check_taps(&mut k)) {
            Some(fds) => host.extend(fds),
            None => crate::bug::report(format_args!("kernel panic in the raw ICMP sockets")),
        }
        // 定时器的设置不是打开文件上的修改；有信号驱动的 I/O、独占门时才要看每一次修改
        let interest = if k.fasync.is_empty() && k.ex_gates.is_empty() {
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
        low_port_offset: options.low_port_offset,
        always_map_low_ports: options.always_map_low_ports,
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
    if options.init {
        machine.boot();
    }
    let pid = machine.spawn(program.as_bytes(), argv, envp, stdio)?;
    let bridge = console.map(|(term, id)| {
        let shared = machine.shared.clone();
        std::thread::Builder::new()
            .name("terminal".into())
            .spawn(move || terminal::bridge(shared, term, id, pid as i32))
            .expect("cannot start the terminal thread")
    });
    let status = if options.init {
        // 程序结束时是关机；guest 请求了重启也只是关机，结束 appine-run
        let power = machine.wait_or_power(pid).err();
        machine.shutdown(power.unwrap_or(crate::Power::PowerOff), SHUTDOWN_TIMEOUT);
        machine.wait(pid)
    } else {
        let status = machine.wait(pid);
        machine.kill_all();
        status
    };
    // 桥接线程把剩下的输出写完才退出
    if let Some(b) = bridge {
        let _ = b.join();
    }
    Ok(RunReport {
        status,
        dbt_stats: machine.dbt_stats(),
    })
}
