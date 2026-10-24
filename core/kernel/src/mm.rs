//! guest 地址空间（设计文档 §5）。
//!
//! M0 只有匿名映射，宿主内存按映射整段申请；VMA 树、文件映射和 arena 分配
//! 在 M1 及之后加入。

use std::ptr::NonNull;

use appine_engine::{MapError, Mmu, PAGE_SIZE, prot};

use crate::errno::Errno;
use crate::uaccess::UserSlice;

/// 一段由 [`Mm`] 持有的宿主内存。
struct HostRegion {
    ptr: NonNull<u8>,
    len: usize,
}

impl Drop for HostRegion {
    fn drop(&mut self) {
        unsafe { appine_hal::unmap(self.ptr, self.len) }
    }
}

/// 一个 guest 进程的地址空间。
pub struct Mm {
    // 字段按声明顺序析构：先销毁页表，再释放它指向的宿主内存。
    mmu: Mmu,
    regions: Vec<HostRegion>,
}

impl Mm {
    /// 创建空地址空间。
    pub fn new() -> Self {
        Mm {
            mmu: Mmu::new(),
            regions: Vec::new(),
        }
    }

    /// 该地址空间的软件页表。
    pub fn mmu(&self) -> &Mmu {
        &self.mmu
    }

    /// 在 `[start, start + len)` 建立清零的匿名映射。
    ///
    /// M0 的调用方保证新映射不与已有映射重叠；修改映射后，调用方必须刷新所有
    /// 使用该地址空间的 CPU 的 TLB。
    pub fn map_anonymous(&mut self, start: u64, len: u64, prot: u32) -> Result<(), Errno> {
        if !start.is_multiple_of(PAGE_SIZE) || len == 0 || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
        let ptr = appine_hal::map_anonymous(host_len).map_err(|_| Errno::ENOMEM)?;
        let region = HostRegion { ptr, len: host_len };
        // SAFETY: region 由 self.regions 持有，生命周期不短于页表
        if let Err(err) = unsafe { self.mmu.map(start, ptr, len, prot) } {
            let _ = self.mmu.unmap(start, len);
            return Err(match err {
                MapError::InvalidArgument => Errno::EINVAL,
                MapError::OutOfMemory => Errno::ENOMEM,
            });
        }
        self.regions.push(region);
        Ok(())
    }

    /// 从 guest 内存读取（相当于 Linux 的 `copy_from_user`），要求每页可读。
    pub fn read(&self, src: UserSlice, dst: &mut [u8]) -> Result<(), Errno> {
        assert_eq!(src.len, dst.len());
        self.for_each_page(src.addr, src.len, prot::READ, |host, done, chunk| unsafe {
            std::ptr::copy_nonoverlapping(host, dst[done..].as_mut_ptr(), chunk);
        })
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

    #[test]
    fn read_back_across_pages() {
        let mut mm = Mm::new();
        mm.map_anonymous(0x10000, 2 * PAGE_SIZE, prot::READ | prot::WRITE)
            .unwrap();
        let data: Vec<u8> = (0..100).collect();
        let addr = 0x10000 + PAGE_SIZE - 50;
        mm.write_kernel(addr, &data).unwrap();
        let mut back = vec![0; data.len()];
        mm.read(UserSlice::new(addr, data.len()), &mut back)
            .unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn read_requires_mapping_and_permission() {
        let mut mm = Mm::new();
        mm.map_anonymous(0x10000, PAGE_SIZE, prot::WRITE).unwrap();
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
}
