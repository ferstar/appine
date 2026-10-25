//! guest 地址空间（设计文档 §5）：VMA 表加 soft-MMU 页表。
//!
//! 每个 VMA 指向一段宿主内存（匿名映射或文件映射），多个 VMA 可以共享同一段
//! （munmap/mprotect 切开之后，或者 fork 之后的共享映射）。宿主内存在最后一个
//! 引用它的 VMA 消失时才释放。文件映射直接用宿主 `mmap(objects/<ino>)`。
//!
//! 一个地址空间可以被多个线程同时使用（线程、vfork）。拆除映射或降低权限之后，
//! 其他线程的 TLB 里可能还留着旧的翻译，所以要先做一次 TLB shootdown（[`Mm::sync`]）：
//! 递增代数，打断正在执行 guest 代码的线程并等它们退出，它们重新进入 guest
//! 代码之前看到新的代数，会刷新自己的 TLB。被解除映射的宿主内存要等 shootdown
//! 完成之后才释放，所以 guest 的数据竞争不会访问到已释放的内存。
//!
//! Tier 1 的翻译按同样的方式作废：可执行的映射被拆除、改了权限，或者 guest 用
//! `ic ivau` 声明修改了代码，这些范围记进 [`CodeLog`] 并递增代码代数；每个线程
//! 按自己错过的范围逐页作废翻译。
//!
//! fork 按设计文档 §5.3 v1 整份复制：私有映射拷贝一份，共享映射与子进程共用。

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
    /// 不计入内存提交量。
    pub const MAP_NORESERVE: u32 = 0x4000;
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

/// 一段可以映射进多个地址空间的宿主内存（System V 共享内存段）。
#[derive(Clone)]
pub(crate) struct SharedMemory {
    region: Rc<HostRegion>,
}

impl SharedMemory {
    /// 新建一段清零的内存，长度按页向上取整。
    pub fn new(len: u64) -> Result<SharedMemory, Errno> {
        let len = page_up(len).ok_or(Errno::ENOMEM)?;
        let (region, _) = Mm::new_anonymous(len, true)?;
        Ok(SharedMemory { region })
    }

    /// 长度（页的整数倍）。
    pub fn len(&self) -> u64 {
        self.region.len as u64
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
    /// 从起点算起有文件内容的字节数（按 4K 向上取整），匿名映射等于 `len`。文件映射
    /// 超出文件末尾的部分一碰宿主就会发 SIGBUS：它们不进页表，guest 访问时得到
    /// SIGBUS（[`Mm::fault_past_eof`]），fork 复制时也不读它们。
    file_len: u64,
    /// 只读的私有文件映射在 fork 之后和别的地址空间共用宿主映射；要改成可写之前，
    /// 先复制一份自己的（[`Mm::mprotect`]）。
    cow: bool,
    /// 文件映射：宿主文件的 (设备号, inode) 和起点对应的文件偏移。共享映射拿它作 futex
    /// 的键；文件被截短时据此找到映射它的页（[`Mm::file_truncated`]）。
    file_id: Option<(u64, u64, u64)>,
    /// 文件的共享映射：复制出来的宿主 fd 和起点对应的文件偏移（mremap 扩展映射时
    /// 接着映射文件后面的内容）。
    file: Option<(Rc<OwnedFd>, u64)>,
    /// shmat 映射的共享内存段：段的 id 和这次 shmat 的起点（切开之后不变）。
    shm: Option<(i32, u64)>,
    /// mlock 锁住了（只记录，宿主内存不会被换出到 guest 能察觉的地方）。
    locked: bool,
    /// 匿名映射可写过，或者内核写过（加载器）。没写过的内容全是零，fork 时不用复制：
    /// V8 为 wasm 预留的 8 GB `PROT_NONE` 逐页检查一遍要一秒。
    written: Cell<bool>,
}

/// 最近几个代码代数作废的范围。
#[derive(Default)]
pub struct CodeLog {
    /// (代码代数, 起点, 长度)，代数递增。
    entries: VecDeque<(u64, u64, u64)>,
}

impl CodeLog {
    /// 保留的代数个数；落后更多的线程整体清空翻译缓存。JVM 每装一个编译好的方法就
    /// `ic ivau` 一串缓存行，只留 64 个时，睡一会儿的 GC、编译线程醒来几乎都要整体
    /// 清空：Maven 构建清空 2400 次，用户态 CPU 时间多 30%。
    const GENERATIONS: u64 = 4096;

    fn push(&mut self, generation: u64, ranges: &[(u64, u64)]) {
        self.entries
            .extend(ranges.iter().map(|&(start, len)| (generation, start, len)));
        while self
            .entries
            .front()
            .is_some_and(|e| e.0 + Self::GENERATIONS <= generation)
        {
            self.entries.pop_front();
        }
    }

