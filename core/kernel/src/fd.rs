//! 打开文件与 fd 表。
//!
//! 一个打开文件（Linux 的 `struct file`）记录文件位置和状态标志，可以被多个
//! fd 共享（dup、fork）；close-on-exec 标志属于 fd 本身。打开文件和 fd 表都
//! 只在持有 BKL 时访问（[`crate::kernel`]）。

use std::fs::File;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::pipe::PipeEnd;
use crate::pseudo::PseudoNode;
use crate::tty::PtyEnd;
use crate::vfs::Node;
use crate::vfs::dev::Device;
use crate::vfs::store::{DirEntry, Ino};

/// 每个进程最多能打开的 fd 数（`/proc/sys/fs/nr_open`，`RLIMIT_NOFILE` 的上限），与 Linux
/// 的默认值相同。
pub const MAX_FDS: usize = 1 << 20;

/// 新进程的 `RLIMIT_NOFILE`：软限制 1024、硬限制 4096，与 Linux 内核的默认值相同。Go 在
/// 启动时把软限制提到硬限制。
pub const DEFAULT_NOFILE: (usize, usize) = (1024, 4096);

/// open(2) 的标志位（arm64 的取值）。
pub mod oflags {
    /// 访问模式掩码。
    pub const O_ACCMODE: u32 = 0o3;
    /// 只读。
    pub const O_RDONLY: u32 = 0o0;
    /// 只写。
    pub const O_WRONLY: u32 = 0o1;
    /// 读写。
    pub const O_RDWR: u32 = 0o2;
    /// 不存在时创建。
    pub const O_CREAT: u32 = 0o100;
    /// 与 `O_CREAT` 合用：已存在时失败。
    pub const O_EXCL: u32 = 0o200;
    /// 不让终端成为控制终端。
    pub const O_NOCTTY: u32 = 0o400;
    /// 截断。
    pub const O_TRUNC: u32 = 0o1000;
    /// 追加写。
    pub const O_APPEND: u32 = 0o2000;
    /// 非阻塞。
    pub const O_NONBLOCK: u32 = 0o4000;
    /// 信号驱动的 I/O：就绪时给属主发信号（`syscall::poll` 的 fasync）。
    pub const O_ASYNC: u32 = 0o20000;
    /// 直接 I/O（只记录）。
    pub const O_DIRECT: u32 = 0o200000;
    /// 必须是目录。
    pub const O_DIRECTORY: u32 = 0o40000;
    /// 不跟随最后一个组件的符号链接。
    pub const O_NOFOLLOW: u32 = 0o100000;
    /// 大文件（64 位上恒为真）。
    pub const O_LARGEFILE: u32 = 0o400000;
    /// 不更新访问时间（只有属主或者有 `CAP_FOWNER` 的进程能用）。
    pub const O_NOATIME: u32 = 0o1000000;
    /// exec 时关闭。
    pub const O_CLOEXEC: u32 = 0o2000000;
    /// 只取路径。
    pub const O_PATH: u32 = 0o10000000;
    /// fcntl(F_SETFL) 能修改的标志。
    pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK | O_ASYNC | O_DIRECT | O_NOATIME;
}

