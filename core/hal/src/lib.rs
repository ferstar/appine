//! 宿主抽象层（设计文档 §3）：把宿主系统的差异挡在内核层之外。
//!
//! 生产宿主是 Android（bionic），开发宿主是 Linux（glibc），两者都是 Linux
//! 内核，所以这里只有一套实现。返回给内核层的错误码是宿主的 errno，在 Linux
//! 宿主上与 guest 的编号一致，可以原样交给 guest。其他宿主（比如 macOS）
//! 只保证能编译，不在支持范围内（设计文档 §13.3）。

pub mod net;

use std::io;
use std::ptr::NonNull;
use std::time::Duration;

/// 申请 `len` 字节、清零、可读写的匿名宿主内存。`reserve` 为 false 时不计入宿主的
/// 内存提交量（`MAP_NORESERVE`），超量使用时在访问时失败，而不是现在失败。
pub fn map_anonymous(len: usize, reserve: bool) -> Result<NonNull<u8>, i32> {
    let noreserve = if reserve { 0 } else { libc::MAP_NORESERVE };
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | noreserve,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    Ok(NonNull::new(ptr.cast()).expect("mmap returned NULL"))
}

/// 申请 `len` 字节不能访问（`PROT_NONE`）的宿主地址空间：访问它时宿主收到 SIGSEGV。
pub fn map_inaccessible(len: usize) -> Result<NonNull<u8>, i32> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    Ok(NonNull::new(ptr.cast()).expect("mmap returned NULL"))
}

/// 释放 [`map_anonymous`] 申请的内存。
///
/// # Safety
///
/// `ptr` 和 `len` 必须与某次 [`map_anonymous`] 的返回值和参数一致，并且之后
/// 不再访问这段内存。
pub unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
}