    /// 代数 `seen` 之后、直到 `now` 作废的范围；记录已经不全时返回 None。
    pub fn since(&self, seen: u64, now: u64) -> Option<Vec<(u64, u64)>> {
        if self.entries.front().is_none_or(|e| e.0 > seen + 1) {
            return None;
        }
        Some(
            self.entries
                .iter()
                .filter(|e| e.0 > seen && e.0 <= now)
                .map(|e| (e.1, e.2))
                .collect(),
        )
    }
}

/// 使用者在重新进入 guest 代码之前检查的代数（[`Mm::sync`]）。
pub struct Generations {
    /// TLB shootdown 的代数。
    pub tlb: Arc<AtomicU64>,
    /// 代码的代数：变化之后按 `log` 作废 Tier 1 的翻译。
    pub code: Arc<AtomicU64>,
    /// 每个代码代数作废的范围。
    pub log: Arc<Mutex<CodeLog>>,
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
    /// 可执行的映射被拆除、改了权限或者内容的范围：这些地方的翻译要作废。
    code_ranges: Vec<(u64, u64)>,
    /// shootdown 的代数：变化之后，使用者重新进入 guest 代码之前刷新 TLB。
    generation: Arc<AtomicU64>,
    /// 代码的代数：变化之后，使用者按 `code_log` 作废 Tier 1 的翻译。数据页的变化（gcc
    /// 的垃圾回收器、malloc 频繁地 mmap/munmap）不影响已经翻译的代码，不改这个代数。
    code_generation: Arc<AtomicU64>,
    code_log: Arc<Mutex<CodeLog>>,
    /// 正在使用这个地址空间的线程。
    users: Vec<Arc<TaskSync>>,
    /// membarrier(2) 登记过的命令（`MEMBARRIER_CMD_REGISTER_*`）。
    pub membarrier: i32,
    /// `mlockall(MCL_FUTURE)`：以后的映射也锁住。
    pub lock_future: bool,
    /// 正在执行的程序文件在仓库里的 inode（ETXTBSY）。
    pub exe: Option<u64>,
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

/// 文件从 `offset` 起还有内容的字节数，按页向上取整，不超过 `len`。
fn file_extent(fd: i32, offset: u64, len: u64) -> u64 {
    let size = appine_hal::fstat(fd).map_or(0, |s| s.st_size.max(0) as u64);
    size.saturating_sub(offset)
        .next_multiple_of(PAGE_SIZE)
        .min(len)
}

/// 丢弃一段私有匿名宿主内存的内容：整页交给宿主回收，宿主页比 4K 大时两头不满一页
/// 的部分清零。原来全部清零，Go、JVM、V8 的 GC 归还内存时反而把这些页都碰了一遍。
///
/// # Safety
///
/// `[ptr, ptr + len)` 是 [`Mm`] 持有的、私有的匿名宿主内存。
unsafe fn discard_host(ptr: NonNull<u8>, len: usize) {
    let page = appine_hal::page_size();
    let addr = ptr.as_ptr() as usize;
    let head = (addr.next_multiple_of(page) - addr).min(len);
    let tail = ((addr + len) & !(page - 1)).max(addr + head) - addr;
    // SAFETY: 偏移都在 [0, len] 里；[head, tail) 按宿主页对齐
    unsafe {
        if head < tail && appine_hal::discard(ptr.add(head), tail - head).is_ok() {
            std::ptr::write_bytes(ptr.as_ptr(), 0, head);
            std::ptr::write_bytes(ptr.as_ptr().add(tail), 0, len - tail);
        } else {
            std::ptr::write_bytes(ptr.as_ptr(), 0, len);
        }
    }
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
            code_ranges: Vec::new(),
            generation: Arc::new(AtomicU64::new(0)),
            code_generation: Arc::new(AtomicU64::new(0)),
            code_log: Arc::default(),
            users: Vec::new(),
            membarrier: 0,
            lock_future: false,
            exe: None,
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
        // 与 Linux 相同，futex 的地址必须可读
        let host = self.mmu.translate(addr, prot::READ)?;
        let (&start, v) = self.vmas.range(..=addr).next_back()?;
        Some(match v.file_id {
            Some((dev, ino, offset)) if v.backing == Backing::Shared => crate::futex::Key::File {
                dev,
                ino,
                offset: offset + (addr - start),
            },
            _ => crate::futex::Key::Host(host.as_ptr() as usize),
        })
    }

    /// shootdown 的代数、代码的代数和代码失效的记录（[`Mm::sync`]）。
    pub fn generations(&self) -> Generations {
        Generations {
            tlb: self.generation.clone(),
            code: self.code_generation.clone(),
            log: self.code_log.clone(),
        }
    }

