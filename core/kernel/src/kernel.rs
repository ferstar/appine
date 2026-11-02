//! 内核层的全局状态、大内核锁和统一的等待机制（设计文档 §6）。
//!
//! 所有 guest 线程共享一个 [`Kernel`]，由一把大内核锁（BKL）保护：处理 syscall
//! 和投递信号时持有，执行 guest 代码时不持有，等待之前释放。内核对象之间用
//! `Rc`/`RefCell` 共享，它们只在持有 BKL 时被访问；唯一的例外是线程自己的
//! [`TaskState`]，只有它所属的线程会访问，等待期间也一直借用着。`Rc` 的计数、`RefCell`
//! 的借用标志都不是原子的，放开 BKL 时连 clone、drop 也不行：系统调用中途放开 BKL 的
//! 只有等待（[`Cx::wait_on`] 等）和 [`Cx::unlocked`]，后者的闭包要是 `Send`，碰不到
//! 内核对象；这两处 panic 时也先重新拿到 BKL，展开时外层各帧丢掉的 `Rc` 才不会和别的
//! 线程同时改计数。放开 BKL 时也不能还借着内核对象，调试构建里由 [`crate::cell`] 检查。
//!
//! 等待统一走宿主 `poll()`：每个线程有一个 eventfd，等待时把它和要等的宿主 fd
//! 放在一起 poll。任何可能让别人的等待条件成立的修改都调用 [`request_wake`]，
//! 释放 BKL 时唤醒正在等待的线程，由它们各自重新检查条件。修改按通道（[`wake`]）区分，
//! 只叫醒关心这个通道的等待者：关闭文件、读写管道这类修改很频繁，不必每次都叫醒在 wait4
//! 里等子进程的 shell 和定时器线程。futex 是例外：停在锁和条件变量上的线程很多，它们用
//! [`Cx::wait_alone`] 等待，只被 FUTEX_WAKE、信号和结束线程组直接通知。

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use appine_engine::DbtStats;
use appine_hal::Event;

use crate::cell::{Ref, RefCell, RefMut};
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

/// 唤醒的通道：等待者登记自己关心哪些（[`Cx::wait_on`]），修改只叫醒关心它的等待者。
pub(crate) mod wake {
    /// 打开文件（管道、socket、PTY、消息队列……）上的修改。这些修改都按对象请求
    /// （[`super::request_wake_object`]）：只听这个通道的等待者被它登记的对象上的修改
    /// （没有登记对象的，任何对象上的修改）和请求所有通道的修改叫醒。
    pub const FILE: u32 = 1;
    /// 其余一切：定时器的设置、信号等。
    pub const OTHER: u32 = 2;
    /// 子进程的状态变化（结束、停止、继续）：wait4、waitid 只听它。
    pub const CHILD: u32 = 4;
    /// 宿主在等的进程（[`crate::Machine::spawn`] 启动的）结束了：`Machine::wait` 只听它。
    pub const WATCHED: u32 = 8;
    /// 有线程结束了（execve 在等别的线程结束，关闭时在等所有线程结束）。
    pub const EXITED: u32 = 16;
    /// 文件锁（flock、fcntl 的记录锁和 OFD 锁）、租约和打开文件的位置锁（f_pos_lock）
    /// 放开了或者变了：只有等它们的线程听。原来走文件类的通道，每次解锁都把所有停在
    /// poll、epoll 里的线程叫醒一遍。
    pub const LOCK: u32 = 32;
    /// 被屏蔽的信号挂起了（发给进程的被所有线程屏蔽，发给线程的被它屏蔽）：只有等它们的
    /// 线程听（sigtimedwait、signalfd 的读，poll、epoll 关注了 signalfd 的）。原来叫醒
    /// 所有等待者：musl 的 fork 在 clone 前后屏蔽所有信号，这时有子进程退出，SIGCHLD 就
    /// 是被所有线程屏蔽的，shell 起管道时常常碰到。
    pub const SIGNAL: u32 = 64;
    pub const ALL: u32 = FILE | OTHER | CHILD | WATCHED | EXITED | LOCK | SIGNAL;
    /// 有只影响某些对象的修改（[`super::request_wake_object`]），不是等待者关心的通道。
    pub(super) const OBJECTS: u32 = 1 << 31;

    /// 等待者等的对象（管道、eventfd、PTY、AF_UNIX 的 channel……）：对象的地址。
    pub type Object = usize;

    /// `RefCell` 里的对象 `c` 的唤醒对象：里面的值的地址，与对象自己的方法里的
    /// [`of_value`] 相同。
    pub fn of<T>(c: &crate::cell::RefCell<T>) -> Object {
        c.as_ptr() as Object
    }

    /// 在对象自己的方法里（只有 `&self`）：与 [`of`] 相同的地址。
    pub fn of_value<T>(v: &T) -> Object {
        v as *const T as Object
    }
}

/// 释放 BKL 时要叫醒的通道。
static WAKE: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// 持有 BKL 时要触发的事件（[`signal_later`]）。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static SIGNALS: std::cell::RefCell<Vec<Arc<Event>>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 这个线程持有 BKL。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 当前线程持有 BKL。
pub(crate) fn bkl_held() -> bool {
    HELD.get()
}

/// 放开 BKL 之后再触发 `event`。被叫醒的线程马上就要拿 BKL：放开之前叫醒它，它只会
/// 再睡下去等锁，等这边放开时又要叫醒一次（shell 等子进程时每次都这样）。调用方持有
/// BKL。
pub(crate) fn signal_later(event: &Arc<Event>) {
    SIGNALS.with_borrow_mut(|s| {
        if !s.iter().any(|e| Arc::ptr_eq(e, event)) {
            s.push(event.clone());
        }
    });
}

