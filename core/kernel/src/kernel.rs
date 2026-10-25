//! 内核层的全局状态、大内核锁和统一的等待机制（设计文档 §6）。
//!
//! 所有 guest 线程共享一个 [`Kernel`]，由一把大内核锁（BKL）保护：处理 syscall
//! 和投递信号时持有，执行 guest 代码时不持有，等待之前释放。内核对象之间用
//! `Rc`/`RefCell` 共享，它们只在持有 BKL 时被访问；唯一的例外是线程自己的
//! [`TaskState`]，只有它所属的线程会访问，等待期间也一直借用着。
//!
//! 等待统一走宿主 `poll()`：每个线程有一个 eventfd，等待时把它和要等的宿主 fd
//! 放在一起 poll。任何可能让别人的等待条件成立的修改都调用 [`request_wake`]，
//! 释放 BKL 时唤醒所有正在等待的线程，由它们各自重新检查条件。futex 是例外：停在锁
//! 和条件变量上的线程很多，它们用 [`Cx::wait_alone`] 等待，只被 FUTEX_WAKE、信号和
//! 结束线程组直接通知。

use std::cell::{Ref, RefCell, RefMut};
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use appine_engine::DbtStats;
use appine_hal::Event;

use crate::errno::Errno;
use crate::fd::{FdTable, FileRef};
use crate::futex::Futexes;
use crate::mm::Mm;
use crate::process::{CpuTime, ExitStatus, ThreadGroup, Tier};
use crate::task::{Task, TaskState};
use crate::tty::Tty;
use crate::vfs::store::Ino;
use crate::vfs::{Node, Vfs};

/// 线程 ID；线程组 ID（pid）就是组里第一个线程的 tid。
pub(crate) type Tid = i32;

/// 内核层内置的 init（设计文档 §6）：只负责收养孤儿进程、回收僵尸进程。
pub(crate) const INIT: Tid = 1;

/// 最大的 tid，与 Linux 64 位上 `pid_max` 的上限相同。
pub(crate) const PID_MAX: Tid = 4 << 20;

/// 主机名、域名的最大长度（`__NEW_UTS_LEN`）。
pub(crate) const UTS_LEN: usize = 64;

/// 有修改可能让等待中的线程继续，释放 BKL 时唤醒它们。
static WAKE: AtomicBool = AtomicBool::new(false);

/// 请求在释放 BKL 时唤醒所有等待中的线程。调用方持有 BKL。
pub(crate) fn request_wake() {
    WAKE.store(true, Ordering::Relaxed);
}

/// 持有 flock(2) 锁的打开文件（弱引用，关闭后锁随之释放），以及是不是排他锁。
pub(crate) type FlockHolder = (std::rc::Weak<RefCell<crate::fd::OpenFile>>, bool);