/// 打开文件的种类。
pub enum FileKind {
    /// 宿主 fd，`appine-run` 把宿主的 stdin/stdout/stderr 原样交给 guest。
    Host(i32),
    /// inode 仓库里的普通文件。
    Regular {
        /// inode。
        ino: Ino,
        /// 宿主 object 文件。
        file: File,
    },
    /// 目录。目录项在第一次 getdents 时取一份快照，lseek 到 0 时丢弃。
    Directory {
        /// 目录节点（仓库里的，或者合成文件系统里的）。
        node: Node,
        /// 目录项快照（含 `.` 和 `..`）。
        entries: Option<Vec<DirEntry>>,
        /// `/proc/<pid>`：打开时的那个进程（当作 pidfd 用）。
        process: Option<std::rc::Weak<RefCell<crate::process::ThreadGroup>>>,
    },
    /// `O_PATH` 打开的文件：只能作为 `*at` 的起点或 fstat 的对象。
    Path(Node),
    /// 管道的一端。
    Pipe(PipeEnd),
    /// 字符设备（`/dev/null` 等），`ino` 是仓库里的设备节点。
    Device {
        /// inode。
        ino: Ino,
        /// 设备。
        dev: Device,
        /// `/dev/kmsg` 的读位置和写的限速（[`Device::Kmsg`] 才有）。
        kmsg: Option<Box<crate::klog::KmsgFile>>,
    },
    /// PTY 的主设备或从设备。
    Pty(PtyEnd),
    /// socket。
    Socket(crate::syscall::SocketRef),
    /// epoll 实例。
    Epoll(Rc<RefCell<crate::syscall::Epoll>>),
    /// eventfd：一个 64 位计数器。
    EventFd(Rc<RefCell<EventFd>>),
    /// timerfd：一个定时器，到期的次数由 read 取走。
    TimerFd(Rc<RefCell<TimerFd>>),
    /// signalfd：read 取走调用者挂起的、在掩码里的信号。
    SignalFd(Rc<RefCell<SignalFd>>),
    /// inotify 实例：read 取走排队的文件系统事件。
    Inotify(Rc<RefCell<crate::syscall::Inotify>>),
    /// pidfd：指向一个进程，进程结束时变为可读。
    Pidfd(Pidfd),
    /// POSIX 消息队列。
    MessageQueue(Rc<RefCell<crate::syscall::MessageQueue>>),
    /// seccomp 的通知 fd（`SECCOMP_FILTER_FLAG_NEW_LISTENER`）。
    SeccompNotify(crate::seccomp::Listener),
    /// eBPF 程序（[`crate::bpf`]）。
    Bpf(std::rc::Rc<crate::bpf::Prog>),
    /// procfs、sysfs、cgroup2 的文件：打开时生成的内容，从偏移 0 再读时重新生成。
    Proc {
        /// 经由的挂载。
        mnt: crate::vfs::MountId,
        /// 节点。
        node: PseudoNode,
        /// 内容。
        data: Vec<u8>,
        /// `data` 是打开时刚生成的，还没有读过、写过：第一次从 0 读时不用再生成。
        fresh: bool,
        /// `/proc/<pid>/mem`：打开时那个进程的地址空间。与 Linux 一样，进程 execve 之后
        /// 读写的仍是它（已经释放时读到 EOF），不是新程序的。
        mm: Option<std::rc::Weak<RefCell<crate::mm::Mm>>>,
        /// 命名空间文件（nsfs）：打开着就让那个命名空间活着，setns、`NS_GET_*` 用它。
        ns: Option<crate::nsproxy::NsRef>,
        /// `/proc/kmsg`：内核日志（poll 要看它）。
        klog: Option<Rc<RefCell<crate::klog::Klog>>>,
    },
}

