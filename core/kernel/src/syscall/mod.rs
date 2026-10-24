//! syscall 分发（设计文档 §7.5）。
//!
//! 编号和参数布局按 aarch64 Linux（asm-generic）。未实现的 syscall 一律返回
//! `-ENOSYS`，绝不崩溃，并记录一条本地调试日志。处理函数在持有 BKL 时运行，
//! 需要等待的会经由 [`Cx::wait`] 暂时释放它。

mod fs;
mod mem;
mod misc;
mod proc;
mod sig;
mod tty;

use appine_engine::Cpu;

use crate::debug;
use crate::errno::Errno;
use crate::kernel::Cx;
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

/// 路径参数的最大长度（含 NUL），与 Linux 的 `PATH_MAX` 相同。
const PATH_MAX: usize = 4096;

/// 按 aarch64 Linux 的约定处理一次 `svc #0`：x8 是编号，x0–x5 是参数，返回值写回 x0。
pub(crate) fn dispatch(cx: &mut Cx<'_>, cpu: &mut Cpu) -> Flow {
    let state = cpu.state_mut();
    state.orig_x0 = state.x[0];
    let nr = state.x[8];
    let a: [u64; 6] = state.x[..6].try_into().unwrap();

    let result = match nr {
        // 不返回，或者自己设置全部寄存器
        93 => return proc::exit(cx, a[0] as i32),
        94 => return proc::exit_group(cx, a[0] as i32),
        139 => return signal::sigreturn(cx, cpu),
        221 => match proc::execve(cx, cpu, a[0], a[1], a[2]) {
            Ok(()) => return Flow::Exec,
            Err(e) => Err(e),
        },

        17 => fs::getcwd(cx, a[0], a[1]),
        23 => fs::dup(cx, a[0] as i32),
        24 => fs::dup3(cx, a[0] as i32, a[1] as i32, a[2] as u32),
        25 => fs::fcntl(cx, a[0] as i32, a[1] as u32, a[2]),
        29 => fs::ioctl(cx, a[0] as i32, a[1] as u32, a[2]),
        33 => fs::mknodat(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
        34 => fs::mkdirat(cx, a[0] as i32, a[1], a[2] as u32),
        35 => fs::unlinkat(cx, a[0] as i32, a[1], a[2] as u32),
        36 => fs::symlinkat(cx, a[0], a[1] as i32, a[2]),
        37 => fs::linkat(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        38 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], 0),
        43 => fs::statfs(cx, a[0], a[1]),
        44 => fs::fstatfs(cx, a[0] as i32, a[1]),
        45 => fs::truncate(cx, a[0], a[1] as i64),
        46 => fs::ftruncate(cx, a[0] as i32, a[1] as i64),
        48 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, 0),
        49 => fs::chdir(cx, a[0]),
        50 => fs::fchdir(cx, a[0] as i32),
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
        71 => fs::sendfile(cx, a[0] as i32, a[1] as i32, a[2], a[3] as usize),
        73 => fs::ppoll(cx, a[0], a[1] as usize, a[2], a[3], a[4] as usize),
        78 => fs::readlinkat(cx, a[0] as i32, a[1], a[2], a[3] as usize),
        79 => fs::newfstatat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        80 => fs::fstat(cx, a[0] as i32, a[1]),
        81 => Ok(0), // sync：仓库的修改已经写进宿主文件
        82 | 83 => fs::fsync(cx, a[0] as i32),
        88 => fs::utimensat(cx, a[0] as i32, a[1], a[2], a[3] as u32),
        95 => proc::waitid(cx, a[0] as u32, a[1] as i32, a[2], a[3] as u32, a[4]),
        96 => proc::set_tid_address(cx, a[0]),
        98 => misc::futex(cx, a[0], a[1] as u32, a[2] as u32, a[3], a[4], a[5] as u32),
        99 => Ok(0), // set_robust_list：robust mutex 的持有者退出时不做处理
        101 => misc::nanosleep(cx, a[0], a[1]),
        113 => misc::clock_gettime(cx, a[0] as i32, a[1]),
        114 => misc::clock_getres(cx, a[0] as i32, a[1]),
        115 => misc::clock_nanosleep(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        123 => misc::sched_getaffinity(cx, a[0] as i32, a[1] as usize, a[2]),
        124 => Ok(0), // sched_yield
        129 => sig::kill(cx, a[0] as i32, a[1] as i32),
        130 => sig::tkill(cx, a[0] as i32, a[1] as i32),
        131 => sig::tgkill(cx, a[0] as i32, a[1] as i32, a[2] as i32),
        132 => sig::sigaltstack(cx, cpu.state().sp, a[0], a[1]),
        133 => sig::rt_sigsuspend(cx, a[0], a[1] as usize),
        134 => sig::rt_sigaction(cx, a[0] as i32, a[1], a[2], a[3] as usize),
        135 => sig::rt_sigprocmask(cx, a[0] as u32, a[1], a[2], a[3] as usize),
        136 => sig::rt_sigpending(cx, a[0], a[1] as usize),
        153 => misc::times(cx, a[0]),
        154 => proc::setpgid(cx, a[0] as i32, a[1] as i32),
        155 => proc::getpgid(cx, a[0] as i32),
        156 => proc::getsid(cx, a[0] as i32),
        157 => proc::setsid(cx),
        158 => Ok(0), // getgroups：root 没有附加组
        160 => misc::uname(cx, a[0]),
        163 => misc::prlimit(cx, 0, a[0] as u32, 0, a[1]),
        164 => misc::prlimit(cx, 0, a[0] as u32, a[1], 0),
        165 => misc::getrusage(cx, a[0] as i32, a[1]),
        166 => misc::umask(cx, a[0] as u32),
        167 => misc::prctl(cx, a[0] as i32, a[1]),
        169 => misc::gettimeofday(cx, a[0], a[1]),
        172 => Ok(cx.tgid() as u64),                // getpid
        173 => Ok(cx.t.group.borrow().ppid as u64), // getppid
        174..=177 => Ok(0),                         // getuid、geteuid、getgid、getegid：root
        178 => Ok(cx.tid() as u64),                 // gettid
        179 => misc::sysinfo(cx, a[0]),
        214 => Ok(cx.mm_mut().brk(a[0])),
        215 => mem::munmap(cx, a[0], a[1]),
        216 => Err(Errno::ENOMEM), // mremap：调用方（musl 的 realloc）会退回到复制
        220 => proc::clone(cx, cpu, a[0], a[1], a[2], a[3], a[4]),
        222 => mem::mmap(cx, a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5]),
        226 => mem::mprotect(cx, a[0], a[1], a[2] as u32),
        227 => Ok(0), // msync：共享映射直接就是宿主的文件映射
        233 => mem::madvise(cx, a[0], a[1], a[2] as i32),
        260 => proc::wait4(cx, a[0] as i32, a[1], a[2] as u32, a[3]),
        261 => misc::prlimit(cx, a[0] as i32, a[1] as u32, a[2], a[3]),
        276 => fs::renameat2(cx, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        278 => misc::getrandom(cx, a[0], a[1] as usize, a[2] as u32),
        436 => fs::close_range(cx, a[0] as u32, a[1] as u32, a[2] as u32),
        439 => fs::faccessat(cx, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        _ => {
            debug::unimplemented_syscall(nr);
            Err(Errno::ENOSYS)
        }
    };
    cpu.state_mut().x[0] = match result {
        Ok(v) => v,
        Err(errno) => errno.to_syscall_return(),
    };
    // 解除了映射或改了权限：其他共享地址空间的线程要先刷新 TLB，才能释放宿主内存
    let sync = cx.task.sync.clone();
    cx.mm_mut().sync(&sync);
    // 返回用户态之前处理挂起的信号和 syscall 重启，与 Linux 一样
    signal::deliver(cx, cpu, true)
}

pub(crate) use proc::label_image;

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
