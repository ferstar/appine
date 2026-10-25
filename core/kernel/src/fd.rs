//! 打开文件与 fd 表。
//!
//! 一个打开文件（Linux 的 `struct file`）记录文件位置和状态标志，可以被多个
//! fd 共享（dup、fork）；close-on-exec 标志属于 fd 本身。打开文件和 fd 表都
//! 只在持有 BKL 时访问（[`crate::kernel`]）。

use std::cell::RefCell;
use std::fs::File;
use std::rc::Rc;
use std::time::{Duration, Instant};

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
    pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK;
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
    /// procfs 的文件：打开时生成的内容。
    Proc {
        /// 节点。
        node: PseudoNode,
        /// 内容。
        data: Vec<u8>,
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
    /// `F_SETOWN_EX` 的 (类型, pid) 和 `F_SETSIG` 的信号：只记录，不发 SIGIO。
    pub owner: (i32, i32),
    pub sig: i32,
    /// 程序在这个打开文件上读、写两个方向各用完数据的次数：epoll 的 `EPOLLET` 据此
    /// 知道程序已经用完了上一次报告的事件（`syscall::poll::drained`）。
    pub drained: [u64; 2],
    /// 打开时已经有 inotify 监视：按 fd 的事件报给打开时经过的目录，最后一个引用释放
    /// 时报告 `IN_CLOSE_*`。
    pub watch: Option<crate::syscall::Opened>,
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
            owner: (0, 0),
            sig: 0,
            drained: [0; 2],
            watch: None,
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
            FileKind::Pipe(end) => end.pipe.borrow().fifo.map(Node::Store),
            FileKind::Host(_)
            | FileKind::Pty(_)
            | FileKind::Socket(_)
            | FileKind::Epoll(_)
            | FileKind::EventFd(_)
            | FileKind::TimerFd(_)
            | FileKind::SignalFd(_)
            | FileKind::Inotify(_)
            | FileKind::Pidfd(_) => None,
            FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => Some(Node::Store(*ino)),
            FileKind::Directory { node, .. } | FileKind::Path(node) => Some(node.clone()),
            FileKind::Proc { node, .. } => Some(Node::Pseudo(node.clone())),
        }
    }

    /// 仓库里的 inode（chmod、chown 等只对它们有意义）。
    pub fn ino(&self) -> Option<Ino> {
        match self.node() {
            Some(Node::Store(ino)) => Some(ino),
            _ => None,
        }
    }

    /// 是否以非阻塞方式打开。
    pub fn nonblocking(&self) -> bool {
        self.flags & oflags::O_NONBLOCK != 0
    }
}

impl Drop for OpenFile {
    /// 最后一个引用消失：等在它上面的线程（flock、epoll）要重新检查。
    fn drop(&mut self) {
        crate::kernel::request_wake();
    }
}

/// signalfd 的状态。
pub struct SignalFd {
    /// 关注的信号（不含 SIGKILL、SIGSTOP）。
    pub mask: u64,
    /// 创建它的线程组和线程：poll 按它们挂起的信号判断是否可读（read 取的是调用者的）。
    pub group: std::rc::Weak<RefCell<crate::process::ThreadGroup>>,
    pub task: std::rc::Weak<crate::task::Task>,
}

impl SignalFd {
    /// 有没有可以读走的信号（poll）。
    pub fn ready(&self) -> bool {
        let group = self.group.upgrade().map_or(0, |g| g.borrow().pending.set());
        let task = self.task.upgrade().map_or(0, |t| t.pending.borrow().set());
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
}

/// 打开文件的共享引用。
pub type FileRef = Rc<RefCell<OpenFile>>;

#[derive(Clone)]
struct Slot {
    file: FileRef,
    cloexec: bool,
}

/// 一个进程的 fd 表。
#[derive(Clone)]
pub struct FdTable {
    slots: Vec<Option<Slot>>,
    /// 新的 fd 必须小于它（`RLIMIT_NOFILE` 的软限制，由 prlimit 同步过来）。
    pub limit: usize,
}

impl Default for FdTable {
    fn default() -> Self {
        FdTable {
            slots: Vec::new(),
            limit: DEFAULT_NOFILE.0,
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
        for (fd, flags) in [
            (0, oflags::O_RDONLY),
            (1, oflags::O_WRONLY),
            (2, oflags::O_WRONLY),
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

    /// 放到不小于 `min` 的最小空闲 fd 上。
    pub fn install(&mut self, file: FileRef, cloexec: bool, min: usize) -> Result<i32, Errno> {
        if min >= self.limit {
            return Err(Errno::EINVAL);
        }
        let fd = (min..self.limit)
            .find(|&i| self.slots.get(i).is_none_or(Option::is_none))
            .ok_or(Errno::EMFILE)?;
        self.install_at(fd as i32, file, cloexec)?;
        Ok(fd as i32)
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
    /// fd 表消失（进程退出）：它持有的记录锁随之失效，等锁的线程要重新检查。
    fn drop(&mut self) {
        crate::kernel::request_wake();
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
        assert_eq!(t.install(f.clone(), false, 11), Err(Errno::EINVAL));
        // dup 出来的 fd 共享同一个打开文件
        f.borrow_mut().pos = 5;
        assert_eq!(t.get(10).unwrap().borrow().pos, 5);
    }
}
