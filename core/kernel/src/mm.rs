//! guest 地址空间（设计文档 §5）：VMA 表加 soft-MMU 页表。
//!
//! 每个 VMA 指向一段宿主内存（匿名映射或文件映射），多个 VMA 可以共享同一段
//! （munmap/mprotect 切开之后，或者 fork 之后的共享映射）。宿主内存在最后一个
//! 引用它的 VMA 消失时才释放。文件映射直接用宿主 `mmap(objects/<ino>)`。
//!
//! 一个地址空间可以被多个线程同时使用（线程、vfork）。拆除映射或修改权限之后，
//! 其他线程的 TLB 里可能还留着旧的翻译，所以要先做一次 TLB shootdown（[`Mm::sync`]）：
//! 递增代数，打断正在执行 guest 代码的线程并等它们退出，它们重新进入 guest
//! 代码之前看到新的代数，会刷新自己的 TLB 和 Tier 1 翻译缓存。被解除映射的宿主
//! 内存要等 shootdown 完成之后才释放，所以 guest 的数据竞争不会访问到已释放的内存。
//!
//! fork 按设计文档 §5.3 v1 整份复制：私有映射拷贝一份，共享映射与子进程共用。

use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use appine_engine::{MapError, Mmu, PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::task::TaskSync;
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
    /// 私有的匿名内存。
    Anonymous,
    /// 文件的私有映射（写时复制）。
    PrivateFile,
    /// 共享映射（文件或匿名）：fork 之后父子进程共用同一段宿主内存。
    Shared,
}

#[derive(Clone)]
struct Vma {
    len: u64,
    prot: u32,
    backing: Backing,
    /// 能否获得写权限：只读打开的文件的共享映射不能（宿主映射本身就是只读的）。
    may_write: bool,
    /// 持有宿主内存的引用：最后一个引用它的 VMA 消失时释放。
    region: Rc<HostRegion>,
    /// `/proc/<pid>/maps` 里显示的名字（映射的文件、程序）。
    name: Option<Rc<[u8]>>,
    /// 这个 VMA 的起点对应的宿主地址。
    host: NonNull<u8>,
    /// 从起点算起有文件内容的字节数（按 4K 向上取整）。私有文件映射超出文件末尾的
    /// 部分一碰宿主就会发 SIGBUS，fork 复制时不能读它们。匿名和共享映射等于 `len`。
    file_len: u64,
    /// 只读的私有文件映射在 fork 之后和别的地址空间共用宿主映射；要改成可写之前，
    /// 先复制一份自己的（[`Mm::mprotect`]）。
    cow: bool,
    /// 文件的共享映射：宿主文件的 (设备号, inode) 和起点对应的文件偏移，作为 futex 的键。
    shared_file: Option<(u64, u64, u64)>,
}

/// 一个 guest 进程的地址空间。
pub struct Mm {
    // 字段按声明顺序析构：先销毁页表，再释放它指向的宿主内存。
    mmu: Mmu,
    vmas: BTreeMap<u64, Vma>,
    /// 已经解除映射、等 shootdown 完成后才能释放的宿主内存。
    graveyard: Vec<Rc<HostRegion>>,
    brk_start: u64,
    brk: u64,
    dirty: bool,
    /// shootdown 的代数：变化之后，使用者重新进入 guest 代码之前刷新 TLB。
    generation: Arc<AtomicU64>,
    /// 正在使用这个地址空间的线程。
    users: Vec<Arc<TaskSync>>,
    /// vDSO 的加载地址（信号处理函数默认返回到其中的 sigreturn 跳板）。
    pub vdso: u64,
    /// 初始栈上 argv 字符串的范围（`/proc/<pid>/cmdline`）。
    pub args: (u64, u64),
    /// 初始栈上 envp 字符串的范围（`/proc/<pid>/environ`）。
    pub env: (u64, u64),
}

