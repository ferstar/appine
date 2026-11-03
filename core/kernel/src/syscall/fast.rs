//! 不持 BKL 的系统调用（kernel/process 的 map）。
//!
//! 并行的 lstat（`git status` 预读索引、构建工具、语言服务器的线程池）原来要排队等
//! BKL，线程越多越慢。这里只处理不改状态、要用的东西都能无锁拿到的常见情况：凭据不必
//! 检查搜索权限、路径从根目录或当前目录开始、整条路径都在仓库的元数据缓存里
//! （[`Cached`]）。做不了时什么也不改，交给持有 BKL 的 [`dispatch`](super::dispatch)，
//! 它顺带把要用的东西填进缓存。
//!
//! 读时钟（`clock_gettime`、`gettimeofday`）也在这里：vDSO 的时间函数只是 svc 存根
//! （§7.2），Go 的 nanotime、JVM 的 `System.nanoTime` 每次都进内核。go build 一次 218
//! 万次，几个线程排队等 BKL 一共等了 2.5 秒。宿主的时钟和线程自己的 CPU 时钟不用内核里
//! 的状态；进程的、别的线程的 CPU 时钟交给持有 BKL 的路径。
//!
//! futex 的等待、唤醒和 nanosleep 也在这里：futex 的等待队列有自己的锁
//! （[`Shared::futexes`]），等待的线程睡在自己的事件上，与持有 BKL 时的等待一样被
//! futex 唤醒、信号和结束线程组叫醒。只处理键就是宿主地址的情况（这一页在线程的页表里
//! 可写、不在可以写的共享文件映射里，[`Mm::futex_key`](crate::mm::Mm::futex_key)）。
//! 不是被 futex 唤醒、也没到期就醒了（多半是来了信号）时，交给持有 BKL 的路径重新做，
//! 由它处理信号和重新开始。
//!
//! 有 seccomp 的线程只有两种系统调用走这里：过滤器对这个编号总是返回 ALLOW 的（按编号缓存，
//! [`crate::seccomp::always_allowed`]），以及 vDSO 里读时钟的（Linux 上不进内核，不检查）。
//! 别的交给持有 BKL 的路径检查。
//!
//! 线程在持有 BKL 时记下自己的根目录、当前目录和凭据（[`Fast`]）。它们可能被别的线程
//! 改掉（`CLONE_FS` 共享的 chdir、线程组共享的凭据），改的一方让
//! [`Shared::fast_changed`] 的计数加一，记下的就作废了。
//!
//! 访问 guest 内存时线程仍然标记着在执行 guest 代码（`TaskSync::in_guest`）：解除映射
//! 的线程要等它退出才释放宿主内存（[`Mm::sync`](crate::mm::Mm::sync)），与 guest 代码
//! 自己访存一样。睡着时不标记，睡醒之后也不再访问 guest 内存。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use appine_engine::{Cpu, PAGE_SIZE, prot};

use super::fs::{
    AT_FDCWD, AT_SYMLINK_NOFOLLOW, ROOT_DEV, encode_stat, encode_statx, newfstatat_flags,
    statx_flags,
};
use super::{PATH_MAX, SysResult};
use crate::errno::Errno;
use crate::futex::{Enqueued, Key};
use crate::kernel::{Shared, Tid};
use crate::task::{Task, TaskState};
use crate::vfs::store::{Ino, Stat};
use crate::vfs::{Cached, Node, ROOT_MOUNT};

/// 线程为不持 BKL 的系统调用记下的状态。
pub(crate) struct Fast {
    /// 记下时的 [`Shared::fast_epoch`]。
    epoch: u64,
    /// 在跟踪系统调用（`APPINE_STRACE`）：都交给持有 BKL 的路径，好打印出来。
    traced: bool,
    /// 不能不持 BKL 解析路径（凭据要检查搜索权限、根目录不在仓库里）时是 None。
    paths: Option<Paths>,
    /// 线程自己的 tid（execve 时可能接过主线程的，那时 [`Shared::fast_epoch`] 也变了）。
    tid: Tid,
    /// 宿主线程的 CPU 时钟和线程开始时它的读数（[`Task::cpu_ns`]）。
    cpu_clock: Option<(i32, u64)>,
    /// 地址空间里可以写的共享文件映射所在的范围（[`crate::mm::Mm::shared_files`]）：那里
    /// futex 的键不是宿主地址。
    shared_files: Arc<RwLock<Vec<(u64, u64)>>>,
    /// vDSO 映射的地址（换地址空间的 execve 让 [`Shared::fast_epoch`] 变了）。
    vdso: u64,
    /// 所在的 time 命名空间的偏移（纳秒）：单调时钟的、boottime 的。setns 换命名空间时
    /// [`Shared::fast_changed`]，有进程进来之后偏移不再变。
    time: [i64; 2],
}

