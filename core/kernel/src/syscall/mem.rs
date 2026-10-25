//! 内存类 syscall。

use std::os::fd::AsRawFd;

use appine_engine::{PAGE_SIZE, prot};

use super::SysResult;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::FileKind;
use crate::kernel::Cx;
use crate::mm::flags;
use crate::process::RLIMIT_MEMLOCK;
use crate::vfs::dev::Device;

const PROT_MASK: u32 = prot::READ | prot::WRITE | prot::EXEC;
/// `PROT_GROWSDOWN`、`PROT_GROWSUP`：只对栈有意义，按 Linux 的做法忽略。
const PROT_GROWS: u32 = 0x0100_0000 | 0x0200_0000;
/// 映射类型位：`MAP_SHARED`、`MAP_PRIVATE`、`MAP_SHARED_VALIDATE`。
const MAP_TYPE: u32 = 0x0f;
const MAP_SHARED_VALIDATE: u32 = 0x03;
/// MAP_SHARED_VALIDATE 认识的标志（Linux 的 `LEGACY_MAP_MASK` 加上 `MAP_FIXED_NOREPLACE`、
/// `MAP_UNINITIALIZED`，以及 MAP_HUGETLB 的页大小位）。
const MAP_VALIDATE_KNOWN: u32 = MAP_TYPE
    | flags::MAP_FIXED
    | flags::MAP_ANONYMOUS
    | 0x0100 // MAP_GROWSDOWN
    | 0x0800 // MAP_DENYWRITE
    | 0x1000 // MAP_EXECUTABLE
    | 0x2000 // MAP_LOCKED
    | 0x4000 // MAP_NORESERVE
    | 0x8000 // MAP_POPULATE
    | 0x1_0000 // MAP_NONBLOCK
    | 0x2_0000 // MAP_STACK
    | 0x4_0000 // MAP_HUGETLB
    | flags::MAP_FIXED_NOREPLACE
    | 0x400_0000 // MAP_UNINITIALIZED
    | 0xfc00_0000; // MAP_HUGE_SHIFT 之上的页大小
const F_SEAL_WRITE: u32 = 0x8;
const F_SEAL_FUTURE_WRITE: u32 = 0x10;
const MADV_DONTNEED: i32 = 4;
const MADV_REMOVE: i32 = 9;
const MADV_FREE: i32 = 8;
const MADV_WIPEONFORK: i32 = 18;
const MADV_KEEPONFORK: i32 = 19;
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
        // MAP_SHARED_VALIDATE 要求认识所有的标志（MAP_SYNC 只对 DAX 文件有效）
        MAP_SHARED_VALIDATE if flags & !MAP_VALIDATE_KNOWN != 0 => {
            return Err(Errno::EOPNOTSUPP);
        }
        MAP_SHARED_VALIDATE => flags::MAP_SHARED,
        k @ (flags::MAP_SHARED | flags::MAP_PRIVATE) => k,
        _ => return Err(Errno::EINVAL),
    };
    let mut flags = (flags & !MAP_TYPE) | kind;
    // 共享的匿名内存不能向下增长
    const MAP_GROWSDOWN: u32 = 0x0100;
    let growsdown = flags & MAP_GROWSDOWN != 0;
    if growsdown && flags & flags::MAP_ANONYMOUS != 0 && kind == flags::MAP_SHARED {
        return Err(Errno::EINVAL);
    }
    // 只读打开的 /dev/zero 的共享映射：不能有写权限，以后也不能改成可写
    let mut deny_write = false;
    // 文件映射在 /proc/<pid>/maps 里显示的路径和 inode
    let mut label = None;
    let file = if flags & flags::MAP_ANONYMOUS != 0 {
        None
    } else {
        let file = cx.fds().get(fd)?;
        // Linux 先查 fd（EBADF），再查长度（EINVAL），最后才查文件的权限（EACCES）
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        let f = file.borrow();
        let shared_write = kind == flags::MAP_SHARED && prot & prot::WRITE != 0;
        match &f.kind {
            FileKind::Regular { file: host, ino } => {
                if !f.readable() {
                    return Err(Errno::EACCES);
                }
                label = Some((f.path.clone(), *ino));
                if shared_write && !f.writable() {
                    return Err(Errno::EACCES);
                }
                // 封印了写入的 memfd 不能有可写的共享映射，以后也不能改成可写
                let sealed = cx
                    .kr()
                    .seals
                    .get(ino)
                    .is_some_and(|s| s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0);
                if sealed && kind == flags::MAP_SHARED {
                    if prot & prot::WRITE != 0 {
                        return Err(Errno::EPERM);
                    }
                    Some((host.as_raw_fd(), offset, false))
                } else {
                    Some((host.as_raw_fd(), offset, f.writable()))
                }
            }
            // /dev/zero：私有映射是匿名内存，共享映射是共享的匿名内存
            FileKind::Device {
                dev: Device::Zero, ..
            } => {
                if !f.readable() {
                    return Err(Errno::EACCES);
                }
                if kind == flags::MAP_SHARED && !f.writable() {
                    if prot & prot::WRITE != 0 {
                        return Err(Errno::EACCES);
                    }
                    deny_write = true;
                }
                flags |= flags::MAP_ANONYMOUS;
                None
            }
            FileKind::Path(_) => return Err(Errno::EBADF),
            _ => return Err(Errno::ENODEV),
        }
    };
    const MAP_LOCKED: u32 = 0x2000;
    if flags & MAP_LOCKED != 0 {
        check_memlock(cx, len.next_multiple_of(PAGE_SIZE)).map_err(|_| Errno::EAGAIN)?;
    }
    let start = cx.mm_mut().mmap(addr, len, prot, flags, file)?;
    // 文件映射的 MAP_GROWSDOWN 不增长（Linux 上长出来的部分也映射文件，没有程序这样用）
    if growsdown && file.is_none() && kind == flags::MAP_PRIVATE {
        cx.mm_mut().set_growsdown(start);
    }
    if let Some((name, ino)) = label {
        cx.mm_mut().label(start, len, name, ino);
    }
    if deny_write {
        cx.mm_mut().deny_write(start, len);
    }
    if flags & MAP_LOCKED != 0 {
        cx.mm_mut().set_locked(start, len, true)?;
    }
    // MAP_POPULATE（带 MAP_NONBLOCK 时不调）、MAP_LOCKED、mlockall(MCL_FUTURE) 都把页
    // 调进内存
    const MAP_POPULATE: u32 = 0x8000;
    const MAP_NONBLOCK: u32 = 0x10000;
    let populate = flags & (MAP_POPULATE | MAP_NONBLOCK) == MAP_POPULATE
        || flags & MAP_LOCKED != 0
        || cx.mm().lock_future && !cx.mm().lock_onfault;
    if populate {
        cx.mm().populate(start, len);
    }
    Ok(start)
}

