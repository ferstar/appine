//! 内存类 syscall。

use std::os::fd::AsRawFd;

use appine_engine::{PAGE_SIZE, prot};

use super::SysResult;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::mm::flags;
use crate::process::Process;

const PROT_MASK: u32 = prot::READ | prot::WRITE | prot::EXEC;
/// `PROT_GROWSDOWN`、`PROT_GROWSUP`：只对栈有意义，按 Linux 的做法忽略。
const PROT_GROWS: u32 = 0x0100_0000 | 0x0200_0000;
/// 映射类型位：`MAP_SHARED`、`MAP_PRIVATE`、`MAP_SHARED_VALIDATE`。
const MAP_TYPE: u32 = 0x0f;
const MAP_SHARED_VALIDATE: u32 = 0x03;
const MADV_DONTNEED: i32 = 4;

pub(super) fn mmap(
    p: &mut Process,
    addr: u64,
    len: u64,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: u64,
) -> SysResult {
    // arm64 的 PROT_BTI、PROT_MTE 也在这里被拒绝：guest 看不到这两项特性
    if prot & !(PROT_MASK | PROT_GROWS) != 0 {
        return Err(Errno::EINVAL);
    }
    let prot = prot & PROT_MASK;
    if !offset.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let kind = match flags & MAP_TYPE {
        MAP_SHARED_VALIDATE => flags::MAP_SHARED,
        k @ (flags::MAP_SHARED | flags::MAP_PRIVATE) => k,
        _ => return Err(Errno::EINVAL),
    };
    let file = if flags & flags::MAP_ANONYMOUS != 0 {
        if kind == flags::MAP_SHARED {
            // 进程间共享的匿名内存随 fork 一起实现；单进程里与私有映射没有区别
            crate::debug::log(format_args!(
                "mmap: MAP_SHARED|MAP_ANONYMOUS treated as private"
            ));
        }
        None
    } else {
        let file = p.fds.get(fd)?;
        let f = file.borrow();
        let FileKind::Regular { file: host, .. } = &f.kind else {
            return Err(if matches!(f.kind, FileKind::Path(_)) {
                Errno::EBADF
            } else {
                Errno::ENODEV
            });
        };
        if !f.readable() {
            return Err(Errno::EACCES);
        }
        let shared_write = kind == flags::MAP_SHARED && prot & prot::WRITE != 0;
        if shared_write && !f.writable() {
            return Err(Errno::EACCES);
        }
        Some((host.as_raw_fd(), offset, f.writable()))
    };
    let kind = if file.is_none() {
        flags::MAP_PRIVATE
    } else {
        kind
    };
    let flags = (flags & !MAP_TYPE) | kind;
    p.mm.mmap(addr, len, prot, flags, file)
}

pub(super) fn munmap(p: &mut Process, addr: u64, len: u64) -> SysResult {
    p.mm.munmap(addr, len).map(|()| 0)
}

pub(super) fn mprotect(p: &mut Process, addr: u64, len: u64, prot: u32) -> SysResult {
    if prot & !(PROT_MASK | PROT_GROWS) != 0 {
        return Err(Errno::EINVAL);
    }
    p.mm.mprotect(addr, len, prot & PROT_MASK).map(|()| 0)
}

pub(super) fn madvise(p: &mut Process, addr: u64, len: u64, advice: i32) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // 其余建议只影响性能，可以忽略；DONTNEED 之后匿名内存必须读回 0
    if advice == MADV_DONTNEED {
        p.mm.discard(addr, len)?;
    }
    Ok(0)
}