/// 释放 BKL 时要叫醒的对象（[`request_wake_object`]）。
static WAKE_OBJECTS: Mutex<Vec<wake::Object>> = Mutex::new(Vec::new());

/// 请求在释放 BKL 时唤醒所有等待中的线程。调用方持有 BKL。
pub(crate) fn request_wake() {
    WAKE.fetch_or(wake::ALL, Ordering::Relaxed);
}

/// 文件锁、租约或者打开文件的位置锁放开了、变了：只叫醒关心 [`wake::LOCK`] 的等待者。
/// 调用方持有 BKL。
pub(crate) fn request_wake_locks() {
    request_wake_on(wake::LOCK);
}

/// 请求在释放 BKL 时叫醒关心 `channels`（[`wake`]）里任何一个通道的等待者。调用方持有
/// BKL。
pub(crate) fn request_wake_on(channels: u32) {
    WAKE.fetch_or(channels, Ordering::Relaxed);
}

/// 只影响对象 `object` 的修改（管道里的数据、eventfd 的计数、PTY 的输入输出、AF_UNIX
/// channel 里的消息……）：只叫醒登记了这个对象的等待者（[`Cx::wait_object`]、
/// [`Cx::wait_objects`]），以及没有指明对象的 [`wake::FILE`] 等待者。原来 shell 的每次
/// 管道读写都把读着别的管道的进程（`$(...)` 外面的 shell、`tail`）叫醒一遍，poll、epoll
/// 的等待者也被每一次这样的修改叫醒。调用方持有 BKL。
pub(crate) fn request_wake_object(object: wake::Object) {
    let mut objects = WAKE_OBJECTS.lock().unwrap_or_else(PoisonError::into_inner);
    if !objects.contains(&object) {
        objects.push(object);
    }
    WAKE.fetch_or(wake::OBJECTS, Ordering::Relaxed);
}

/// 一个正在等待的线程：它的事件、它关心的通道（[`wake`]）和它等的对象（排好序；None
/// 时任何对象上的修改都叫醒它）。一个 epoll 可能关注几千个文件，每次有对象被修改都要
/// 查一遍，所以二分查找。
type Waiter = (Arc<Event>, u32, Option<Box<[wake::Object]>>);
/// 在等命名空间里的进程被回收的 pid 命名空间 1 号进程：命名空间、进程、可能变成孤儿的进程组。
type HeldReaper = (Rc<crate::pidns::PidNs>, Rc<RefCell<ThreadGroup>>, Vec<Tid>);

/// 持有 flock(2) 锁的打开文件（弱引用，关闭后锁随之释放），以及是不是排他锁。
pub(crate) type FlockHolder = (std::rc::Weak<RefCell<crate::fd::OpenFile>>, bool);