pub(super) fn mremap(
    cx: &mut Cx<'_>,
    old: u64,
    old_len: u64,
    new_len: u64,
    flags: u32,
    new_addr: u64,
) -> SysResult {
    const MREMAP_MAYMOVE: u32 = 1;
    const MREMAP_FIXED: u32 = 2;
    const MREMAP_DONTUNMAP: u32 = 4;
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
        || flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0
        || flags & MREMAP_DONTUNMAP != 0 && (flags & MREMAP_MAYMOVE == 0 || old_len != new_len)
    {
        return Err(Errno::EINVAL);
    }
    let fixed = (flags & MREMAP_FIXED != 0).then_some(new_addr);
    if flags & MREMAP_DONTUNMAP != 0 {
        let (new, refill) = cx.mm_mut().mremap_dontunmap(old, old_len, fixed)?;
        // 私有文件映射原来的地址上重新映射仓库里的文件；打不开（比如已经删除并回收）
        // 时只好给空的匿名内存
        let vfs = cx.vfs()?;
        for r in refill {
            let file = (r.ino != 0)
                .then(|| vfs.store().open_object(r.ino, false).ok())
                .flatten();
            let mut mm = cx.mm_mut();
            match &file {
                Some(f) => {
                    let flags = flags::MAP_PRIVATE | flags::MAP_FIXED;
                    let src = Some((f.as_raw_fd(), r.offset, false));
                    mm.mmap(r.start, r.len, r.prot, flags, src)?;
                    mm.label(r.start, r.len, r.name, r.ino);
                }
                None => mm.map_anonymous(r.start, r.len, r.prot)?,
            }
            mm.set_locked(r.start, r.len, false)?;
        }
        return Ok(new);
    }
    cx.mm_mut()
        .mremap(old, old_len, new_len, flags & MREMAP_MAYMOVE != 0, fixed)
}

/// remap_file_pages：已经废弃，与 Linux 4.0 起一样用 mmap 模拟（[`Mm::remap_file_pages`]）。
/// `prot` 必须是 0，`flags` 只看 `MAP_NONBLOCK`。
///
/// [`Mm::remap_file_pages`]: crate::mm::Mm::remap_file_pages
pub(super) fn remap_file_pages(
    cx: &mut Cx<'_>,
    start: u64,
    size: u64,
    prot: u64,
    pgoff: u64,
    flags: u32,
) -> SysResult {
    const MAP_NONBLOCK: u32 = 0x10000;
    if prot != 0 {
        return Err(Errno::EINVAL);
    }
    let start = start & !(PAGE_SIZE - 1);
    let size = size & !(PAGE_SIZE - 1);
    let offset = pgoff
        .checked_add(size / PAGE_SIZE)
        .and(pgoff.checked_mul(PAGE_SIZE))
        .ok_or(Errno::EINVAL)?;
    cx.mm_mut().remap_file_pages(start, size, offset)?;
    if flags & MAP_NONBLOCK == 0 {
        cx.mm().populate(start, size);
    }
    Ok(0)
}