/// 丢弃一段私有匿名内存的内容（`MADV_DONTNEED`）：宿主回收这些页，之后读到的是零。
///
/// # Safety
///
/// `ptr` 和 `len` 按宿主页对齐，落在 [`map_anonymous`] 申请、还没有释放的内存里。
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> Result<(), i32> {
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_DONTNEED) } != 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// 向宿主 fd 写入，遇到 `EINTR` 自动重试。
pub fn write(fd: i32, buf: &[u8]) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// 在宿主 fd 的 `offset` 处写入（pwrite），遇到 `EINTR` 自动重试。
pub fn pwrite(fd: i32, buf: &[u8], offset: u64) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// 把宿主文件 `fd_in` 从 `off_in` 开始的最多 `len` 字节拷到 `fd_out` 的 `off_out` 处，
/// 数据不经过用户态，返回拷贝的字节数。用 sendfile：Android 14 之前 App 的 seccomp 不允许
/// copy_file_range（会收到 SIGSYS）。sendfile 写在输出 fd 的位置上，所以先把它移到
/// `off_out`，调用方要保证这期间没有别人用这个 fd 的位置。遇到 `EINTR` 自动重试。
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn copy_file(
    fd_in: i32,
    off_in: u64,
    fd_out: i32,
    off_out: u64,
    len: usize,
) -> Result<usize, i32> {
    if unsafe { libc::lseek(fd_out, off_out as libc::off_t, libc::SEEK_SET) } < 0 {
        return Err(last_errno());
    }
    loop {
        let mut off = off_in as libc::off_t;
        let n = unsafe { libc::sendfile(fd_out, fd_in, &mut off, len) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// 从宿主 fd 的 `offset` 处读取（pread），遇到 `EINTR` 自动重试。
pub fn pread(fd: i32, buf: &mut [u8], offset: u64) -> Result<usize, i32> {
    loop {
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                offset as libc::off_t,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

fn last_errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// 宿主页大小。
pub fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// 一段宿主文件映射：`ptr` 指向请求的文件偏移处，`base`/`len` 是实际映射的范围，
/// 释放时传给 [`unmap`]。
pub struct FileMapping {
    /// 请求的文件偏移在宿主内存中的位置。
    pub ptr: NonNull<u8>,
    /// 实际映射的起点（按宿主页对齐）。
    pub base: NonNull<u8>,
    /// 实际映射的长度。
    pub len: usize,
}

/// 映射宿主文件 `fd` 的 `[offset, offset + len)`。宿主页可能比 guest 的 4K 页大，
/// 所以起点向下对齐到宿主页。`shared` 为 false 时是写时复制的私有映射。
pub fn map_file(
    fd: i32,
    offset: u64,
    len: usize,
    shared: bool,
    writable: bool,
) -> Result<FileMapping, i32> {
    let prot = if !shared || writable {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };
    let flags = if shared {
        libc::MAP_SHARED
    } else {
        libc::MAP_PRIVATE
    };
    map_file_with(fd, offset, len, prot, flags)
}

/// 只读的私有映射：好几个地址空间共用、谁也不能写的（[`map_file`]）。
pub fn map_file_read_only(fd: i32, offset: u64, len: usize) -> Result<FileMapping, i32> {
    map_file_with(fd, offset, len, libc::PROT_READ, libc::MAP_PRIVATE)
}

fn map_file_with(
    fd: i32,
    offset: u64,
    len: usize,
    prot: i32,
    flags: i32,
) -> Result<FileMapping, i32> {
    let page = page_size() as u64;
    let aligned = offset & !(page - 1);
    let delta = (offset - aligned) as usize;
    let map_len = (len + delta).div_ceil(page as usize) * page as usize;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            prot,
            flags,
            fd,
            aligned as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_errno());
    }
    let base = NonNull::new(ptr.cast::<u8>()).expect("mmap returned NULL");
    Ok(FileMapping {
        ptr: unsafe { base.add(delta) },
        base,
        len: map_len,
    })
}

/// 用宿主的随机源填满 `buf`。
pub fn fill_random(buf: &mut [u8]) -> Result<(), i32> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

/// flock(2)：`exclusive` 时取排他锁，否则取共享锁；`wait` 为 false 时拿不到就立即返回
/// `EWOULDBLOCK`。同一个 fd 上已有的锁会被转换。（标准库的 `File::lock` 在 Android 上
/// 没有实现。）
pub fn flock(fd: i32, exclusive: bool, wait: bool) -> Result<(), i32> {
    let mut op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if !wait {
        op |= libc::LOCK_NB;
    }
    loop {
        if unsafe { libc::flock(fd, op) } == 0 {
            return Ok(());
        }
        match last_errno() {
            libc::EINTR => continue,
            e => return Err(e),
        }
    }
}

/// 读取宿主 fd 的元数据。
pub fn fstat(fd: i32) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

/// 打开一个宿主目录，只用来作 [`stat_at`] 的起点（`O_PATH`）。
pub fn open_dir(path: &std::path::Path) -> Result<std::os::fd::OwnedFd, i32> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// 打开目录 `dir` 里名为 `name` 的文件（openat，总是带 `O_CLOEXEC`），新建时权限是
/// `mode`（再去掉 umask）。
pub fn open_at(
    dir: i32,
    name: &std::ffi::CStr,
    flags: i32,
    mode: u32,
) -> Result<std::fs::File, i32> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// utimensat 的时间参数：(秒, 纳秒)，None 表示不改。
fn utime_spec(atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> [libc::timespec; 2] {
    let ts = |t: Option<(i64, i64)>| match t {
        Some((sec, nsec)) => libc::timespec {
            tv_sec: sec as libc::time_t,
            tv_nsec: nsec as _,
        },
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    };
    [ts(atime), ts(mtime)]
}

/// [`set_times_at`]、[`set_times_fd`] 的时间取这个值时，由宿主取现在的时间（UTIME_NOW）。
pub const TIME_NOW: (i64, i64) = (0, libc::UTIME_NOW);

/// 设置目录 `dir` 里名为 `name` 的文件的访问时间和修改时间（utimensat，跟随符号链接），
/// 时间是 (秒, 纳秒)，None 表示不改，[`TIME_NOW`] 表示现在。
pub fn set_times_at(
    dir: i32,
    name: &std::ffi::CStr,
    atime: Option<(i64, i64)>,
    mtime: Option<(i64, i64)>,
) -> Result<(), i32> {
    let times = utime_spec(atime, mtime);
    if unsafe { libc::utimensat(dir, name.as_ptr(), times.as_ptr(), 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// 设置打开的文件 `fd` 的访问时间和修改时间（futimens），参数与 [`set_times_at`] 相同。
pub fn set_times_fd(
    fd: i32,
    atime: Option<(i64, i64)>,
    mtime: Option<(i64, i64)>,
) -> Result<(), i32> {
    let times = utime_spec(atime, mtime);
    if unsafe { libc::futimens(fd, times.as_ptr()) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// 修改目录 `dir` 里名为 `name` 的文件的权限位（fchmodat，跟随符号链接）。
pub fn chmod_at(dir: i32, name: &std::ffi::CStr, mode: u32) -> Result<(), i32> {
    if unsafe { libc::fchmodat(dir, name.as_ptr(), mode as libc::mode_t, 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// 删除目录 `dir` 里名为 `name` 的文件（unlinkat）。
pub fn unlink_at(dir: i32, name: &std::ffi::CStr) -> Result<(), i32> {
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// 读取目录 `dir` 里名为 `name` 的文件的元数据（跟随符号链接）。
pub fn stat_at(dir: i32, name: &std::ffi::CStr) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, 0) } < 0 {
        return Err(last_errno());
    }
    Ok(st)
}

/// 对宿主 fd 发 ioctl，`arg` 作为参数缓冲区（由调用方保证足够大）。
pub fn ioctl(fd: i32, request: u64, arg: &mut [u8]) -> Result<i32, i32> {
    let rc = unsafe { libc::ioctl(fd, request as _, arg.as_mut_ptr()) };
    if rc < 0 { Err(last_errno()) } else { Ok(rc) }
}

/// 从宿主 fd 读取，遇到 `EINTR` 自动重试。
pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let errno = last_errno();
        if errno != libc::EINTR {
            return Err(errno);
        }
    }
}

/// 调整宿主 fd 的文件位置。
pub fn lseek(fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
    let rc = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(rc as u64)
    }
}

/// Linux 的 `CLOCK_BOOTTIME`。
pub const CLOCK_BOOTTIME: i32 = 7;

// time_t、long 在 32 位宿主上是 32 位，这里的转换在 64 位宿主上是恒等的
#[allow(clippy::unnecessary_cast)]
fn timespec_pair(ts: &libc::timespec) -> (i64, i64) {
    (ts.tv_sec as i64, ts.tv_nsec as i64)
}

/// 读取宿主时钟，时钟编号按 Linux 的定义：(秒, 纳秒)。
pub fn clock_gettime(clock: i32) -> Result<(i64, i64), i32> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(clock as libc::clockid_t, &mut ts) } < 0 {
        return Err(last_errno());
    }
    Ok(timespec_pair(&ts))
}

/// 宿主时钟的精度：(秒, 纳秒)。
pub fn clock_getres(clock: i32) -> Result<(i64, i64), i32> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_getres(clock as libc::clockid_t, &mut ts) } < 0 {
        return Err(last_errno());
    }
    Ok(timespec_pair(&ts))
}

/// 把宿主进程的 fd 软限制提高到硬限制，返回新的软限制。所有 guest 进程共用宿主的
/// fd：每个打开的仓库文件、socket 都占一个，默认的 1024 很快就会用完。
#[allow(clippy::unnecessary_cast)] // rlim_t 在 32 位宿主上是 32 位
pub fn raise_fd_limit() -> u64 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } < 0 {
        return 0;
    }
    // macOS 上硬限制可能是无穷大，但 setrlimit 不接受超过 OPEN_MAX（10240）
    #[cfg(target_os = "macos")]
    let hard = rl.rlim_max.min(10240);
    #[cfg(not(target_os = "macos"))]
    let hard = rl.rlim_max;
    if rl.rlim_cur < hard {
        rl.rlim_cur = hard;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) } < 0 {
            let _ = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
        }
    }
    rl.rlim_cur as u64
}