    /// guest 修改了 `[start, start + len)` 里的代码（`ic ivau`），在下一次 [`Mm::sync`]
    /// 时作废这里的翻译。
    pub fn invalidate_code(&mut self, start: u64, len: u64) {
        self.code_ranges.push((start, len));
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

    /// 如果有映射被拆除或降低了权限，做一次 TLB shootdown，然后释放被解除映射的宿主内存；
    /// 有代码要作废时同样等所有使用者退出 guest 代码。
    ///
    /// 调用方持有 BKL；`current` 是调用线程，它自己在重新进入 guest 代码之前刷新。
    pub fn sync(&mut self, current: &Arc<TaskSync>) {
        let code = std::mem::take(&mut self.code_ranges);
        let dirty = std::mem::take(&mut self.dirty);
        if !dirty && code.is_empty() {
            return;
        }
        // 先记下范围、再改代数：读到新代数的使用者一定能找到对应的范围。SeqCst 与使用者
        // 那边"先标记在执行 guest 代码、再读代数"配对：要么它读到新的代数，要么这里看到
        // 它正在执行，等它被打断后退出
        if !code.is_empty() {
            let generation = self.code_generation.load(Ordering::Relaxed) + 1;
            self.code_log.lock().unwrap().push(generation, &code);
            self.code_generation.store(generation, Ordering::SeqCst);
        }
        if dirty {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
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

    /// 把 VMA 的一段放进页表。`PROT_NONE` 的不放：V8、JVM 预留的大片地址空间逐页写
    /// 页表很占内存（8 GB 要 16 MB），mprotect 加上权限时再放（[`Mm::mprotect`]）。
    fn map_pages(
        mmu: &mut Mmu,
        start: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), Errno> {
        if mmu_prot(prot) == 0 || len == 0 {
            return Ok(());
        }
        // SAFETY: 宿主内存由 VMA 持有，生命周期不短于页表里的映射
        if let Err(e) = unsafe { mmu.map(start, host, len, mmu_prot(prot)) } {
            let _ = mmu.unmap(start, len);
            return Err(map_err(e));
        }
        Ok(())
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
        Self::map_pages(&mut self.mmu, start, host, len, prot)?;
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
                file_id: None,
                file: None,
                shm: None,
                locked: self.lock_future,
                written: Cell::new(prot & prot::WRITE != 0),
            },
        );
        Ok(())
    }

    /// 把一个完整的 VMA 放到 `start`；调用方保证区间空闲。
    fn insert_vma(&mut self, start: u64, v: Vma) -> Result<(), Errno> {
        Self::map_pages(&mut self.mmu, start, v.host, v.file_len, v.prot)?;
        self.vmas.insert(start, v);
        Ok(())
    }

    /// 文件映射超出文件末尾的页从页表里拿掉（[`Vma::file_len`]）。
    fn trim_to_file(&mut self, start: u64) -> Result<(), Errno> {
        let v = &self.vmas[&start];
        if v.file_len < v.len {
            self.mmu
                .unmap(start + v.file_len, v.len - v.file_len)
                .map_err(map_err)?;
        }
        Ok(())
    }

    /// guest 访问 `addr` 出错时调用。地址落在文件映射超出文件末尾的部分时返回
    /// `Some(true)`，应当发 SIGBUS；共享映射的文件后来变长了，就把新的内容映射进来，
    /// 返回 `Some(false)`，重新执行那条指令即可。其余情况返回 None。
    pub fn fault_past_eof(&mut self, addr: u64) -> Option<bool> {
        let (&s, v) = self.vmas.range(..=addr).next_back()?;
        if addr >= s + v.len || addr < s + v.file_len {
            return None;
        }
        if let (Backing::Shared, Some((fd, offset))) = (v.backing, &v.file) {
            let extent = file_extent(std::os::fd::AsRawFd::as_raw_fd(&**fd), *offset, v.len);
            if addr < s + extent {
                let (from, prot) = (v.file_len, v.prot);
                // SAFETY: 宿主映射覆盖整个 VMA，[from, extent) 现在有文件内容
                let host = unsafe { v.host.add(from as usize) };
                Self::map_pages(&mut self.mmu, s + from, host, extent - from, prot).ok()?;
                self.vmas.get_mut(&s).unwrap().file_len = extent;
                return Some(false);
            }
        }
        Some(true)
    }

    /// 新的匿名宿主内存。宿主内存总是可读写（guest 的权限由页表管），所以 `reserve`
    /// 按 Linux 的提交量规则由调用方决定：只有可写的私有映射计入，`MAP_NORESERVE`、
    /// 只读和 `PROT_NONE` 的不计入。V8 为每块 wasm 内存预留 8 GB 的 `PROT_NONE`，
    /// 计入的话在内存小的宿主上直接失败。
    fn new_anonymous(len: u64, reserve: bool) -> Result<(Rc<HostRegion>, NonNull<u8>), Errno> {
        let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
        let base = appine_hal::map_anonymous(host_len, reserve).map_err(|_| Errno::ENOMEM)?;
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
        let (region, host) = Self::new_anonymous(len, prot & prot::WRITE != 0)?;
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
                let reserve =
                    prot & prot::WRITE != 0 && (shared || flags & flags::MAP_NORESERVE == 0);
                let (region, host) = Self::new_anonymous(len, reserve)?;
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
            Some((fd, offset, _)) => file_extent(fd, offset, len),
            None => len,
        };
        // st_dev 在 macOS 上是 i32
        #[allow(clippy::unnecessary_cast)]
        let file_id = file.and_then(|(fd, offset, _)| {
            appine_hal::fstat(fd)
                .ok()
                .map(|s| (s.st_dev as u64, s.st_ino as u64, offset))
        });
        let dup = match file {
            // SAFETY: fd 在这次调用期间一直打开着
            Some((fd, offset, _)) if shared => Some((
                Rc::new(
                    unsafe { BorrowedFd::borrow_raw(fd) }
                        .try_clone_to_owned()
                        .map_err(|_| Errno::EMFILE)?,
                ),
                offset,
            )),
            _ => None,
        };
        self.insert(start, len, prot, backing, may_write, region, host)?;
        let v = self.vmas.get_mut(&start).unwrap();
        v.file_len = file_len;
        v.file_id = file_id;
        v.file = dup;
        self.trim_to_file(start)?;
        Ok(start)
    }

    /// 实现 mremap(2)：`fixed` 是 `MREMAP_FIXED` 的目标地址。变长时先尝试原地扩展，不行再搬到新的地址（`may_move`）：搬家不复制
    /// 数据，只是把同一段宿主内存映射到新地址。新增的部分是匿名内存，共享文件映射
    /// 则接着映射文件后面的内容；私有文件映射不能扩展。
    pub fn mremap(
        &mut self,
        old: u64,
        old_len: u64,
        new_len: u64,
        may_move: bool,
        fixed: Option<u64>,
    ) -> Result<u64, Errno> {
        let old_len = page_up(old_len).filter(|&l| l != 0).ok_or(Errno::EINVAL)?;
        let new_len = page_up(new_len).filter(|&l| l != 0).ok_or(Errno::EINVAL)?;
        let old_end = old.checked_add(old_len).ok_or(Errno::EINVAL)?;
        if !old.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        // Linux 要求原来的区间在一个 VMA 里；这里不合并相邻的 VMA（比如原地扩展过的），
        // 所以只要求区间被首尾相接的 VMA 完全覆盖
        self.check_mapped(old, old_len).map_err(|_| Errno::EFAULT)?;
        let (&s, v) = self.vmas.range(..old_end).next_back().unwrap();
        let vma_end = s + v.len;
        let template = v.clone();
        let grow = new_len.saturating_sub(old_len);
        // 扩展出来的部分接在原来区间的后面，对应 VMA 里的偏移 old_end - s
        let extension = |mm: &Mm| -> Result<Option<Vma>, Errno> {
            (grow > 0)
                .then(|| mm.extension(&template, old_end - s, grow))
                .transpose()
        };
        if let Some(dst) = fixed {
            if !dst.is_multiple_of(PAGE_SIZE)
                || dst.checked_add(new_len).is_none_or(|e| e > 1 << VA_BITS)
                || dst < old_end && old < dst + new_len
            {
                return Err(Errno::EINVAL);
            }
            let ext = extension(self)?;
            self.munmap(dst, new_len)?;
            if new_len < old_len {
                self.munmap(old + new_len, old_len - new_len)?;
            }
            self.move_range(old, old_len.min(new_len), dst)?;
            if let Some(e) = ext {
                self.insert_vma(dst + old_len, e)?;
            }
            return Ok(dst);
        }
        if grow == 0 {
            if new_len < old_len {
                self.munmap(old + new_len, old_len - new_len)?;
            }
            return Ok(old);
        }
        if old_end == vma_end && old_end + grow <= MMAP_TOP && self.is_free(old_end, grow) {
            let e = extension(self)?.unwrap();
            self.insert_vma(old_end, e)?;
            return Ok(old);
        }
        if !may_move {
            return Err(Errno::ENOMEM);
        }
        let dst = self.find_free(new_len).ok_or(Errno::ENOMEM)?;
        let e = extension(self)?.unwrap();
        self.move_range(old, old_len, dst)?;
        self.insert_vma(dst + old_len, e)?;
        Ok(dst)
    }