/// msync：共享映射直接就是宿主的文件映射，不需要写回，只检查参数。
pub(super) fn msync(cx: &mut Cx<'_>, addr: u64, len: u64, flags: u32) -> SysResult {
    const MS_ASYNC: u32 = 1;
    const MS_INVALIDATE: u32 = 2;
    const MS_SYNC: u32 = 4;
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || flags & (MS_ASYNC | MS_SYNC) == MS_ASYNC | MS_SYNC
        || !addr.is_multiple_of(PAGE_SIZE)
    {
        return Err(Errno::EINVAL);
    }
    cx.mm().check_mapped(addr, len)?;
    // 锁住的页不能作废
    if flags & MS_INVALIDATE != 0 && cx.mm().locked_in(addr, len) > 0 {
        return Err(Errno::EBUSY);
    }
    Ok(0)
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
    let len = len
        .checked_next_multiple_of(PAGE_SIZE)
        .filter(|&l| addr.checked_add(l).is_some())
        .ok_or(Errno::EINVAL)?;
    // 没有 KSM、透明大页和内存故障注入（MERGEABLE、HUGEPAGE、COLLAPSE、HWPOISON 等）
    if !matches!(advice, 0..=4 | 8..=11 | 16..=24) {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if advice == MADV_REMOVE {
        return cx.mm_mut().remove(addr, len).map(|()| 0);
    }
    let (locked, private_anon) = cx.mm().advice_range(addr, len)?;
    match advice {
        MADV_DONTNEED if locked => Err(Errno::EINVAL),
        // 其余建议只影响性能，可以忽略；DONTNEED 之后匿名内存必须读回 0
        MADV_DONTNEED => cx.mm_mut().discard(addr, len).map(|()| 0),
        MADV_FREE | MADV_WIPEONFORK | MADV_KEEPONFORK if !private_anon => Err(Errno::EINVAL),
        _ => Ok(0),
    }
}

/// 没有 `CAP_IPC_LOCK` 时，锁住的内存不能超过 `RLIMIT_MEMLOCK`（为 0 时根本不能锁）。
/// `more` 是这次要新锁住的字节数。
fn check_memlock(cx: &Cx<'_>, more: u64) -> Result<(), Errno> {
    if cx.cred().capable(cap::IPC_LOCK) {
        return Ok(());
    }
    let limit = cx.t.group.borrow().rlimits[RLIMIT_MEMLOCK as usize].0;
    if limit == 0 {
        return Err(Errno::EPERM);
    }
    if cx.mm().locked_bytes() + more > limit {
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

/// mlock、munlock、mlock2：记录哪些映射被锁住（宿主内存不会被换出到 guest 能察觉的
/// 地方），检查范围和 `RLIMIT_MEMLOCK`。与 Linux 一样，锁住时把页调进内存，
/// `MLOCK_ONFAULT` 时不调。
pub(super) fn mlock(cx: &mut Cx<'_>, addr: u64, len: u64, flags: u32, lock: bool) -> SysResult {
    const MLOCK_ONFAULT: u32 = 1;
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(Errno::EINVAL);
    }
    let start = addr & !(PAGE_SIZE - 1);
    let span = (addr.saturating_add(len)).next_multiple_of(PAGE_SIZE) - start;
    if lock {
        let already = cx.mm().locked_in(start, span);
        check_memlock(cx, span - already)?;
    }
    cx.mm_mut().set_locked(addr, len, lock)?;
    if lock && flags & MLOCK_ONFAULT == 0 {
        cx.mm().populate(start, span);
    }
    Ok(0)
}

pub(super) fn mlockall(cx: &mut Cx<'_>, flags: u32) -> SysResult {
    if flags == 0
        || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0
        || flags & (MCL_CURRENT | MCL_FUTURE) == 0
    {
        return Err(Errno::EINVAL);
    }
    if flags & MCL_CURRENT != 0 {
        let mm = cx.mm();
        let more = mm.size() - mm.locked_bytes();
        drop(mm);
        check_memlock(cx, more)?;
        cx.mm_mut().set_all_locked(true);
        if flags & MCL_ONFAULT == 0 {
            cx.mm().populate(0, u64::MAX);
        }
    } else {
        check_memlock(cx, 0)?;
    }
    let mut mm = cx.mm_mut();
    mm.lock_future = flags & MCL_FUTURE != 0;
    mm.lock_onfault = flags & MCL_ONFAULT != 0;
    Ok(0)
}

pub(super) fn munlockall(cx: &mut Cx<'_>) -> SysResult {
    let mut mm = cx.mm_mut();
    mm.set_all_locked(false);
    mm.lock_future = false;
    mm.lock_onfault = false;
    Ok(0)
}

/// mincore：按宿主内存报告每页是否驻留。
pub(super) fn mincore(cx: &mut Cx<'_>, addr: u64, len: u64, vec: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let out: Vec<u8> = cx
        .mm()
        .residency(addr, len)?
        .into_iter()
        .map(u8::from)
        .collect();
    cx.mm()
        .write(crate::uaccess::UserSlice::new(vec, out.len()), &out)?;
    Ok(0)
}