/// 内核层的全局状态。
pub(crate) struct Kernel {
    /// 所有活着的线程。
    pub tasks: BTreeMap<Tid, Rc<Task>>,
    /// 所有线程组，包括僵尸进程和 init。
    pub groups: BTreeMap<Tid, Rc<RefCell<ThreadGroup>>>,
    /// 在新的 pid 命名空间里的线程、进程所在的那个命名空间（[`crate::pidns`]），编号不再
    /// 使用时从各层的表里去掉。
    pid_nss: std::collections::HashMap<Tid, Rc<crate::pidns::PidNs>>,
    /// 退出了、在等命名空间里别的进程被回收的 pid 命名空间 1 号进程。
    held_reapers: Vec<HeldReaper>,
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
    /// 所有 PTY，按所在的 devpts 实例（设备号）和实例里的编号；主设备和从设备都关闭之后
    /// 条目失效。
    pub ptys: BTreeMap<(u64, u32), Weak<RefCell<Tty>>>,
    /// 宿主持有主设备的 PTY（[`crate::terminal`]），按 PTY 编号。
    pub consoles: BTreeMap<u32, crate::terminal::Console>,
    /// flock(2) 的锁：每个 inode 上持有锁的打开文件，以及是不是排他锁。打开文件关闭后
    /// 弱引用失效，锁随之释放。
    pub flocks: BTreeMap<Ino, Vec<FlockHolder>>,
    /// fcntl(2) 的记录锁。
    pub locks: crate::locks::Locks,
    /// 开机时的 UTS、IPC 命名空间（[`crate::nsproxy`]）。
    pub init_ns: crate::nsproxy::NsProxy,
    /// 各个 IPC 命名空间里的 System V IPC 对象和 POSIX 消息队列，按命名空间的编号；没有
    /// 线程用了的命名空间丢掉（[`Kernel::collect_ipc`]）。
    pub ipcs: BTreeMap<u64, (Weak<crate::nsproxy::IpcNs>, crate::ipc::Ipc)>,
    /// cgroup 树（[`crate::cgroup`]）。
    pub cgroups: crate::cgroup::Cgroups,
    /// 加载了的 eBPF 程序的编号（[`crate::bpf`]）。
    pub bpf: crate::bpf::Registry,
    /// 新建的网络命名空间（[`crate::nsproxy::NetNs`]）：分配宿主上的回环地址时避开还在用的。
    net_nss: Vec<Weak<crate::nsproxy::NetNs>>,
    /// AF_UNIX socket 绑定的地址。
    pub unix: crate::unix::Names,
    /// 消息里带着打开文件的 AF_UNIX channel（`unix::collect` 用）。
    pub unix_inflight: Vec<Weak<RefCell<crate::unix::Channel>>>,
    /// 正在被打开的命名管道（FIFO）：同一个 inode 的所有打开共用一个管道对象。
    pub fifos: BTreeMap<Ino, Weak<RefCell<crate::pipe::Pipe>>>,
    /// memfd 的封印（F_ADD_SEALS），以 inode 为键；不在这里的文件不能加封印。
    pub seals: BTreeMap<Ino, u32>,
    /// `/proc/sys/kernel/core_pattern`、`core_uses_pid`（`crate::coredump`）。
    pub core_pattern: Vec<u8>,
    pub core_uses_pid: bool,
    /// 内核日志（`syscall::misc::syslog`）：`/proc/sys/kernel/printk` 的四个级别（控制台、
    /// 默认消息、控制台最低、控制台默认），`SYSLOG_ACTION_CONSOLE_OFF` 存下的控制台级别，
    /// `/proc/sys/kernel/dmesg_restrict`。
    pub printk: [i32; 4],
    pub saved_console_loglevel: Option<i32>,
    pub dmesg_restrict: bool,
    /// `/proc/sys/kernel/panic`：只记下，appine 的内核层没有 panic（内核 bug 只结束撞上它的
    /// 进程）。
    pub panic: i32,
    /// `/proc/sys/fs/protected_symlinks`、`protected_hardlinks`（Linux 的 `may_follow_link`、
    /// `may_linkat`），默认与 systemd、Android 设的一样打开。
    pub protected_symlinks: bool,
    pub protected_hardlinks: bool,
    /// `/proc/sys/kernel/unprivileged_bpf_disabled`：0 时没有能力的进程也能用 bpf(2) 的一部分
    /// 命令，1 时改不回去，2（默认，`BPF_UNPRIV_DEFAULT_OFF`）时 root 可以改成 0。
    pub unprivileged_bpf_disabled: u8,
    /// 宿主按 inittab 启动了（[`crate::Machine::boot`]）：内置的 init 这时与 busybox 的一样
    /// 处理 SIGTERM、SIGUSR1、SIGUSR2（[`crate::Power`]），收到的记在 `power` 里，由宿主
    /// 取走（[`crate::Machine::take_power`]）去关机。
    pub init_handlers: bool,
    pub power: Option<crate::Power>,
    /// 宿主开始关机、结束所有进程了（[`crate::Machine::shutdown`]、`kill_all`）：还在开机的
    /// [`crate::Machine::boot`] 不再运行后面的命令。
    pub stopping: bool,
    /// 内核日志的记录（[`crate::klog`]）：`/dev/kmsg` 的打开文件也拿着它。
    pub klog: Rc<RefCell<crate::klog::Klog>>,
    /// `/proc/sys/fs/pipe-max-size`：没有 `CAP_SYS_RESOURCE` 时管道的最大容量。
    pub pipe_max_size: usize,
    /// `/proc/sys/fs/nr_open`：fd 的上限（`RLIMIT_NOFILE` 不能超过它）。
    pub nr_open: usize,
    /// `/proc/sys/kernel/threads-max`：同时存在的线程数的上限，root 也受它限制。
    pub threads_max: usize,
    /// App 内存的上限和 OOM killer（[`crate::oom`]）。
    pub oom: crate::oom::Oom,
    /// 内核层结束了的 guest 进程（内核 bug、引擎的宿主故障、OOM killer），等 App 取走
    /// （[`crate::Machine::take_kills`]）。
    pub kills: RefCell<Vec<crate::bug::KillReport>>,
    /// 同一个程序的地址空间共用 Tier 1 翻译的组（[`crate::mm::CodeGroup`]）。
    pub code_groups: crate::mm::CodeGroups,
    /// 平均负载（[`crate::loadavg`]）。
    pub load: crate::loadavg::LoadAvg,
    /// 被跟踪的线程退出了、还没被跟踪者的 wait 取走的（[`crate::ptrace`]）。
    pub trace_exits: Vec<crate::ptrace::TraceExit>,
    next_tid: Tid,
    /// 正在等待的线程。
    waiters: Vec<Waiter>,
    /// 等待中、因为边沿触发少等了一些事件的 epoll_wait（`syscall::poll`）。
    pub edge_waiters: Vec<Arc<Event>>,
    /// `EPOLLEXCLUSIVE` 关注的文件上等着的线程（`syscall::poll` 的独占门），由定时器线程
    /// 检查。
    pub ex_gates: Vec<crate::syscall::ExGate>,
    /// 设置了 `O_ASYNC` 的打开文件（`syscall::poll` 的 fasync）。
    pub fasync: Vec<crate::syscall::Fasync>,
    /// 各个网络命名空间的 ICMP 汇集点（[`crate::rawicmp`]），由定时器线程看着。
    pub icmp_taps: Vec<std::rc::Weak<crate::cell::RefCell<crate::rawicmp::Tap>>>,
    /// 文件租约，按 inode（`syscall::lease`）。
    pub leases: BTreeMap<Ino, Vec<crate::syscall::Lease>>,
    /// `/proc/sys/fs/lease-break-time`：打破租约时最多等多少秒。
    pub lease_break_time: u64,
    /// `/proc/sys/kernel/seccomp/actions_logged`：记哪些 seccomp 动作（`seccomp::logged`）。
    pub seccomp_logged: u32,
    /// 网络接口的流量计数（[`crate::traffic`]）。
    pub traffic: crate::traffic::Traffic,
    /// 1024 以下的端口映射和 guest 在非回环地址上的监听（[`crate::portmap`]）。socket 的
    /// 租约也引用它，所以用 Rc 共享。
    pub portmap: std::rc::Rc<crate::cell::RefCell<crate::portmap::PortMap>>,
    /// 从宿主外面连到发布的端口的转发（[`crate::forward`]）。
    pub forward: crate::forward::Forwarder,
    /// 过滤链丢弃的 TCP 连接连去的宿主地址（`syscall::net::blackhole`），第一次用到时建。
    pub blackhole: std::cell::OnceCell<Option<Vec<u8>>>,
    /// 被 nat 换过目的地址、还没被接受的 TCP 连接原来的目的地址（[`crate::origdst`]，
    /// `SO_ORIGINAL_DST`）。
    pub orig_dst: crate::origdst::Table,
    /// 宿主交来的主表路由（[`crate::Machine::set_routes`]，Android 上取自 `LinkProperties`），
    /// None 时读宿主的（[`crate::route`]）。
    pub host_routes: Option<Vec<appine_hal::net::Route>>,
    /// AIO 上下文占用的事件数（`/proc/sys/fs/aio-nr`）和上限（`aio-max-nr`）。
    pub aio_nr: u64,
    pub aio_max_nr: u64,
    /// 写过的 `/proc/sys/user/max_*_namespaces`（按名字；只记下，不实际限制）。
    pub max_namespaces: BTreeMap<Vec<u8>, u64>,
    /// inotify 的所有监视。打开文件析构时要报告 `IN_CLOSE_*`，所以用 Rc 共享。
    pub inotify: Rc<RefCell<crate::syscall::Registry>>,
    /// 所有地址空间共用的只读文件映射（[`crate::mm::FileMaps`]）。
    pub file_maps: crate::mm::FileMaps,
}