/// 一个打开文件。
pub struct OpenFile {
    /// 种类。
    pub kind: FileKind,
    /// 访问模式和状态标志（不含 `O_CLOEXEC`）。
    pub flags: u32,
    /// 文件位置；目录是下一个目录项的下标。
    pub pos: u64,
    /// `/proc/<pid>/fd/<n>` 显示的路径（打开时的绝对路径）。
    pub path: Option<Rc<[u8]>>,
    /// 经由路径打开时最后经过的目录和名字：经由 `/proc/<pid>/fd/<n>` 把它当挂载点时
    /// （bubblewrap 这样挂），挂载记下它（[`crate::vfs::Mount::at`]）。
    pub dentry: Option<crate::vfs::Dentry>,
    /// 仓库里的普通文件、设备节点和 FIFO 是经由哪个挂载打开的（[`OpenFile::node`]）：
    /// 只读、noexec 这些标志按它算。目录和 `O_PATH` 的节点本来就带着挂载。
    pub mnt: crate::vfs::MountId,
    /// `F_SETOWN_EX` 的 (类型, pid) 和 `F_SETSIG` 的信号：`O_ASYNC` 时就绪发给谁、发
    /// 什么信号（0 是 SIGIO），文件租约被打破时也发给它们。
    pub owner: (i32, i32),
    pub sig: i32,
    /// 程序在这个打开文件上读、写两个方向各用完数据的次数：epoll 的 `EPOLLET` 据此
    /// 知道程序已经用完了上一次报告的事件（`syscall::poll::drained`）。
    pub drained: [u64; 2],
    /// 读到数据的次数：设置了 `O_ASYNC` 时读过之后仍然可读，就当作可能来了新数据，再
    /// 通知一次（`syscall::poll` 的 fasync）。
    pub reads: u64,
    /// `EPOLLEXCLUSIVE` 的关注报告过的边沿（`syscall::poll` 的数据代数）和报告它的 epoll：
    /// 同一个边沿别的 epoll 不再报告（Linux 只叫醒一个等待者）。
    pub exclusive_claim: std::cell::Cell<Option<([u64; 4], usize)>>,
    /// 宿主 fd 上新到的事件：有边沿触发或者 `EPOLLEXCLUSIVE` 的关注时才建立
    /// （`syscall::poll`）。
    pub host_edges: Option<HostEdges>,
    /// 打开的仓库文件：inotify 按 fd 的事件报给打开时经过的目录，最后一个引用释放时
    /// 报告 `IN_CLOSE_*`。
    pub watch: Option<crate::syscall::Opened>,
    /// 用文件位置的一次读写正在不持 BKL 地进行（相当于 Linux 的 f_pos_lock，见
    /// `syscall::fs` 的 `unlocked_io`）：0 没有，1 有，2 有并且还有线程在等它。
    pub pos_busy: u8,
    /// 加过 flock、OFD 锁或者租约：它们以打开文件为持有者，关闭时随之释放，等锁的线程
    /// 要重新检查。
    pub holds_locks: bool,
    /// 不是经由 open 建的（memfd_create，Linux 的 `alloc_file_pseudo`）：不算文件的写者，
    /// 执行它不是 ETXTBSY；经由 `/proc/<pid>/fd/<n>` 重新打开的照常算。也不经由挂载，
    /// noexec、nosuid 不适用。
    pub pseudo: bool,
}

impl OpenFile {
    /// 新建打开文件。
    pub fn new(kind: FileKind, flags: u32) -> FileRef {
        Self::with_path(kind, flags, None)
    }

    /// 新建打开文件，并记下打开时的绝对路径。
    pub fn with_path(kind: FileKind, flags: u32, path: Option<Rc<[u8]>>) -> FileRef {
        Rc::new(RefCell::new(OpenFile {
            kind,
            flags: flags & !oflags::O_CLOEXEC,
            pos: 0,
            path,
            dentry: None,
            mnt: crate::vfs::ROOT_MOUNT,
            owner: (0, 0),
            sig: 0,
            drained: [0; 2],
            reads: 0,
            exclusive_claim: std::cell::Cell::new(None),
            host_edges: None,
            pos_busy: 0,
            holds_locks: false,
            watch: None,
            pseudo: false,
        }))
    }

    /// 是否可读。
    pub fn readable(&self) -> bool {
        self.flags & oflags::O_PATH == 0 && self.flags & oflags::O_ACCMODE != oflags::O_WRONLY
    }

    /// 是否可写。
    pub fn writable(&self) -> bool {
        self.flags & oflags::O_PATH == 0 && self.flags & oflags::O_ACCMODE != oflags::O_RDONLY
    }

    /// 对应的文件系统节点（宿主 fd、匿名管道和 PTY 没有）。
    pub fn node(&self) -> Option<Node> {
        match &self.kind {
            FileKind::Pipe(end) => end.pipe.borrow().fifo.map(|ino| Node::Store(self.mnt, ino)),
            FileKind::Host(_)
            | FileKind::Pty(_)
            | FileKind::Socket(_)
            | FileKind::Epoll(_)
            | FileKind::EventFd(_)
            | FileKind::TimerFd(_)
            | FileKind::SignalFd(_)
            | FileKind::Inotify(_)
            | FileKind::Pidfd(_)
            | FileKind::SeccompNotify(_)
            | FileKind::Bpf(_)
            | FileKind::MessageQueue(_) => None,
            FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => {
                Some(Node::Store(self.mnt, *ino))
            }
            FileKind::Directory { node, .. } | FileKind::Path(node) => Some(node.clone()),
            FileKind::Proc { mnt, node, .. } => Some(Node::Pseudo(*mnt, node.clone())),
        }
    }