/// 宿主进程的 fd 软限制。
#[allow(clippy::unnecessary_cast)] // rlim_t 在 32 位宿主上是 32 位
pub fn fd_limit() -> u64 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } < 0 {
        return 1024;
    }
    if rl.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        rl.rlim_cur as u64
    }
}

/// 宿主进程用掉的匿名内存（字节），分项：`/proc/self/status` 的 RssAnon、RssShmem、VmSwap，
/// 换出到 swap（Android 的 zram）的也算；文件页可以回收，不算。
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryUsage {
    /// RssAnon：驻留的私有匿名内存。没有 RssAnon 的老内核（4.5 之前）是 VmRSS。
    pub anon: u64,
    /// RssShmem：驻留的共享匿名内存（memfd、`MAP_SHARED|MAP_ANONYMOUS`）。
    pub shmem: u64,
    /// VmSwap：换出去的。
    pub swap: u64,
}

/// 宿主进程的匿名内存（[`MemoryUsage`]）。读不了时返回 None。
pub fn memory_usage() -> Option<MemoryUsage> {
    use std::os::unix::fs::FileExt;
    static STATUS: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    let status = STATUS
        .get_or_init(|| std::fs::File::open("/proc/self/status").ok())
        .as_ref()?;
    // 从头读时内核重新生成整个文件
    let mut buf = [0u8; 4096];
    let n = status.read_at(&mut buf, 0).ok()?;
    let field = |name: &[u8]| -> Option<u64> {
        let line = buf[..n]
            .split(|&b| b == b'\n')
            .find(|l| l.starts_with(name))?;
        let digits = line[name.len()..].trim_ascii().strip_suffix(b"kB")?;
        std::str::from_utf8(digits).ok()?.trim().parse().ok()
    };
    let (anon, shmem) = match field(b"RssAnon:") {
        Some(anon) => (anon, field(b"RssShmem:").unwrap_or(0)),
        None => (field(b"VmRSS:")?, 0),
    };
    Some(MemoryUsage {
        anon: anon * 1024,
        shmem: shmem * 1024,
        swap: field(b"VmSwap:").unwrap_or(0) * 1024,
    })
}