    /// mremap 扩展出来的 `len` 字节：接在 `template` 里偏移 `offset` 处之后，类型和
    /// 权限与它相同。
    fn extension(&self, template: &Vma, offset: u64, len: u64) -> Result<Vma, Errno> {
        let (region, host, file) = match (&template.backing, &template.file) {
            (Backing::Shared, Some((fd, start))) => {
                let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
                let file_offset = start + offset;
                let writable = template.may_write;
                let m = appine_hal::map_file(
                    std::os::fd::AsRawFd::as_raw_fd(&**fd),
                    file_offset,
                    host_len,
                    true,
                    writable,
                )
                .map_err(Errno)?;
                let region = Rc::new(HostRegion {
                    base: m.base,
                    len: m.len,
                });
                (region, m.ptr, Some((fd.clone(), file_offset)))
            }
            (Backing::PrivateFile, _) => return Err(Errno::ENOMEM),
            _ => {
                let (region, host) = Self::new_anonymous(len, template.prot & prot::WRITE != 0)?;
                (region, host, None)
            }
        };
        let file_len = file.as_ref().map_or(len, |(fd, offset)| {
            file_extent(std::os::fd::AsRawFd::as_raw_fd(&**fd), *offset, len)
        });
        Ok(Vma {
            len,
            prot: template.prot,
            backing: template.backing,
            may_write: template.may_write,
            region,
            name: template.name.clone(),
            host,
            file_len,
            cow: false,
            file_id: template.file_id.map(|(d, i, o)| (d, i, o + offset)),
            file,
            shm: None,
            locked: template.locked,
            written: Cell::new(template.prot & prot::WRITE != 0),
        })
    }

    /// 把 `[old, old + len)` 里的映射原样搬到 `dst`（宿主内存不动）。
    fn move_range(&mut self, old: u64, len: u64, dst: u64) -> Result<(), Errno> {
        self.split_at(old);
        self.split_at(old + len);
        let starts: Vec<u64> = self.vmas.range(old..old + len).map(|(&s, _)| s).collect();
        let mut moved = Vec::with_capacity(starts.len());
        for s in starts {
            let v = self.vmas.remove(&s).unwrap();
            self.mmu.unmap(s, v.len).map_err(map_err)?;
            if v.prot & prot::EXEC != 0 {
                self.code_ranges.push((s, v.len));
            }
            moved.push((dst + (s - old), v));
        }
        for (s, v) in moved {
            self.insert_vma(s, v)?;
        }
        self.dirty = true;
        Ok(())
    }

    /// shmat：把共享内存段 `id` 映射到 `addr`（None 时自己找地方）。`remap` 时可以
    /// 覆盖已有的映射，否则区间必须空闲。
    pub fn attach_shm(
        &mut self,
        addr: Option<u64>,
        mem: &SharedMemory,
        prot: u32,
        remap: bool,
        id: i32,
    ) -> Result<u64, Errno> {
        let len = mem.len();
        let start = match addr {
            Some(a) => {
                if a.checked_add(len).is_none_or(|e| e > 1 << VA_BITS) {
                    return Err(Errno::EINVAL);
                }
                if !self.is_free(a, len) {
                    if !remap {
                        return Err(Errno::EINVAL);
                    }
                    self.munmap(a, len)?;
                }
                a
            }
            None => self.find_free(len).ok_or(Errno::ENOMEM)?,
        };
        let host = mem.region.base;
        self.insert(
            start,
            len,
            prot,
            Backing::Shared,
            true,
            mem.region.clone(),
            host,
        )?;
        self.vmas.get_mut(&start).unwrap().shm = Some((id, start));
        Ok(start)
    }

    /// shmdt：解除从 `addr` 开始的那次 shmat 的映射，返回段的 id。
    pub fn detach_shm(&mut self, addr: u64) -> Result<i32, Errno> {
        let id = self
            .vmas
            .get(&addr)
            .and_then(|v| v.shm)
            .filter(|&(_, base)| base == addr)
            .map(|(id, _)| id)
            .ok_or(Errno::EINVAL)?;
        let parts: Vec<(u64, u64)> = self
            .vmas
            .range(addr..)
            .take_while(|(_, v)| v.shm == Some((id, addr)))
            .map(|(&s, v)| (s, v.len))
            .collect();
        for (s, len) in parts {
            self.munmap(s, len)?;
        }
        Ok(id)
    }

    /// 共享内存段 `id` 在这个地址空间里被 shmat 了几次。
    pub fn shm_attachments(&self, id: i32) -> usize {
        let mut bases: Vec<u64> = self
            .vmas
            .values()
            .filter_map(|v| v.shm.filter(|&(i, _)| i == id).map(|(_, b)| b))
            .collect();
        bases.sort_unstable();
        bases.dedup();
        bases.len()
    }