    /// 仓库里的 inode（chmod、chown 等只对它们有意义）。
    pub fn ino(&self) -> Option<Ino> {
        self.node()?.ino()
    }

    /// 是否以非阻塞方式打开。
    pub fn nonblocking(&self) -> bool {
        self.flags & oflags::O_NONBLOCK != 0
    }
}

impl Drop for OpenFile {
    /// 最后一个引用消失：持有的 flock、OFD 锁、租约随之释放，等锁的线程要重新检查
    /// （[`wake::LOCK`](crate::kernel::wake::LOCK)）；宿主 socket 的 fd 要关掉了，在 poll、
    /// epoll、阻塞的收发里等着它的线程的宿主 poll 还拿着它（宿主 socket 要等它们放手才
    /// 真正关闭），只叫醒登记了这个 socket 的等待者。别的文件关闭时不必叫醒谁：管道、
    /// PTY、AF_UNIX socket 的另一方由 `PipeEnd`、`PtyEnd`、`UnixSocket` 叫醒（只叫醒等这个
    /// 管道、PTY、连接的），epoll、eventfd、inotify、消息队列这类对象关闭时没有线程在等它
    /// （阻塞在上面的收发拿着打开文件，与 Linux 一样不因别人关闭 fd 而醒）。shell 每次
    /// exec、退出都要关好几个文件，原来每关一个都把读着管道的进程叫醒一遍。
    fn drop(&mut self) {
        match &self.kind {
            FileKind::Socket(s)
                if !s
                    .try_borrow()
                    .is_ok_and(|s| matches!(*s, crate::net::Socket::Unix(_))) =>
            {
                crate::kernel::request_wake_object(crate::kernel::wake::of(s));
            }
            _ => {}
        }
        if self.holds_locks {
            crate::kernel::request_wake_locks();
        }
    }
}

/// signalfd 的状态。与 Linux 一样不属于哪个线程：poll、epoll 和读看的都是调用的线程和
/// 它的线程组挂起的信号（[`crate::task::current`]），fork 出来的进程看自己的。
pub struct SignalFd {
    /// 关注的信号（不含 SIGKILL、SIGSTOP）。
    pub mask: u64,
    /// 创建它的线程组：不在 guest 线程上检查它时（定时器线程替信号驱动的 I/O、独占门
    /// 检查）只看这个线程组挂起的信号。
    pub group: std::rc::Weak<RefCell<crate::process::ThreadGroup>>,
}

impl SignalFd {
    /// 调用的线程和它的线程组；不在 guest 线程上时是创建它的线程组。
    fn pending(
        &self,
    ) -> (
        Option<Rc<crate::task::Task>>,
        Option<Rc<RefCell<crate::process::ThreadGroup>>>,
    ) {
        match crate::task::current() {
            Some((task, group)) => (Some(task), Some(group)),
            None => (None, self.group.upgrade()),
        }
    }

    /// 调用的线程和它的线程组一共挂起过几次信号（`syscall::poll` 的数据代数）。
    pub fn arrivals(&self) -> u64 {
        let (task, group) = self.pending();
        let group = group.map_or(0, |g| g.borrow().pending.arrivals());
        group + task.map_or(0, |t| t.pending.borrow().arrivals())
    }

    /// 有没有调用的线程读得走的信号（poll）。
    pub fn ready(&self) -> bool {
        let (task, group) = self.pending();
        let group = group.map_or(0, |g| g.borrow().pending.set());
        let task = task.map_or(0, |t| t.pending.borrow().set());
        (group | task) & self.mask != 0
    }
}

/// pidfd 指向的进程。拿的是弱引用：pid 分给了别的进程之后，它仍然指向原来那个。
pub struct Pidfd {
    pub tgid: crate::kernel::Tid,
    pub group: std::rc::Weak<RefCell<crate::process::ThreadGroup>>,
}

