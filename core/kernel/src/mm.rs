//! guest 地址空间（设计文档 §5）：VMA 表加 soft-MMU 页表。
//!
//! 每个 VMA 指向一段宿主内存（匿名映射或文件映射），多个 VMA 可以共享同一段
//! （munmap/mprotect 切开之后）。宿主内存在最后一个引用它的 VMA 消失时才释放。
//! 文件映射直接用宿主 `mmap(objects/<ino>)`，代码页因此经由 page cache 在
//! guest 进程之间共享（§5.2）。
//!
//! 任何拆除映射或修改权限的操作都会把 [`Mm::take_dirty`] 置位，调用方据此
//! 刷新 CPU 的 TLB 和 Tier 1 的翻译缓存。

use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::rc::Rc;

use appine_engine::{MapError, Mmu, PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::uaccess::UserSlice;

/// mmap 的 flags（guest ABI）。
pub mod flags {
    /// 共享映射。
    pub const MAP_SHARED: u32 = 0x01;
    /// 私有映射。
    pub const MAP_PRIVATE: u32 = 0x02;
    /// 固定地址，覆盖已有映射。
    pub const MAP_FIXED: u32 = 0x10;
    /// 匿名映射。
    pub const MAP_ANONYMOUS: u32 = 0x20;
    /// 固定地址，但不覆盖已有映射。
    pub const MAP_FIXED_NOREPLACE: u32 = 0x10_0000;
}

/// mmap 分配区的上界：栈下方留出 1 GiB 的间隔。
const MMAP_TOP: u64 = crate::loader::STACK_TOP - crate::loader::STACK_SIZE - (1 << 30);
/// 最低可分配地址，与 Linux 的 mmap_min_addr 默认值相同。
const MMAP_MIN: u64 = 0x10000;

/// 一段由 [`Mm`] 持有的宿主内存。
struct HostRegion {
    base: NonNull<u8>,
    len: usize,
}

impl Drop for HostRegion {
    fn drop(&mut self) {
        unsafe { appine_hal::unmap(self.base, self.len) }
    }
}

/// 映射的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// 匿名内存。
    Anonymous,
    /// 文件的私有映射（写时复制）。
    PrivateFile,
    /// 文件的共享映射。
    SharedFile,
}

#[derive(Clone)]
struct Vma {
    len: u64,
    prot: u32,
    backing: Backing,
    /// 能否获得写权限：只读打开的文件的共享映射不能（宿主映射本身就是只读的）。
    may_write: bool,
    /// 只为持有宿主内存的引用：最后一个引用它的 VMA 消失时释放。
    _region: Rc<HostRegion>,
    /// 这个 VMA 的起点对应的宿主地址。
    host: NonNull<u8>,
}

/// 一个 guest 进程的地址空间。
pub struct Mm {
    // 字段按声明顺序析构：先销毁页表，再释放它指向的宿主内存。
    mmu: Mmu,
    vmas: BTreeMap<u64, Vma>,
    brk_start: u64,
    brk: u64,
    dirty: bool,
}

fn page_up(v: u64) -> Option<u64> {
    v.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1))
}

/// guest 权限对应的页表权限：arm64 没有只写页，可写意味着可读。
fn mmu_prot(p: u32) -> u32 {
    let mut r = p & (prot::READ | prot::WRITE | prot::EXEC);
    if r & prot::WRITE != 0 {
        r |= prot::READ;
    }
    r
}

fn map_err(e: MapError) -> Errno {
    match e {
        MapError::InvalidArgument => Errno::EINVAL,
        MapError::OutOfMemory => Errno::ENOMEM,
    }
}

impl Mm {
    /// 创建空地址空间。
    pub fn new() -> Self {
        Mm {
            mmu: Mmu::new(),
            vmas: BTreeMap::new(),
            brk_start: 0,
            brk: 0,
            dirty: false,
        }
    }

    /// 该地址空间的软件页表。
    pub fn mmu(&self) -> &Mmu {
        &self.mmu
    }

    /// 自上次调用以来是否有映射被拆除或改了权限。
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// `[start, start + len)` 是否完全空闲。
    fn is_free(&self, start: u64, len: u64) -> bool {
        // 起点在区间终点之前的最后一个 VMA 不能伸进区间
        self.vmas
            .range(..start + len)
            .next_back()
            .is_none_or(|(&s, v)| s + v.len <= start)
    }