// SAFETY: Kernel 及其中的 Rc/RefCell 只在持有 BKL（Shared::kernel 这把锁）时访问，
// 见模块文档
unsafe impl Send for Kernel {}

impl Kernel {
    pub fn new(tier: Tier) -> Kernel {
        // 在任何 guest 线程改变宿主亲和性之前记下宿主的 CPU
        appine_hal::host_cpus();
        let init_ns = crate::nsproxy::NsProxy::init();
        Kernel {
            tasks: BTreeMap::new(),
            groups: BTreeMap::new(),
            pid_nss: Default::default(),
            held_reapers: Vec::new(),
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
            init_ns: init_ns.clone(),
            net_nss: Vec::new(),
            cgroups: Default::default(),
            bpf: Default::default(),
            ipcs: BTreeMap::from([(
                init_ns.ipc.id,
                (Rc::downgrade(&init_ns.ipc), crate::ipc::Ipc::default()),
            )]),
            unix: Default::default(),
            unix_inflight: Vec::new(),
            fifos: BTreeMap::new(),
            seals: BTreeMap::new(),
            core_pattern: b"core".to_vec(),
            core_uses_pid: false,
            // CONSOLE_LOGLEVEL_DEFAULT、MESSAGE_LOGLEVEL_DEFAULT、CONSOLE_LOGLEVEL_MIN 的默认
            // 配置；没有 CONFIG_SECURITY_DMESG_RESTRICT
            printk: [7, 4, 1, 7],
            saved_console_loglevel: None,
            dmesg_restrict: false,
            panic: 0,
            protected_symlinks: true,
            protected_hardlinks: true,
            unprivileged_bpf_disabled: 2,
            init_handlers: false,
            power: None,
            stopping: false,
            klog: Default::default(),
            kills: Default::default(),
            pipe_max_size: crate::pipe::PIPE_MAX_SIZE,
            nr_open: crate::fd::MAX_FDS,
            threads_max: crate::process::default_threads_max(),
            oom: Default::default(),
            code_groups: Default::default(),
            load: crate::loadavg::LoadAvg::new(Instant::now()),
            trace_exits: Vec::new(),
            next_tid: INIT + 1,
            waiters: Vec::new(),
            edge_waiters: Vec::new(),
            ex_gates: Vec::new(),
            fasync: Vec::new(),
            icmp_taps: Vec::new(),
            leases: BTreeMap::new(),
            lease_break_time: 45,
            seccomp_logged: crate::seccomp::logged::DEFAULT,
            traffic: Default::default(),
            portmap: Default::default(),
            forward: Default::default(),
            blackhole: Default::default(),
            orig_dst: Default::default(),
            host_routes: None,
            aio_nr: 0,
            aio_max_nr: 65536,
            max_namespaces: BTreeMap::new(),
            inotify: Rc::default(),
            file_maps: Default::default(),
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

    /// 给 pid 命名空间 `ns` 里的新线程、进程选好全局编号（Linux 的 `alloc_pid` 的检查部分）。
    /// `set_tid` 是 clone3 指定的各层编号，从 `ns` 那层起往外：层数不能比 `ns` 的多，
    /// 编号在 1 到 pid_max 之间、命名空间里还没有进程时只能是 1（EINVAL），`cred` 要在那层的
    /// 属主里有 `CAP_SYS_ADMIN` 或者 `CAP_CHECKPOINT_RESTORE`（EPERM），编号已经有人用了是
    /// EEXIST。指定了全局编号就用它，否则另外分配。
    pub fn choose_pid(
        &mut self,
        ns: &Rc<crate::pidns::PidNs>,
        cred: &crate::cred::Cred,
        set_tid: &[Tid],
    ) -> Result<Tid, Errno> {
        use crate::cred::cap;
        let levels = ns.levels_from(0);
        if set_tid.len() > levels.len() {
            return Err(Errno::EINVAL);
        }
        for (n, &tid) in levels.iter().rev().zip(set_tid) {
            if !(1..PID_MAX).contains(&tid) || tid != 1 && n.level > 0 && n.reaper.get().is_none() {
                return Err(Errno::EINVAL);
            }
            if !cred.ns_capable(&n.owner, cap::SYS_ADMIN)
                && !cred.ns_capable(&n.owner, cap::CHECKPOINT_RESTORE)
            {
                return Err(Errno::EPERM);
            }
            let taken = if n.level == 0 {
                self.tasks.contains_key(&tid) || self.groups.contains_key(&tid)
            } else {
                n.find(tid).is_some()
            };
            if taken {
                return Err(Errno::EEXIST);
            }
        }
        match set_tid.get(ns.level as usize) {
            Some(&tid) => Ok(tid),
            None => self.alloc_tid(),
        }
    }

    /// 新建一个空的 IPC 命名空间，属于用户命名空间 `owner`（Linux 的 `create_ipc_ns`：
    /// 限制都是默认值）。`creator` 是建立它的进程的 fsuid、fsgid：mqueue 文件系统的根目录
    /// 属于它（`mq_init_ns` 里 `mqueue_get_inode` 取 `current_fsuid()`）。
    pub fn new_ipc_ns(
        &mut self,
        owner: Rc<crate::cred::UserNs>,
        creator: (u32, u32),
    ) -> Rc<crate::nsproxy::IpcNs> {
        let ns = Rc::new(crate::nsproxy::IpcNs {
            id: crate::vfs::new_ns_id(),
            owner,
        });
        let ipc = crate::ipc::Ipc {
            mq_dir: crate::cell::RefCell::new(crate::syscall::MqDir::new(creator.0, creator.1)),
            ..Default::default()
        };
        let objects = (Rc::downgrade(&ns), ipc);
        self.ipcs.insert(ns.id, objects);
        ns
    }

    /// 新建一个网络命名空间，属于用户命名空间 `owner`：里面只有关着的 lo，宿主上的回环
    /// 地址 127.a.b.0/24 取一个还没有命名空间用的（a、b 不全为 0）；IPv4 的 sysctl 照抄
    /// 开机时的命名空间现在的（[`crate::netconf`]）。
    pub fn new_net_ns(
        &mut self,
        owner: Rc<crate::cred::UserNs>,
    ) -> Result<Rc<crate::nsproxy::NetNs>, Errno> {
        self.net_nss.retain(|w| w.strong_count() > 0);
        let used: Vec<[u8; 2]> = self
            .net_nss
            .iter()
            .filter_map(|w| w.upgrade().and_then(|n| n.host_net))
            .collect();
        // 127.255.0.0/16 留给开机时的命名空间里的虚拟地址（crate::netns::INIT_BLOCK），
        // 127.254.0.0/16 留给从外面转发进来的连接（crate::forward::OUTSIDE_BLOCK）
        let net = (1..=u16::MAX)
            .map(u16::to_be_bytes)
            .find(|n| {
                n[0] != crate::netns::INIT_BLOCK
                    && n[0] != crate::forward::OUTSIDE_BLOCK
                    && !used.contains(n)
            })
            .ok_or(Errno::ENOMEM)?;
        let devs = crate::netdev::Devices::for_new_ns(&self.init_ns.net.devs.borrow().conf);
        let ns = Rc::new(crate::nsproxy::NetNs {
            id: crate::vfs::new_ns_id(),
            owner,
            host_net: Some(net),
            lo: Default::default(),
            devs: crate::cell::RefCell::new(devs),
            nft: Default::default(),
            listeners: Default::default(),
        });
        self.net_nss.push(Rc::downgrade(&ns));
        Ok(ns)
    }

    /// 所有活着的网络命名空间，开机时的在前（地址换算用，[`crate::netns::World`]）。
    pub fn world(&self) -> crate::netns::World {
        let mut all = vec![self.init_ns.net.clone()];
        all.extend(self.net_nss.iter().filter_map(Weak::upgrade));
        crate::netns::World { all }
    }

    /// IPC 命名空间 `id` 里的对象。
    pub fn ipc(&self, id: u64) -> &crate::ipc::Ipc {
        &self.ipcs[&id].1
    }

    pub fn ipc_mut(&mut self, id: u64) -> &mut crate::ipc::Ipc {
        &mut self.ipcs.get_mut(&id).expect("IPC namespace in use").1
    }

    /// 没有线程用了的 IPC 命名空间：丢掉里面的对象（Linux 的 `free_ipc_ns`；共享内存段
    /// 还映射着的，内存由映射留着）。
    pub fn collect_ipc(&mut self) {
        self.ipcs.retain(|_, (ns, _)| ns.strong_count() > 0);
    }

    /// 新的线程、进程 `tid` 在 pid 命名空间 `ns` 里：在各层分配编号（[`crate::pidns`]），
    /// `set_tid` 见 [`Kernel::choose_pid`]。
    pub fn register_pid(
        &mut self,
        ns: &Rc<crate::pidns::PidNs>,
        tid: Tid,
        set_tid: &[Tid],
    ) -> Result<(), Errno> {
        if ns.level == 0 {
            return Ok(());
        }
        crate::pidns::PidNs::register(ns, tid, set_tid)?;
        self.pid_nss.insert(tid, ns.clone());
        Ok(())
    }

    /// 线程退出、僵尸进程被回收之后：编号 `tid` 线程和进程都不再用了，从各层的表里去掉。
    /// 等着命名空间里的进程都被回收的 1 号进程（[`Kernel::hold_reaper`]）这时等到了就结束。
    pub fn release_pid(&mut self, tid: Tid) {
        if !self.tasks.contains_key(&tid)
            && !self.groups.contains_key(&tid)
            && let Some(ns) = self.pid_nss.remove(&tid)
        {
            crate::pidns::PidNs::unregister(&ns, tid);
            while let Some(i) = self.held_reapers.iter().position(|(ns, g, _)| {
                let tgid = g.borrow().tgid;
                ns.members().iter().all(|&m| m == tgid)
            }) {
                let (_, group, pgrps) = self.held_reapers.swap_remove(i);
                crate::process::finish_reported(self, &group, pgrps);
            }
        }
    }

    /// pid 命名空间 `ns` 的 1 号进程 `group` 退出了，命名空间里还有别的进程：等它们都被
    /// 回收了再报告给父进程（`pgrps` 是 [`crate::process::finish_group`] 算好的进程组）。
    pub fn hold_reaper(
        &mut self,
        ns: Rc<crate::pidns::PidNs>,
        group: Rc<RefCell<ThreadGroup>>,
        pgrps: Vec<Tid>,
    ) {
        self.held_reapers.push((ns, group, pgrps));
    }

    /// 进程 `tgid` 所在的 pid 命名空间。
    pub fn pid_ns_of(&self, tgid: Tid) -> Rc<crate::pidns::PidNs> {
        self.groups
            .get(&tgid)
            .map_or_else(crate::pidns::PidNs::init, |g| g.borrow().pid_ns.clone())
    }

    /// 全局编号 `g` 在进程 `viewer` 的 pid 命名空间里的编号，看不到时是 0。
    pub fn pid_seen_by(&self, viewer: Tid, g: Tid) -> Tid {
        self.pid_ns_of(viewer).vnr(g).unwrap_or(0)
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
        let channels = WAKE.swap(0, Ordering::AcqRel);
        if channels == 0 {
            return;
        }
        let mut objects = WAKE_OBJECTS.lock().unwrap_or_else(PoisonError::into_inner);
        for (event, interest, wanted) in &self.waiters {
            let hit = interest & channels & wake::ALL != 0
                || interest & wake::FILE != 0
                    && !objects.is_empty()
                    && wanted
                        .as_ref()
                        .is_none_or(|w| objects.iter().any(|o| w.binary_search(o).is_ok()));
            if hit {
                signal_later(event);
            }
        }
        objects.clear();
    }
}

/// 取 BKL 时最多自旋多久（[`Shared::lock`]）。原来自旋 100 次（不到 1 微秒），四个
/// 进程同时起进程时总吞吐还不如一个：持锁的线程一放开，等锁的线程都已经睡下，叫醒它们
/// 的这段时间锁空着。10 到 60 微秒都差不多，更长时自旋的线程反而占了持锁线程的核。
const BKL_SPIN: std::time::Duration = std::time::Duration::from_micros(30);

/// 同时最多几个等 BKL 的线程自旋（[`Shared::lock`]），其余的直接睡下：宿主 CPU 数的一半，
/// 1 到 4 个。原来只让一个自旋，8 个核上 8 个进程同时起进程时锁几乎一直有人等，自旋的
/// 那个一拿到锁，下一次交接就只能叫醒睡着的，叫醒的这段时间锁空着（占了三成的时间）。
/// 让 4 个自旋，同时 exec 快 7–10%、跑管道快 13%，这时多用 18% 的 CPU；go build、Maven
/// 不变。再多没有更快，自旋的线程反而要占持锁线程的核。
fn max_spinners() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| (appine_hal::host_cpus().len() / 2).clamp(1, 4) as u32)
}

