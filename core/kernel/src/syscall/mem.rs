//! 内存类 syscall。

use std::os::fd::AsRawFd;

use appine_engine::{PAGE_SIZE, prot};

use super::SysResult;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::Cx;
use crate::mm::flags;

const PROT_MASK: u32 = prot::READ | prot::WRITE | prot::EXEC;
/// `PROT_GROWSDOWN`、`PROT_GROWSUP`：只对栈有意义，按 Linux 的做法忽略。
const PROT_GROWS: u32 = 0x0100_0000 | 0x0200_0000;
/// 映射类型位：`MAP_SHARED`、`MAP_PRIVATE`、`MAP_SHARED_VALIDATE`。
const MAP_TYPE: u32 = 0x0f;
const MAP_SHARED_VALIDATE: u32 = 0x03;
const MADV_DONTNEED: i32 = 4;
const MCL_CURRENT: u32 = 1;
const MCL_FUTURE: u32 = 2;
const MCL_ONFAULT: u32 = 4;

pub(super) fn mmap(
    cx: &mut Cx<'_>,
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
        None
    } else {
        let file = cx.fds().get(fd)?;
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
    let flags = (flags & !MAP_TYPE) | kind;
    cx.mm_mut().mmap(addr, len, prot, flags, file)
}

pub(super) fn munmap(cx: &mut Cx<'_>, addr: u64, len: u64) -> SysResult {
    cx.mm_mut().munmap(addr, len).map(|()| 0)
}

pub(super) fn mprotect(cx: &mut Cx<'_>, addr: u64, len: u64, prot: u32) -> SysResult {
    if prot & !(PROT_MASK | PROT_GROWS) != 0 {
        return Err(Errno::EINVAL);
    }
    cx.mm_mut()
        .mprotect(addr, len, prot & PROT_MASK)
        .map(|()| 0)
}

pub(super) fn madvise(cx: &mut Cx<'_>, addr: u64, len: u64, advice: i32) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // 其余建议只影响性能，可以忽略；DONTNEED 之后匿名内存必须读回 0
    if advice == MADV_DONTNEED {
        cx.mm_mut().discard(addr, len)?;
    }
    Ok(0)
}

/// 每一页都已映射时返回页数，否则 ENOMEM（mlock、mincore）。
fn mapped_pages(cx: &Cx<'_>, addr: u64, len: u64) -> Result<u64, Errno> {
    let end = addr
        .checked_add(len)
        .and_then(|e| e.checked_next_multiple_of(PAGE_SIZE))
        .ok_or(Errno::ENOMEM)?;
    let start = addr & !(PAGE_SIZE - 1);
    let mm = cx.mm();
    for page in (start..end).step_by(PAGE_SIZE as usize) {
        if mm.mmu().translate(page, 0).is_none() {
            return Err(Errno::ENOMEM);
        }
    }
    Ok((end - start) / PAGE_SIZE)
}

/// mlock、munlock、mlock2：宿主内存不会被换出到 guest 能察觉的地方，只检查范围。
pub(super) fn mlock(cx: &mut Cx<'_>, addr: u64, len: u64, flags: u32) -> SysResult {
    const MLOCK_ONFAULT: u32 = 1;
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(Errno::EINVAL);
    }
    mapped_pages(cx, addr, len).map(|_| 0)
}

pub(super) fn mlockall(flags: u32) -> SysResult {
    if flags == 0
        || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0
        || flags & (MCL_CURRENT | MCL_FUTURE) == 0
    {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// mincore：已映射的页都报告为在内存里。
pub(super) fn mincore(cx: &mut Cx<'_>, addr: u64, len: u64, vec: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let pages = mapped_pages(cx, addr, len)? as usize;
    let out = vec![1u8; pages];
    cx.mm()
        .write(crate::uaccess::UserSlice::new(vec, pages), &out)?;
    Ok(0)
}