impl Pidfd {
    /// 进程还没有被回收时返回它的线程组（可能是僵尸进程）。
    pub fn group(
        &self,
        k: &crate::kernel::Kernel,
    ) -> Option<Rc<RefCell<crate::process::ThreadGroup>>> {
        let g = self.group.upgrade()?;
        k.groups
            .get(&self.tgid)
            .is_some_and(|o| Rc::ptr_eq(o, &g))
            .then_some(g)
    }

    /// 所有线程都已退出（poll 可读）。
    pub fn exited(&self) -> bool {
        self.group
            .upgrade()
            .is_none_or(|g| g.borrow().threads.is_empty())
    }
}

/// timerfd 的状态。
pub struct TimerFd {
    /// 时钟（`CLOCK_REALTIME` 等）：设置绝对时间时按它换算。
    pub clock: i32,
    /// 下一次到期的时刻（宿主的单调时钟）；None 表示没有启动。
    pub deadline: Option<Instant>,
    /// 到期之后重新计时的间隔，0 表示只触发一次。
    pub interval: Duration,
    /// 已经到期、还没有被读走的次数。
    pub expirations: u64,
    /// 一共到期过几次（不因为读走而减少）：epoll 的 `EPOLLET` 每次到期都触发。
    pub fired: u64,
    /// 上一次 timerfd_settime 的 flags（`/proc/<pid>/fdinfo`）。
    pub settime_flags: u32,
}

impl TimerFd {
    /// 离下一次到期还有多久，没有启动时是 0（timerfd_gettime 的 `it_value`）。
    pub fn remaining(&mut self, now: Instant) -> Duration {
        self.update(now);
        self.deadline
            .map_or(Duration::ZERO, |d| d.saturating_duration_since(now))
    }

    /// `/proc/<pid>/fdinfo` 里 timerfd 特有的几行。
    pub fn fdinfo(&mut self) -> String {
        let left = self.remaining(Instant::now());
        format!(
            "clockid: {}\nticks: {}\nsettime flags: 0{:o}\nit_value: ({}, {})\nit_interval: ({}, {})\n",
            self.clock,
            self.expirations,
            self.settime_flags,
            left.as_secs(),
            left.subsec_nanos(),
            self.interval.as_secs(),
            self.interval.subsec_nanos()
        )
    }

    /// 把 `now` 之前到期的次数记进 `expirations`。
    pub fn update(&mut self, now: Instant) {
        let Some(d) = self.deadline.filter(|&d| d <= now) else {
            return;
        };
        self.fired += 1;
        if self.interval.is_zero() {
            self.expirations += 1;
            self.deadline = None;
        } else {
            let n = ((now - d).as_nanos() / self.interval.as_nanos()) as u64 + 1;
            self.expirations += n;
            self.deadline = Some(d + self.interval * n as u32);
        }
    }
}

/// eventfd 的状态。
pub struct EventFd {
    /// 计数器。
    pub count: u64,
    /// `EFD_SEMAPHORE`：每次读只减 1。
    pub semaphore: bool,
    /// 写入、读出的次数：与 Linux 一样，每次写都让读的一方的 `EPOLLET` 再触发一次（mio
    /// 的唤醒器就靠它，从来不读走计数），每次读都让写的一方再触发一次。
    pub writes: u64,
    pub reads: u64,
}

/// 宿主 fd 上新到的事件（[`appine_hal::Edges`]）累计的次数。
pub struct HostEdges {
    edges: appine_hal::Edges,
    counts: std::cell::Cell<[u64; 2]>,
    /// 宿主 epoll 上可能有还没取走的事件：最近一次和宿主 fd 一起 poll 时它可读
    /// （[`HostEdges::polled`]）。
    unread: std::cell::Cell<bool>,
}

impl HostEdges {
    /// 开始记录宿主 fd `fd` 上新到的事件。
    pub fn new(fd: i32) -> Result<HostEdges, i32> {
        Ok(HostEdges {
            edges: appine_hal::Edges::new(fd)?,
            counts: std::cell::Cell::new([0; 2]),
            unread: std::cell::Cell::new(true),
        })
    }

