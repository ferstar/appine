//! syscall 分发（设计文档 §7.5）。
//!
//! 编号和参数布局按 aarch64 Linux（asm-generic）。未实现的 syscall 一律返回
//! `-ENOSYS`，绝不崩溃，并记录一条本地调试日志。处理函数在持有 BKL 时运行，
//! 需要等待的会经由 [`Cx::wait`] 暂时释放它。

mod aio;
mod cred;
pub(crate) mod fast;
mod fs;
mod inotify;
mod ipc;
mod lease;
mod mem;
mod misc;
mod mount;
mod mqueue;
mod net;
mod ns;
mod poll;
mod posix_timer;
mod proc;
mod ptrace;
mod sched;
mod sig;
mod timerfd;
mod tty;
mod unix;
mod xattr;

pub(crate) use aio::{AioContext, released as aio_released};
pub(crate) use fs::ENTROPY;
pub(crate) use fs::{create_exclusive, unlink_path, write_at};
pub(crate) use inotify::{Inotify, Opened, Registry};
pub(crate) use lease::Lease;
pub(crate) use mount::collect_namespaces;
pub(crate) use mqueue::{Limits as MqLimits, MessageQueue};
pub(crate) use net::SocketRef;
pub(crate) use poll::{Epoll, ExGate, Fasync, check_exclusive, check_fasync};
pub(crate) use ptrace::encode_regset;
pub(crate) use sched::host_started;
pub(crate) use tty::{hang_up, hang_up_closed};

use std::sync::atomic::Ordering;

use appine_engine::Cpu;

use crate::debug;
use crate::errno::Errno;
use crate::kernel::Cx;
use crate::process::ExitStatus;
use crate::signal;
use crate::uaccess::UserSlice;

/// 处理完一次进入内核之后，运行循环该怎么继续。
pub(crate) enum Flow {
    /// 继续执行 guest。
    Continue,
    /// execve 换了地址空间和程序：刷新翻译缓存后继续。
    Exec,
    /// 线程退出。
    Exit,
}

/// syscall 的返回值：成功时是写回 x0 的值。
type SysResult = Result<u64, Errno>;

/// 单次在 guest 与宿主之间拷贝的最大字节数，避免按 guest 给的长度一次性分配内存。
const IO_CHUNK: usize = 64 * 1024;

/// sendfile、copy_file_range 一次最多拷这么多，与 Linux 的 `MAX_RW_COUNT` 相同。
const MAX_COPY: usize = 0x7fff_f000;

/// 路径参数的最大长度（含 NUL），与 Linux 的 `PATH_MAX` 相同。
const PATH_MAX: usize = 4096;

/// restart_syscall 的编号：没有处理函数的信号打断 `ERESTART_RESTARTBLOCK` 的 syscall 之后，
/// 重新执行的 svc 改成它（[`crate::signal::deliver`]）。
pub(crate) const NR_RESTART_SYSCALL: u64 = 128;

/// 编号 -1：跟踪者在入口把编号改成它时跳过这个系统调用（Linux 的 `NO_SYSCALL`）。
const NO_SYSCALL: u64 = u64::MAX;