/// 内核层的全局状态。
pub(crate) struct Kernel {
    /// 所有活着的线程。
    pub tasks: BTreeMap<Tid, Rc<Task>>,
    /// 所有线程组，包括僵尸进程和 init。
    pub groups: BTreeMap<Tid, Rc<RefCell<ThreadGroup>>>,
    /// futex 等待队列。
    pub futexes: Futexes,
    /// 执行引擎。
    pub tier: Tier,
    /// 宿主在等它结束的进程（[`crate::Machine::spawn`] 启动的进程）：结束后记下结束方式。
    pub watched: BTreeMap<Tid, Option<ExitStatus>>,
    /// 根文件系统；没有时 guest 看不到任何文件系统。
    pub vfs: Option<Rc<crate::vfs::Vfs>>,
    /// Machine 正在关闭：定时器线程退出。
    pub shutting_down: bool,
    /// 已退出线程的 Tier 1 统计之和。
    pub dbt_stats: Option<DbtStats>,
    /// 已退出线程用掉的 CPU 时间之和（`/proc/stat`）。
    pub cpu: CpuTime,
    /// 创建过的进程数（`/proc/stat` 的 processes）。
    pub forks: u64,
    /// 本次运行的 `/proc/sys/kernel/random/boot_id`。
    pub boot_id: [u8; 16],
    /// 所有 PTY，按编号；主设备和从设备都关闭之后条目失效。
    pub ptys: BTreeMap<u32, Weak<RefCell<Tty>>>,
    /// 宿主持有主设备的 PTY（[`crate::terminal`]），按 PTY 编号。
    pub consoles: BTreeMap<u32, crate::terminal::Console>,
    /// flock(2) 的锁：每个 inode 上持有锁的打开文件，以及是不是排他锁。打开文件关闭后
    /// 弱引用失效，锁随之释放。
    pub flocks: BTreeMap<Ino, Vec<FlockHolder>>,
    /// fcntl(2) 的记录锁。
    pub locks: crate::locks::Locks,
    /// System V IPC 对象。
    pub ipc: crate::ipc::Ipc,
    /// AF_UNIX socket 绑定的地址。
    pub unix: crate::unix::Names,
    /// 消息里带着打开文件的 AF_UNIX channel（`unix::collect` 用）。
    pub unix_inflight: Vec<Weak<RefCell<crate::unix::Channel>>>,
    /// 正在被打开的命名管道（FIFO）：同一个 inode 的所有打开共用一个管道对象。
    pub fifos: BTreeMap<Ino, Weak<RefCell<crate::pipe::Pipe>>>,
    /// memfd 的封印（F_ADD_SEALS），以 inode 为键；不在这里的文件不能加封印。
    pub seals: BTreeMap<Ino, u32>,
    /// 主机名和 NIS 域名（uname、sethostname、`/proc/sys/kernel/hostname`），最长
    /// [`UTS_LEN`] 字节。
    pub hostname: Vec<u8>,
    pub domainname: Vec<u8>,
    /// `/proc/sys/fs/pipe-max-size`：没有 `CAP_SYS_RESOURCE` 时管道的最大容量。
    pub pipe_max_size: usize,
    /// `/proc/sys/fs/nr_open`：fd 的上限（`RLIMIT_NOFILE` 不能超过它）。
    pub nr_open: usize,
    /// `/proc/sys/kernel/threads-max`：同时存在的线程数的上限，root 也受它限制。
    pub threads_max: usize,
    /// App 内存的上限和 OOM killer（[`crate::oom`]）。
    pub oom: crate::oom::Oom,
    next_tid: Tid,
    /// 正在等待的线程的事件。
    waiters: Vec<Arc<Event>>,
    /// 等待中、因为边沿触发少等了一些事件的 epoll_wait（`syscall::poll`）。
    pub edge_waiters: Vec<Arc<Event>>,
    /// 设置了 `O_ASYNC` 的打开文件（`syscall::poll` 的 fasync）。
    pub fasync: Vec<crate::syscall::Fasync>,
    /// 文件租约，按 inode（`syscall::lease`）。
    pub leases: BTreeMap<Ino, Vec<crate::syscall::Lease>>,
    /// `/proc/sys/fs/lease-break-time`：打破租约时最多等多少秒。
    pub lease_break_time: u64,
    /// POSIX 消息队列，按名字（`syscall::mqueue`）。
    pub mqueues: BTreeMap<Vec<u8>, Rc<RefCell<crate::syscall::MessageQueue>>>,
    /// `/proc/sys/fs/mqueue/*`。
    pub mq_limits: crate::syscall::MqLimits,
    /// AIO 上下文占用的事件数（`/proc/sys/fs/aio-nr`）和上限（`aio-max-nr`）。
    pub aio_nr: u64,
    pub aio_max_nr: u64,
    /// inotify 的所有监视。打开文件析构时要报告 `IN_CLOSE_*`，所以用 Rc 共享。
    pub inotify: Rc<RefCell<crate::syscall::Registry>>,
}

// SAFETY: Kernel 及其中的 Rc/RefCell 只在持有 BKL（Shared::kernel 这把锁）时访问，
// 见模块文档
unsafe impl Send for Kernel {}