struct Paths {
    vfs: Cached,
    root: Ino,
    /// 当前目录不在开机时的根文件系统里（在 `/proc` 里、bind 挂载里）时是 None。
    cwd: Option<Ino>,
}

impl Fast {
    /// 记下线程现在的状态。持有 BKL 时调用。
    pub(crate) fn new(shared: &Shared, task: &Task, t: &TaskState) -> Fast {
        let epoch = shared.fast_epoch();
        let fs = t.fs.borrow();
        // 只在开机时的根文件系统里解析：别的挂载里 `..` 要经由挂载点
        let paths = match (&t.vfs, &fs.root) {
            (Some(vfs), &Node::Store(ROOT_MOUNT, root)) if t.group.borrow().cred.searches_all() => {
                let vfs = vfs.cached();
                vfs.usable.then(|| Paths {
                    vfs,
                    root,
                    cwd: match fs.cwd {
                        Node::Store(ROOT_MOUNT, ino) => Some(ino),
                        _ => None,
                    },
                })
            }
            _ => None,
        };
        Fast {
            epoch,
            traced: crate::debug::tracing(),
            paths,
            tid: task.tid.get(),
            cpu_clock: task.clock.get().map(|c| (c, task.cpu_base.get().0)),
            shared_files: t.mm.borrow().shared_files(),
            vdso: t.mm.borrow().vdso,
            time: task.ns.borrow().time.offsets_ns(),
        }
    }

    /// 记下的过时了就重新记下。持有 BKL 时调用。
    pub(crate) fn refresh(&mut self, shared: &Shared, task: &Task, t: &TaskState) {
        if shared.fast_epoch() != self.epoch {
            *self = Fast::new(shared, task, t);
        }
    }

    /// 不持 BKL 执行 `cpu` 停在的系统调用。做完了（结果写进了 x0）返回 true；返回 false
    /// 时由调用方持有 BKL 正常处理，这里除了 [`TaskState::fast_sleep`] 什么也没改。
    pub(crate) fn syscall(&self, shared: &Shared, task: &Task, cpu: &mut Cpu) -> bool {
        // 还有没作废的 ic ivau 时要先进内核作废（task::handle）；PTRACE_SYSCALL 时要在
        // 入口和出口停下，单步时系统调用做完要报告（crate::ptrace）；有 seccomp 时要先检查
        // （crate::seccomp）
        if self.traced
            || cpu.state().ivau_page != 0
            || task.syscall_traced.load(Ordering::Relaxed)
            || task.stepping.load(Ordering::Relaxed)
            || !self.seccomp_allows(task, cpu)
        {
            return false;
        }
        crate::bug::trap(crate::bug::Trap::Fast, self.tid);
        let Some(r) = self.dispatch(shared, task, cpu) else {
            return false;
        };
        cpu.state_mut().x[0] = match r {
            Ok(v) => v,
            Err(e) => e.to_syscall_return(),
        };
        true
    }

    /// 有 seccomp 时不检查也能执行的：vDSO 里读时钟的存根（Linux 上它们不进内核），以及过滤器
    /// 对这个编号总是返回 ALLOW 的（[`crate::seccomp::always_allowed`]）。
    fn seccomp_allows(&self, task: &Task, cpu: &Cpu) -> bool {
        if !task.seccomp_on.load(Ordering::Relaxed) {
            return true;
        }
        let s = cpu.state();
        crate::vdso::clock_stub_at(self.vdso, s.pc, s.x[8])
            || crate::seccomp::always_allowed(task, s.x[8])
    }