/// 所有 guest 线程共享的内核对象。
pub(crate) struct Shared {
    kernel: Mutex<Kernel>,
    /// 有线程持有 BKL（只是提示）：等锁的线程自旋时只读它，看起来空了才去抢，不让锁所在
    /// 的缓存行在几个核之间来回。
    held: AtomicBool,
    /// 在自旋等 BKL 的线程数：最多 [`max_spinners`] 个，别的直接睡下，不白占几个核。
    spinning: AtomicU32,
    /// 叫醒定时器线程。
    pub timers: Arc<Event>,
    /// 定时器线程的定期检查（内存上限 [`crate::oom`]、平均负载 [`crate::loadavg`]）停下来了：
    /// 所有线程都在等待。
    pub checks_idle: AtomicBool,
    /// 线程为不持 BKL 的系统调用记下的状态可能过时了就加一（[`Shared::fast_changed`]）。
    fast: AtomicU64,
    /// futex 的等待队列。不持 BKL 的 futex（`syscall::fast`）只取这把锁；持有 BKL 时也可以
    /// 再取它，反过来不行。检查 futex 的值和登记成为等待者在同一次持锁里做，唤醒也要
    /// 取它，所以两者之间不会漏掉唤醒。
    futexes: Mutex<Futexes>,
}