    /// 起点在 `[start, start + len)` 里的映射以后不能改成可写（只读打开的 /dev/zero 的
    /// 共享映射）。
    pub fn deny_write(&mut self, start: u64, len: u64) {
        for (_, v) in self.vmas.range_mut(start..start.saturating_add(len)) {
            v.may_write = false;
        }
    }

    /// mlock、munlock：锁住或解锁 `[addr, addr + len)`（按页扩大），区间必须完全已映射。
    pub fn set_locked(&mut self, addr: u64, len: u64, locked: bool) -> Result<(), Errno> {
        let start = addr & !(PAGE_SIZE - 1);
        let end = page_up(addr.checked_add(len).ok_or(Errno::ENOMEM)?).ok_or(Errno::ENOMEM)?;
        self.check_mapped(start, end - start)?;
        self.split_at(start);
        self.split_at(end);
        for (_, v) in self.vmas.range_mut(start..end) {
            v.locked = locked;
        }
        Ok(())
    }

    /// mlockall、munlockall：锁住或解锁所有映射。
    pub fn set_all_locked(&mut self, locked: bool) {
        for v in self.vmas.values_mut() {
            v.locked = locked;
        }
    }

    /// 锁住的字节数（`/proc/<pid>/status` 的 VmLck、`RLIMIT_MEMLOCK`）。
    pub fn locked_bytes(&self) -> u64 {
        self.vmas.values().filter(|v| v.locked).map(|v| v.len).sum()
    }

    /// `[addr, addr + len)` 里已经锁住的字节数（mlock 只为新锁住的部分计数）。
    pub fn locked_in(&self, addr: u64, len: u64) -> u64 {
        let end = addr.saturating_add(len);
        self.vmas
            .iter()
            .filter(|&(&s, v)| v.locked && s < end && s + v.len > addr)
            .map(|(&s, v)| (s + v.len).min(end) - s.max(addr))
            .sum()
    }

    /// madvise 关心的区间性质：(区间里有没有锁住的页, 是不是全部是私有的匿名内存)。
    /// 区间必须完全已映射。
    pub fn advice_range(&self, addr: u64, len: u64) -> Result<(bool, bool), Errno> {
        self.check_mapped(addr, len)?;
        let end = addr + len;
        let mut locked = false;
        let mut anon = true;
        if let Some((_, v)) = self.vmas.range(..addr).next_back() {
            locked |= v.locked;
            anon &= v.backing == Backing::Anonymous;
        }
        for (_, v) in self.vmas.range(addr..end) {
            locked |= v.locked;
            anon &= v.backing == Backing::Anonymous;
        }
        Ok((locked, anon))
    }

    /// mincore：`[addr, addr + len)` 里每页是否在宿主内存里。区间必须完全已映射。
    pub fn residency(&self, addr: u64, len: u64) -> Result<Vec<bool>, Errno> {
        let end = page_up(addr.checked_add(len).ok_or(Errno::ENOMEM)?).ok_or(Errno::ENOMEM)?;
        self.check_mapped(addr, end - addr)?;
        let mut out = Vec::with_capacity(((end - addr) / PAGE_SIZE) as usize);
        let mut cursor = addr;
        while cursor < end {
            let (&s, v) = self.vmas.range(..=cursor).next_back().unwrap();
            let stop = end.min(s + v.len);
            // SAFETY: [cursor, stop) 落在这个 VMA 里，宿主内存由它持有
            let host = unsafe { v.host.as_ptr().add((cursor - s) as usize) };
            out.extend(appine_hal::page_residency(host, (stop - cursor) as usize));
            cursor = stop;
        }
        Ok(out)
    }

