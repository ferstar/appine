//! 内核层的全局状态、大内核锁和统一的等待机制（设计文档 §6）。
//!
//! 所有 guest 线程共享一个 [`Kernel`]，由一把大内核锁（BKL）保护：处理 syscall
//! 和投递信号时持有，执行 guest 代码时不持有，等待之前释放。内核对象之间用
//! `Rc`/`RefCell` 共享，它们只在持有 BKL 时被访问；唯一的例外是线程自己的
//! [`TaskState`]，只有它所属的线程会访问，等待期间也一直借用着。
//!
//! 等待统一走宿主 `poll()`：每个线程有一个 eventfd，等待时把它和要等的宿主 fd
//! 放在一起 poll。任何可能让别人的等待条件成立的修改都调用 [`request_wake`]，
//! 释放 BKL 时唤醒所有正在等待的线程，由它们各自重新检查条件。

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
use crate::fd::FdTable;
use crate::futex::Futexes;
use crate::mm::Mm;
use crate::process::{ExitStatus, ThreadGroup, Tier};
use crate::task::{Task, TaskState};
use crate::tty::Tty;
use crate::vfs::{Node, Vfs};

/// 线程 ID；线程组 ID（pid）就是组里第一个线程的 tid。
pub(crate) type Tid = i32;

/// 内核层内置的 init（设计文档 §6）：只负责收养孤儿进程、回收僵尸进程。
pub(crate) const INIT: Tid = 1;

/// 最大的 tid，与 Linux 64 位上 `pid_max` 的上限相同。
const PID_MAX: Tid = 4 << 20;

/// 有修改可能让等待中的线程继续，释放 BKL 时唤醒它们。
static WAKE: AtomicBool = AtomicBool::new(false);

/// 请求在释放 BKL 时唤醒所有等待中的线程。调用方持有 BKL。
pub(crate) fn request_wake() {
    WAKE.store(true, Ordering::Relaxed);
}

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
    /// 第一个 guest 进程的结束方式；设置之后整个会话结束。
    pub session: Option<ExitStatus>,
    /// 已退出线程的 Tier 1 统计之和。
    pub dbt_stats: Option<DbtStats>,
    /// 已退出线程用掉的 CPU 时间之和（`/proc/stat`）。
    pub cpu_ns: u64,
    /// 创建过的进程数（`/proc/stat` 的 processes）。
    pub forks: u64,
    /// 本次运行的 `/proc/sys/kernel/random/boot_id`。
    pub boot_id: [u8; 16],
    /// 所有 PTY，按编号；主设备和从设备都关闭之后条目失效。
    pub ptys: BTreeMap<u32, Weak<RefCell<Tty>>>,
    /// 接到宿主终端的那个 PTY 的主设备（[`crate::terminal`]）。
    pub terminal: Option<crate::tty::PtyEnd>,
    next_tid: Tid,
    /// 正在等待的线程的事件。
    waiters: Vec<Arc<Event>>,
}

// SAFETY: Kernel 及其中的 Rc/RefCell 只在持有 BKL（Shared::kernel 这把锁）时访问，
// 见模块文档
unsafe impl Send for Kernel {}

impl Kernel {
    pub fn new(tier: Tier) -> Kernel {
        Kernel {
            tasks: BTreeMap::new(),
            groups: BTreeMap::new(),
            futexes: Futexes::default(),
            tier,
            session: None,
            dbt_stats: None,
            cpu_ns: 0,
            forks: 0,
            boot_id: {
                let mut id = [0u8; 16];
                let _ = appine_hal::fill_random(&mut id);
                id[6] = (id[6] & 0x0f) | 0x40;
                id[8] = (id[8] & 0x3f) | 0x80;
                id
            },
            ptys: BTreeMap::new(),
            terminal: None,
            next_tid: INIT + 1,
            waiters: Vec::new(),
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
}

impl Shared {
    pub fn new(kernel: Kernel) -> Shared {
        Shared {
            kernel: Mutex::new(kernel),
        }
    }

    /// 获取 BKL。内核层的 bug 导致的 panic 会直接终止宿主进程，所以不需要处理中毒。
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
/// 到达 `deadline` 时重新获取 BKL 返回。调用方重新检查自己的条件。
pub(crate) fn block<'a>(
    shared: &'a Shared,
    mut k: Bkl<'a>,
    event: &Arc<Event>,
    deadline: Option<Instant>,
    host: &mut [(i32, i16, i16)],
) -> (Bkl<'a>, Result<(), Errno>) {
    const POLLIN: i16 = 1;
    k.waiters.push(event.clone());
    drop(k);
    let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let mut fds: Vec<(i32, i16, i16)> = host.iter().map(|&(fd, ev, _)| (fd, ev, 0)).collect();
    fds.push((event.fd(), POLLIN, 0));
    let r = appine_hal::poll(&mut fds, timeout);
    let mut k = shared.lock();
    event.clear();
    k.waiters.retain(|e| !Arc::ptr_eq(e, event));
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

    /// procfs、devpts 的视图（路径解析用）。
    pub fn view<'v>(&'v self, vfs: &'v Vfs) -> crate::pseudo::View<'v> {
        crate::pseudo::View {
            k: self.kr(),
            vfs,
            tid: self.tid(),
            tgid: self.tgid(),
        }
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
        let k = self.k.take().expect("BKL is held");
        let (k, r) = block(self.shared, k, &self.task.event, deadline, host);
        self.k = Some(k);
        r
    }

    /// 释放 BKL 执行 `f`（可能阻塞的宿主 I/O）。`f` 不能访问任何内核对象。
    pub fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        drop(self.k.take());
        let r = f();
        self.k = Some(self.shared.lock());
        r
    }
}