impl Shared {
    pub fn new(kernel: Kernel) -> Shared {
        Shared {
            kernel: Mutex::new(kernel),
            held: AtomicBool::new(false),
            spinning: AtomicU32::new(0),
            timers: Arc::new(Event::new().expect("cannot create an eventfd for the timers")),
            checks_idle: AtomicBool::new(false),
            fast: AtomicU64::new(0),
            futexes: Mutex::new(Futexes::default()),
        }
    }

    /// 线程为不持 BKL 的系统调用记下的状态（`syscall::fast`）可能过时了：某个线程的根目录、
    /// 当前目录或者凭据变了。持有 BKL 时调用。
    pub fn fast_changed(&self) {
        self.fast.fetch_add(1, Ordering::Release);
    }

    /// [`Shared::fast_changed`] 的次数。
    pub fn fast_epoch(&self) -> u64 {
        self.fast.load(Ordering::Acquire)
    }

    /// futex 的等待队列（[`Shared::futexes`]）。
    pub fn futexes(&self) -> MutexGuard<'_, Futexes> {
        self.futexes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 线程要接着执行 guest 代码了：定期检查停着时叫醒定时器线程。不需要持有 BKL。
    pub fn resume_checks(&self) {
        if self.checks_idle.load(Ordering::Relaxed)
            && self.checks_idle.swap(false, Ordering::Relaxed)
        {
            self.timers.signal();
        }
    }