    /// msync(2) 的范围检查：区间必须完全已映射。
    pub fn check_mapped(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let end = page_up(len)
            .and_then(|l| addr.checked_add(l))
            .ok_or(Errno::ENOMEM)?;
        let mut cursor = addr;
        while cursor < end {
            let (&s, v) = self
                .vmas
                .range(..=cursor)
                .next_back()
                .filter(|&(&s, v)| s + v.len > cursor)
                .ok_or(Errno::ENOMEM)?;
            cursor = s + v.len;
        }
        Ok(())
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
        if let Some(f) = &mut tail.file_id {
            f.2 += head_len;
        }
        if let Some(f) = &mut tail.file {
            f.1 += head_len;
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
            if vma.prot & prot::EXEC != 0 {
                self.code_ranges.push((s, len));
            }
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
        for (&s, v) in self.vmas.range_mut(addr..end) {
            // 原来可执行的页：不再可执行时翻译要作废；W^X 的 JIT 改写代码之前也先去掉执行权限
            if v.prot & prot::EXEC != 0 && v.prot != prot {
                self.code_ranges.push((s, v.len));
            }
            // 只增加权限时 TLB 里的旧表项只是少了权限，未命中时会重新填写，不必刷新
            let (old, new) = (mmu_prot(v.prot), mmu_prot(prot));
            self.dirty |= old & !new != 0;
            // PROT_NONE 的页不在页表里（[`Mm::map_pages`]）：去掉全部权限时拿掉，从没有
            // 权限变成有权限时放进去（文件映射只放文件末尾之前的部分）
            if new == 0 {
                self.mmu.unmap(s, v.len).map_err(map_err)?;
            } else if old == 0 {
                Self::map_pages(&mut self.mmu, s, v.host, v.file_len, prot)?;
            } else {
                self.mmu.protect(s, v.len, new).map_err(map_err)?;
            }
            v.prot = prot;
            if prot & prot::WRITE != 0 {
                v.written.set(true);
            }
        }
        Ok(())
    }

    /// madvise(MADV_DONTNEED)：匿名内存交给宿主回收（之后读到零），其余映射保持不变。
    /// 区间必须完全已映射。
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
                if v.prot & prot::EXEC != 0 {
                    self.code_ranges.push((cursor, stop - cursor));
                }
                // SAFETY: [cursor, stop) 落在这个 VMA 内，对应的宿主内存是它私有的匿名内存
                unsafe {
                    discard_host(v.host.add((cursor - s) as usize), (stop - cursor) as usize)
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
        child.exe = self.exe;
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
                let (region, host) = Self::new_anonymous(v.len, v.prot & prot::WRITE != 0)?;
                if v.backing != Backing::Anonymous || v.written.get() {
                    // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
                    unsafe {
                        copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), v.file_len as usize)
                    };
                }
                (region, host)
            };
            child.insert(start, v.len, v.prot, v.backing, v.may_write, region, host)?;
            let c = child.vmas.get_mut(&start).unwrap();
            c.written.set(v.written.get());
            c.name = v.name.clone();
            c.file_len = v.file_len;
            c.cow = v.cow && share;
            c.file_id = v.file_id;
            c.file = v.file.clone();
            c.shm = v.shm;
            child.trim_to_file(start)?;
        }
        Ok(child)
    }

    /// 给一个和别的地址空间共用宿主内存的私有文件映射复制一份自己的。
    fn unshare(&mut self, start: u64) -> Result<(), Errno> {
        let v = &self.vmas[&start];
        let (region, host) = Self::new_anonymous(v.len, v.prot & prot::WRITE != 0)?;
        // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
        unsafe { copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), v.file_len as usize) };
        // 超出文件末尾的页仍然不进页表
        let (file_len, p) = (v.file_len, v.prot);
        Self::map_pages(&mut self.mmu, start, host, file_len, p)?;
        let v = self.vmas.get_mut(&start).unwrap();
        let old = std::mem::replace(&mut v.region, region);
        v.host = host;
        v.cow = false;
        // 其他线程的 TLB 里可能还有旧的翻译：shootdown 之后才释放
        self.graveyard.push(old);
        self.dirty = true;
        Ok(())
    }

    /// 宿主文件 (`dev`, `ino`) 被截短成 `size` 字节：映射它的 VMA 里超出新末尾的页从
    /// 页表里拿掉，之后 guest 访问得到 SIGBUS，与 Linux 的 truncate 相同。不拿掉的话，
    /// 宿主访问超出文件末尾的映射会收到 SIGBUS，整个 App 退出。返回有没有改动，有改动时
    /// 要做 shootdown（[`Mm::sync`]）。
    pub fn file_truncated(&mut self, dev: u64, ino: u64, size: u64) -> bool {
        let mut changed = false;
        for (&s, v) in self.vmas.iter_mut() {
            let Some((d, i, offset)) = v.file_id else {
                continue;
            };
            let extent = size
                .saturating_sub(offset)
                .next_multiple_of(PAGE_SIZE)
                .min(v.len);
            if (d, i) != (dev, ino) || extent >= v.file_len {
                continue;
            }
            // unmap 只会因为参数不合法失败，这里的区间来自 VMA
            let _ = self.mmu.unmap(s + extent, v.file_len - extent);
            if v.prot & prot::EXEC != 0 {
                self.code_ranges.push((s + extent, v.file_len - extent));
            }
            v.file_len = extent;
            changed = true;
        }
        self.dirty |= changed;
        changed
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

    /// 内核自己往 guest 内存写入，比如加载程序、构造初始栈。只要求已映射，不检查权限，
    /// 所以按 VMA 找宿主内存：`PROT_NONE` 的页不在页表里。文件映射超出文件末尾的部分
    /// 不能写。
    pub(crate) fn write_kernel(&self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        let mut done = 0;
        while done < data.len() {
            let cur = addr.checked_add(done as u64).ok_or(Errno::EFAULT)?;
            let (&s, v) = self
                .vmas
                .range(..=cur)
                .next_back()
                .filter(|&(&s, v)| cur < s + v.file_len)
                .ok_or(Errno::EFAULT)?;
            let chunk = ((s + v.file_len - cur) as usize).min(data.len() - done);
            v.written.set(true);
            // SAFETY: [cur, cur + chunk) 落在这个 VMA 有内容的部分，宿主内存由它持有
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data[done..].as_ptr(),
                    v.host.as_ptr().add((cur - s) as usize),
                    chunk,
                )
            };
            done += chunk;
        }
        Ok(())
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
    fn only_executable_changes_invalidate_translations() {
        let mut mm = Mm::new();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let rx = prot::READ | prot::EXEC;
        let data = mm.mmap(0, 2 * PAGE_SIZE, RW, anon, None).unwrap();
        let code = mm.mmap(0, 2 * PAGE_SIZE, rx, anon, None).unwrap();
        // 数据页的变化只要刷新 TLB
        mm.munmap(data, PAGE_SIZE).unwrap();
        mm.mprotect(data + PAGE_SIZE, PAGE_SIZE, prot::READ)
            .unwrap();
        mm.mprotect(code, PAGE_SIZE, rx).unwrap();
        assert!(std::mem::take(&mut mm.dirty) && mm.code_ranges.is_empty());
        // 只增加权限：TLB 不必刷新
        mm.mprotect(data + PAGE_SIZE, PAGE_SIZE, RW).unwrap();
        assert!(!mm.dirty);
        // 代码页去掉执行权限（W^X 的 JIT 改写代码之前）、被拆除：这些页的翻译要作废
        mm.mprotect(code, PAGE_SIZE, RW).unwrap();
        assert_eq!(std::mem::take(&mut mm.code_ranges), [(code, PAGE_SIZE)]);
        mm.munmap(code + PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(mm.code_ranges, [(code + PAGE_SIZE, PAGE_SIZE)]);
    }

    #[test]
    fn code_log_reports_missed_ranges() {
        let mut log = CodeLog::default();
        log.push(1, &[(0x1000, 0x1000)]);
        log.push(2, &[(0x5000, 0x2000), (0x9000, 0x1000)]);
        assert_eq!(log.since(0, 2).unwrap().len(), 3);
        assert_eq!(
            log.since(1, 2),
            Some(vec![(0x5000, 0x2000), (0x9000, 0x1000)])
        );
        // 读到的代数之后才记下的范围不算
        assert_eq!(log.since(0, 1), Some(vec![(0x1000, 0x1000)]));
        let last = CodeLog::GENERATIONS + 36;
        for g in 3..=last {
            log.push(g, &[(g << 12, 0x1000)]);
        }
        // 太旧的记录已经挤掉：只能整体清空
        assert_eq!(log.since(0, last), None);
        assert_eq!(log.since(last - 1, last), Some(vec![(last << 12, 0x1000)]));
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
    fn fork_skips_only_never_written_anonymous_mappings() {
        let mut mm = Mm::new();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        // V8 式的预留：PROT_NONE 加 MAP_NORESERVE，再把一部分改成可写
        let reserved = mm
            .mmap(0, 1 << 33, 0, anon | flags::MAP_NORESERVE, None)
            .unwrap();
        mm.mprotect(reserved, PAGE_SIZE, RW).unwrap();
        mm.write_kernel(reserved, b"heap").unwrap();
        // 写过之后去掉权限：内容还在，fork 要复制
        let hidden = mm.mmap(0, PAGE_SIZE, RW, anon, None).unwrap();
        mm.write_kernel(hidden, b"kept").unwrap();
        mm.mprotect(hidden, PAGE_SIZE, 0).unwrap();
        // 加载器那样映射成只读再由内核写入
        let text = mm.mmap(0, PAGE_SIZE, prot::READ, anon, None).unwrap();
        mm.write_kernel(text, b"code").unwrap();
        let child = mm.fork().unwrap();
        let mut buf = [0u8; 4];
        child.read(UserSlice::new(reserved, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"heap");
        child.read(UserSlice::new(text, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"code");
        // PROT_NONE 的页不在页表里，权限加回来之后内容还在
        let mut child = child;
        assert!(child.mmu().translate(hidden, 0).is_none());
        assert!(child.mmu().translate(reserved + PAGE_SIZE, 0).is_none());
        child.mprotect(hidden, PAGE_SIZE, prot::READ).unwrap();
        child.read(UserSlice::new(hidden, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"kept");
        assert!(!child.vmas[&(reserved + PAGE_SIZE)].written.get());
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
        let mut child = mm.fork().unwrap();
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
        // 超出文件末尾的页不在页表里，复制一份之后也一样
        for m in [&mut mm, &mut child] {
            for base in [ro, rw] {
                assert!(m.mmu().translate(base + PAGE_SIZE, prot::READ).is_some());
                assert!(m.mmu().translate(base + 2 * PAGE_SIZE, 0).is_none());
                assert_eq!(m.fault_past_eof(base + 2 * PAGE_SIZE), Some(true));
                assert_eq!(m.fault_past_eof(base + PAGE_SIZE), None);
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shared_file_mappings_follow_the_end_of_the_file() {
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!("appine-eof-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(100).unwrap();
        let mut mm = Mm::new();
        let a = mm
            .mmap(
                0,
                4 * PAGE_SIZE,
                RW,
                flags::MAP_SHARED,
                Some((f.as_raw_fd(), 0, true)),
            )
            .unwrap();
        assert!(mm.mmu().translate(a, prot::WRITE).is_some());
        assert!(mm.mmu().translate(a + PAGE_SIZE, 0).is_none());
        assert_eq!(mm.fault_past_eof(a + PAGE_SIZE), Some(true));
        // 文件变长之后，访问新的部分只需要重新执行
        f.set_len(2 * PAGE_SIZE + 1).unwrap();
        assert_eq!(mm.fault_past_eof(a + 2 * PAGE_SIZE), Some(false));
        assert!(mm.mmu().translate(a + 2 * PAGE_SIZE, prot::WRITE).is_some());
        assert_eq!(mm.fault_past_eof(a + 3 * PAGE_SIZE), Some(true));
        // 不在任何映射里的地址交给普通的 SIGSEGV 处理
        assert_eq!(mm.fault_past_eof(a + 4 * PAGE_SIZE), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncation_removes_pages_past_the_new_end() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let path = std::env::temp_dir().join(format!("appine-trunc-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(3 * PAGE_SIZE).unwrap();
        let mut mm = Mm::new();
        let file = |shared| Some((f.as_raw_fd(), 0, shared));
        let shared = mm
            .mmap(0, 3 * PAGE_SIZE, RW, flags::MAP_SHARED, file(true))
            .unwrap();
        let private = mm
            .mmap(
                0,
                3 * PAGE_SIZE,
                prot::READ,
                flags::MAP_PRIVATE,
                file(false),
            )
            .unwrap();
        let hidden = mm
            .mmap(0, 3 * PAGE_SIZE, 0, flags::MAP_PRIVATE, file(false))
            .unwrap();
        let meta = f.metadata().unwrap();
        // 别的文件变短不影响这些映射
        assert!(!mm.file_truncated(meta.dev(), meta.ino() + 1, 0));
        f.set_len(PAGE_SIZE + 10).unwrap();
        assert!(mm.file_truncated(meta.dev(), meta.ino(), PAGE_SIZE + 10));
        for a in [shared, private] {
            // 文件末尾所在的那一页还在，后面的页访问时得到 SIGBUS
            assert!(mm.mmu().translate(a + PAGE_SIZE, prot::READ).is_some());
            assert!(mm.mmu().translate(a + 2 * PAGE_SIZE, 0).is_none());
            assert_eq!(mm.fault_past_eof(a + 2 * PAGE_SIZE), Some(true));
        }
        // 没有权限的映射改成可读时也只放文件末尾之前的页
        mm.mprotect(hidden, 3 * PAGE_SIZE, prot::READ).unwrap();
        assert!(mm.mmu().translate(hidden + PAGE_SIZE, prot::READ).is_some());
        assert!(mm.mmu().translate(hidden + 2 * PAGE_SIZE, 0).is_none());
        // 再次截短到同样的长度没有改动；共享映射的文件变长之后可以接着访问
        assert!(!mm.file_truncated(meta.dev(), meta.ino(), PAGE_SIZE + 10));
        f.set_len(3 * PAGE_SIZE).unwrap();
        assert_eq!(mm.fault_past_eof(shared + 2 * PAGE_SIZE), Some(false));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prot_none_stays_out_of_the_page_table() {
        let mut mm = Mm::new();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_NORESERVE;
        // V8 为一块 wasm 内存预留的大小：一页都不进页表
        let a = mm.mmap(0, 1 << 33, 0, anon, None).unwrap();
        let page = a + (1 << 32);
        assert!(mm.mmu().translate(page, 0).is_none());
        mm.mprotect(page, PAGE_SIZE, RW).unwrap();
        mm.write(UserSlice::new(page, 2), b"hi").unwrap();
        assert!(mm.mmu().translate(page + PAGE_SIZE, 0).is_none());
        // 去掉权限再加回来，内容还在
        mm.mprotect(page, PAGE_SIZE, 0).unwrap();
        assert!(mm.mmu().translate(page, 0).is_none());
        mm.mprotect(page, PAGE_SIZE, prot::READ).unwrap();
        let mut buf = [0u8; 2];
        mm.read(UserSlice::new(page, 2), &mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        // 内核写入不看页表：没有权限的页也能写
        mm.write_kernel(a, b"k").unwrap();
        mm.munmap(a, 1 << 33).unwrap();
        assert!(mm.mmu().translate(page, 0).is_none());
    }

    #[test]
    fn discard_zeroes_private_anonymous_memory() {
        let mut mm = Mm::new();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let len = 8 * PAGE_SIZE;
        let a = mm.mmap(0, len, RW, anon, None).unwrap();
        mm.write(UserSlice::new(a, len as usize), &vec![7u8; len as usize])
            .unwrap();
        // 不按宿主页对齐的一段：中间交给宿主，两头清零
        mm.discard(a + PAGE_SIZE, 5 * PAGE_SIZE).unwrap();
        let mut buf = vec![0u8; len as usize];
        mm.read(UserSlice::new(a, len as usize), &mut buf).unwrap();
        let p = PAGE_SIZE as usize;
        assert!(buf[..p].iter().all(|&b| b == 7));
        assert!(buf[p..6 * p].iter().all(|&b| b == 0));
        assert!(buf[6 * p..].iter().all(|&b| b == 7));
    }

    #[test]
    fn mremap_grows_moves_and_shrinks() {
        let mut mm = Mm::new();
        let fixed = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_FIXED;
        let a = mm.mmap(0x100000, 2 * PAGE_SIZE, RW, fixed, None).unwrap();
        mm.write_kernel(a, b"keep").unwrap();
        // 后面是空的：原地扩展
        let b = mm
            .mremap(a, 2 * PAGE_SIZE, 3 * PAGE_SIZE, false, None)
            .unwrap();
        assert_eq!(b, a);
        mm.write_kernel(a + 2 * PAGE_SIZE, b"x").unwrap();
        // 后面被占住：不许搬家就失败，许可就搬到别处，内容不变
        mm.mmap(a + 3 * PAGE_SIZE, PAGE_SIZE, RW, fixed, None)
            .unwrap();
        assert_eq!(
            mm.mremap(a, 3 * PAGE_SIZE, 5 * PAGE_SIZE, false, None),
            Err(Errno::ENOMEM)
        );
        let c = mm
            .mremap(a, 3 * PAGE_SIZE, 5 * PAGE_SIZE, true, None)
            .unwrap();
        assert_ne!(c, a);
        assert!(!mm.is_mapped(a));
        let mut buf = [0u8; 4];
        mm.read(UserSlice::new(c, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"keep");
        mm.read(UserSlice::new(c + 4 * PAGE_SIZE, 4), &mut buf)
            .unwrap();
        assert_eq!(buf, [0; 4]);
        // 缩小：去掉尾部
        assert_eq!(mm.mremap(c, 5 * PAGE_SIZE, PAGE_SIZE, false, None), Ok(c));
        assert!(!mm.is_mapped(c + PAGE_SIZE));
        // 没有完全映射的区间
        assert_eq!(
            mm.mremap(c, 2 * PAGE_SIZE, PAGE_SIZE, true, None),
            Err(Errno::EFAULT)
        );
        assert_eq!(mm.check_mapped(c, 2 * PAGE_SIZE), Err(Errno::ENOMEM));
        assert_eq!(mm.check_mapped(c, PAGE_SIZE), Ok(()));
    }

    #[test]
    fn mremap_extends_shared_file_mappings_with_the_file() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!("appine-mremap-{}", std::process::id()));
        let mut f = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(&vec![1u8; PAGE_SIZE as usize]).unwrap();
        f.write_all(&vec![2u8; PAGE_SIZE as usize]).unwrap();
        let mut mm = Mm::new();
        let a = mm
            .mmap(
                0,
                PAGE_SIZE,
                RW,
                flags::MAP_SHARED,
                Some((f.as_raw_fd(), 0, true)),
            )
            .unwrap();
        drop(f);
        let b = mm.mremap(a, PAGE_SIZE, 2 * PAGE_SIZE, true, None).unwrap();
        let mut buf = [0u8; 1];
        mm.read(UserSlice::new(b + PAGE_SIZE, 1), &mut buf).unwrap();
        assert_eq!(buf, [2]);
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