/// 宿主的物理内存总量和空闲量（字节）。
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(clippy::unnecessary_cast)] // unsigned long 在 32 位宿主上是 32 位
pub fn memory_info() -> (u64, u64) {
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } < 0 {
        return (0, 0);
    }
    let unit = info.mem_unit.max(1) as u64;
    (info.totalram as u64 * unit, info.freeram as u64 * unit)
}

/// 宿主的物理内存总量和空闲量（字节）。
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn memory_info() -> (u64, u64) {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    ((pages.max(0) as u64) * page_size() as u64, 0)
}

/// 宿主 `/proc/meminfo` 的几项（字节），guest 的 `/proc/meminfo` 和 sysinfo 照搬它们，与
/// docker 的容器看到的一样。
#[derive(Debug, Clone, Copy, Default)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub swap_cached: u64,
    pub active: u64,
    pub inactive: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub dirty: u64,
    pub anon_pages: u64,
    pub mapped: u64,
    pub shmem: u64,
    pub slab: u64,
    pub sreclaimable: u64,
}

/// 宿主的内存状况：读 `/proc/meminfo`。读不了时（宿主不让读）只有 sysinfo 给得出的几项，
/// 可用量按空闲加缓冲区估计。
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(clippy::unnecessary_cast)] // unsigned long 在 32 位宿主上是 32 位
pub fn host_meminfo() -> MemInfo {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|v| v.trim().strip_suffix("kB"))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(0, |kb| kb * 1024)
        };
        let total = field("MemTotal");
        if total != 0 {
            return MemInfo {
                total,
                free: field("MemFree"),
                available: field("MemAvailable"),
                buffers: field("Buffers"),
                cached: field("Cached"),
                swap_cached: field("SwapCached"),
                active: field("Active"),
                inactive: field("Inactive"),
                swap_total: field("SwapTotal"),
                swap_free: field("SwapFree"),
                dirty: field("Dirty"),
                anon_pages: field("AnonPages"),
                mapped: field("Mapped"),
                shmem: field("Shmem"),
                slab: field("Slab"),
                sreclaimable: field("SReclaimable"),
            };
        }
    }
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } < 0 {
        return MemInfo::default();
    }
    let unit = info.mem_unit.max(1) as u64;
    let free = info.freeram as u64 * unit;
    let buffers = info.bufferram as u64 * unit;
    MemInfo {
        total: info.totalram as u64 * unit,
        free,
        available: free + buffers,
        buffers,
        swap_total: info.totalswap as u64 * unit,
        swap_free: info.freeswap as u64 * unit,
        shmem: info.sharedram as u64 * unit,
        ..MemInfo::default()
    }
}

/// 宿主的内存状况：只知道物理内存的总量。
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn host_meminfo() -> MemInfo {
    MemInfo {
        total: memory_info().0,
        ..MemInfo::default()
    }
}