impl Kernel {
    pub fn new(tier: Tier) -> Kernel {
        // 在任何 guest 线程改变宿主亲和性之前记下宿主的 CPU
        appine_hal::host_cpus();
        Kernel {
            tasks: BTreeMap::new(),
            groups: BTreeMap::new(),
            futexes: Futexes::default(),
            tier,
            watched: BTreeMap::new(),
            vfs: None,
            shutting_down: false,
            dbt_stats: None,
            cpu: CpuTime::default(),
            forks: 0,
            boot_id: {
                let mut id = [0u8; 16];
                let _ = appine_hal::fill_random(&mut id);
                id[6] = (id[6] & 0x0f) | 0x40;
                id[8] = (id[8] & 0x3f) | 0x80;
                id
            },
            ptys: BTreeMap::new(),
            consoles: BTreeMap::new(),
            flocks: BTreeMap::new(),
            locks: crate::locks::Locks::default(),
            ipc: crate::ipc::Ipc::default(),
            unix: Default::default(),
            unix_inflight: Vec::new(),
            fifos: BTreeMap::new(),
            seals: BTreeMap::new(),
            hostname: b"localhost".to_vec(),
            domainname: b"(none)".to_vec(),
            pipe_max_size: crate::pipe::PIPE_MAX_SIZE,
            nr_open: crate::fd::MAX_FDS,
            threads_max: crate::process::default_threads_max(),
            oom: Default::default(),
            next_tid: INIT + 1,
            waiters: Vec::new(),
            edge_waiters: Vec::new(),
            fasync: Vec::new(),
            leases: BTreeMap::new(),
            lease_break_time: 45,
            mqueues: BTreeMap::new(),
            mq_limits: Default::default(),
            aio_nr: 0,
            aio_max_nr: 65536,
            inotify: Rc::default(),
        }
    }

    /// 分配一个没有被线程或线程组（含僵尸进程）占用的 tid。
    pub fn alloc_tid(&mut self) -> Result<Tid, Errno> {
        for _ in 0..PID_MAX {
            let tid = self.next_tid;
            self.next_tid = if tid >= PID_MAX { INIT + 1 } else { tid + 1 };
            if !self.tasks.contains_key(&tid) && !self.groups.contains_key(&tid) {
                return Ok(tid);
            }
        }
        Err(Errno::EAGAIN)
    }

    /// 最后分配的 tid（`/proc/sys/kernel/ns_last_pid`）。
    pub fn last_pid(&self) -> Tid {
        self.next_tid - 1
    }

    /// 写 `ns_last_pid`：之后从 `pid + 1` 开始分配（被占用的跳过）。
    pub fn set_last_pid(&mut self, pid: Tid) {
        self.next_tid = if pid >= PID_MAX {
            INIT + 1
        } else {
            (pid + 1).max(INIT + 1)
        };
    }

    /// 所有 fd 表里的打开文件。同一个打开文件在几个 fd 表里时出现几次。
    pub(crate) fn open_files(&self) -> Vec<FileRef> {
        let mut tables: Vec<Rc<RefCell<FdTable>>> = Vec::new();
        let mut files = Vec::new();
        for t in self.tasks.values() {
            let fds = t.res.borrow().fds.clone();
            if tables.iter().any(|p| Rc::ptr_eq(p, &fds)) {
                continue;
            }
            {
                let table = fds.borrow();
                files.extend(
                    table
                        .open_fds()
                        .into_iter()
                        .filter_map(|fd| table.get(fd).ok()),
                );
            }
            tables.push(fds);
        }
        files
    }

    fn flush_wakeups(&self) {
        if WAKE.swap(false, Ordering::AcqRel) {
            for event in &self.waiters {
                event.signal();
            }
        }
    }
}

/// 所有 guest 线程共享的内核对象。
pub(crate) struct Shared {
    kernel: Mutex<Kernel>,
    /// 叫醒定时器线程。
    pub timers: Arc<Event>,
    /// 内存上限的检查停下来了（所有线程都在等待，[`crate::oom`]）。
    pub oom_idle: AtomicBool,
}

impl Shared {
    pub fn new(kernel: Kernel) -> Shared {
        Shared {
            kernel: Mutex::new(kernel),
            timers: Arc::new(Event::new().expect("cannot create an eventfd for the timers")),
            oom_idle: AtomicBool::new(false),
        }
    }

    /// 线程要接着执行 guest 代码了：内存上限的检查停着时叫醒定时器线程。不需要持有 BKL。
    pub fn resume_oom_checks(&self) {
        if self.oom_idle.load(Ordering::Relaxed) && self.oom_idle.swap(false, Ordering::Relaxed) {
            self.timers.signal();
        }
    }

    /// 获取 BKL。内核层 panic 时锁会中毒：出问题的进程已经被结束（[`crate::bug`]），
    /// 其余的照常使用它。
    pub fn lock(&self) -> Bkl<'_> {
        Bkl(self.kernel.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// 持有 BKL 的凭证；释放时唤醒等待中的线程。
pub(crate) struct Bkl<'a>(MutexGuard<'a, Kernel>);

impl Deref for Bkl<'_> {
    type Target = Kernel;
    fn deref(&self) -> &Kernel {
        &self.0
    }
}