/// `/proc/<pid>/maps` 的一行。
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    pub shared: bool,
    pub name: Option<Rc<[u8]>>,
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
            graveyard: Vec::new(),
            brk_start: 0,
            brk: 0,
            dirty: false,
            generation: Arc::new(AtomicU64::new(0)),
            users: Vec::new(),
            vdso: 0,
            args: (0, 0),
            env: (0, 0),
        }
    }

    /// 该地址空间的软件页表。
    pub fn mmu(&self) -> &Mmu {
        &self.mmu
    }

    /// `addr` 处 futex 的键；没有映射时返回 None。
    pub fn futex_key(&self, addr: u64) -> Option<crate::futex::Key> {
        let host = self.mmu.translate(addr, 0)?;
        let (&start, v) = self.vmas.range(..=addr).next_back()?;
        Some(match v.shared_file {
            Some((dev, ino, offset)) => crate::futex::Key::File {
                dev,
                ino,
                offset: offset + (addr - start),
            },
            None => crate::futex::Key::Host(host.as_ptr() as usize),
        })
    }

    /// shootdown 的代数（[`Mm::sync`]）。
    pub fn generation(&self) -> Arc<AtomicU64> {
        self.generation.clone()
    }

    /// 登记一个开始使用这个地址空间的线程。
    pub fn add_user(&mut self, user: Arc<TaskSync>) {
        self.users.push(user);
    }

    /// 注销一个不再使用这个地址空间的线程（退出或 execve）。
    pub fn remove_user(&mut self, user: &Arc<TaskSync>) {
        self.users.retain(|u| !Arc::ptr_eq(u, user));
    }

    /// 是否还有别的线程在使用这个地址空间。
    pub fn shared_with_others(&self) -> bool {
        self.users.len() > 1
    }

    /// 如果有映射被拆除或改了权限，做一次 TLB shootdown，然后释放被解除映射的宿主内存。
    ///
    /// 调用方持有 BKL；`current` 是调用线程，它自己在重新进入 guest 代码之前刷新。
    pub fn sync(&mut self, current: &Arc<TaskSync>) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        // SeqCst 与使用者那边"先标记在执行 guest 代码、再读代数"配对：要么它读到新的代数，
        // 要么这里看到它正在执行，等它被打断后退出
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.quiesce(current);
        self.graveyard.clear();
    }

    /// 打断其他正在执行 guest 代码的使用者，等它们全部退出。它们接下来要获取 BKL，
    /// 而调用方正持有 BKL，所以在调用方释放 BKL 之前，这个地址空间里不会再执行
    /// guest 代码（fork 据此复制出一致的内存快照）。
    pub fn quiesce(&self, current: &Arc<TaskSync>) {
        let others = self.users.iter().filter(|u| !Arc::ptr_eq(u, current));
        for user in others.clone() {
            // SAFETY: 线程在销毁自己的 Cpu 之前（持有 BKL）从 users 里注销
            unsafe { user.kicker.kick() };
        }
        for user in others {
            while user.in_guest.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
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
                region,
                name: None,
                host,
                file_len: len,
                cow: false,
                shared_file: None,
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
                let backing = if shared {
                    Backing::Shared
                } else {
                    Backing::Anonymous
                };
                (backing, true, region, host)
            }
            Some((fd, offset, writable)) => {
                if !offset.is_multiple_of(PAGE_SIZE) {
                    return Err(Errno::EINVAL);
                }
                let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
                let m =
                    appine_hal::map_file(fd, offset, host_len, shared, writable).map_err(Errno)?;
                let backing = if shared {
                    Backing::Shared
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
        let file_len = match file {
            Some((fd, offset, _)) if !shared => {
                let size = appine_hal::fstat(fd).map_or(0, |s| s.st_size.max(0) as u64);
                size.saturating_sub(offset)
                    .next_multiple_of(PAGE_SIZE)
                    .min(len)
            }
            _ => len,
        };
        // st_dev 在 macOS 上是 i32
        #[allow(clippy::unnecessary_cast)]
        let shared_file = match file {
            Some((fd, offset, _)) if shared => appine_hal::fstat(fd)
                .ok()
                .map(|s| (s.st_dev as u64, s.st_ino as u64, offset)),
            _ => None,
        };
        self.insert(start, len, prot, backing, may_write, region, host)?;
        let v = self.vmas.get_mut(&start).unwrap();
        v.file_len = file_len;
        v.shared_file = shared_file;
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
        tail.file_len = v.file_len.saturating_sub(head_len);
        if let Some(f) = &mut tail.shared_file {
            f.2 += head_len;
        }
        // SAFETY: addr 落在 VMA 内部，偏移不越过宿主映射
        tail.host = unsafe { tail.host.add(head_len as usize) };
        let head = self.vmas.get_mut(&s).unwrap();
        head.len = head_len;
        head.file_len = head.file_len.min(head_len);
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
            let vma = self.vmas.remove(&s).unwrap();
            self.mmu.unmap(s, len).map_err(map_err)?;
            self.graveyard.push(vma.region);
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
        if prot & prot::WRITE != 0 {
            let shared: Vec<u64> = self
                .vmas
                .range(addr..end)
                .filter(|(_, v)| v.cow)
                .map(|(&s, _)| s)
                .collect();
            for s in shared {
                self.unshare(s)?;
            }
        }
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

    /// fork：复制出一个新的地址空间。私有映射逐页拷贝（全零的页跳过，新的匿名
    /// 内存本来就是零，这样没碰过的栈页不会在子进程里占用内存；私有文件映射只拷贝
    /// 文件末尾之前的部分）。共享映射和只读的私有文件映射与子进程共用宿主内存。
    pub fn fork(&mut self) -> Result<Mm, Errno> {
        let mut child = Mm::new();
        child.brk_start = self.brk_start;
        child.brk = self.brk;
        child.vdso = self.vdso;
        child.args = self.args;
        child.env = self.env;
        for (&start, v) in self.vmas.iter_mut() {
            let share = match v.backing {
                Backing::Shared => true,
                // 两边都不能写，内容就不会分叉；谁要改成可写，再复制自己的一份
                Backing::PrivateFile if v.prot & prot::WRITE == 0 => {
                    v.cow = true;
                    true
                }
                _ => false,
            };
            let (region, host) = if share {
                (v.region.clone(), v.host)
            } else {
                let (region, host) = Self::new_anonymous(v.len)?;
                // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
                unsafe { copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), v.file_len as usize) };
                (region, host)
            };
            child.insert(start, v.len, v.prot, v.backing, v.may_write, region, host)?;
            let c = child.vmas.get_mut(&start).unwrap();
            c.name = v.name.clone();
            c.file_len = if share { v.file_len } else { v.len };
            c.cow = v.cow && share;
        }
        Ok(child)
    }

    /// 给一个和别的地址空间共用宿主内存的私有文件映射复制一份自己的。
    fn unshare(&mut self, start: u64) -> Result<(), Errno> {
        let v = &self.vmas[&start];
        let (region, host) = Self::new_anonymous(v.len)?;
        // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
        unsafe { copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), v.file_len as usize) };
        let (len, p) = (v.len, v.prot);
        // SAFETY: region 由 VMA 持有，生命周期不短于页表里的映射
        unsafe { self.mmu.map(start, host, len, mmu_prot(p)) }.map_err(map_err)?;
        let v = self.vmas.get_mut(&start).unwrap();
        let old = std::mem::replace(&mut v.region, region);
        v.host = host;
        v.file_len = v.len;
        v.cow = false;
        // 其他线程的 TLB 里可能还有旧的翻译：shootdown 之后才释放
        self.graveyard.push(old);
        self.dirty = true;
        Ok(())
    }

    /// 给起点落在 `[start, start + len)` 里的映射起名字（`/proc/<pid>/maps` 用）。
    pub fn label(&mut self, start: u64, len: u64, name: Rc<[u8]>) {
        for (_, v) in self.vmas.range_mut(start..start.saturating_add(len)) {
            v.name = Some(name.clone());
        }
    }

    /// 所有映射，按地址排列；栈、堆和 vDSO 按 Linux 的习惯命名。
    pub fn regions(&self) -> Vec<Region> {
        let stack = crate::loader::STACK_TOP - crate::loader::STACK_SIZE;
        self.vmas
            .iter()
            .map(|(&s, v)| {
                let special: Option<&[u8]> = if s == stack {
                    Some(b"[stack]")
                } else if s == self.vdso {
                    Some(b"[vdso]")
                } else if s >= self.brk_start && s < self.brk && self.brk_start != 0 {
                    Some(b"[heap]")
                } else {
                    None
                };
                Region {
                    start: s,
                    end: s + v.len,
                    prot: v.prot,
                    shared: v.backing == Backing::Shared,
                    name: v.name.clone().or_else(|| special.map(Rc::from)),
                }
            })
            .collect()
    }

    /// 映射的总大小（字节）。
    pub fn size(&self) -> u64 {
        self.vmas.values().map(|v| v.len).sum()
    }

    /// 驻留在宿主内存里的 4K 页数。
    pub fn resident_pages(&self) -> u64 {
        self.vmas
            .values()
            .map(|v| appine_hal::resident_pages(v.host.as_ptr(), v.len as usize) as u64)
            .sum()
    }

    /// guest 地址是否落在某个映射里（区分 SEGV_MAPERR 和 SEGV_ACCERR）。
    pub fn is_mapped(&self, addr: u64) -> bool {
        self.vmas
            .range(..=addr)
            .next_back()
            .is_some_and(|(&s, v)| addr < s + v.len)
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

/// 逐页拷贝，跳过全零的页（目标内存已经清零）。
///
/// # Safety
///
/// `src` 和 `dst` 都必须指向至少 `len` 字节、互不重叠的可访问内存，`len` 是页大小的整数倍。
unsafe fn copy_nonzero_pages(src: *const u8, dst: *mut u8, len: usize) {
    const PAGE: usize = PAGE_SIZE as usize;
    for off in (0..len).step_by(PAGE) {
        let page = unsafe { std::slice::from_raw_parts(src.add(off), PAGE) };
        let (head, words, tail) = unsafe { page.align_to::<u64>() };
        let zero = head.iter().all(|&b| b == 0)
            && words.iter().all(|&w| w == 0)
            && tail.iter().all(|&b| b == 0);
        if !zero {
            unsafe { std::ptr::copy_nonoverlapping(src.add(off), dst.add(off), PAGE) };
        }
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

        // 中间挖掉一页：两侧仍然可用，数据不变；宿主内存要等 shootdown 之后才释放
        mm.munmap(a + PAGE_SIZE, PAGE_SIZE).unwrap();
        assert!(std::mem::take(&mut mm.dirty));
        assert_eq!(mm.graveyard.len(), 1);
        // 几乎整个地址空间都没有映射：立即成功，什么都不做
        mm.munmap(MMAP_MIN, a - MMAP_MIN).unwrap();
        assert!(!mm.dirty);
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
    fn fork_copies_private_and_shares_shared_mappings() {
        let mut mm = Mm::new();
        let private = mm
            .mmap(
                0,
                2 * PAGE_SIZE,
                RW,
                flags::MAP_PRIVATE | flags::MAP_ANONYMOUS,
                None,
            )
            .unwrap();
        let shared = mm
            .mmap(
                0,
                PAGE_SIZE,
                RW,
                flags::MAP_SHARED | flags::MAP_ANONYMOUS,
                None,
            )
            .unwrap();
        mm.write_kernel(private + PAGE_SIZE, b"parent").unwrap();
        let child = mm.fork().unwrap();
        mm.write_kernel(private + PAGE_SIZE, b"PARENT").unwrap();
        mm.write_kernel(shared, b"both").unwrap();
        let mut buf = [0u8; 6];
        child
            .read(UserSlice::new(private + PAGE_SIZE, 6), &mut buf)
            .unwrap();
        assert_eq!(&buf, b"parent");
        child
            .read(UserSlice::new(shared, 4), &mut buf[..4])
            .unwrap();
        assert_eq!(&buf[..4], b"both");
        // 映射自顶向下分配：private 上方就是 mmap 区的上界，没有映射
        assert!(child.is_mapped(private) && !child.is_mapped(private + 2 * PAGE_SIZE));
    }

    #[test]
    fn fork_never_reads_private_file_mappings_past_the_end() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!("appine-mm-{}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[7u8; 5000]).unwrap();
        let f = std::fs::File::open(&path).unwrap();
        let mut mm = Mm::new();
        // 映射 3 页，文件只有 5000 字节：最后一页完全在文件末尾之后，碰了就是 SIGBUS
        let ro = mm
            .mmap(
                0,
                3 * PAGE_SIZE,
                prot::READ,
                flags::MAP_PRIVATE,
                Some((f.as_raw_fd(), 0, false)),
            )
            .unwrap();
        let rw = mm
            .mmap(
                0,
                3 * PAGE_SIZE,
                RW,
                flags::MAP_PRIVATE,
                Some((f.as_raw_fd(), 0, false)),
            )
            .unwrap();
        mm.write_kernel(rw, b"parent").unwrap();
        let child = mm.fork().unwrap();
        let mut b = [0u8; 6];
        child
            .read(UserSlice::new(ro + 4000, 1), &mut b[..1])
            .unwrap();
        assert_eq!(b[0], 7);
        child.read(UserSlice::new(rw, 6), &mut b).unwrap();
        assert_eq!(&b, b"parent");
        // 共用的只读映射改成可写之前先复制一份：父进程写入，子进程看不到
        mm.mprotect(ro, PAGE_SIZE, RW).unwrap();
        mm.write(UserSlice::new(ro, 1), &[1]).unwrap();
        child.read(UserSlice::new(ro, 1), &mut b[..1]).unwrap();
        assert_eq!(b[0], 7);
        let _ = std::fs::remove_file(&path);
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