/// 在宿主 fd 上等待事件：`fds` 的每一项是 (fd, events, revents)，`timeout` 为 None 时一直等。
/// 返回就绪的 fd 数；被宿主信号打断时返回 `EINTR`。
pub fn poll(fds: &mut [(i32, i16, i16)], timeout: Option<Duration>) -> Result<usize, i32> {
    let mut raw: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, events, _)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    let rc = poll_raw(&mut raw, timeout);
    if rc < 0 {
        return Err(last_errno());
    }
    for (slot, r) in fds.iter_mut().zip(&raw) {
        slot.2 = r.revents;
    }
    Ok(rc as usize)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn poll_raw(raw: &mut [libc::pollfd], timeout: Option<Duration>) -> i32 {
    let ts = timeout.map(|t| libc::timespec {
        tv_sec: t.as_secs().min(i32::MAX as u64) as libc::time_t,
        tv_nsec: t.subsec_nanos() as _,
    });
    let ts_ptr = ts
        .as_ref()
        .map_or(std::ptr::null(), |t| t as *const libc::timespec);
    unsafe {
        libc::ppoll(
            raw.as_mut_ptr(),
            raw.len() as libc::nfds_t,
            ts_ptr,
            std::ptr::null(),
        )
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn poll_raw(raw: &mut [libc::pollfd], timeout: Option<Duration>) -> i32 {
    // 向上取整到毫秒，避免把很短的等待变成忙等
    let ms = timeout.map_or(-1, |t| {
        t.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
    });
    unsafe { libc::poll(raw.as_mut_ptr(), raw.len() as libc::nfds_t, ms) }
}

/// 宿主 fd 上新发生的事件：一个边沿触发的宿主 epoll，只关注这一个 fd。宿主 poll 只能
/// 看到 fd 现在就不就绪，看不到已经可读时又来了数据；宿主 epoll 的边沿触发每来一次
/// 数据（每次唤醒）都报告一次，正好是 guest 的 `EPOLLET` 要的。
pub struct Edges {
    epfd: std::os::fd::OwnedFd,
}

impl Edges {
    /// 开始记录 `fd` 上的事件。刚建立时 fd 已经就绪的话，第一次 [`Edges::take`] 就会报告。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn new(fd: i32) -> Result<Edges, i32> {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(last_errno());
        }
        let epfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: 0,
        };
        if unsafe { libc::epoll_ctl(raw, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(last_errno());
        }
        Ok(Edges { epfd })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn new(_fd: i32) -> Result<Edges, i32> {
        Err(libc::ENOSYS)
    }

    /// 宿主 epoll 本身的 fd：有新事件时可读，可以放进 [`poll`] 里等。
    pub fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.epfd.as_raw_fd()
    }

    /// 取走上次以来发生的事件：(读方向, 写方向) 有没有新的。出错、挂断算读方向。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn take(&self) -> (bool, bool) {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        if unsafe { libc::epoll_wait(self.fd(), &mut ev, 1, 0) } != 1 {
            return (false, false);
        }
        let out = (libc::EPOLLOUT | libc::EPOLLWRNORM | libc::EPOLLWRBAND) as u32;
        (ev.events & !out != 0, ev.events & out != 0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn take(&self) -> (bool, bool) {
        (false, false)
    }
}

/// 当前线程的 CPU 时间时钟，其他线程也可以用 [`clock_gettime`] 读它（线程退出后失效）。
pub fn thread_cpu_clock() -> Option<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut id: libc::clockid_t = 0;
        (unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut id) } == 0).then_some(id)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// 宿主 CPU `cpu` 的 sysfs 目录（`/sys/devices/system/cpu/cpu<n>`）下的 `path`：是文件时
/// 返回内容，是目录时返回空的内容；没有或者读不了（Android 的 SELinux 可能不让 App 读
/// cpufreq）时返回 None。
pub fn cpu_sysfs(cpu: usize, path: &str) -> Option<Vec<u8>> {
    let full = format!("/sys/devices/system/cpu/cpu{cpu}/{path}");
    let meta = std::fs::metadata(&full).ok()?;
    if meta.is_dir() {
        return Some(Vec::new());
    }
    std::fs::read(&full).ok()
}

/// 本进程可以用的宿主 CPU 的编号（最多 64 个），第一次调用时的快照：guest 看到的 CPU
/// `i` 是其中第 `i` 个。之后宿主线程的亲和性被改了（guest 的 sched_setaffinity）也不
/// 影响它。
pub fn host_cpus() -> &'static [usize] {
    static CPUS: std::sync::OnceLock<Vec<usize>> = std::sync::OnceLock::new();
    CPUS.get_or_init(|| {
        // 也考虑了 cgroup 的 CPU 配额
        let n = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(64);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                // CPU_SETSIZE 在 Linux 上是 i32，在 Android 上是 usize
                #[allow(clippy::unnecessary_cast)]
                let cpus: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
                    .filter(|&i| libc::CPU_ISSET(i, &set))
                    .take(n)
                    .collect();
                if !cpus.is_empty() {
                    return cpus;
                }
            }
        }
        (0..n).collect()
    })
}