    /// 获取 BKL。内核层 panic 时锁会中毒：出问题的进程已经被结束（[`crate::bug`]），
    /// 其余的照常使用它。
    ///
    /// 持锁的时间通常只有几微秒（一次 lstat），长的（execve、fork、进程退出）十几到几十
    /// 微秒，先自旋最多 [`BKL_SPIN`]：睡下再被叫醒要几十微秒，释放的一方还要多一次 futex
    /// 唤醒，锁在这期间空着。`git status` 的并行 lstat 原来比单线程还慢。自旋只读
    /// [`Shared::held`]，看起来空了才去抢；已经有 [`max_spinners`] 个在自旋时直接睡下。
    pub fn lock(&self) -> Bkl<'_> {
        if let Ok(k) = self.kernel.try_lock() {
            return Bkl::new(k, &self.held);
        }
        if self.spinning.fetch_add(1, Ordering::Acquire) < max_spinners() {
            if let Some(k) = self.spin() {
                return k;
            }
        } else {
            self.spinning.fetch_sub(1, Ordering::Release);
        }
        Bkl::new(
            self.kernel.lock().unwrap_or_else(PoisonError::into_inner),
            &self.held,
        )
    }

    /// 自旋等 BKL，最多 [`BKL_SPIN`]；调用方已经把 [`Shared::spinning`] 加了一，这里减掉。
    fn spin(&self) -> Option<Bkl<'_>> {
        let mut deadline = None;
        for i in 0u32.. {
            if !self.held.load(Ordering::Relaxed) {
                let k = match self.kernel.try_lock() {
                    Ok(k) => Some(k),
                    Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock) => None,
                };
                if let Some(k) = k {
                    self.spinning.fetch_sub(1, Ordering::Release);
                    return Some(Bkl::new(k, &self.held));
                }
            }
            std::hint::spin_loop();
            // 读时钟比自旋一圈贵得多，隔一阵看一次
            if i % 64 == 63 {
                let now = Instant::now();
                if now >= *deadline.get_or_insert(now + BKL_SPIN) {
                    break;
                }
            }
        }
        self.spinning.fetch_sub(1, Ordering::Release);
        None
    }
}

/// 持有 BKL 的凭证；释放时唤醒等待中的线程，再把持有期间释放的宿主内存交还宿主
/// （[`crate::mm::release_unmapped`]）。
pub(crate) struct Bkl<'a>(ManuallyDrop<MutexGuard<'a, Kernel>>, &'a AtomicBool);

impl<'a> Bkl<'a> {
    fn new(guard: MutexGuard<'a, Kernel>, held: &'a AtomicBool) -> Bkl<'a> {
        held.store(true, Ordering::Relaxed);
        HELD.set(true);
        crate::mm::defer_unmaps();
        Bkl(ManuallyDrop::new(guard), held)
    }
}

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
        // 持有期间关掉了最后一个主设备的终端要挂断，挂断要发信号、叫醒等待者
        let k: &mut Kernel = &mut self.0;
        if crate::bug::catch(|| crate::syscall::hang_up_closed(k)).is_none() {
            crate::bug::report(format_args!("kernel panic while hanging up terminals"));
        }
        self.0.flush_wakeups();
        self.1.store(false, Ordering::Relaxed);
        // SAFETY: 之后不再使用 self.0
        unsafe { ManuallyDrop::drop(&mut self.0) };
        HELD.set(false);
        crate::task::pool::notify_pending();
        flush_signals();
        crate::mm::release_unmapped();
        crate::mm::spare::refill();
    }
}

/// 触发 [`signal_later`] 攒下的事件。
pub(crate) fn flush_signals() {
    SIGNALS.with_borrow_mut(|s| s.drain(..).for_each(|e| e.signal()));
}

/// 释放 BKL 等待：事件被触发（[`request_wake`]、信号）、`host` 里的宿主 fd 就绪或者
/// 到达 `deadline` 时重新获取 BKL 返回。调用方重新检查自己的条件。`interest` 是关心的
/// 通道（[`wake`]）；为 0 时不登记在 [`request_wake`] 要叫醒的名单里，只有直接通知
/// `event` 才会醒（futex）。
pub(crate) fn block<'a>(
    shared: &'a Shared,
    k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
    interest: u32,
) -> (Bkl<'a>, Result<(), Errno>) {
    block_for(shared, k, event, deadline, host, interest, None)
}

