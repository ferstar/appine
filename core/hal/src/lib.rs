//! 宿主抽象层（设计文档 §3）：把宿主系统的差异挡在内核层之外。
//!
//! 生产宿主是 Android（bionic），开发宿主是 Linux（glibc），两者都是 Linux
//! 内核，所以这里只有一套实现。返回给内核层的错误码是宿主的 errno，在 Linux
//! 宿主上与 guest 的编号一致，可以原样交给 guest。其他宿主（比如 macOS）
//! 只保证能编译，不在支持范围内（设计文档 §13.3）。

use std::io;
use std::ptr::NonNull;
use std::time::Duration;

/// 申请 `len` 字节、清零、可读写的匿名宿主内存。
pub fn map_anonymous(len: usize) -> Result<NonNull<u8>, i32> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
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
    let page = page_size() as u64;
    let aligned = offset & !(page - 1);
    let delta = (offset - aligned) as usize;
    let map_len = (len + delta).div_ceil(page as usize) * page as usize;
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

/// 读取宿主 fd 的元数据。
pub fn fstat(fd: i32) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
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