    fn dispatch(&self, shared: &Shared, task: &Task, cpu: &Cpu) -> Option<SysResult> {
        let s = cpu.state();
        let a = [s.x[0], s.x[1], s.x[2], s.x[3], s.x[4], s.x[5]];
        match s.x[8] {
            98 if !self.in_shared_file(a[0]) => futex(shared, task, cpu, &a, self.time[0]),
            101 => nanosleep(task, cpu, a[0]),
            115 => {
                let clock = a[0] as i32;
                clock_nanosleep(task, cpu, (clock, self.offset(clock)), a[1] as u32, a[2])
            }
            113 => self.clock_gettime(cpu, a[0] as i32, a[1]),
            114 => clock_getres(cpu, a[0] as i32, a[1]),
            169 => gettimeofday(cpu, a[0], a[1]),
            79 | 291 => {
                let p = self.paths.as_ref()?;
                if shared.fast_epoch() != self.epoch {
                    return None;
                }
                match s.x[8] {
                    79 => p.newfstatat(cpu, a[0] as i32, a[1], a[2], a[3] as u32),
                    _ => p.statx(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
                }
            }
            _ => None,
        }
    }

    /// `misc::clock_gettime` 里宿主的时钟和线程自己的 CPU 时钟。
    fn clock_gettime(&self, cpu: &Cpu, clock: i32, tp: u64) -> Option<SysResult> {
        let (sec, nsec) = if self.own_cpu_clock(clock) {
            let (host, base) = self.cpu_clock?;
            let (sec, nsec) = appine_hal::clock_gettime(host).ok()?;
            let ns = (sec as u64 * 1_000_000_000 + nsec as u64).saturating_sub(base);
            ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
        } else if host_clock(clock) {
            super::misc::add_offset(appine_hal::clock_gettime(clock).ok()?, self.offset(clock))
        } else {
            return None;
        };
        write(cpu, tp, &pair(sec, nsec))?;
        Some(Ok(0))
    }

    /// time 命名空间里时钟 `clock` 的偏移（[`crate::nsproxy::TimeNs::offset`]）。
    fn offset(&self, clock: i32) -> i64 {
        match clock {
            1 | 4 | 6 => self.time[0],
            7 | 9 => self.time[1],
            _ => 0,
        }
    }

    /// `addr` 在可以写的共享文件映射里。
    fn in_shared_file(&self, addr: u64) -> bool {
        let ranges = self
            .shared_files
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        ranges.iter().any(|&(s, e)| (s..e).contains(&addr))
    }

    /// `clock` 是线程自己的 CPU 时钟：`CLOCK_THREAD_CPUTIME_ID`，或者
    /// `pthread_getcpuclockid` 编码出来的自己的（`misc::cpu_clock`）。
    fn own_cpu_clock(&self, clock: i32) -> bool {
        clock == CLOCK_THREAD_CPUTIME_ID
            || clock < 0 && clock & 3 != 3 && clock & 4 != 0 && {
                let tid = !(clock >> 3);
                tid == 0 || tid == self.tid
            }
    }
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// 直接用宿主的时钟（`misc::clock_gettime` 里不是 CPU 时钟的）。宿主不认识的交给持有
/// BKL 的路径报错。
fn host_clock(clock: i32) -> bool {
    clock >= 0 && clock != CLOCK_PROCESS_CPUTIME_ID && clock != CLOCK_THREAD_CPUTIME_ID
}

fn clock_getres(cpu: &Cpu, clock: i32, res: u64) -> Option<SysResult> {
    if !host_clock(clock) {
        return None;
    }
    let (sec, nsec) = appine_hal::clock_getres(clock).ok()?;
    if res != 0 {
        write(cpu, res, &pair(sec, nsec))?;
    }
    Some(Ok(0))
}

fn gettimeofday(cpu: &Cpu, tv: u64, tz: u64) -> Option<SysResult> {
    if tv != 0 {
        let (sec, nsec) = appine_hal::clock_gettime(CLOCK_REALTIME).ok()?;
        write(cpu, tv, &pair(sec, nsec / 1000))?;
    }
    if tz != 0 {
        write(cpu, tz, &[0; 8])?;
    }
    Some(Ok(0))
}

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const TIMER_ABSTIME: u32 = 1;

/// `misc::futex` 的等待和唤醒，`a` 是 syscall 的参数。超时在 FUTEX_WAIT 里是相对的，在
/// WAIT_BITSET 里是绝对的。
/// `monotonic` 是 time 命名空间里单调时钟的偏移（WAIT_BITSET 的绝对超时按它换算）。
fn futex(
    shared: &Shared,
    task: &Task,
    cpu: &Cpu,
    a: &[u64; 6],
    monotonic: i64,
) -> Option<SysResult> {
    let (uaddr, op, val, timeout, val3) = (a[0], a[1] as u32, a[2] as u32, a[3], a[5] as u32);
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    let bitset = match cmd {
        FUTEX_WAIT | FUTEX_WAKE => u32::MAX,
        FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET if val3 != 0 => val3,
        _ => return None,
    };
    // 私有内存的键是宿主地址（Mm::futex_key）。借来的写时复制的页要先换成自己的，只读的
    // 映射里的 futex 很少见，都交给持锁的路径
    if !uaddr.is_multiple_of(4) {
        return None;
    }
    let host = cpu.translate(uaddr, prot::WRITE)?.as_ptr().cast::<u32>();
    let key = Key::Host(host as usize);
    if cmd == FUTEX_WAKE || cmd == FUTEX_WAKE_BITSET {
        let woken = shared.futexes().wake(key, val, bitset);
        crate::kernel::flush_signals();
        return Some(Ok(woken as u64));
    }
    let deadline = match timeout {
        0 => None,
        addr => {
            let t = read_timespec(cpu, addr)?;
            Some(if cmd == FUTEX_WAIT {
                Instant::now() + t
            } else {
                let (clock, offset) = if op & FUTEX_CLOCK_REALTIME != 0 {
                    (CLOCK_REALTIME, 0)
                } else {
                    (CLOCK_MONOTONIC, monotonic)
                };
                let t = super::misc::to_host_clock(offset, t);
                super::misc::deadline_from_clock(clock, t).ok()?
            })
        }
    };
    let id = {
        let mut futexes = shared.futexes();
        // SAFETY: host 指向这一页里按 4 字节对齐的 guest 内存；线程标记着在执行 guest 代码，
        // 这一页在此期间不会被释放
        let now = unsafe { AtomicU32::from_ptr(host) }.load(Ordering::SeqCst);
        if now != val {
            return Some(Err(Errno::EAGAIN));
        }
        futexes.enqueue(key, bitset, task.event.clone())
    };
    let waiting = Enqueued::new(shared, [id]);
    crate::bug::trap(crate::bug::Trap::FutexWait, task.tgid);
    let woke = sleep(task, deadline);
    match waiting.leave() {
        Some(_) => Some(Ok(0)),
        // 多半是来了信号：持锁的路径重新检查值、重新登记，处理信号。相对的超时按这里算好的
        // 到期时刻接着等（[`TaskState::fast_sleep`]）
        None if woke => {
            if cmd == FUTEX_WAIT && deadline.is_some() {
                task.state.borrow_mut().fast_sleep = deadline;
            }
            None
        }
        None => Some(Err(Errno::ETIMEDOUT)),
    }
}

fn nanosleep(task: &Task, cpu: &Cpu, req: u64) -> Option<SysResult> {
    let deadline = Instant::now() + read_timespec(cpu, req)?;
    sleep_until(task, deadline, true)
}

/// `misc::clock_nanosleep` 里墙上时钟、单调时钟和启动以来的时钟。`offset` 是 time 命名空间
/// 里这个时钟的偏移（绝对时刻按它换算）。
fn clock_nanosleep(
    task: &Task,
    cpu: &Cpu,
    (clock, offset): (i32, i64),
    flags: u32,
    req: u64,
) -> Option<SysResult> {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME)
        || flags & !TIMER_ABSTIME != 0
    {
        return None;
    }
    let t = read_timespec(cpu, req)?;
    if flags & TIMER_ABSTIME == 0 {
        return sleep_until(task, Instant::now() + t, true);
    }
    // 绝对时刻：持锁的路径按同样的参数重新换算，就是接着睡
    let t = super::misc::to_host_clock(offset, t);
    let deadline = super::misc::deadline_from_clock(clock, t).ok()?;
    sleep_until(task, deadline, false)
}

/// 睡到 `deadline`。被叫醒（多半是来了信号）时交给持锁的路径接着睡，`relative` 时把到期
/// 时刻记在 [`TaskState::fast_sleep`] 里：持锁的路径不重新从现在算，被信号打断时也按它
/// 算剩下多少。
fn sleep_until(task: &Task, deadline: Instant, relative: bool) -> Option<SysResult> {
    if !sleep(task, Some(deadline)) {
        return Some(Ok(0));
    }
    if relative {
        task.state.borrow_mut().fast_sleep = Some(deadline);
    }
    None
}

/// 不持 BKL 睡到 `deadline`（None 是一直睡），或者线程的事件被触发（futex 唤醒、信号、
/// 被结束）。睡着时不算在执行 guest 代码：解除映射的线程不必等它（Mm::sync），回到运行
/// 循环时重新标记。事件被触发（或者宿主信号打断了等待）时返回 true。
fn sleep(task: &Task, deadline: Option<Instant>) -> bool {
    const POLLIN: i16 = 1;
    task.sync.in_guest.store(false, Ordering::SeqCst);
    task.sleeping.store(true, Ordering::Relaxed);
    let woke = loop {
        let now = Instant::now();
        let timeout = match deadline {
            Some(d) if now >= d => break false,
            Some(d) => Some(d - now),
            None => None,
        };
        let mut fds = [(task.event.fd(), POLLIN, 0)];
        match appine_hal::poll(&mut fds, timeout) {
            Ok(0) => {}
            _ => break true,
        }
    };
    task.sleeping.store(false, Ordering::Relaxed);
    if woke {
        task.event.clear();
    }
    woke
}

/// 读 guest 内存里的 `struct timespec`（`super::read_timespec`）。读不了或者不合法时返回
/// None，由持锁的路径报错。
fn read_timespec(cpu: &Cpu, addr: u64) -> Option<Duration> {
    let mut b = [0u8; 16];
    read(cpu, addr, &mut b)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    Some(Duration::new(sec as u64, nsec as u32))
}

/// `struct timespec`、`struct timeval`。
fn pair(a: i64, b: i64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    buf
}

impl Paths {
    fn newfstatat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        buf: u64,
        flags: u32,
    ) -> Option<SysResult> {
        newfstatat_flags(flags).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_stat(&st, ROOT_DEV))?;
        Some(Ok(0))
    }

    fn statx(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        flags: u32,
        mask: u32,
        buf: u64,
    ) -> Option<SysResult> {
        let flags = statx_flags(flags, mask).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_statx(&encode_stat(&st, ROOT_DEV)))?;
        Some(Ok(0))
    }

    /// `fs::stat_at` 里路径不为空、从根目录或当前目录开始的情况。
    fn stat(&self, cpu: &Cpu, dirfd: i32, path: u64, flags: u32) -> Option<Result<Stat, Errno>> {
        let mut buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut buf)?;
        let cwd = match path.first()? {
            b'/' => self.root,
            _ if dirfd == AT_FDCWD => self.cwd?,
            _ => return None,
        };
        self.vfs
            .stat(self.root, cwd, path, flags & AT_SYMLINK_NOFOLLOW == 0)
    }
}