    /// 自顶向下找一段长度为 `len` 的空闲区间。
    pub fn find_free(&self, len: u64) -> Option<u64> {
        let mut top = MMAP_TOP;
        for (&s, v) in self.vmas.range(..MMAP_TOP).rev() {
            let end = s + v.len;
            if end <= top && top - end >= len {
                return Some(top - len);
            }
            top = top.min(s);
        }
        (top >= MMAP_MIN + len).then(|| top - len)
    }

    /// 把一段宿主内存映射到 `[start, start + len)`；调用方保证区间空闲。
    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        start: u64,
        len: u64,
        prot: u32,
        backing: Backing,
        may_write: bool,
        region: Rc<HostRegion>,
        host: NonNull<u8>,
    ) -> Result<(), Errno> {
        // SAFETY: region 由 VMA 持有，生命周期不短于页表里的映射
        if let Err(e) = unsafe { self.mmu.map(start, host, len, mmu_prot(prot)) } {
            let _ = self.mmu.unmap(start, len);
            return Err(map_err(e));
        }
        self.vmas.insert(
            start,
            Vma {
                len,
                prot,
                backing,
                may_write,
                _region: region,
                host,
            },
        );
        Ok(())
    }

    fn new_anonymous(len: u64) -> Result<(Rc<HostRegion>, NonNull<u8>), Errno> {
        let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
        let base = appine_hal::map_anonymous(host_len).map_err(|_| Errno::ENOMEM)?;
        Ok((
            Rc::new(HostRegion {
                base,
                len: host_len,
            }),
            base,
        ))
    }

    /// 在固定地址建立清零的匿名映射（加载器使用），区间必须空闲。
    pub fn map_anonymous(&mut self, start: u64, len: u64, prot: u32) -> Result<(), Errno> {
        if !start.is_multiple_of(PAGE_SIZE) || len == 0 || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        if !self.is_free(start, len) {
            return Err(Errno::EEXIST);
        }
        let (region, host) = Self::new_anonymous(len)?;
        self.insert(start, len, prot, Backing::Anonymous, true, region, host)
    }

    /// 实现 mmap(2)。`file` 是宿主 fd、文件偏移，以及文件是否以可写方式打开。
    pub fn mmap(
        &mut self,
        addr: u64,
        len: u64,
        prot: u32,
        flags: u32,
        file: Option<(i32, u64, bool)>,
    ) -> Result<u64, Errno> {
        let len = page_up(len).filter(|&l| l != 0).ok_or(Errno::EINVAL)?;
        let shared = flags & 3 == flags::MAP_SHARED;
        if !shared && flags & 3 != flags::MAP_PRIVATE {
            return Err(Errno::EINVAL);
        }
        let fixed = flags & (flags::MAP_FIXED | flags::MAP_FIXED_NOREPLACE) != 0;
        let start = if fixed {
            if !addr.is_multiple_of(PAGE_SIZE)
                || addr.checked_add(len).is_none_or(|e| e > 1 << VA_BITS)
            {
                return Err(Errno::EINVAL);
            }
            if !self.is_free(addr, len) {
                if flags & flags::MAP_FIXED_NOREPLACE != 0 {
                    return Err(Errno::EEXIST);
                }
                self.munmap(addr, len)?;
            }
            addr
        } else {
            let hint = addr & !(PAGE_SIZE - 1);
            if hint >= MMAP_MIN
                && hint.checked_add(len).is_some_and(|e| e <= MMAP_TOP)
                && self.is_free(hint, len)
            {
                hint
            } else {
                self.find_free(len).ok_or(Errno::ENOMEM)?
            }
        };

        let (backing, may_write, region, host) = match file {
            None => {
                let (region, host) = Self::new_anonymous(len)?;
                (Backing::Anonymous, true, region, host)
            }
            Some((fd, offset, writable)) => {
                if !offset.is_multiple_of(PAGE_SIZE) {
                    return Err(Errno::EINVAL);
                }
                let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
                let m =
                    appine_hal::map_file(fd, offset, host_len, shared, writable).map_err(Errno)?;
                let backing = if shared {
                    Backing::SharedFile
                } else {
                    Backing::PrivateFile
                };
                let region = Rc::new(HostRegion {
                    base: m.base,
                    len: m.len,
                });
                (backing, !shared || writable, region, m.ptr)
            }
        };
        if prot & prot::WRITE != 0 && !may_write {
            return Err(Errno::EACCES);
        }
        self.insert(start, len, prot, backing, may_write, region, host)?;
        Ok(start)
    }

    /// 在 `addr` 处把跨过它的 VMA 切成两段。
    fn split_at(&mut self, addr: u64) {
        let Some((&s, v)) = self.vmas.range(..addr).next_back() else {
            return;
        };
        if s + v.len <= addr {
            return;
        }
        let mut tail = v.clone();
        let head_len = addr - s;
        tail.len -= head_len;
        // SAFETY: addr 落在 VMA 内部，偏移不越过宿主映射
        tail.host = unsafe { tail.host.add(head_len as usize) };
        self.vmas.get_mut(&s).unwrap().len = head_len;
        self.vmas.insert(addr, tail);
    }

    /// 实现 munmap(2)。
    pub fn munmap(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        let len = page_up(len).ok_or(Errno::EINVAL)?;
        if !addr.is_multiple_of(PAGE_SIZE)
            || len == 0
            || addr.checked_add(len).is_none_or(|e| e > 1 << VA_BITS)
        {
            return Err(Errno::EINVAL);
        }
        let end = addr + len;
        self.split_at(addr);
        self.split_at(end);
        // 页表按页解除映射，所以只处理确实存在的 VMA：guest 传一个覆盖整个地址空间的长度也很快
        let doomed: Vec<(u64, u64)> = self
            .vmas
            .range(addr..end)
            .map(|(&s, v)| (s, v.len))
            .collect();
        for (s, len) in doomed {
            self.vmas.remove(&s);
            self.mmu.unmap(s, len).map_err(map_err)?;
            self.dirty = true;
        }
        Ok(())
    }

    /// 实现 mprotect(2)：区间必须完全已映射。
    pub fn mprotect(&mut self, addr: u64, len: u64, prot: u32) -> Result<(), Errno> {
        let len = page_up(len).ok_or(Errno::EINVAL)?;
        if !addr.is_multiple_of(PAGE_SIZE) || addr.checked_add(len).is_none() {
            return Err(Errno::EINVAL);
        }
        let end = addr + len;
        // 区间必须被 VMA 连续覆盖：先看跨过 addr 的那个，再逐个往后接
        let (mut cursor, mut writable) = (addr, true);
        if let Some((&s, v)) = self
            .vmas
            .range(..addr)
            .next_back()
            .filter(|&(&s, v)| s + v.len > addr)
        {
            cursor = s + v.len;
            writable = v.may_write;
        }
        for (&s, v) in self.vmas.range(addr..end) {
            if s > cursor {
                return Err(Errno::ENOMEM);
            }
            cursor = cursor.max(s + v.len);
            writable &= v.may_write;
        }
        if cursor < end {
            return Err(Errno::ENOMEM);
        }
        if prot & prot::WRITE != 0 && !writable {
            return Err(Errno::EACCES);
        }
        self.split_at(addr);
        self.split_at(end);
        for (_, v) in self.vmas.range_mut(addr..end) {
            v.prot = prot;
        }
        self.mmu
            .protect(addr, len, mmu_prot(prot))
            .map_err(map_err)?;
        self.dirty = true;
        Ok(())
    }

    /// madvise(MADV_DONTNEED)：匿名内存清零，其余映射保持不变。区间必须完全已映射。
    pub fn discard(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        let len = page_up(len).ok_or(Errno::EINVAL)?;
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let mut cursor = addr;
        while cursor < end {
            let (&s, v) = self
                .vmas
                .range(..=cursor)
                .next_back()
                .ok_or(Errno::ENOMEM)?;
            if s + v.len <= cursor {
                return Err(Errno::ENOMEM);
            }
            let stop = end.min(s + v.len);
            if v.backing == Backing::Anonymous {
                // SAFETY: [cursor, stop) 落在这个 VMA 内，对应的宿主内存由它持有
                unsafe {
                    std::ptr::write_bytes(
                        v.host.as_ptr().add((cursor - s) as usize),
                        0,
                        (stop - cursor) as usize,
                    )
                };
            }
            cursor = stop;
        }
        Ok(())
    }

    /// 设置初始的 program break（加载器使用）。
    pub fn set_brk(&mut self, start: u64) {
        self.brk_start = start;
        self.brk = start;
    }

    /// 实现 brk(2)：返回新的 break，失败时返回原值。
    pub fn brk(&mut self, new: u64) -> u64 {
        if new < self.brk_start {
            return self.brk;
        }
        let (Some(old_end), Some(new_end)) = (page_up(self.brk), page_up(new)) else {
            return self.brk;
        };
        if new_end > old_end {
            let grow = new_end - old_end;
            if !self.is_free(old_end, grow)
                || self
                    .map_anonymous(old_end, grow, prot::READ | prot::WRITE)
                    .is_err()
            {
                return self.brk;
            }
        } else if new_end < old_end && self.munmap(new_end, old_end - new_end).is_err() {
            return self.brk;
        }
        self.brk = new;
        new
    }

    /// 从 guest 内存读取（相当于 Linux 的 `copy_from_user`），要求每页可读。
    pub fn read(&self, src: UserSlice, dst: &mut [u8]) -> Result<(), Errno> {
        assert_eq!(src.len, dst.len());
        self.for_each_page(src.addr, src.len, prot::READ, |host, done, chunk| unsafe {
            std::ptr::copy_nonoverlapping(host, dst[done..].as_mut_ptr(), chunk);
        })
    }

    /// 写入 guest 内存（相当于 Linux 的 `copy_to_user`），要求每页可写。
    pub fn write(&self, dst: UserSlice, data: &[u8]) -> Result<(), Errno> {
        assert_eq!(dst.len, data.len());
        self.for_each_page(dst.addr, dst.len, prot::WRITE, |host, done, chunk| unsafe {
            std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host, chunk);
        })
    }

    /// 读取以 NUL 结尾的字符串（不含 NUL），最长 `max` 字节，超出时返回 ENAMETOOLONG。
    pub fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::new();
        let mut cur = addr;
        loop {
            let chunk = (PAGE_SIZE - cur % PAGE_SIZE) as usize;
            let host = self.mmu.translate(cur, prot::READ).ok_or(Errno::EFAULT)?;
            // SAFETY: translate 保证这一页在本次调用期间有效
            let bytes = unsafe { std::slice::from_raw_parts(host.as_ptr(), chunk) };
            if let Some(n) = bytes.iter().position(|&b| b == 0) {
                out.extend_from_slice(&bytes[..n]);
                return if out.len() > max {
                    Err(Errno::ENAMETOOLONG)
                } else {
                    Ok(out)
                };
            }
            out.extend_from_slice(bytes);
            if out.len() > max {
                return Err(Errno::ENAMETOOLONG);
            }
            cur = cur.checked_add(chunk as u64).ok_or(Errno::EFAULT)?;
        }
    }

    /// 内核自己往 guest 内存写入，比如加载程序、构造初始栈。只要求已映射，不检查权限。
    pub(crate) fn write_kernel(&self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        self.for_each_page(addr, data.len(), 0, |host, done, chunk| unsafe {
            std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host, chunk);
        })
    }

    /// 按页遍历 `[addr, addr + len)`，对每一段调用 `f(宿主指针, 已处理字节数, 本段长度)`。
    fn for_each_page(
        &self,
        addr: u64,
        len: usize,
        required_prot: u32,
        mut f: impl FnMut(*mut u8, usize, usize),
    ) -> Result<(), Errno> {
        let mut done = 0;
        while done < len {
            let cur = addr.checked_add(done as u64).ok_or(Errno::EFAULT)?;
            let page_left = (PAGE_SIZE - cur % PAGE_SIZE) as usize;
            let chunk = page_left.min(len - done);
            let host = self
                .mmu
                .translate(cur, required_prot)
                .ok_or(Errno::EFAULT)?;
            f(host.as_ptr(), done, chunk);
            done += chunk;
        }
        Ok(())
    }
}