    /// 到现在为止读、写两个方向各到过几次事件。
    pub fn counts(&self) -> [u64; 2] {
        self.unread.set(false);
        let (r, w) = self.edges.take();
        let [a, b] = self.counts.get();
        let now = [a + r as u64, b + w as u64];
        self.counts.set(now);
        now
    }

    /// 与 [`HostEdges::counts`] 相同，但刚刚和宿主 fd 一起 poll 过（[`HostEdges::polled`]）：
    /// 那时宿主 epoll 不可读就不必再问宿主，每次检查少一次系统调用。
    pub fn counts_polled(&self) -> [u64; 2] {
        if self.unread.get() {
            self.counts()
        } else {
            self.counts.get()
        }
    }

    /// 和宿主 fd 一起 poll 了宿主 epoll 的 fd，得到 `revents`。
    pub fn polled(&self, revents: i16) {
        self.unread.set(self.unread.get() || revents != 0);
    }

    /// 有新事件时可读的宿主 fd，等待时放进宿主 poll。
    pub fn fd(&self) -> i32 {
        self.edges.fd()
    }
}

/// 打开文件的共享引用。
pub type FileRef = Rc<RefCell<OpenFile>>;

#[derive(Clone)]
struct Slot {
    file: FileRef,
    cloexec: bool,
}

/// 一个进程的 fd 表。
pub struct FdTable {
    slots: Vec<Option<Slot>>,
    /// 新的 fd 必须小于它（`RLIMIT_NOFILE` 的软限制，由 prlimit 同步过来）。
    pub limit: usize,
    /// 加过 POSIX 记录锁（锁属于 fd 表，见 `crate::locks`）。
    pub holds_locks: bool,
}

impl Default for FdTable {
    fn default() -> Self {
        FdTable {
            slots: Vec::new(),
            limit: DEFAULT_NOFILE.0,
            holds_locks: false,
        }
    }
}

impl Clone for FdTable {
    /// fork、`CLONE_FILES` 之外的 clone、unshare：POSIX 锁属于原来的 fd 表，不随之复制。
    fn clone(&self) -> Self {
        FdTable {
            slots: self.slots.clone(),
            limit: self.limit,
            holds_locks: false,
        }
    }
}

impl FdTable {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 0/1/2 分别是宿主的 stdin/stdout/stderr。
    pub fn with_host_stdio() -> Self {
        let mut t = Self::new();
        // 与原来一样带着 O_LARGEFILE：宿主那边多半是 open 打开的终端、文件
        for (fd, flags) in [
            (0, oflags::O_RDONLY | oflags::O_LARGEFILE),
            (1, oflags::O_WRONLY | oflags::O_LARGEFILE),
            (2, oflags::O_WRONLY | oflags::O_LARGEFILE),
        ] {
            t.install(OpenFile::new(FileKind::Host(fd), flags), false, 0)
                .unwrap();
        }
        t
    }

