//! syscall 分发（设计文档 §7.5）。
//!
//! 编号和参数布局按 aarch64 Linux（asm-generic）。未实现的 syscall 一律返回
//! `-ENOSYS`，绝不崩溃，并记录一条本地调试日志。

mod fs;
mod mem;
mod misc;

use appine_engine::Cpu;

use crate::debug;
use crate::errno::Errno;
use crate::process::{Process, Signal};

/// syscall 处理完之后，运行循环该怎么继续。
pub(crate) enum Flow {
    /// 返回值已写入 x0，继续执行 guest。
    Continue,
    /// guest 请求退出，参数是退出码。
    Exit(i32),
    /// guest 被信号终止（M1b 还没有信号投递，只有默认动作）。
    Kill(Signal),
}

/// syscall 的返回值：成功时是写回 x0 的值。
type SysResult = Result<u64, Errno>;

/// 单次在 guest 与宿主之间拷贝的最大字节数，避免按 guest 给的长度一次性分配内存。
const IO_CHUNK: usize = 64 * 1024;

/// 路径参数的最大长度（含 NUL），与 Linux 的 `PATH_MAX` 相同。
const PATH_MAX: usize = 4096;

/// 按 aarch64 Linux 的约定处理一次 `svc #0`：x8 是编号，x0–x5 是参数，返回值写回 x0。
pub(crate) fn dispatch(cpu: &mut Cpu, p: &mut Process) -> Flow {
    let state = cpu.state_mut();
    state.orig_x0 = state.x[0];
    let nr = state.x[8];
    let a: [u64; 6] = state.x[..6].try_into().unwrap();

    let result = match nr {
        17 => fs::getcwd(p, a[0], a[1]),
        23 => fs::dup(p, a[0] as i32),
        24 => fs::dup3(p, a[0] as i32, a[1] as i32, a[2] as u32),
        25 => fs::fcntl(p, a[0] as i32, a[1] as u32, a[2]),
        29 => fs::ioctl(p, a[0] as i32, a[1] as u32, a[2]),
        34 => fs::mkdirat(p, a[0] as i32, a[1], a[2] as u32),
        48 => fs::faccessat(p, a[0] as i32, a[1], a[2] as u32, 0),
        49 => fs::chdir(p, a[0]),
        50 => fs::fchdir(p, a[0] as i32),
        56 => fs::openat(p, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        57 => fs::close(p, a[0] as i32),
        61 => fs::getdents64(p, a[0] as i32, a[1], a[2] as usize),
        62 => fs::lseek(p, a[0] as i32, a[1] as i64, a[2] as u32),
        63 => fs::read(p, a[0] as i32, a[1], a[2] as usize),
        64 => fs::write(p, a[0] as i32, a[1], a[2] as usize),
        65 => fs::readv(p, a[0] as i32, a[1], a[2] as usize),
        66 => fs::writev(p, a[0] as i32, a[1], a[2] as usize),
        67 => fs::pread(p, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
        68 => fs::pwrite(p, a[0] as i32, a[1], a[2] as usize, a[3] as i64),
        71 => fs::sendfile(p, a[0] as i32, a[1] as i32, a[2], a[3] as usize),
        73 => fs::ppoll(p, a[0], a[1] as usize, a[2]),
        78 => fs::readlinkat(p, a[0] as i32, a[1], a[2], a[3] as usize),
        79 => fs::newfstatat(p, a[0] as i32, a[1], a[2], a[3] as u32),
        80 => fs::fstat(p, a[0] as i32, a[1]),
        93 | 94 => return Flow::Exit((a[0] & 0xff) as i32),
        96 => misc::set_tid_address(p, a[0]),
        98 => misc::futex(p, a[0], a[1] as u32, a[2] as u32),
        99 => Ok(0), // set_robust_list：单线程进程用不到
        101 => misc::nanosleep(p, a[0], a[1]),
        113 => misc::clock_gettime(p, a[0] as i32, a[1]),
        114 => misc::clock_getres(p, a[0] as i32, a[1]),
        115 => misc::clock_nanosleep(p, a[0] as i32, a[1] as u32, a[2], a[3]),
        124 => Ok(0), // sched_yield
        129 => misc::kill(p, a[0] as i32, a[1] as i32),
        130 => misc::tkill(p, a[0] as i32, a[1] as i32),
        131 => misc::tgkill(p, a[0] as i32, a[1] as i32, a[2] as i32),
        134 => misc::rt_sigaction(p, a[0] as i32, a[1], a[2], a[3] as usize),
        135 => misc::rt_sigprocmask(p, a[0] as u32, a[1], a[2], a[3] as usize),
        158 => Ok(0), // getgroups：root 没有附加组
        160 => misc::uname(p, a[0]),
        163 => misc::prlimit(p, 0, a[0] as u32, 0, a[1]),
        164 => misc::prlimit(p, 0, a[0] as u32, a[1], 0),
        166 => misc::umask(p, a[0] as u32),
        167 => misc::prctl(p, a[0] as i32, a[1]),
        169 => misc::gettimeofday(p, a[0], a[1]),
        172 | 178 => Ok(p.pid as u64), // getpid、gettid
        173 => Ok(1),                  // getppid：父进程是内置的 init
        174..=177 => Ok(0),            // getuid、geteuid、getgid、getegid：root
        179 => misc::sysinfo(p, a[0]),
        214 => Ok(p.mm.brk(a[0])),
        215 => mem::munmap(p, a[0], a[1]),
        216 => Err(Errno::ENOMEM), // mremap：调用方（musl 的 realloc）会退回到复制
        222 => mem::mmap(p, a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5]),
        226 => mem::mprotect(p, a[0], a[1], a[2] as u32),
        233 => mem::madvise(p, a[0], a[1], a[2] as i32),
        261 => misc::prlimit(p, a[0] as i32, a[1] as u32, a[2], a[3]),
        278 => misc::getrandom(p, a[0], a[1] as usize, a[2] as u32),
        439 => fs::faccessat(p, a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        _ => {
            debug::unimplemented_syscall(nr);
            Err(Errno::ENOSYS)
        }
    };
    cpu.state_mut().x[0] = match result {
        Ok(v) => v,
        Err(errno) => errno.to_syscall_return(),
    };
    // 返回用户态之前处理挂起的信号，与 Linux 一样
    match p.next_signal() {
        Some(sig) => Flow::Kill(sig),
        None => Flow::Continue,
    }
}

/// 读取 guest 的路径参数。
fn read_path(p: &Process, addr: u64) -> Result<Vec<u8>, Errno> {
    p.mm.read_cstr(addr, PATH_MAX - 1)
}

/// 写一个整数类型到 guest 内存。
fn put<const N: usize>(p: &Process, addr: u64, bytes: [u8; N]) -> Result<(), Errno> {
    p.mm.write(crate::uaccess::UserSlice::new(addr, N), &bytes)
}

/// 从 guest 内存读一个定长字节数组。
fn get<const N: usize>(p: &Process, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    p.mm.read(crate::uaccess::UserSlice::new(addr, N), &mut b)?;
    Ok(b)
}