/// 把本进程的宿主线程 `tid` 限制在 [`host_cpus`] 里由 `mask` 选出的 CPU 上（第 `i` 位是
/// 第 `i` 个）。失败时保持原样。
pub fn set_thread_affinity(tid: i32, mask: u64) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for (i, &cpu) in host_cpus().iter().enumerate() {
            if mask & (1 << i) != 0 {
                libc::CPU_SET(cpu, &mut set);
            }
        }
        libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = (tid, mask);
}

/// 设置本进程的宿主线程 `tid` 的 nice 值。失败时（没有权限提高优先级）保持原样。
pub fn set_thread_nice(tid: i32, nice: i32) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        // Linux 上 PRIO_PROCESS 作用于单个线程
        libc::setpriority(libc::PRIO_PROCESS as _, tid as libc::id_t, nice);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = (tid, nice);
}

/// 当前宿主线程的 tid。
pub fn gettid() -> i32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        0
    }
}

/// 调用线程到现在在宿主内核里花的时间（`getrusage(RUSAGE_THREAD)` 的 stime）。
pub fn thread_system_ns_self() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Linux 的 UAPI 常量；libc crate 在 Android 上没有定义它
        const RUSAGE_THREAD: libc::c_int = 1;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(RUSAGE_THREAD, &mut ru) } == 0 {
            let t = ru.ru_stime;
            return t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
        }
        0
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        0
    }
}

/// 设置调用线程的名字（`/proc/self/task/<tid>/comm`，调试器里看到的），超过 15 字节的
/// 部分截掉。
pub fn set_thread_name(name: &str) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut buf = [0u8; 16];
        let n = name.len().min(15);
        buf[..n].copy_from_slice(&name.as_bytes()[..n]);
        unsafe { libc::prctl(libc::PR_SET_NAME, buf.as_ptr()) };
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = name;
}

/// 本进程的宿主线程 `tid` 在宿主内核里花的时间（`/proc/self/task/<tid>/stat` 的
/// stime，精度是一个时钟滴答）。线程已经退出或者读不了时返回 None。
pub fn thread_system_ns(tid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
    // 第 2 列是括号里的线程名，可能含空格；之后从第 3 列（状态）数起，stime 是第 15 列
    let rest = &stat[stat.rfind(')')? + 1..];
    let ticks: u64 = rest.split_ascii_whitespace().nth(12)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(ticks * (1_000_000_000 / hz))
}

/// 本进程的宿主线程 `tid` 自愿、非自愿的上下文切换次数（`/proc/self/task/<tid>/status`）。
pub fn thread_ctxt_switches(tid: i32) -> Option<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
    };
    Some((
        field("voluntary_ctxt_switches:")?,
        field("nonvoluntary_ctxt_switches:")?,
    ))
}

/// 把 `[ptr, ptr + len)` 的页调进内存：每个宿主页读一个字节。匿名内存没写过的页映射到
/// 零页，不额外占内存。
///
/// # Safety
///
/// `[ptr, ptr + len)` 是可读的内存，读它不会产生 SIGBUS（文件映射不能超出文件末尾）。
pub unsafe fn populate(ptr: *const u8, len: usize) {
    let page = page_size();
    let mut at = ptr as usize & !(page - 1);
    let end = ptr as usize + len;
    while at < end {
        // SAFETY: 由调用方保证；读的是 at 和 ptr 中较大的那个，在区间里
        unsafe { std::ptr::read_volatile(at.max(ptr as usize) as *const u8) };
        at += page;
    }
}

/// 宿主文件 `fd` 在 `[off, off + len)`（`len` 为 0 时到文件末尾）里的页缓存状况，按 4K
/// 页计（`struct cachestat` 的五项）。在缓存里的页数用 mincore 查：文件映射的 mincore
/// 查的就是页缓存，不依赖宿主内核的版本和文件系统（宿主的 cachestat 要 6.5，在
/// overlayfs 上也数不到底层文件的页）。脏页、写回、逐出几项宿主不给，是 0。
pub fn cachestat(fd: i32, off: u64, len: u64) -> Result<[u64; 5], i32> {
    let size = fstat(fd)?.st_size as u64;
    // 范围里、文件末尾之内的 4K 页
    let first = off / 4096;
    let last = match len {
        0 => u64::MAX,
        n => off.saturating_add(n - 1) / 4096,
    }
    .min(size.saturating_sub(1) / 4096);
    let pages = if size == 0 || last < first {
        0
    } else {
        last - first + 1
    };
    Ok([resident_in_file(fd, first, pages), 0, 0, 0, 0])
}