    /// 取出 fd 对应的打开文件。
    pub fn get(&self, fd: i32) -> Result<FileRef, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .map(|s| s.file.clone())
            .ok_or(Errno::EBADF)
    }

    /// 不小于 `min` 的最小空闲 fd。
    pub fn free_slot(&self, min: usize) -> Result<i32, Errno> {
        (min..self.limit)
            .find(|&i| self.slots.get(i).is_none_or(Option::is_none))
            .map(|i| i as i32)
            .ok_or(Errno::EMFILE)
    }

    /// 放到不小于 `min` 的最小空闲 fd 上。
    pub fn install(&mut self, file: FileRef, cloexec: bool, min: usize) -> Result<i32, Errno> {
        let fd = self.free_slot(min)?;
        self.install_at(fd, file, cloexec)?;
        Ok(fd)
    }

    /// 放到指定的 fd 上，返回被替换掉的原来的文件（dup2/dup3）。
    pub fn install_at(
        &mut self,
        fd: i32,
        file: FileRef,
        cloexec: bool,
    ) -> Result<Option<FileRef>, Errno> {
        let i = usize::try_from(fd)
            .ok()
            .filter(|&i| i < self.limit)
            .ok_or(Errno::EBADF)?;
        if self.slots.len() <= i {
            self.slots.resize(i + 1, None);
        }
        let old = self.slots[i].replace(Slot { file, cloexec });
        Ok(old.map(|s| s.file))
    }

    /// 关闭 fd，返回关闭的文件。
    pub fn close(&mut self, fd: i32) -> Result<FileRef, Errno> {
        let slot = usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get_mut(i))
            .ok_or(Errno::EBADF)?;
        slot.take().map(|s| s.file).ok_or(Errno::EBADF)
    }

    /// 所有打开着的 fd，从小到大。
    pub fn open_fds(&self) -> Vec<i32> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .map(|(i, _)| i as i32)
            .collect()
    }

    /// execve：关闭所有带 close-on-exec 标志的 fd，返回关闭的文件。
    pub fn close_on_exec(&mut self) -> Vec<FileRef> {
        let mut closed = Vec::new();
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|s| s.cloexec) {
                closed.extend(slot.take().map(|s| s.file));
            }
        }
        closed
    }

    /// close_range(2)：关闭（或只标记 close-on-exec）`[first, last]` 里的 fd，返回
    /// 关闭的文件。
    pub fn close_range(&mut self, first: usize, last: usize, cloexec_only: bool) -> Vec<FileRef> {
        let mut closed = Vec::new();
        let end = last.saturating_add(1).min(self.slots.len());
        for slot in self.slots.iter_mut().take(end).skip(first) {
            match slot {
                Some(s) if cloexec_only => s.cloexec = true,
                _ => closed.extend(slot.take().map(|s| s.file)),
            }
        }
        closed
    }

    /// 读取 close-on-exec 标志。
    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        self.slot(fd).map(|s| s.cloexec)
    }

    /// 设置 close-on-exec 标志。
    pub fn set_cloexec(&mut self, fd: i32, cloexec: bool) -> Result<(), Errno> {
        let i = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        let slot = self
            .slots
            .get_mut(i)
            .and_then(Option::as_mut)
            .ok_or(Errno::EBADF)?;
        slot.cloexec = cloexec;
        Ok(())
    }

    fn slot(&self, fd: i32) -> Result<&Slot, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }
}

impl Drop for FdTable {
    /// fd 表消失（进程退出）：它持有的记录锁随之失效，等锁的线程要重新检查。没加过锁的
    /// 不必：原来每个进程退出都把所有读着管道、在 poll 里等着的线程叫醒一遍。
    fn drop(&mut self) {
        if self.holds_locks {
            crate::kernel::request_wake_locks();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_lowest_free_fd() {
        let mut t = FdTable::with_host_stdio();
        let f = OpenFile::new(FileKind::Host(1), oflags::O_WRONLY);
        assert_eq!(t.install(f.clone(), false, 0), Ok(3));
        t.close(1).unwrap();
        assert_eq!(t.install(f.clone(), true, 0), Ok(1));
        assert_eq!(t.cloexec(1), Ok(true));
        assert_eq!(t.install(f.clone(), false, 10), Ok(10));
        assert_eq!(t.close(7).err(), Some(Errno::EBADF));
        assert_eq!(t.get(-1).err(), Some(Errno::EBADF));
        // 新的 fd 不能超过 RLIMIT_NOFILE
        t.limit = 11;
        for fd in [4, 5, 6, 7, 8, 9] {
            assert_eq!(t.install(f.clone(), false, 0), Ok(fd));
        }
        assert_eq!(t.install(f.clone(), false, 0), Err(Errno::EMFILE));
        // 起点超出上限也是没有空位（F_DUPFD 的 EINVAL 由调用方检查）
        assert_eq!(t.install(f.clone(), false, 11), Err(Errno::EMFILE));
        // dup 出来的 fd 共享同一个打开文件
        f.borrow_mut().pos = 5;
        assert_eq!(t.get(10).unwrap().borrow().pos, 5);
    }
}