/// 按 aarch64 Linux 的约定处理一次 `svc #0`：x8 是编号，x0–x5 是参数，返回值写回 x0。
pub(crate) fn dispatch(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Flow {
    let state = cpu.state_mut();
    state.orig_x0 = state.x[0];
    let mut nr = state.x[8];
    cx.grow_stack_to(state.sp);
    // PTRACE_SYSCALL：入口停下，跟踪者可能改了编号（NT_ARM_SYSTEM_CALL）和参数。编号改成
    // -1 时跳过这个系统调用，x0 是跟踪者设好的返回值；程序自己用 -1 时与 Linux 一样是
    // ENOSYS（crate::ptrace）
    let watched = crate::ptrace::syscall_start(cx, cpu, nr);
    let traced = watched == Some(true);
    let filtered = cx.task.seccomp_on.load(Ordering::Relaxed);
    if (traced || filtered) && nr == NO_SYSCALL {
        cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
    }
    if traced {
        match crate::ptrace::syscall_entry(cx, cpu) {
            Some(n) => nr = n,
            None => return Flow::Exit,
        }
    }
    // seccomp：与 arm64 Linux 一样在入口停住之后检查，跟踪者要跳过的（-1）也检查。不执行
    // 的，x0 已经是返回值（crate::seccomp）
    if filtered {
        match crate::seccomp::check(cx, cpu, nr) {
            crate::seccomp::Verdict::Run(n) => nr = n,
            crate::seccomp::Verdict::Skip => nr = NO_SYSCALL,
            crate::seccomp::Verdict::Exit => return Flow::Exit,
        }
    }
    // 出口停不停与 arm64 Linux 一样看出口时的跟踪状态（syscall_trace_exit 重新读标志）：
    // 跟踪者在系统调用中途停住时（PTRACE_EVENT_SECCOMP，fork、clone 的 PTRACE_EVENT_*）
    // 改用 PTRACE_SYSCALL 继续的，出口也停，入口没停过也一样（strace --seccomp-bpf 这样
    // 拿返回值）。vDSO 里读时钟的存根在 Linux 上不进内核，有 seccomp 时也没有出口的停止
    let exit_stop =
        watched.is_some() || (filtered && !crate::vdso::clock_stub(cx, cpu.state().pc, nr));
    if (traced || filtered) && nr == NO_SYSCALL {
        if !crate::ptrace::syscall_exit(cx, cpu) {
            return Flow::Exit;
        }
        return signal::deliver(cx, cpu, true);
    }
    let a: [u64; 6] = cpu.state().x[..6].try_into().unwrap();
    let started = debug::trace_start();
    use xattr::Target::{Fd, Path};

    let result = match nr {
        // 不返回，或者自己设置全部寄存器。被跟踪时退出之前可能停下来报告
        // （PTRACE_EVENT_EXIT），execve 之后要报告 exec 和系统调用的出口
        93 => {
            let flow = proc::exit(cx, a[0] as i32);
            crate::ptrace::exit(cx, cpu, ExitStatus::Exited(a[0] as i32 & 0xff));
            return flow;
        }
        94 => {
            let flow = proc::exit_group(cx, a[0] as i32);
            crate::ptrace::exit(cx, cpu, ExitStatus::Exited(a[0] as i32 & 0xff));
            return flow;
        }
        139 => return signal::sigreturn(cx, cpu),
        #[cfg(any(debug_assertions, feature = "test-hooks"))]
        crate::bug::TEST_SYSCALL => crate::bug::test_hook(cx, cpu, a[0], a[1]),
        221 => {
            let tid = cx.tid();
            match proc::execve(cx, cpu, a[0], a[1], a[2]) {
                Ok(()) => return exec_done(cx, cpu, tid),
                Err(e) => Err(e),
            }
        }
        281 => {
            let tid = cx.tid();
            match proc::execveat(cx, cpu, a[0] as i32, a[1], (a[2], a[3]), a[4] as u32) {
                Ok(()) => return exec_done(cx, cpu, tid),
                Err(e) => Err(e),
            }
        }
        117 => ptrace::ptrace(cx, a[0] as i64, a[1] as i32, a[2], a[3]),
        277 => crate::seccomp::seccomp(cx, a[0] as u32, a[1] as u32, a[2]),
        270 | 271 => {
            ptrace::process_vm(cx, a[0] as i32, (a[1], a[2]), (a[3], a[4]), a[5], nr == 271)
        }

        5 | 6 => xattr::setxattr(
            cx,
            Path(a[0], nr == 5),
            a[1],
            a[2],
            a[3] as usize,
            a[4] as u32,
        ),
        7 => xattr::setxattr(cx, Fd(a[0] as i32), a[1], a[2], a[3] as usize, a[4] as u32),
        8 | 9 => xattr::getxattr(cx, Path(a[0], nr == 8), a[1], a[2], a[3] as usize),
        0 => aio::io_setup(cx, a[0] as u32, a[1]),
        1 => aio::io_destroy(cx, a[0]),
        2 => aio::io_submit(cx, a[0], a[1] as i64, a[2]),
        3 => aio::io_cancel(cx, a[0], a[1], a[2]),
        4 => aio::io_getevents(cx, a[0], a[1] as i64, a[2] as i64, a[3], a[4], None),
        292 => match aio::sigmask_arg(cx, a[5]) {
            Ok(mask) => aio::io_getevents(cx, a[0], a[1] as i64, a[2] as i64, a[3], a[4], mask),
            Err(e) => Err(e),
        },
        10 => xattr::getxattr(cx, Fd(a[0] as i32), a[1], a[2], a[3] as usize),
        11 | 12 => xattr::listxattr(cx, Path(a[0], nr == 11), a[1], a[2] as usize),
        13 => xattr::listxattr(cx, Fd(a[0] as i32), a[1], a[2] as usize),
        14 | 15 => xattr::removexattr(cx, Path(a[0], nr == 14), a[1]),
        16 => xattr::removexattr(cx, Fd(a[0] as i32), a[1]),
        17 => fs::getcwd(cx, a[0], a[1]),
        19 => poll::eventfd2(cx, a[0] as u32, a[1] as u32),
        20 => poll::epoll_create1(cx, a[0] as u32),
        21 => poll::epoll_ctl(cx, a[0] as i32, a[1] as u32, a[2] as i32, a[3]),
        22 => {
            let deadline = poll::deadline_ms(a[3] as i32);
            poll::epoll_pwait(
                cx,
                a[0] as i32,
                a[1],
                a[2] as i32,
                deadline,
                a[4],
                a[5] as usize,
            )
        }
        23 => fs::dup(cx, a[0] as i32),
        24 => fs::dup3(cx, a[0] as i32, a[1] as i32, a[2] as u32),
        25 => fs::fcntl(cx, a[0] as i32, a[1] as u32, a[2]),
        26 => inotify::inotify_init1(cx, a[0] as u32),
        27 => inotify::inotify_add_watch(cx, a[0] as i32, a[1], a[2] as u32),
        28 => inotify::inotify_rm_watch(cx, a[0] as i32, a[1] as i32),
        29 => fs::ioctl(cx, a[0] as i32, a[1] as u32, a[2]),
        32 => fs::flock(cx, a[0] as i32, a[1] as u32),
        33 => fs::mknodat(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
        34 => fs::mkdirat(cx, a[0] as i32, a[1], a[2] as u32),
        35 => fs::unlinkat(cx, a[0] as i32, a[1], a[2] as u32),
        36 => fs::symlinkat(cx, a[0], a[1] as i32, a[2]),
        37 => fs::linkat(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        38 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], 0),
        39 => mount::umount2(cx, a[0], a[1] as u32),
        40 => mount::mount(cx, a[0], a[1], a[2], a[3], a[4]),
        41 => mount::pivot_root(cx, a[0], a[1]),
        268 => ns::setns(cx, a[0] as i32, a[1] as i32),
        43 => fs::statfs(cx, a[0], a[1]),
        44 => fs::fstatfs(cx, a[0] as i32, a[1]),
        45 => fs::truncate(cx, a[0], a[1] as i64),
        46 => fs::ftruncate(cx, a[0] as i32, a[1] as i64),
        47 => fs::fallocate(cx, a[0] as i32, a[1] as u32, a[2] as i64, a[3] as i64),
        48 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, 0),
        49 => fs::chdir(cx, a[0]),
        50 => fs::fchdir(cx, a[0] as i32),
        51 => fs::chroot(cx, a[0]),
        52 => fs::fchmod(cx, a[0] as i32, a[1] as u32),
        53 => fs::fchmodat(cx, a[0] as i32, a[1], a[2] as u32),
        54 => fs::fchownat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4] as u32),
        55 => fs::fchown(cx, a[0] as i32, a[1] as u32, a[2] as u32),
        56 => fs::openat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        57 => fs::close(cx, a[0] as i32),
        59 => fs::pipe2(cx, a[0], a[1] as u32),
        61 => fs::getdents64(cx, a[0] as i32, a[1], a[2] as usize),
        62 => fs::lseek(cx, a[0] as i32, a[1] as i64, a[2] as u32),
        63 => fs::read(cx, a[0] as i32, a[1], a[2] as usize),
        64 => fs::write(cx, a[0] as i32, a[1], a[2] as usize),
        65 => fs::readv(cx, a[0] as i32, a[1], a[2] as usize),
        66 => fs::writev(cx, a[0] as i32, a[1], a[2] as usize),
        67 => fs::pread(cx, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
        68 => fs::pwrite(cx, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
        // 64 位上 pos_h 不参与偏移（Linux 的 pos_from_hilo）
        69 | 70 => fs::pvectored(
            cx,
            a[0] as i32,
            a[1],
            a[2] as usize,
            a[3] as i64,
            None,
            nr == 70,
        ),
        71 => fs::sendfile(cx, a[0] as i32, a[1] as i32, a[2], a[3] as usize),
        72 => poll::pselect6(cx, a[0] as i32, [a[1], a[2], a[3]], a[4], a[5]),
        73 => poll::ppoll(cx, a[0], a[1] as usize, a[2], a[3], a[4] as usize),
        74 => sig::signalfd4(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32),
        75 => fs::vmsplice(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32),
        76 => fs::splice(
            cx,
            a[0] as i32,
            a[1],
            a[2] as i32,
            a[3],
            a[4] as usize,
            a[5] as u32,
        ),
        77 => fs::tee(cx, a[0] as i32, a[1] as i32, a[2] as usize, a[3] as u32),
        78 => fs::readlinkat(cx, a[0] as i32, a[1], a[2], a[3] as usize),
        79 => fs::newfstatat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        80 => fs::fstat(cx, a[0] as i32, a[1]),
        81 => fs::sync(cx),
        82 | 83 => fs::fsync(cx, a[0] as i32),
        84 => fs::cache_hint(cx, a[0] as i32, a[1] as i64, a[2] as i64, a[3] as u32, 0),
        85 => timerfd::timerfd_create(cx, a[0] as i32, a[1] as u32),
        86 => timerfd::timerfd_settime(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        87 => timerfd::timerfd_gettime(cx, a[0] as i32, a[1]),
        88 => fs::utimensat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        90 => cred::capget(cx, a[0], a[1]),
        91 => cred::capset(cx, a[0], a[1]),
        95 => proc::waitid(cx, a[0] as u32, a[1] as i32, a[2], a[3] as u32, a[4]),
        96 => proc::set_tid_address(cx, a[0]),
        98 => misc::futex(cx, a[0], a[1] as u32, a[2] as u32, a[3], a[4], a[5] as u32),
        92 => misc::personality(cx, a[0] as u32),
        99 => misc::set_robust_list(cx, a[0], a[1]),
        100 => misc::get_robust_list(cx, a[0] as i32, a[1], a[2]),
        101 => misc::nanosleep(cx, a[0], a[1]),
        102 => misc::getitimer(cx, a[0] as i32, a[1]),
        103 => misc::setitimer(cx, a[0] as i32, a[1], a[2]),
        107 => posix_timer::timer_create(cx, a[0] as i32, a[1], a[2]),
        108 => posix_timer::timer_gettime(cx, a[0] as i32, a[1]),
        109 => posix_timer::timer_getoverrun(cx, a[0] as i32),
        110 => posix_timer::timer_settime(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        111 => posix_timer::timer_delete(cx, a[0] as i32),
        112 => misc::clock_settime(cx, a[0] as i32, a[1]),
        113 => misc::clock_gettime(cx, a[0] as i32, a[1]),
        114 => misc::clock_getres(cx, a[0] as i32, a[1]),
        115 => misc::clock_nanosleep(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        118 => sched::sched_setparam(cx, a[0] as i32, a[1]),
        119 => sched::sched_setscheduler(cx, a[0] as i32, a[1] as i32, a[2]),
        120 => sched::sched_getscheduler(cx, a[0] as i32),
        121 => sched::sched_getparam(cx, a[0] as i32, a[1]),
        122 => sched::sched_setaffinity(cx, a[0] as i32, a[1] as usize, a[2]),
        123 => sched::sched_getaffinity(cx, a[0] as i32, a[1] as usize, a[2]),
        124 => Ok(0), // sched_yield
        125 | 126 => sched::sched_priority_range(a[0] as i32, nr == 125),
        127 => sched::sched_rr_get_interval(cx, a[0] as i32, a[1]),
        NR_RESTART_SYSCALL => misc::restart_syscall(cx),
        129 => sig::kill(cx, a[0] as i32, a[1] as i32),
        130 => sig::tkill(cx, a[0] as i32, a[1] as i32),
        131 => sig::tgkill(cx, a[0] as i32, a[1] as i32, a[2] as i32),
        132 => sig::sigaltstack(cx, cpu.state().sp, a[0], a[1]),
        133 => sig::rt_sigsuspend(cx, a[0], a[1] as usize),
        134 => sig::rt_sigaction(cx, a[0] as i32, a[1], a[2], a[3] as usize),
        135 => sig::rt_sigprocmask(cx, a[0] as u32, a[1], a[2], a[3] as usize),
        136 => sig::rt_sigpending(cx, a[0], a[1] as usize),
        137 => sig::rt_sigtimedwait(cx, a[0], a[1], a[2], a[3] as usize),
        138 => sig::sigqueueinfo(cx, a[0] as i32, None, a[1] as i32, a[2]),
        140 => sched::setpriority(cx, a[0] as i32, a[1] as i32, a[2] as i32),
        141 => sched::getpriority(cx, a[0] as i32, a[1] as i32),
        142 => misc::reboot(cx, a[0] as u32, a[1] as u32, a[2] as u32),
        143 => cred::setregid(cx, a[0], a[1]),
        144 => cred::setgid(cx, a[0]),
        145 => cred::setreuid(cx, a[0], a[1]),
        146 => cred::setuid(cx, a[0]),
        147 => cred::setresuid(cx, a[0], a[1], a[2]),
        148 => cred::getres(cx, false, [a[0], a[1], a[2]]),
        149 => cred::setresgid(cx, a[0], a[1], a[2]),
        150 => cred::getres(cx, true, [a[0], a[1], a[2]]),
        151 => cred::setfsuid(cx, a[0]),
        152 => cred::setfsgid(cx, a[0]),
        153 => misc::times(cx, a[0]),
        154 => proc::setpgid(cx, a[0] as i32, a[1] as i32),
        155 => proc::getpgid(cx, a[0] as i32),
        156 => proc::getsid(cx, a[0] as i32),
        157 => proc::setsid(cx),
        158 => cred::getgroups(cx, a[0] as i32, a[1]),
        159 => cred::setgroups(cx, a[0] as i32, a[1]),
        160 => misc::uname(cx, a[0]),
        161 => misc::set_uts_name(cx, a[0], a[1] as i64, false),
        162 => misc::set_uts_name(cx, a[0], a[1] as i64, true),
        163 => misc::prlimit(cx, 0, a[0] as u32, 0, a[1]),
        164 => misc::prlimit(cx, 0, a[0] as u32, a[1], 0),
        165 => misc::getrusage(cx, a[0] as i32, a[1]),
        166 => misc::umask(cx, a[0] as u32),
        167 => misc::prctl(cx, a[0] as i32, [a[1], a[2], a[3], a[4]]),
        169 => misc::gettimeofday(cx, a[0], a[1]),
        170 => Err(Errno::EPERM),    // settimeofday：guest 不能改宿主的时间
        172 => Ok(cx.tgid() as u64), // getpid
        173 => Ok(cx.t.group.borrow().ppid as u64), // getppid
        174 | 175 => cred::getuid(cx, nr == 175),
        176 | 177 => cred::getgid(cx, nr == 177),
        168 => misc::getcpu(cx, a[0], a[1]),
        178 => Ok(cx.tid() as u64), // gettid
        179 => misc::sysinfo(cx, a[0]),
        186 => ipc::msgget(cx, a[0] as i32, a[1] as i32),
        187 => ipc::msgctl(cx, a[0] as i32, a[1] as i32, a[2]),
        188 => ipc::msgrcv(cx, a[0] as i32, a[1], a[2], a[3] as i64, a[4] as i32),
        189 => ipc::msgsnd(cx, a[0] as i32, a[1], a[2], a[3] as i32),
        190 => ipc::semget(cx, a[0] as i32, a[1] as i32, a[2] as i32),
        191 => ipc::semctl(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3]),
        192 => ipc::semtimedop(cx, a[0] as i32, a[1], a[2], a[3]),
        193 => ipc::semtimedop(cx, a[0] as i32, a[1], a[2], 0),
        194 => ipc::shmget(cx, a[0] as i32, a[1], a[2] as i32),
        195 => ipc::shmctl(cx, a[0] as i32, a[1] as i32, a[2]),
        196 => ipc::shmat(cx, a[0] as i32, a[1], a[2] as i32),
        97 => proc::unshare(cx, a[0]),
        180 => mqueue::mq_open(cx, a[0], a[1] as u32, a[2] as u32, a[3]),
        181 => mqueue::mq_unlink(cx, a[0]),
        182 => mqueue::mq_timedsend(cx, a[0] as i32, a[1], a[2] as usize, a[3] as u32, a[4]),
        183 => mqueue::mq_timedreceive(cx, a[0] as i32, a[1], a[2] as usize, a[3], a[4]),
        184 => mqueue::mq_notify(cx, a[0] as i32, a[1]),
        185 => mqueue::mq_getsetattr(cx, a[0] as i32, a[1], a[2]),
        197 => ipc::shmdt(cx, a[0]),
        198 => net::socket(cx, a[0] as i32, a[1] as u32, a[2] as i32),
        199 => net::socketpair(cx, a[0] as i32, a[1] as u32, a[2] as i32, a[3]),
        200 => net::bind(cx, a[0] as i32, a[1], a[2] as u32),
        201 => net::listen(cx, a[0] as i32, a[1] as i32),
        202 => net::accept4(cx, a[0] as i32, a[1], a[2], 0),
        203 => net::connect(cx, a[0] as i32, a[1], a[2] as u32),
        204 => net::getsockname(cx, a[0] as i32, a[1], a[2]),
        205 => net::getpeername(cx, a[0] as i32, a[1], a[2]),
        206 => net::sendto(
            cx,
            a[0] as i32,
            a[1],
            a[2] as usize,
            a[3] as u32,
            a[4],
            a[5] as u32,
        ),
        207 => net::recvfrom(
            cx,
            a[0] as i32,
            a[1],
            a[2] as usize,
            a[3] as u32,
            a[4],
            a[5],
        ),
        208 => net::setsockopt(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4] as u32),
        209 => net::getsockopt(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4]),
        210 => net::shutdown(cx, a[0] as i32, a[1] as i32),
        211 => net::sendmsg(cx, a[0] as i32, a[1], a[2] as u32),
        212 => net::recvmsg(cx, a[0] as i32, a[1], a[2] as u32),
        214 => Ok(cx.mm_mut().brk(a[0])),
        223 => fs::fadvise64(cx, a[0] as i32, a[2] as i64, a[3] as i32),
        213 => fs::cache_hint(cx, a[0] as i32, a[1] as i64, a[2] as i64, 0, 1),
        215 => mem::munmap(cx, a[0], a[1]),
        216 => mem::mremap(cx, a[0], a[1], a[2], a[3] as u32, a[4]),
        220 => proc::clone(cx, cpu, a[0], a[1], a[2], a[3], a[4]),
        222 => mem::mmap(cx, a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5]),
        226 => mem::mprotect(cx, a[0], a[1], a[2] as u32),
        227 => mem::msync(cx, a[0], a[1], a[2] as u32),
        228 => mem::mlock(cx, a[0], a[1], 0, true),
        229 => mem::mlock(cx, a[0], a[1], 0, false),
        230 => mem::mlockall(cx, a[0] as u32),
        231 => mem::munlockall(cx),
        232 => mem::mincore(cx, a[0], a[1], a[2]),
        233 => mem::madvise(cx, a[0], a[1], a[2] as i32),
        234 => mem::remap_file_pages(cx, a[0], a[1], a[2], a[3], a[4] as u32),
        240 => sig::sigqueueinfo(cx, a[0] as i32, Some(a[1] as i32), a[2] as i32, a[3]),
        242 => net::accept4(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        243 => net::recvmmsg(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
        260 => proc::wait4(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
        261 => misc::prlimit(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        267 => fs::cache_hint(cx, a[0] as i32, 0, 0, 0, 2).and_then(|_| fs::sync(cx)),
        264 => fs::name_to_handle_at(cx, a[0] as i32, a[1], a[2], a[3], a[4] as u32),
        265 => fs::open_by_handle_at(cx, a[0] as i32, a[1], a[2] as u32),
        269 => net::sendmmsg(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        272 => proc::kcmp(cx, a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4]),
        274 => sched::sched_setattr(cx, a[0] as i32, a[1], a[2] as u32),
        275 => sched::sched_getattr(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        276 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        278 => misc::getrandom(cx, a[0], a[1] as usize, a[2] as u32),
        283 => misc::membarrier(cx, a[0] as i32, a[1] as u32),
        279 => fs::memfd_create(cx, a[0], a[1] as u32),
        284 => mem::mlock(cx, a[0], a[1], a[2] as u32, true),
        285 => fs::copy_file_range(
            cx,
            a[0] as i32,
            a[1],
            a[2] as i32,
            a[3],
            a[4] as usize,
            a[5] as u32,
        ),
        286 | 287 => fs::pvectored(
            cx,
            a[0] as i32,
            a[1],
            a[2] as usize,
            a[3] as i64,
            Some(a[5] as u32),
            nr == 287,
        ),
        291 => fs::statx(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
        424 => sig::pidfd_send_signal(cx, a[0] as i32, a[1] as i32, a[2], a[3] as u32),
        434 => proc::pidfd_open(cx, a[0] as i32, a[1] as u32),
        435 => proc::clone3(cx, cpu, a[0], a[1] as usize),
        436 => fs::close_range(cx, a[0] as u32, a[1] as u32, a[2] as u32),
        438 => proc::pidfd_getfd(cx, a[0] as i32, a[1] as i32, a[2] as u32),
        439 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        441 => match poll::deadline_ts(cx, a[3]) {
            Ok(deadline) => poll::epoll_pwait(
                cx,
                a[0] as i32,
                a[1],
                a[2] as i32,
                deadline,
                a[4],
                a[5] as usize,
            ),
            Err(e) => Err(e),
        },
        449 => misc::futex_waitv(cx, a[0], a[1] as u32, a[2] as u32, a[3], a[4] as i32),
        451 => fs::cachestat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        _ => {
            debug::unimplemented_syscall(nr);
            Err(Errno::ENOSYS)
        }
    };
    // fd 上的读写返回 EAGAIN，或者 read、write、recvfrom、sendto 没有读写满要求的长度：
    // 程序在这个方向上用完了 EPOLLET 上次报告的事件（poll::drained）。表里是 (参数下标,
    // 方向)：sendfile 写 a[0]、读 a[1]，splice 和 tee 读 a[0]、写 a[2]
    let dirs: &[(usize, usize)] = match nr {
        63 | 65 | 202 | 207 | 212 | 242 | 243 => &[(0, 0)],
        64 | 66 | 206 | 211 | 269 => &[(0, 1)],
        71 => &[(0, 1), (1, 0)],
        76 | 77 => &[(0, 0), (2, 1)],
        _ => &[],
    };
    let short = matches!((nr, &result), (63 | 64 | 206 | 207, Ok(n)) if *n < a[2]);
    if !dirs.is_empty() && (short || result == Err(Errno::EAGAIN)) {
        for &(i, dir) in dirs {
            let file = cx.fds().get(a[i] as i32);
            if let Ok(file) = file {
                poll::drained(cx, &file, dir);
            }
        }
    }
    debug::trace_syscall(started, cx.tid(), nr, &a, &result);
    cpu.state_mut().x[0] = match result {
        Ok(v) => v,
        Err(errno) => errno.to_syscall_return(),
    };
    // 解除了映射或改了权限：其他共享地址空间的线程要先刷新 TLB，才能释放宿主内存
    let sync = cx.task.sync.clone();
    cx.mm_mut().sync(&sync);
    // PTRACE_SYSCALL：出口停下，跟踪者看到返回值；单步执行的 svc 在这里报告
    if exit_stop && !crate::ptrace::syscall_exit(cx, cpu) {
        return Flow::Exit;
    }
    crate::ptrace::syscall_stepped(cx);
    // 返回用户态之前处理挂起的信号和 syscall 重启，与 Linux 一样
    signal::deliver(cx, cpu, true)
}

/// execve 成功了（`old_tid` 是之前的 tid）：被跟踪时报告 exec（`PTRACE_EVENT_EXEC` 或者
/// SIGTRAP）和系统调用的出口，执行新程序的第一条指令之前投递信号。
fn exec_done(cx: &mut Cx<'_>, cpu: &mut Cpu, old_tid: crate::kernel::Tid) -> Flow {
    crate::ptrace::exec_hw_flush(cx, cpu);
    if !crate::ptrace::traced(cx.task) {
        return Flow::Exec;
    }
    if !crate::ptrace::exec(cx, cpu, old_tid) || !crate::ptrace::syscall_exit(cx, cpu) {
        return Flow::Exit;
    }
    match signal::deliver(cx, cpu, false) {
        Flow::Exit => Flow::Exit,
        _ => Flow::Exec,
    }
}

pub(crate) use proc::{interp_label, label_image};

/// 读取 guest 的路径参数。
fn read_path(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    cx.mm().read_cstr(addr, PATH_MAX - 1)
}

/// 写一个整数类型到 guest 内存。
fn put<const N: usize>(cx: &Cx<'_>, addr: u64, bytes: [u8; N]) -> Result<(), Errno> {
    cx.mm().write(UserSlice::new(addr, N), &bytes)
}

/// 从 guest 内存读一个定长字节数组。
fn get<const N: usize>(cx: &Cx<'_>, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    cx.mm().read(UserSlice::new(addr, N), &mut b)?;
    Ok(b)
}

/// 读取 `struct timespec`，秒或纳秒超出范围时返回 EINVAL。
fn read_timespec(cx: &Cx<'_>, addr: u64) -> Result<std::time::Duration, Errno> {
    let b: [u8; 16] = get(cx, addr)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(std::time::Duration::new(sec as u64, nsec as u32))
}

/// 写一对 64 位整数（timespec、timeval、rlimit）。
fn write_pair(cx: &Cx<'_>, addr: u64, a: i64, b: i64) -> Result<(), Errno> {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_le_bytes());
    buf[8..].copy_from_slice(&b.to_le_bytes());
    put(cx, addr, buf)
}