impl DerefMut for Bkl<'_> {
    fn deref_mut(&mut self) -> &mut Kernel {
        &mut self.0
    }
}

impl Drop for Bkl<'_> {
    fn drop(&mut self) {
        self.0.flush_wakeups();
    }
}

/// 释放 BKL 等待：事件被触发（[`request_wake`]、信号）、`host` 里的宿主 fd 就绪或者
/// 到达 `deadline` 时重新获取 BKL 返回。调用方重新检查自己的条件。`listed` 为 false
/// 时不登记在 [`request_wake`] 要叫醒的名单里，只有直接通知 `event` 才会醒（futex）。
pub(crate) fn block<'a>(
    shared: &'a Shared,
    mut k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
    listed: bool,
) -> (Bkl<'a>, Result<(), Errno>) {
    const POLLIN: i16 = 1;
    if listed {
        k.waiters.push(event.clone());
    }
    drop(k);
    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let mut fds: Vec<(i32, i16, i16)> = host.iter().map(|&(fd, ev, _)| (fd, ev, 0)).collect();
    fds.push((event.fd(), POLLIN, 0));
    let r = appine_hal::poll(&mut fds, timeout);
    let mut k = shared.lock();
    event.clear();
    if listed {
        k.waiters.retain(|e| !Arc::ptr_eq(e, event));
    }
    for (h, f) in host.iter_mut().zip(&fds) {
        h.2 = f.2;
    }
    let r = match r {
        Ok(_) => Ok(()),
        // 宿主信号打断 poll（比如调试器）：当作一次普通的唤醒
        Err(e) if e == Errno::EINTR.0 => Ok(()),
        Err(e) => Err(Errno(e)),
    };
    (k, r)
}

/// 一次进入内核（syscall、信号投递）的上下文：持有 BKL 的凭证和当前线程。
pub(crate) struct Cx<'a> {
    pub shared: &'a Arc<Shared>,
    k: Option<Bkl<'a>>,
    /// 当前线程里其他线程也会访问的部分。
    pub task: &'a Rc<Task>,
    /// 当前线程私有的状态。
    pub t: &'a mut TaskState,
}