impl Default for Mm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RW: u32 = prot::READ | prot::WRITE;

    #[test]
    fn read_back_across_pages() {
        let mut mm = Mm::new();
        mm.map_anonymous(0x10000, 2 * PAGE_SIZE, RW).unwrap();
        let data: Vec<u8> = (0..100).collect();
        let addr = 0x10000 + PAGE_SIZE - 50;
        mm.write_kernel(addr, &data).unwrap();
        let mut back = vec![0; data.len()];
        mm.read(UserSlice::new(addr, data.len()), &mut back)
            .unwrap();
        assert_eq!(back, data);
        mm.write_kernel(addr, b"hello\0").unwrap();
        assert_eq!(mm.read_cstr(addr, 64).unwrap(), b"hello");
        assert_eq!(mm.read_cstr(addr, 3), Err(Errno::ENAMETOOLONG));
    }

    #[test]
    fn read_requires_mapping_and_permission() {
        let mut mm = Mm::new();
        mm.map_anonymous(0x10000, PAGE_SIZE, prot::EXEC).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(
            mm.read(UserSlice::new(0x10000, 8), &mut buf),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            mm.read(UserSlice::new(0x20000, 8), &mut buf),
            Err(Errno::EFAULT)
        );
    }

    #[test]
    fn mmap_munmap_mprotect_split_vmas() {
        let mut mm = Mm::new();
        let a = mm
            .mmap(
                0,
                4 * PAGE_SIZE,
                RW,
                flags::MAP_PRIVATE | flags::MAP_ANONYMOUS,
                None,
            )
            .unwrap();
        assert!(a + 4 * PAGE_SIZE <= MMAP_TOP);
        mm.write_kernel(a + 3 * PAGE_SIZE, b"tail").unwrap();

        // 中间挖掉一页：两侧仍然可用，数据不变
        mm.munmap(a + PAGE_SIZE, PAGE_SIZE).unwrap();
        assert!(mm.take_dirty());
        // 几乎整个地址空间都没有映射：立即成功，什么都不做
        mm.munmap(MMAP_MIN, a - MMAP_MIN).unwrap();
        assert!(!mm.take_dirty());
        assert_eq!(mm.vmas.len(), 2);
        assert!(mm.mmu().translate(a + PAGE_SIZE, 0).is_none());
        let mut buf = [0u8; 4];
        mm.read(UserSlice::new(a + 3 * PAGE_SIZE, 4), &mut buf)
            .unwrap();
        assert_eq!(&buf, b"tail");

        // mprotect 跨过空洞失败；在完整映射上切出只读的一页
        assert_eq!(
            mm.mprotect(a, 3 * PAGE_SIZE, prot::READ),
            Err(Errno::ENOMEM)
        );
        mm.mprotect(a + 2 * PAGE_SIZE, PAGE_SIZE, prot::READ)
            .unwrap();
        assert!(mm.mmu().translate(a + 2 * PAGE_SIZE, prot::WRITE).is_none());
        assert!(mm.mmu().translate(a + 3 * PAGE_SIZE, prot::WRITE).is_some());
        assert_eq!(mm.vmas.len(), 3);

        // MAP_FIXED 覆盖已有映射；NOREPLACE 不覆盖
        let fixed = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_FIXED;
        assert_eq!(mm.mmap(a, 2 * PAGE_SIZE, RW, fixed, None).unwrap(), a);
        let noreplace = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_FIXED_NOREPLACE;
        assert_eq!(
            mm.mmap(a, PAGE_SIZE, RW, noreplace, None),
            Err(Errno::EEXIST)
        );
    }

    #[test]
    fn brk_grows_and_shrinks() {
        let mut mm = Mm::new();
        mm.set_brk(0x50_0123);
        assert_eq!(mm.brk(0), 0x50_0123);
        assert_eq!(mm.brk(0x50_3000), 0x50_3000);
        mm.write_kernel(0x50_2ff0, &[1; 16]).unwrap();
        assert_eq!(mm.brk(0x50_1000), 0x50_1000);
        assert!(mm.mmu().translate(0x50_2000, 0).is_none());
    }
}