/// 文件 `fd` 从第 `first` 个 4K 页起的 `pages` 页里有几页在宿主的页缓存里。分段映射，
/// 大文件也不占太多地址空间；映射不了（比如只写打开的）的部分当作不在。
fn resident_in_file(fd: i32, first: u64, pages: u64) -> u64 {
    const WINDOW: u64 = 1 << 16;
    let mut total = 0;
    let mut at = first;
    while at < first + pages {
        let n = (first + pages - at).min(WINDOW);
        let span = (n * 4096) as usize;
        let Ok(m) = map_file(fd, at * 4096, span, true, false) else {
            break;
        };
        total += resident_pages(m.ptr.as_ptr(), span) as u64;
        unsafe { unmap(m.base, m.len) };
        at += n;
    }
    total
}

/// `[ptr, ptr + len)` 里驻留在内存中的 4K 页数（mincore）。
pub fn resident_pages(ptr: *const u8, len: usize) -> usize {
    let page = page_size();
    let start = ptr as usize & !(page - 1);
    let span = (ptr as usize + len).next_multiple_of(page) - start;
    let mut vec = vec![0u8; span / page];
    let rc = unsafe { libc::mincore(start as *mut libc::c_void, span, vec.as_mut_ptr().cast()) };
    if rc < 0 {
        return 0;
    }
    let host_pages = vec.iter().filter(|&&b| b & 1 != 0).count();
    (host_pages * page / 4096).min(len.div_ceil(4096))
}

/// `[ptr, ptr + len)` 里每个 4K 页是否在宿主内存里（mincore）；宿主页更大时，同一个
/// 宿主页里的 4K 页结果相同。出错时全部报告为不在。
pub fn page_residency(ptr: *const u8, len: usize) -> Vec<bool> {
    let page = page_size();
    let start = ptr as usize & !(page - 1);
    let span = (ptr as usize + len).next_multiple_of(page) - start;
    let mut vec = vec![0u8; span / page];
    let rc = unsafe { libc::mincore(start as *mut libc::c_void, span, vec.as_mut_ptr().cast()) };
    (0..len.div_ceil(4096))
        .map(|i| rc == 0 && vec[(ptr as usize + i * 4096 - start) / page] & 1 != 0)
        .collect()
}

/// `[ptr, ptr + len)` 里每个 4K 页有没有被碰过（`/proc/self/pagemap` 的"在内存里"或
/// "已换出"位）；宿主页更大时，同一个宿主页里的 4K 页结果相同。读不了 pagemap 时
/// 返回 None。不能用 mincore：换出到 swap（Android 的 zram）的页它也报告"不在"。
pub fn touched_pages(ptr: *const u8, len: usize) -> Option<Vec<bool>> {
    use std::os::unix::fs::FileExt;
    static PAGEMAP: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    let pagemap = PAGEMAP
        .get_or_init(|| std::fs::File::open("/proc/self/pagemap").ok())
        .as_ref()?;
    let page = page_size();
    let first = ptr as usize / page;
    let last = (ptr as usize + len).div_ceil(page);
    let mut raw = vec![0u8; (last - first) * 8];
    pagemap.read_exact_at(&mut raw, first as u64 * 8).ok()?;
    let touched = |host: usize| {
        let i = (host - first) * 8;
        let entry = u64::from_le_bytes(raw[i..i + 8].try_into().unwrap());
        entry & (3 << 62) != 0
    };
    Some(
        (0..len.div_ceil(4096))
            .map(|i| touched((ptr as usize + i * 4096) / page))
            .collect(),
    )
}

/// 一个宿主文件系统的容量（statvfs）。
#[derive(Debug, Clone, Copy, Default)]
pub struct FsStats {
    /// 块大小。
    pub block_size: u64,
    /// 总块数。
    pub blocks: u64,
    /// 空闲块数。
    pub free: u64,
    /// 非特权用户可用的块数。
    pub available: u64,
    /// inode 总数。
    pub files: u64,
    /// 空闲 inode 数。
    pub files_free: u64,
}