impl<'a> Cx<'a> {
    pub fn new(
        shared: &'a Arc<Shared>,
        k: Bkl<'a>,
        task: &'a Rc<Task>,
        t: &'a mut TaskState,
    ) -> Cx<'a> {
        Cx {
            shared,
            k: Some(k),
            task,
            t,
        }
    }

    /// 全局状态。
    pub fn k(&mut self) -> &mut Kernel {
        self.k.as_mut().expect("BKL is held")
    }

    /// 同时借用全局状态和当前线程的私有状态。
    pub fn parts(&mut self) -> (&mut Kernel, &mut TaskState) {
        (self.k.as_mut().expect("BKL is held"), self.t)
    }

    /// 只读的全局状态。
    pub fn kr(&self) -> &Kernel {
        self.k.as_ref().expect("BKL is held")
    }

    /// 当前线程的 tid。
    pub fn tid(&self) -> Tid {
        self.task.tid.get()
    }

    /// 当前进程的 pid（线程组 ID）。
    pub fn tgid(&self) -> Tid {
        self.task.tgid
    }

    /// 地址空间。
    pub fn mm(&self) -> Ref<'_, Mm> {
        self.t.mm.borrow()
    }

    /// 可修改的地址空间。
    pub fn mm_mut(&self) -> RefMut<'_, Mm> {
        self.t.mm.borrow_mut()
    }

    /// 让 `addr` 上面紧挨着的向下增长的映射（主线程栈、`MAP_GROWSDOWN`）长到它所在的页，
    /// 大小不超过 `RLIMIT_STACK` 的当前值（[`Mm::grow_down`]）。
    pub fn grow_stack(&self, addr: u64) -> bool {
        let limit = self.t.group.borrow().rlimits[crate::process::RLIMIT_STACK as usize].0;
        self.mm_mut().grow_down(addr, limit)
    }

    /// syscall 入口：SP 已经低于主线程栈的底部（程序分配了大的栈帧但还没碰）时，先让栈
    /// 长到 SP。syscall 用到的栈上缓冲区都在 SP 以上（arm64 没有 red zone），这样内核
    /// 访问它们时与 Linux 一样不会 EFAULT。SP 在栈里（常见情况）只比较一次。
    pub fn grow_stack_to(&self, sp: u64) {
        if sp >= self.mm().stack_low() {
            return;
        }
        let limit = self.t.group.borrow().rlimits[crate::process::RLIMIT_STACK as usize].0;
        // 别的线程的栈在 mmap 区里，离主线程栈远得多：不用查
        if crate::loader::STACK_TOP - sp <= limit {
            self.mm_mut().grow_down(sp, limit);
        }
    }

    /// fd 表。
    pub fn fds(&self) -> Ref<'_, FdTable> {
        self.t.fds.borrow()
    }

    /// 可修改的 fd 表。
    pub fn fds_mut(&self) -> RefMut<'_, FdTable> {
        self.t.fds.borrow_mut()
    }

    /// 根文件系统；没有 rootfs 时 guest 看不到任何文件。
    pub fn vfs(&self) -> Result<Rc<Vfs>, Errno> {
        self.t.vfs.clone().ok_or(Errno::ENOENT)
    }

    /// 当前目录。
    pub fn cwd(&self) -> Node {
        self.t.fs.borrow().cwd.clone()
    }

    /// 根目录（chroot）。
    pub fn root(&self) -> Node {
        self.t.fs.borrow().root.clone()
    }

    /// 换掉地址空间（execve），同时更新 procfs 能看到的那份。
    pub fn set_mm(&mut self, mm: Rc<RefCell<Mm>>) {
        self.task.res.borrow_mut().mm = mm.clone();
        self.t.mm = mm;
    }

    /// 换掉 fd 表（execve、close_range 不再共享时），同时更新 procfs 能看到的那份。
    pub fn set_fds(&mut self, fds: Rc<RefCell<FdTable>>) {
        self.task.res.borrow_mut().fds = fds.clone();
        self.t.fds = fds;
    }

    /// 换掉根目录、当前目录和 umask（unshare），同时更新 procfs 能看到的那份。
    pub fn set_fs(&mut self, fs: Rc<RefCell<crate::task::FsContext>>) {
        self.task.res.borrow_mut().fs = fs.clone();
        self.t.fs = fs;
    }

    /// procfs、devpts 的视图（路径解析用）。
    pub fn view<'v>(&'v self, vfs: &'v Vfs) -> crate::pseudo::View<'v> {
        crate::pseudo::View {
            k: self.kr(),
            vfs,
            tid: self.tid(),
            tgid: self.tgid(),
        }
    }

    /// 当前进程的凭据。
    pub fn cred(&self) -> crate::cred::Cred {
        self.t.group.borrow().cred.clone()
    }

    /// AF_UNIX 的 `SO_PEERCRED`、`SCM_CREDENTIALS`：pid 和有效用户 ID、组 ID。
    pub fn ucred(&self) -> crate::unix::Ucred {
        let g = self.t.group.borrow();
        crate::unix::Ucred::of(self.tgid(), g.cred.euid, g.cred.egid)
    }

    /// 文件创建掩码。
    pub fn umask(&self) -> u32 {
        self.t.fs.borrow().umask
    }

    /// 有没有可以投递的信号，或者线程已经被要求退出。
    pub fn interrupted(&self) -> bool {
        crate::signal::interrupted(self)
    }

    /// 释放 BKL 等待，见 [`block`]。等待期间不能借用任何共享的内核对象。
    pub fn wait(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
    ) -> Result<(), Errno> {
        self.wait_on(deadline, host, true)
    }

    /// 只等直接的通知（futex 唤醒、信号、被结束）或者超时，[`request_wake`] 不会叫醒它。
    /// 调用方必须保证让它的条件成立的每个地方都直接通知了 `self.task.event`。
    pub fn wait_alone(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        self.wait_on(deadline, &mut [], false)
    }

    fn wait_on(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        listed: bool,
    ) -> Result<(), Errno> {
        let k = self.k.take().expect("BKL is held");
        self.task.sleeping.set(true);
        let (k, r) = block(self.shared, k, &self.task.event, deadline, host, listed);
        self.task.sleeping.set(false);
        self.k = Some(k);
        r
    }

    /// 释放 BKL 执行 `f`（可能阻塞的宿主 I/O）。`f` 不能访问任何内核对象。
    pub fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        self.task.sleeping.set(true);
        drop(self.k.take());
        let r = f();
        self.k = Some(self.shared.lock());
        self.task.sleeping.set(false);
        r
    }
}