/// 把 guest 内存里以 NUL 结尾的路径读进 `buf`（`syscall::read_path`）。读不了或者太长
/// 时返回 None。
fn read_path<'b>(cpu: &Cpu, addr: u64, buf: &'b mut [u8; PATH_MAX]) -> Option<&'b [u8]> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: 线程标记着在执行 guest 代码，这一页的宿主内存在此期间不会被释放
        let bytes = unsafe { std::slice::from_raw_parts(host.as_ptr(), chunk) };
        if let Some(n) = bytes.iter().position(|&b| b == 0) {
            buf[done..done + n].copy_from_slice(&bytes[..n]);
            return Some(&buf[..done + n]);
        }
        buf[done..done + chunk].copy_from_slice(bytes);
        done += chunk;
    }
    None
}

/// 读 guest 内存，要求每页可读；读不了时返回 None。
fn read(cpu: &Cpu, addr: u64, buf: &mut [u8]) -> Option<()> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: 同 read_path
        unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), buf[done..].as_mut_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}

/// 写 guest 内存，要求每页可写；写不了时返回 None（可能已经写了一部分，正常路径会
/// 重新写）。
fn write(cpu: &Cpu, addr: u64, data: &[u8]) -> Option<()> {
    let mut done = 0;
    while done < data.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(data.len() - done);
        let host = cpu.translate(cur, prot::WRITE)?;
        // SAFETY: 同 read_path
        unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host.as_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}