/// 读取 `path` 所在宿主文件系统的容量。
#[allow(clippy::unnecessary_cast)] // fsblkcnt_t 等类型在不同宿主上宽度不同
pub fn statvfs(path: &std::path::Path) -> Result<FsStats, i32> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } < 0 {
        return Err(last_errno());
    }
    let frsize = if st.f_frsize != 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    Ok(FsStats {
        block_size: frsize,
        blocks: st.f_blocks as u64,
        free: st.f_bfree as u64,
        available: st.f_bavail as u64,
        files: st.f_files as u64,
        files_free: st.f_ffree as u64,
    })
}

/// 读空一个非阻塞 fd 里现有的数据（通知用的管道）。
pub fn drain(fd: i32) {
    let mut buf = [0u8; 64];
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// fd 是不是终端。
pub fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// 把宿主终端切到 raw 模式，析构时恢复原来的设置。
pub struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    /// 切换 `fd` 所在的终端：关掉回显、行编辑和信号，让每个按键原样到达。
    pub fn enter(fd: i32) -> Result<RawMode, i32> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } < 0 {
            return Err(last_errno());
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } < 0 {
            return Err(last_errno());
        }
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
    }
}

static WINCH_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_sigwinch(_: libc::c_int) {
    let fd = WINCH_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // 只调用 async-signal-safe 的 write；管道满了说明已经有通知在等着
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

/// 宿主收到 SIGWINCH（终端窗口大小改变）时变为可读的 fd（非阻塞），只能调用一次。
pub fn sigwinch_notifier() -> Result<i32, i32> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(last_errno());
    }
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    WINCH_FD.store(fds[1], std::sync::atomic::Ordering::Relaxed);
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as usize;
    sa.sa_flags = libc::SA_RESTART;
    if unsafe { libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut()) } < 0 {
        return Err(last_errno());
    }
    Ok(fds[0])
}

/// 跨线程唤醒用的事件：Linux 上是 eventfd，其他宿主上用管道代替。
///
/// [`Event::signal`] 之后，它的 fd 变为可读，直到 [`Event::clear`]。
pub struct Event {
    read: i32,
    write: i32,
}

impl Event {
    /// 新建一个未触发的事件。
    pub fn new() -> Result<Event, i32> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if fd < 0 {
                return Err(last_errno());
            }
            Ok(Event {
                read: fd,
                write: fd,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
                return Err(last_errno());
            }
            for fd in fds {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                }
            }
            Ok(Event {
                read: fds[0],
                write: fds[1],
            })
        }
    }

    /// 用来 poll 的 fd（等待 `POLLIN`）。
    pub fn fd(&self) -> i32 {
        self.read
    }

    /// 触发事件，可以从任意线程调用。
    pub fn signal(&self) {
        let one = 1u64.to_ne_bytes();
        // 计数器已满（eventfd）或管道已满时写入失败，但事件本来就处于触发状态
        unsafe { libc::write(self.write, one.as_ptr().cast(), one.len()) };
    }

    /// 事件是否处于触发状态（不清除它）。
    pub fn is_signaled(&self) -> bool {
        const POLLIN: i16 = 1;
        let mut fds = [(self.read, POLLIN, 0)];
        matches!(poll(&mut fds, Some(Duration::ZERO)), Ok(n) if n > 0)
    }

    /// 清除触发状态。
    pub fn clear(&self) {
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.read, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.read);
            if self.write != self.read {
                libc::close(self.write);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    #[test]
    fn checking_an_event_does_not_clear_it() {
        let e = Event::new().unwrap();
        assert!(!e.is_signaled());
        e.signal();
        assert!(e.is_signaled());
        assert!(e.is_signaled());
        e.clear();
        assert!(!e.is_signaled());
    }

    #[test]
    fn counts_written_pages_as_cached() {
        let path = std::env::temp_dir().join(format!("appine-cachestat-{}", std::process::id()));
        let mut f = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&[7u8; 3 * 4096 + 100]).unwrap();
        let fd = f.as_raw_fd();
        // 刚写的页都在页缓存里；范围超出文件末尾的部分不算
        assert_eq!(cachestat(fd, 0, 0).unwrap()[0], 4);
        assert_eq!(cachestat(fd, 4096, 4096).unwrap()[0], 1);
        assert_eq!(cachestat(fd, 1 << 20, 0).unwrap(), [0; 5]);
        std::fs::remove_file(path).unwrap();
    }
}