/// [`block`]；`objects` 不为 None 时，只影响别的对象的修改（[`request_wake_object`]）
/// 不叫醒它。
pub(crate) fn block_for<'a>(
    shared: &'a Shared,
    mut k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
    interest: u32,
    objects: Option<&[wake::Object]>,
) -> (Bkl<'a>, Result<(), Errno>) {
    const POLLIN: i16 = 1;
    crate::cell::assert_none_held();
    let listed = interest != 0;
    if listed {
        let objects = objects.map(|o| {
            let mut o = Box::<[wake::Object]>::from(o);
            o.sort_unstable();
            o
        });
        k.waiters.push((event.clone(), interest, objects));
    }
    drop(k);
    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let mut fds: Vec<(i32, i16, i16)> = host.iter().map(|&(fd, ev, _)| (fd, ev, 0)).collect();
    fds.push((event.fd(), POLLIN, 0));
    let r = appine_hal::poll(&mut fds, timeout);
    let mut k = shared.lock();
    event.clear();
    if listed {
        k.waiters.retain(|(e, ..)| !Arc::ptr_eq(e, event));
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

    /// 挂载命名空间的编号。
    pub fn mnt_ns(&self) -> u64 {
        self.t.fs.borrow().mnt_ns.id
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
        self.shared.fast_changed();
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

    /// 所在的 UTS、IPC 命名空间。
    /// 线程所在的 time 命名空间里时钟 `clock` 的偏移（纳秒）。
    pub fn time_offset(&self, clock: i32) -> i64 {
        self.task.ns.borrow().time.offset(clock)
    }

    pub fn ns(&self) -> crate::nsproxy::NsProxy {
        self.task.ns.borrow().clone()
    }

    /// 所在的 IPC 命名空间里的对象。
    pub fn ipc(&self) -> &crate::ipc::Ipc {
        let id = self.task.ns.borrow().ipc.id;
        self.kr().ipc(id)
    }

    pub fn ipc_mut(&mut self) -> &mut crate::ipc::Ipc {
        let id = self.task.ns.borrow().ipc.id;
        self.k().ipc_mut(id)
    }

    /// 所在的 pid 命名空间。
    pub fn pid_ns(&self) -> Rc<crate::pidns::PidNs> {
        self.t.group.borrow().pid_ns.clone()
    }

    /// 新建的子进程所在的 pid 命名空间。
    pub fn pid_ns_children(&self) -> Rc<crate::pidns::PidNs> {
        let ns = self.task.pid_ns_children.borrow().clone();
        ns.unwrap_or_else(|| self.pid_ns())
    }

    /// 全局编号 `g` 在自己的 pid 命名空间里的编号，看不到时是 0（返回给 guest 的 pid）。
    pub fn pid_out(&self, g: Tid) -> Tid {
        self.pid_ns().vnr(g).unwrap_or(0)
    }

    /// guest 给的 pid（自己的 pid 命名空间里的编号）对应的全局编号，看不到的是一个不存在
    /// 的编号；0 和负数不变（Linux 的 `find_vpid`）。
    pub fn pid_in(&self, l: Tid) -> Tid {
        if l <= 0 {
            return l;
        }
        self.pid_ns().find(l).unwrap_or(crate::syscall::NO_PID)
    }

    /// 当前进程的凭据。
    pub fn cred(&self) -> crate::cred::Cred {
        self.t.group.borrow().cred.clone()
    }

    /// AF_UNIX 的 `SO_PEERCRED`：pid 和有效用户 ID、组 ID（内核 ID，Linux 的 `cred_to_ucred`）。
    pub fn ucred(&self) -> crate::unix::Ucred {
        let g = self.t.group.borrow();
        crate::unix::Ucred::of(self.tgid(), g.cred.euid, g.cred.egid)
    }

    /// 没有声明时附带的 `SCM_CREDENTIALS`：pid 和真实用户 ID、组 ID（Linux 的
    /// `maybe_add_creds` 用 `current_uid_gid`）。
    pub fn scm_cred(&self) -> crate::unix::Ucred {
        let g = self.t.group.borrow();
        crate::unix::Ucred::of(self.tgid(), g.cred.ruid, g.cred.rgid)
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
        self.wait_on(deadline, host, wake::ALL)
    }

    /// 只等直接的通知（futex 唤醒、信号、被结束）或者超时，[`request_wake`] 不会叫醒它。
    /// 调用方必须保证让它的条件成立的每个地方都直接通知了 `self.task.event`。
    pub fn wait_alone(&mut self, deadline: Option<Instant>) -> Result<(), Errno> {
        self.wait_on(deadline, &mut [], 0)
    }

    /// 与 [`Cx::wait`] 相同，但只被 `interest` 里的通道（[`wake`]）叫醒。调用方必须保证
    /// 让它的条件成立的修改都请求了这些通道。
    pub fn wait_on(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
    ) -> Result<(), Errno> {
        self.wait_for(deadline, host, interest, None)
    }

    /// 等对象 `object`（管道）上的修改（[`request_wake_object`]）、请求所有通道的修改、
    /// 直接的通知或者超时。
    pub fn wait_object(&mut self, object: wake::Object) -> Result<(), Errno> {
        self.wait_for(None, &mut [], wake::FILE, Some(&[object]))
    }

    /// 与 [`Cx::wait`] 相同，但只影响对象的修改（[`request_wake_object`]）只在是 `objects`
    /// 里的对象时才叫醒它。调用方必须保证让它的条件成立的修改要么请求了这些对象之一，
    /// 要么请求了 `interest` 里的某个通道（[`request_wake_on`]、[`request_wake`]）。
    pub fn wait_objects(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
        objects: &[wake::Object],
    ) -> Result<(), Errno> {
        self.wait_for(deadline, host, interest, Some(objects))
    }

    fn wait_for(
        &mut self,
        deadline: Option<Instant>,
        host: &mut [(i32, i16, i16)],
        interest: u32,
        objects: Option<&[wake::Object]>,
    ) -> Result<(), Errno> {
        let k = self.k.take().expect("BKL is held");
        self.task.sleeping.store(true, Ordering::Relaxed);
        let relock = Relock(self);
        let cx = &mut *relock.0;
        let event = &cx.task.event;
        let (k, r) = block_for(cx.shared, k, event, deadline, host, interest, objects);
        cx.k = Some(k);
        r
    }

    /// 释放 BKL 执行 `f`（可能阻塞的宿主 I/O）。`f` 不能访问任何内核对象：它是 `Send`，
    /// 捕获不了 `Rc`、`RefCell` 和它们的引用。
    pub fn unlocked<R>(&mut self, f: impl FnOnce() -> R + Send) -> R {
        crate::cell::assert_none_held();
        self.task.sleeping.store(true, Ordering::Relaxed);
        drop(self.k.take());
        let _relock = Relock(self);
        f()
    }
}

/// 放开了 BKL 的 [`Cx`]（[`Cx::wait_on`] 等、[`Cx::unlocked`]）：析构时还没有重新拿到
/// BKL 就拿到它。正常返回时就是重新拿锁；放锁期间 panic（内核层的 bug，[`crate::bug`]）
/// 时也一样，展开经过的外层各帧丢掉的 `Rc`（vfs、打开文件……）要在持有 BKL 时丢：计数
/// 不是原子的，会和同时持有 BKL 的线程一起改，改丢了就提前释放。
struct Relock<'c, 'a>(&'c mut Cx<'a>);

impl Drop for Relock<'_, '_> {
    fn drop(&mut self) {
        if self.0.k.is_none() {
            self.0.k = Some(self.0.shared.lock());
        }
        self.0.task.sleeping.store(false, Ordering::Relaxed);
    }
}
