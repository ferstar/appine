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
//! fork 时私有匿名内存写时复制（设计文档 §5.3），其余的私有映射拷贝一份，共享映射与
//! 子进程共用。写时复制按页进行：fork 时写过的页不复制，父子进程的页表都指向父进程的
//! 宿主页、去掉写权限。宿主页在它的 VMA 里的一方是拥有者，它把页借出；别的地址空间借用。
//! - 借用方要写时复制一份到自己 VMA 里对应的位置（[`Mm::materialize`]）。
//! - 拥有者要写、解除映射、搬走或者丢弃借出的页之前，先让每个借用方复制自己的一份
//!   （[`Mm::recall`]）。
//! - 拥有者的地址空间销毁时页留给借用方（孤儿页，谁也不能再写），它持有那段宿主内存。
//! - 最后一个借用方还回来时，拥有者的页表项马上恢复写权限：shell fork 之后子进程马上
//!   exec，父进程不必为此缺页。
//!
//! 页表项改指别处之前，使用这个地址空间的线程要先刷新 TLB（[`Mm::shootdown`]）；只是
//! 恢复写权限不必。

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::mem::ManuallyDrop;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use appine_engine::{MapError, Mmu, PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::loader::{STACK_SIZE, STACK_TOP};
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

/// 栈顶和 mmap 分配区上界之间至少留的间隔：默认的栈大小上限再加 1 GiB。Linux 是
/// 128 MB 加上栈的随机偏移（arm64 上最多约 1 GB）。
const MIN_GAP: u64 = STACK_SIZE + (1 << 30);
/// 这个间隔的上限，与 Linux 的 `MAX_GAP` 相同。
const MAX_GAP: u64 = STACK_TOP / 6 * 5;
/// 最低可分配地址，与 Linux 的 mmap_min_addr 默认值相同。
const MMAP_MIN: u64 = 0x10000;
/// 向下增长的映射与下面的映射之间至少要留的间隔（Linux 的 `stack_guard_gap`，256 页）。
const STACK_GUARD_GAP: u64 = 256 * PAGE_SIZE;
/// `/proc/sys/vm/max_map_count`：一个地址空间最多有多少个 VMA。每个 VMA 是一个宿主
/// 映射，而宿主的映射数（Android 上也是 65530）是整个 App 进程共用的。
pub(crate) static MAX_MAP_COUNT: AtomicUsize = AtomicUsize::new(65530);

/// 堆（brk）第一次扩展时预留的宿主地址空间：之后的扩展只是加长同一个 VMA。不碰的部分
/// 不占内存（`MAP_NORESERVE`）；超出的部分照旧单独映射。
const HEAP_RESERVE: u64 = 256 << 20;

/// fork 时子进程这么大以内的映射切自同一段宿主内存（[`Mm::fork`]）。这样的映射解除
/// 映射之后，宿主内存要等那一段全都不用了才交还，所以只给小的。
const FORK_PIECE_MAX: u64 = 1 << 20;

/// fork 时这么大以内的写过的匿名映射不看哪些页碰过，全部借出（[`Mm::lend`]）。
const LEND_ALL_MAX: u64 = 16 * PAGE_SIZE;

/// [`Mm::carve`] 切出来的：每个 VMA 的起点 → 那一段宿主内存和它的宿主地址。
type Carved = BTreeMap<u64, (Arc<HostRegion>, NonNull<u8>)>;

/// 一段由 [`Mm`] 持有的宿主内存。用 Arc 共享：只读文件映射的宿主映射所有地址空间共用
/// （[`FileMaps`]），execve 不持 BKL 建新的地址空间时也会拿到它。
struct HostRegion {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: 只是一段宿主地址范围，释放（Drop）时才用到它
unsafe impl Send for HostRegion {}
unsafe impl Sync for HostRegion {}

impl Drop for HostRegion {
    fn drop(&mut self) {
        let (base, len) = (self.base, self.len);
        let deferred =
            UNMAPPED.with_borrow_mut(|u| u.as_mut().map(|u| u.regions.push((base, len))));
        if deferred.is_some() {
            UNMAPPING.fetch_add(len, Ordering::Relaxed);
        } else {
            // SAFETY: 这段宿主内存归这个 HostRegion，已经没有人用它
            unsafe { appine_hal::unmap(base, len) }
        }
    }
}

/// 持有 BKL 时释放、等放开之后再交还的东西。
#[derive(Default)]
struct Unmapped {
    /// 宿主内存：(起点, 长度)。
    regions: Vec<(NonNull<u8>, usize)>,
    /// 销毁的地址空间的页表：逐个释放中间表要好几微秒。
    mmus: Vec<Mmu>,
}

thread_local! {
    /// 持有 BKL 时释放的宿主内存，放开 BKL 之后才交还宿主（[`release_unmapped`]）；
    /// 不持有 BKL 时是 None，当场交还。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static UNMAPPED: RefCell<Option<Unmapped>> = const { RefCell::new(None) };
}

/// 驻留内存的一次取样（[`Mm::rss_plan`]）。
struct RssPlan {
    /// 不用 mincore 就知道的 4K 页数：借来的页、缓存着的文件映射。
    fixed: u64,
    /// 要 mincore 的宿主范围，按起点排好。
    spans: Vec<(usize, usize)>,
}

impl RssPlan {
    /// 要量的几段合计不超过这么多页时不量，按全部驻留算（[`RssPlan::estimate`]）。
    const SMALL: usize = 256;

    /// 驻留页数的峰值用的估计：要 mincore 的几段合计不超过 1 MB 时按全部驻留算，最多
    /// 多算 1 MB。`/bin/true` 这样的短命进程退出时要量 8 段、合计 45 页，量出来 12 页，
    /// 却要持着 BKL 做 4 次 mincore（还要等宿主的 mmap_lock）。分配了大量内存的照常量。
    fn estimate(&self) -> u64 {
        let pages: usize = self.spans.iter().map(|&(_, len)| len.div_ceil(4096)).sum();
        if pages <= Self::SMALL {
            self.fixed + pages as u64
        } else {
            self.measure()
        }
    }

    /// 驻留的 4K 页数。宿主上首尾相接的合起来一次 mincore。
    fn measure(&self) -> u64 {
        let mut total = self.fixed;
        let mut rest = &self.spans[..];
        while let Some(&(start, mut len)) = rest.first() {
            let n = rest[1..]
                .iter()
                .take_while(|&&(s, l)| {
                    let next = s == start + len;
                    len += if next { l } else { 0 };
                    next
                })
                .count();
            total += appine_hal::resident_pages(start as *const u8, len) as u64;
            rest = &rest[1 + n..];
        }
        total
    }
}

/// 文件映射驻留的页数的缓存（[`Mm::resident_pages`]），键是 (设备号, inode, 文件偏移,
/// 长度)。
struct FileResidency;

impl FileResidency {
    /// 缓存的结果用多久：page cache 里一个文件的页变化得很慢。
    const TTL: std::time::Duration = std::time::Duration::from_secs(1);
    /// 最多缓存几段，满了就清空。
    const MAX: usize = 1024;

    fn get(
        key: (u64, u64, u64, u64),
        now: std::time::Instant,
        measure: impl FnOnce() -> u64,
    ) -> u64 {
        type Map = HashMap<(u64, u64, u64, u64), (u64, std::time::Instant)>;
        static CACHE: Mutex<Option<Map>> = Mutex::new(None);
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let map = cache.get_or_insert_with(HashMap::new);
        if let Some(&(n, at)) = map.get(&key)
            && now.duration_since(at) < Self::TTL
        {
            return n;
        }
        if map.len() >= Self::MAX {
            map.clear();
        }
        let n = measure();
        map.insert(key, (n, now));
        n
    }
}

/// fork 时要用的匿名宿主内存（堆和栈预留的地址空间、小映射切出来的那一段）预先映射
/// 好几段，fork 持有 BKL 时直接拿来用。同一个程序反复 fork 时要的长度总是那几个，用掉
/// 一段就在放开 BKL 之后补上（[`refill`](spare::refill)）：宿主的 mmap 原来占 fork
/// 持锁时间的四成，四个进程同时起进程时总吞吐还不如一个。
pub(crate) mod spare {
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::HostRegion;
    use crate::errno::Errno;

    /// 每个长度最多留几段。
    const PER_LEN: usize = 2;
    /// 最多记几种长度。
    const LENS: usize = 8;

    struct Spares {
        /// (长度, 起点)：`MAP_NORESERVE`、没碰过的匿名宿主内存。
        maps: Vec<(usize, usize)>,
        /// 要补上的长度，最近用到的在后面。
        wanted: Vec<usize>,
    }

    static SPARES: Mutex<Spares> = Mutex::new(Spares {
        maps: Vec::new(),
        wanted: Vec::new(),
    });

    /// `len` 字节不计入提交量（`MAP_NORESERVE`）、清零的匿名宿主内存：有预先映射好的就
    /// 拿来用，没有就当场映射。
    pub(super) fn anonymous(len: u64) -> Result<(Arc<HostRegion>, NonNull<u8>), Errno> {
        let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
        let found = {
            let mut s = SPARES.lock().unwrap_or_else(PoisonError::into_inner);
            s.wanted.retain(|&l| l != host_len);
            s.wanted.push(host_len);
            if s.wanted.len() > LENS {
                let old = s.wanted.remove(0);
                // 不再要的长度留着的也交还
                s.maps.retain(|&(l, base)| {
                    if l == old {
                        // SAFETY: 预先映射的、没人用过的宿主内存
                        unsafe { appine_hal::unmap(NonNull::new(base as *mut u8).unwrap(), l) };
                    }
                    l != old
                });
            }
            s.maps
                .iter()
                .position(|&(l, _)| l == host_len)
                .map(|i| s.maps.swap_remove(i).1)
        };
        let base = match found.and_then(|b| NonNull::new(b as *mut u8)) {
            Some(base) => base,
            None => appine_hal::map_anonymous(host_len, false).map_err(|_| Errno::ENOMEM)?,
        };
        Ok((
            Arc::new(HostRegion {
                base,
                len: host_len,
            }),
            base,
        ))
    }

    /// 补上用掉的。放开 BKL 之后调用。
    pub(crate) fn refill() {
        loop {
            let need = {
                let s = SPARES.lock().unwrap_or_else(PoisonError::into_inner);
                s.wanted
                    .iter()
                    .copied()
                    .find(|&l| s.maps.iter().filter(|m| m.0 == l).count() < PER_LEN)
            };
            let Some(len) = need else {
                return;
            };
            let Ok(base) = appine_hal::map_anonymous(len, false) else {
                return;
            };
            let mut s = SPARES.lock().unwrap_or_else(PoisonError::into_inner);
            if s.wanted.contains(&len) && s.maps.iter().filter(|m| m.0 == len).count() < PER_LEN {
                s.maps.push((len, base.as_ptr() as usize));
            } else {
                drop(s);
                // SAFETY: 刚刚映射、没人用过
                unsafe { appine_hal::unmap(base, len) };
            }
        }
    }
}

/// 释放了、还没交还宿主的字节数：OOM killer 等它们交还了再量。
pub(crate) static UNMAPPING: AtomicUsize = AtomicUsize::new(0);

/// 开始持有 BKL：之后释放的宿主内存等放开 BKL 再交还（[`release_unmapped`]）。
pub(crate) fn defer_unmaps() {
    UNMAPPED.with_borrow_mut(|u| {
        u.get_or_insert_with(Unmapped::default);
    });
}

/// 放开了 BKL：把持有期间释放的宿主内存交还宿主。进程退出、execve 时一次有十几二十个
/// 映射，宿主拆页表、刷 TLB 要几十微秒，不必让等 BKL 的线程（等着回收它的 shell、管道
/// 另一头的进程）跟着等。首尾相接的一起交还，少刷几次 TLB。
pub(crate) fn release_unmapped() {
    let Some(Unmapped {
        regions: mut list,
        mmus,
    }) = UNMAPPED.with_borrow_mut(Option::take)
    else {
        return;
    };
    // 先释放页表，再交还它指向的宿主内存
    drop(mmus);
    if list.is_empty() {
        return;
    }
    list.sort_unstable_by_key(|&(base, _)| base);
    let total: usize = list.iter().map(|&(_, len)| len).sum();
    let mut run = list[0];
    for &(base, len) in &list[1..] {
        // SAFETY: 两段都是交还的宿主内存；前一段在宿主上正好到 base 为止（宿主页比 4K
        // 大时长度向上取整，接不上的不会凑巧相等）
        if unsafe { run.0.add(run.1) } == base {
            run.1 += len;
        } else {
            unsafe { appine_hal::unmap(run.0, run.1) };
            run = (base, len);
        }
    }
    unsafe { appine_hal::unmap(run.0, run.1) };
    UNMAPPING.fetch_sub(total, Ordering::Release);
}

/// 一段可以映射进多个地址空间的宿主内存（System V 共享内存段）。
#[derive(Clone)]
pub(crate) struct SharedMemory {
    region: Arc<HostRegion>,
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

/// mremap(`MREMAP_DONTUNMAP`) 之后，原来的地址上要重新映射文件的一段：它原来是私有
/// 文件映射，Linux 上那里的页之后从文件重新读进来。
pub struct Refill {
    pub start: u64,
    pub len: u64,
    pub prot: u32,
    /// 映射的文件在仓库里的 inode（0 表示不知道）和这一段对应的文件偏移。
    pub ino: u64,
    pub offset: u64,
    pub name: Option<Rc<[u8]>>,
}

#[derive(Clone)]
struct Vma {
    len: u64,
    prot: u32,
    backing: Backing,
    /// 能否获得写权限：只读打开的文件的共享映射不能（宿主映射本身就是只读的）。
    may_write: bool,
    /// 持有宿主内存的引用：最后一个引用它的 VMA 消失时释放。
    region: Arc<HostRegion>,
    /// `/proc/<pid>/maps` 里显示的名字（映射的文件、程序）和文件的 inode。
    name: Option<Rc<[u8]>>,
    ino: u64,
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
    /// 私有文件映射：映射时文件内容的版本（[`content::version`]），没有可信的版本时是
    /// None。没写过的映射里是这个版本的文件内容（[`Mm::code_sources`]）。
    content: Option<(u64, u64)>,
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
    /// `MAP_GROWSDOWN`：访问它下面的地址时向下增长（[`Mm::grow_down`]）。
    growsdown: bool,
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
        // 代数递增：二分找到起点，只看错过的那几条（JVM 下整个记录有上万条）
        let first = self.entries.partition_point(|e| e.0 <= seen);
        Some(
            self.entries
                .range(first..)
                .take_while(|e| e.0 <= now)
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

/// 一个地址空间的 Tier 1 翻译，这个地址空间里的线程共用（[`Mm::code_space`]）。
pub struct CodeSpace {
    pub space: Arc<appine_engine::DbtSpace>,
    /// 已经按代码失效的记录作废到的代码代数：第一个看到新代数的线程作废一次就够了。
    pub seen: Mutex<u64>,
}

/// 地址空间的句柄（[`Mm::into_shared`]）：写时复制时同一个 fork 家族的地址空间经由它
/// 找到对方。
type MmRef = Weak<RefCell<Mm>>;

/// 一个借出的宿主页。
struct Loan {
    /// 拥有者和页在它那里的 guest 地址；拥有者的地址空间销毁之后是 None（孤儿页）。
    owner: Option<(MmRef, u64)>,
    /// 借用方和页在它那里的 guest 地址。通常只有一个（fork 出来的子进程），它不另外分配：
    /// 大进程 fork 时每页都有一条记录。
    first: Option<(MmRef, u64)>,
    others: Vec<(MmRef, u64)>,
}

impl Loan {
    fn new(owner: Option<(MmRef, u64)>, borrower: (MmRef, u64)) -> Loan {
        Loan {
            owner,
            first: Some(borrower),
            others: Vec::new(),
        }
    }

    fn lend(&mut self, borrower: (MmRef, u64)) {
        match self.first {
            None => self.first = Some(borrower),
            Some(_) => self.others.push(borrower),
        }
    }

    /// 去掉借用方 `mm`（页在它那里的地址是 `page`），返回还有没有别的借用方。
    fn give_back(&mut self, mm: &MmRef, page: u64) -> bool {
        let it = |(m, a): &(MmRef, u64)| *a == page && m.ptr_eq(mm);
        if self.first.as_ref().is_some_and(it) {
            self.first = self.others.pop();
        } else {
            self.others.retain(|b| !it(b));
        }
        self.first.is_some()
    }

    fn borrowers(self) -> impl Iterator<Item = (MmRef, u64)> {
        self.first.into_iter().chain(self.others)
    }
}

/// 宿主页地址的哈希：按页对齐的地址去掉低位再乘一个奇数常数就足够分散。默认的 SipHash
/// 在大进程 fork 时占一成多。
#[derive(Default, Clone, Copy)]
struct PageHasher(u64);

impl std::hash::Hasher for PageHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.0 = (n as u64 >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// 同一个 fork 家族共用的借出记录，键是宿主页的地址。
type Loans = Rc<RefCell<HashMap<usize, Loan, std::hash::BuildHasherDefault<PageHasher>>>>;

/// 页表里连续的一段：guest 地址、宿主地址、长度。逐页改页表项之前先接成段。
#[derive(Default)]
struct Runs {
    runs: Vec<(u64, usize, u64)>,
}

impl Runs {
    fn push(&mut self, page: u64, host: NonNull<u8>) {
        let host = host.as_ptr() as usize;
        if let Some((g, h, len)) = self.runs.last_mut()
            && *g + *len == page
            && *h + *len as usize == host
        {
            *len += PAGE_SIZE;
            return;
        }
        self.runs.push((page, host, PAGE_SIZE));
    }
}

/// 借来的页。
struct Borrowed {
    host: NonNull<u8>,
    /// 持有那段宿主内存：拥有者解除映射、销毁之后它也不会释放。
    region: Arc<HostRegion>,
}

/// 一个地址空间里写时复制的页（guest 地址）。只在私有匿名映射里。
#[derive(Default)]
struct Cow {
    /// 借出的页：页表项没有写权限。
    lent: BTreeSet<u64>,
    /// 借来的页：页表项指向拥有者的宿主页，没有写权限。
    borrowed: BTreeMap<u64, Borrowed>,
}

/// 一个 guest 进程的地址空间。
pub struct Mm {
    // 先销毁页表，再释放它指向的宿主内存（Drop 里先放开页表，字段再按声明顺序析构）。
    mmu: ManuallyDrop<Mmu>,
    vmas: BTreeMap<u64, Vma>,
    /// 已经解除映射、等 shootdown 完成后才能释放的宿主内存。
    graveyard: Vec<Arc<HostRegion>>,
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
    /// Tier 1 的翻译，第一个线程开始运行时创建（[`Mm::code_space`]）。
    code_space: Option<Arc<CodeSpace>>,
    /// membarrier(2) 登记过的命令（`MEMBARRIER_CMD_REGISTER_*`）。
    pub membarrier: i32,
    /// `mlockall(MCL_FUTURE)`：以后的映射也锁住。
    pub lock_future: bool,
    /// 同时带了 `MCL_ONFAULT`：以后的映射锁住但不预先调进内存。
    pub lock_onfault: bool,
    /// 正在执行的程序文件在仓库里的 inode（ETXTBSY）。
    pub exe: Option<u64>,
    /// vDSO 的加载地址（信号处理函数默认返回到其中的 sigreturn 跳板）。
    pub vdso: u64,
    /// 初始栈上 argv 字符串的范围（`/proc/<pid>/cmdline`）。
    pub args: (u64, u64),
    /// 初始栈上 envp 字符串的范围（`/proc/<pid>/environ`）。
    pub env: (u64, u64),
    /// mmap 分配区的上界（不含），按 execve 时的 `RLIMIT_STACK` 定下（[`Mm::set_layout`]）。
    mmap_top: u64,
    /// 初始的栈指针（Linux 的 `start_stack`）：`/proc/<pid>/maps` 把包含它的栈标成
    /// `[stack]`。
    start_stack: u64,
    /// 主线程栈目前的底部；还没有栈时是 0。
    stack_low: u64,
    /// 写时复制的页和同一个 fork 家族共用的借出记录（模块文档）。只在持有 BKL 时访问，
    /// 也会经由别的地址空间的共享借用修改，所以放在 RefCell 里。
    cow: RefCell<Cow>,
    loans: Loans,
    /// 自己的句柄，借出页时记下（[`Mm::into_shared`]）。
    me: MmRef,
    /// 只读文件映射的宿主映射，所有地址空间共用（[`FileMaps`]）。
    files: FileMaps,
}

/// 只读私有文件映射（程序和库的代码段）的宿主映射，内核里的所有地址空间共用：同一个
/// 版本（[`content::version`]）的同一段文件再映射时直接用它，宿主页表还在，不必重新映射、
/// 缺页，退出时也不必拆掉（shell 每起一个进程，程序和动态链接器的代码段各一次）。用它的
/// VMA 与 fork 之后父子共用宿主内存时一样标记为 `cow`，要写之前复制一份自己的。宿主上是
/// 只读映射，万一有地方忘了复制就直接出错，不会悄悄改了别人的代码。
#[derive(Clone, Default)]
pub(crate) struct FileMaps(Arc<Mutex<Vec<FileMap>>>);

struct FileMap {
    /// (设备号, inode, 文件偏移, 长度, ctime, 大小)。
    key: [u64; 6],
    region: Arc<HostRegion>,
    host: NonNull<u8>,
    /// 最近一次用到的序号，满了时去掉最久没用的。
    used: u64,
}

// SAFETY: host 指向 region 持有的宿主内存
unsafe impl Send for FileMap {}

impl FileMaps {
    /// 最多保留几段。只占宿主地址空间，页是 page cache 里的，宿主内存紧张时可以回收。
    const MAX: usize = 128;

    fn get(&self, key: &[u64; 6]) -> Option<(Arc<HostRegion>, NonNull<u8>)> {
        let mut maps = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = maps.iter().map(|m| m.used).max().unwrap_or(0) + 1;
        let m = maps.iter_mut().find(|m| m.key == *key)?;
        m.used = next;
        Some((m.region.clone(), m.host))
    }

    fn insert(&self, key: [u64; 6], region: Arc<HostRegion>, host: NonNull<u8>) {
        let mut maps = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let used = maps.iter().map(|m| m.used).max().unwrap_or(0) + 1;
        if maps.len() >= Self::MAX
            && let Some(old) = (0..maps.len()).min_by_key(|&i| maps[i].used)
        {
            maps.swap_remove(old);
        }
        maps.push(FileMap {
            key,
            region,
            host,
            used,
        });
    }

    /// 全部丢掉（宿主内存紧张时）。还在用的映射由用它的 VMA 持有，不受影响。
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// `/proc/<pid>/maps` 的一行。
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    pub shared: bool,
    pub name: Option<Rc<[u8]>>,
    /// 文件映射：起点对应的文件偏移和文件的 inode。
    pub file: Option<(u64, u64)>,
    /// mlock 锁住了。
    pub locked: bool,
    /// 能获得写权限（smaps 的 `mw`）。
    pub may_write: bool,
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
    extent(appine_hal::fstat(fd).map_or(0, |s| s.st_size), offset, len)
}

/// 大小为 `size` 的文件从 `offset` 起还有内容的字节数（[`file_extent`]）。
fn extent(size: i64, offset: u64, len: u64) -> u64 {
    (size.max(0) as u64)
        .saturating_sub(offset)
        .next_multiple_of(PAGE_SIZE)
        .min(len)
}

/// 宿主文件内容的版本（[`content::version`]），没有可信的版本时是 None。
pub(crate) fn content_version(st: &libc::stat, dev: u64, ino: u64) -> Option<(u64, u64)> {
    content::version(st, dev, ino)
}

/// 判断两段文件映射的内容是否相同（[`Mm::code_sources`]）。
mod content {
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// 有过可写共享映射的宿主文件 (设备号, inode)：经由共享映射写进去的内容不一定改
    /// ctime（页一直是脏的就不再经过 page_mkwrite），这样的文件不再相信 ctime。
    static WRITABLY_SHARED: Mutex<Option<HashSet<(u64, u64)>>> = Mutex::new(None);

    /// 文件 (`dev`, `ino`) 有了可写的共享映射。
    pub(super) fn writably_shared(dev: u64, ino: u64) {
        let mut set = WRITABLY_SHARED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.get_or_insert_with(HashSet::new).insert((dev, ino));
    }

    /// 宿主文件内容的版本：ctime（纳秒）和大小。内容的每一次修改（write、truncate、
    /// fallocate……）宿主都会把 ctime 改成当时的时间，utimensat 也改不回去，所以两次量到
    /// 的版本相同，之间就没有改过。宿主的时间戳取自粗粒度的时钟，ctime 离现在太近时，
    /// 之后紧接着的修改可能得到同一个 ctime，这时不给版本；有过可写共享映射的文件也不给。
    #[allow(clippy::unnecessary_cast)]
    pub(super) fn version(st: &libc::stat, dev: u64, ino: u64) -> Option<(u64, u64)> {
        const RACY_NS: i128 = 2_000_000_000;
        let ctime = st.st_ctime as i128 * 1_000_000_000 + st.st_ctime_nsec as i128;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as i128;
        if ctime + RACY_NS > now {
            return None;
        }
        let set = WRITABLY_SHARED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if set.as_ref().is_some_and(|s| s.contains(&(dev, ino))) {
            return None;
        }
        Some((ctime as u64, st.st_size as u64))
    }
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
            mmu: ManuallyDrop::new(Mmu::new()),
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
            code_space: None,
            membarrier: 0,
            lock_future: false,
            lock_onfault: false,
            exe: None,
            vdso: 0,
            args: (0, 0),
            env: (0, 0),
            mmap_top: STACK_TOP - MIN_GAP,
            start_stack: 0,
            stack_low: 0,
            cow: RefCell::default(),
            loans: Rc::default(),
            me: Weak::new(),
            files: FileMaps::default(),
        }
    }

    /// 与别的地址空间共用只读文件映射（[`FileMaps`]）。
    pub fn share_files(&mut self, files: &FileMaps) {
        self.files = files.clone();
    }

    /// 放进 `Rc<RefCell<..>>`（`TaskState` 持有的形式），记下自己的句柄：借用方还回最后
    /// 一个借用时经由它让拥有者恢复写权限。
    pub fn into_shared(self) -> Rc<RefCell<Mm>> {
        Rc::new_cyclic(|me| {
            let mut mm = self;
            mm.me = me.clone();
            RefCell::new(mm)
        })
    }

    /// 按栈大小的上限 `stack_limit`（execve 时的 `RLIMIT_STACK`）定下 mmap 分配区的
    /// 上界，与 Linux 的 `mmap_base` 一样给栈留出这么大的间隔，再加上
    /// [`STACK_GUARD_GAP`]，并限制在 [`MIN_GAP`] 和 [`MAX_GAP`] 之间。不限时，Linux
    /// 改为自底向上分配（legacy 布局），这里仍然自顶向下，间隔取 [`MAX_GAP`]。
    pub fn set_layout(&mut self, stack_limit: u64) {
        let gap = stack_limit
            .saturating_add(STACK_GUARD_GAP)
            .clamp(MIN_GAP, MAX_GAP);
        self.mmap_top = (STACK_TOP - gap) & !(PAGE_SIZE - 1);
    }

    /// 该地址空间的软件页表。
    pub fn mmu(&self) -> &Mmu {
        &self.mmu
    }

    /// `addr` 处 futex 的键；没有映射时返回 None。
    pub fn futex_key(&self, addr: u64) -> Option<crate::futex::Key> {
        // 私有内存的键是宿主地址：借来的页先换成自己的，否则写时复制之后键就变了，等在
        // 旧键上的线程再也叫不醒；也不会和拥有者那边的键相同
        let page = addr & !(PAGE_SIZE - 1);
        if self.cow.borrow().borrowed.contains_key(&page) {
            self.materialize(page, true);
        }
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

    /// 这个地址空间的 Tier 1 翻译，没有时创建；宿主不支持 Tier 1 时返回 None。创建时优先
    /// 拿回池里同一个程序（按可执行文件的 inode）留下的翻译，它在第一次运行之前逐块核对。
    pub fn code_space(&mut self) -> Option<Arc<CodeSpace>> {
        if self.code_space.is_none() {
            let space = appine_engine::DbtSpace::new(self.exe.unwrap_or(0))?;
            // 从池里拿到上一个使用者留下的翻译时，同一个版本的程序文件里的块不必再核对
            space.trust(&self.code_sources());
            self.code_space = Some(Arc::new(CodeSpace {
                space,
                seen: Mutex::new(self.code_generation.load(Ordering::SeqCst)),
            }));
        }
        self.code_space.clone()
    }

    /// 内容由文件和版本就能确定的可执行映射（[`appine_engine::DbtSource`]）：没写过、
    /// 有可信版本（[`content::version`]）的私有文件映射。翻译放回池里之前记下它们，下一个
    /// 使用者的同一段映射是同一个版本的同一段文件时，里面的块不必逐块核对（核对要把程序
    /// 和动态链接器的代码段每一页都读一遍，shell 每起一个进程都要做）。
    fn code_sources(&self) -> Vec<appine_engine::DbtSource> {
        self.vmas
            .iter()
            .filter(|(_, v)| {
                v.prot & prot::EXEC != 0 && v.backing == Backing::PrivateFile && !v.written.get()
            })
            .filter_map(|(&start, v)| {
                let ((dev, ino, offset), (ctime, size)) = (v.file_id?, v.content?);
                Some(appine_engine::DbtSource {
                    start,
                    len: v.len,
                    id: [dev, ino, offset, ctime, size],
                })
            })
            .take(appine_engine::DbtSpace::SOURCES)
            .collect()
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
    /// 只有代码要作废时只记下范围、改代数。
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
        if !dirty && self.graveyard.is_empty() {
            // 只有代码变了（JIT 的 ic ivau）：翻译由这个地址空间的线程共用，改代码的线程
            // 重新进入 guest 代码之前作废一次，作废块的入口就跳往转发桩了，其他使用者不必
            // 打断。在那之前它们可能还执行旧的翻译，与硬件上别的核在上下文同步之前可能还执行
            // 旧指令一样。要等的是 membarrier 的 SYNC_CORE（它调用 quiesce）
            return;
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
        let mut top = self.mmap_top;
        for (&s, v) in self.vmas.range(..self.mmap_top).rev() {
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
        region: Arc<HostRegion>,
        host: NonNull<u8>,
    ) -> Result<(), Errno> {
        self.room_for(1)?;
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
                ino: 0,
                host,
                file_len: len,
                cow: false,
                file_id: None,
                content: None,
                file: None,
                shm: None,
                locked: self.lock_future,
                written: Cell::new(prot & prot::WRITE != 0),
                growsdown: false,
            },
        );
        Ok(())
    }

    /// 还能不能再多 `more` 个 VMA（`vm.max_map_count`）。
    fn room_for(&self, more: usize) -> Result<(), Errno> {
        if self.vmas.len() + more > MAX_MAP_COUNT.load(Ordering::Relaxed) {
            return Err(Errno::ENOMEM);
        }
        Ok(())
    }

    /// 在这些地址上拆开 VMA 要多出几个。
    fn splits(&self, addrs: &[u64]) -> usize {
        addrs
            .iter()
            .filter(|&&a| {
                self.vmas
                    .range(..a)
                    .next_back()
                    .is_some_and(|(&s, v)| s + v.len > a)
            })
            .count()
    }

    /// 把一个完整的 VMA 放到 `start`；调用方保证区间空闲。
    fn insert_vma(&mut self, start: u64, v: Vma) -> Result<(), Errno> {
        self.room_for(1)?;
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
    fn new_anonymous(len: u64, reserve: bool) -> Result<(Arc<HostRegion>, NonNull<u8>), Errno> {
        let host_len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
        let base = appine_hal::map_anonymous(host_len, reserve).map_err(|_| Errno::ENOMEM)?;
        Ok((
            Arc::new(HostRegion {
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

    /// 把从 `start` 开始的映射标记为向下增长（`MAP_GROWSDOWN` 的私有匿名映射）。
    pub fn set_growsdown(&mut self, start: u64) {
        if let Some(v) = self.vmas.get_mut(&start) {
            v.growsdown = true;
        }
    }

    /// 映射主线程的初始栈 `[STACK_TOP - len, STACK_TOP)`（加载器使用）：可读写，向下增长。
    /// 宿主内存在它下面预留到一共 `reserve` 字节（不碰不占内存），栈在这个范围里增长时
    /// 只是就地加长同一个 VMA。
    pub fn map_stack(&mut self, len: u64, reserve: u64) -> Result<(), Errno> {
        let start = STACK_TOP - len;
        if !self.is_free(start, len) {
            return Err(Errno::EEXIST);
        }
        let (region, base) = Self::new_anonymous(reserve.max(len), false)?;
        // SAFETY: 这段宿主内存至少有 len 字节
        let host = unsafe { NonNull::new_unchecked(base.as_ptr().add(region.len - len as usize)) };
        let rw = prot::READ | prot::WRITE;
        self.insert(start, len, rw, Backing::Anonymous, true, region, host)?;
        self.vmas.get_mut(&start).unwrap().growsdown = true;
        self.stack_low = start;
        Ok(())
    }

    /// 主线程栈目前的底部；还没有栈时是 0。
    pub fn stack_low(&self) -> u64 {
        self.stack_low
    }

    /// 访问没有映射的 `addr`（guest 的访存出错，或者内核访问 guest 内存之前）时调用：
    /// 它上面紧挨着的是向下增长的映射（主线程栈、`MAP_GROWSDOWN`）时，把映射延伸到
    /// `addr` 所在的页，返回 true。与 Linux 的 `expand_downwards` 一样，延伸之后与下面
    /// 那个可访问的映射之间要留出 [`STACK_GUARD_GAP`]，大小不能超过 `limit`
    /// （`RLIMIT_STACK` 的当前值），否则不增长（guest 收到 SIGSEGV，内核访问得到 EFAULT）。
    ///
    /// 宿主内存下面还有预留的空间时就地加长这个 VMA。没有时另外申请一段，长出来的部分
    /// 是一个新的 VMA，宿主内存在它下面预留到与栈现在的大小相同：栈越长，VMA 的个数只按
    /// 对数增加。
    pub fn grow_down(&mut self, addr: u64, limit: u64) -> bool {
        let page = addr & !(PAGE_SIZE - 1);
        if page < MMAP_MIN || self.is_mapped(addr) {
            return false;
        }
        let Some((&above, v)) = self.vmas.range(addr..).next() else {
            return false;
        };
        if !v.growsdown {
            return false;
        }
        let (prot, locked) = (v.prot, v.locked);
        if let Some((&s, below)) = self.vmas.range(..page).next_back()
            && !below.growsdown
            && below.prot != 0
            && page - (s + below.len) < STACK_GUARD_GAP
        {
            return false;
        }
        // 只是因为宿主内存不连续才分开的几个 VMA，在 Linux 上是同一个
        let mut end = above;
        for (&s, o) in self.vmas.range(above..) {
            if s != end || !o.growsdown || o.prot != prot {
                break;
            }
            end = s + o.len;
        }
        let (grow, size) = (above - page, end - above);
        if size + grow > limit {
            return false;
        }
        let grown = if self.spare_below(above) >= grow {
            self.extend_down(above, grow)
        } else {
            let reserve = size.clamp(grow, limit - size);
            self.new_stack_chunk(page, grow, reserve, prot, locked)
        };
        if grown && above == self.stack_low {
            self.stack_low = page;
        }
        grown
    }

    /// VMA `start` 的宿主内存下面还空着、可以就地向下加长的字节数：同一段宿主内存里
    /// 别的 VMA（拆开的、被 mremap 搬走的部分）占着的不算。
    fn spare_below(&self, start: u64) -> u64 {
        let v = &self.vmas[&start];
        let host = v.host.as_ptr() as usize;
        let low = self
            .vmas
            .values()
            .filter(|o| Arc::ptr_eq(&o.region, &v.region))
            .map(|o| o.host.as_ptr() as usize + o.len as usize)
            .filter(|&end| end <= host)
            .fold(v.region.base.as_ptr() as usize, usize::max);
        (host - low) as u64
    }

    /// 向下增长的 VMA `start` 就地往下加长 `grow` 字节，用它的宿主内存下面空着的部分
    /// （[`Mm::spare_below`]）。
    fn extend_down(&mut self, start: u64, grow: u64) -> bool {
        let v = &self.vmas[&start];
        let (page, prot) = (start - grow, v.prot);
        // SAFETY: v.host 下面至少还有 grow 字节属于同一段宿主内存，没有别的映射在用
        let host = unsafe { NonNull::new_unchecked(v.host.as_ptr().sub(grow as usize)) };
        // 栈的下面一段解除映射之后又长回来：以前写过的内容要变回零
        unsafe { discard_host(host, grow as usize) };
        if Self::map_pages(&mut self.mmu, page, host, grow, prot).is_err() {
            return false;
        }
        let mut v = self.vmas.remove(&start).unwrap();
        v.host = host;
        v.len += grow;
        v.file_len += grow;
        self.vmas.insert(page, v);
        true
    }

    /// 在 `page` 映射 `grow` 字节的新 VMA，接在上面那个向下增长的 VMA 下面。宿主内存申请
    /// `reserve` 字节，VMA 放在最上面，下面留着以后就地增长；申请不到时只申请 `grow` 字节。
    fn new_stack_chunk(
        &mut self,
        page: u64,
        grow: u64,
        reserve: u64,
        prot: u32,
        locked: bool,
    ) -> bool {
        let Ok((region, base)) =
            Self::new_anonymous(reserve, false).or_else(|_| Self::new_anonymous(grow, false))
        else {
            return false;
        };
        // SAFETY: 这段宿主内存至少有 grow 字节
        let host = unsafe { NonNull::new_unchecked(base.as_ptr().add(region.len - grow as usize)) };
        let anon = Backing::Anonymous;
        if self
            .insert(page, grow, prot, anon, true, region, host)
            .is_err()
        {
            return false;
        }
        let v = self.vmas.get_mut(&page).unwrap();
        v.growsdown = true;
        v.locked = locked;
        true
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
                && hint.checked_add(len).is_some_and(|e| e <= self.mmap_top)
                && self.is_free(hint, len)
            {
                hint
            } else {
                self.find_free(len).ok_or(Errno::ENOMEM)?
            }
        };

        let st = file.and_then(|(fd, ..)| appine_hal::fstat(fd).ok());
        // st_dev 在 macOS 上是 i32
        #[allow(clippy::unnecessary_cast)]
        let file_id = st
            .as_ref()
            .zip(file)
            .map(|(s, (_, offset, _))| (s.st_dev as u64, s.st_ino as u64, offset));
        let content = match (&st, file_id) {
            (Some(_), Some((dev, ino, _))) if shared && file.is_some_and(|f| f.2) => {
                content::writably_shared(dev, ino);
                None
            }
            (Some(s), Some((dev, ino, _))) if !shared => content::version(s, dev, ino),
            _ => None,
        };
        // 宿主映射与别的地址空间共用（FileMaps）
        let mut cached = false;
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
                let backing = if shared {
                    Backing::Shared
                } else {
                    Backing::PrivateFile
                };
                // 有可信版本的只读私有映射：所有地址空间共用一份宿主映射（FileMaps）
                let key = st
                    .as_ref()
                    .zip(content)
                    .filter(|_| !shared && prot & prot::WRITE == 0)
                    .map(|(s, (ctime, size))| {
                        #[allow(clippy::unnecessary_cast)]
                        let (dev, ino) = (s.st_dev as u64, s.st_ino as u64);
                        [dev, ino, offset, len, ctime, size]
                    });
                if let Some((region, host)) = key.as_ref().and_then(|k| self.files.get(k)) {
                    cached = true;
                    (backing, true, region, host)
                } else {
                    let m = match key {
                        Some(_) => appine_hal::map_file_read_only(fd, offset, host_len),
                        None => appine_hal::map_file(fd, offset, host_len, shared, writable),
                    }
                    .map_err(Errno)?;
                    let region = Arc::new(HostRegion {
                        base: m.base,
                        len: m.len,
                    });
                    if let Some(key) = key {
                        self.files.insert(key, region.clone(), m.ptr);
                        cached = true;
                    }
                    (backing, !shared || writable, region, m.ptr)
                }
            }
        };
        if prot & prot::WRITE != 0 && !may_write {
            return Err(Errno::EACCES);
        }
        let file_len = match file {
            Some((_, offset, _)) => extent(st.as_ref().map_or(0, |s| s.st_size), offset, len),
            None => len,
        };
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
        v.content = content;
        v.cow = cached;
        v.file = dup;
        self.trim_to_file(start)?;
        Ok(start)
    }

    /// mremap(`MREMAP_DONTUNMAP`)：内容搬到新地址（`fixed` 或者新找一段），原来的地址上
    /// 留下权限相同、不再锁住的映射。与 Linux 5.13 起一样支持所有映射，原来的地址上
    /// 相当于丢掉了页表：私有匿名映射变成空的；共享映射（文件或匿名）与新地址共用同
    /// 一段宿主内存，相当于之后从文件或共享内存重新读进来；私有文件映射空出来，由调用
    /// 方按返回的 [`Refill`] 重新映射文件。
    pub fn mremap_dontunmap(
        &mut self,
        old: u64,
        len: u64,
        fixed: Option<u64>,
    ) -> Result<(u64, Vec<Refill>), Errno> {
        let len = page_up(len).filter(|&l| l != 0).ok_or(Errno::EINVAL)?;
        if !old.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        self.check_mapped(old, len).map_err(|_| Errno::EFAULT)?;
        let end = old + len;
        let dst = match fixed {
            Some(d) => d,
            None => self.find_free(len).ok_or(Errno::ENOMEM)?,
        };
        self.split_at(old);
        self.split_at(end);
        let (mut empty, mut alias, mut refill) = (Vec::new(), Vec::new(), Vec::new());
        for (&s, v) in self.vmas.range(old..end) {
            match v.backing {
                Backing::Anonymous => empty.push((s, v.len, v.prot)),
                Backing::Shared => {
                    let mut v = v.clone();
                    v.locked = false;
                    alias.push((s, v));
                }
                Backing::PrivateFile => refill.push(Refill {
                    start: s,
                    len: v.len,
                    prot: v.prot,
                    ino: v.ino,
                    offset: v.file_id.map_or(0, |f| f.2),
                    name: v.name.clone(),
                }),
            }
        }
        let new = self.mremap(old, len, len, true, Some(dst))?;
        for (start, len, prot) in empty {
            self.map_anonymous(start, len, prot)?;
            self.vmas.get_mut(&start).unwrap().locked = false;
        }
        for (start, v) in alias {
            self.insert_vma(start, v)?;
        }
        Ok((new, refill))
    }

    /// remap_file_pages(2)：与 Linux 4.0 起一样用 mmap 模拟，把 `[start, start + len)`
    /// 换成同一个文件从 `offset` 开始的内容。区间要落在首尾相接、映射同一个文件、权限
    /// 相同的共享映射里。
    pub fn remap_file_pages(&mut self, start: u64, len: u64, offset: u64) -> Result<(), Errno> {
        let end = start
            .checked_add(len)
            .filter(|&e| e > start)
            .ok_or(Errno::EINVAL)?;
        let (&s, v) = self
            .vmas
            .range(..=start)
            .next_back()
            .filter(|(s, v)| **s + v.len > start)
            .ok_or(Errno::EINVAL)?;
        let Some((fd, _)) = v.file.clone() else {
            return Err(Errno::EINVAL);
        };
        let (prot, may_write, name, ino, locked) =
            (v.prot, v.may_write, v.name.clone(), v.ino, v.locked);
        // 同一个文件：按宿主的 (设备号, inode) 比较
        let id = v.file_id.map(|(d, i, _)| (d, i));
        let mut at = s + v.len;
        while at < end {
            let next = self
                .vmas
                .get(&at)
                .filter(|n| n.file.is_some() && n.file_id.map(|(d, i, _)| (d, i)) == id)
                .filter(|n| n.prot == prot)
                .ok_or(Errno::EINVAL)?;
            at += next.len;
        }
        let raw = std::os::fd::AsRawFd::as_raw_fd(&*fd);
        let flags = flags::MAP_SHARED | flags::MAP_FIXED;
        self.mmap(start, len, prot, flags, Some((raw, offset, may_write)))?;
        self.label(start, len, name, ino);
        if locked {
            self.set_locked(start, len, true)?;
        }
        Ok(())
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
        if old_end == vma_end && old_end + grow <= self.mmap_top && self.is_free(old_end, grow) {
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
                let region = Arc::new(HostRegion {
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
            ino: template.ino,
            host,
            file_len,
            cow: false,
            file_id: template.file_id.map(|(d, i, o)| (d, i, o + offset)),
            content: None,
            file,
            shm: None,
            locked: template.locked,
            written: Cell::new(template.prot & prot::WRITE != 0),
            growsdown: false,
        })
    }

    /// 把 `[old, old + len)` 里的映射原样搬到 `dst`（宿主内存不动）。
    fn move_range(&mut self, old: u64, len: u64, dst: u64) -> Result<(), Errno> {
        // 写时复制的页按 guest 地址记录：搬走之前先各自复制
        self.cow_settle(old, old + len);
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
        self.room_for(self.splits(&[start, end]))?;
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

    /// madvise(`MADV_REMOVE`)：共享映射里这一段清零，文件映射就是在文件里打洞。与 Linux
    /// 相同，有锁住的页或者私有的匿名映射是 EINVAL，其余私有的或者不能获得写权限的映射
    /// 是 EACCES（6.7 起看的是能不能获得写权限，不是现在有没有）。区间必须完全已映射。
    pub fn remove(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        self.check_mapped(addr, len)?;
        let end = addr + len;
        let first = self
            .vmas
            .range(..=addr)
            .next_back()
            .map_or(addr, |(&s, _)| s);
        let overlapping = || {
            self.vmas
                .range(first..end)
                .filter(move |(s, v)| *s + v.len > addr)
        };
        for (_, v) in overlapping() {
            if v.locked || v.backing == Backing::Anonymous {
                return Err(Errno::EINVAL);
            }
            if v.backing != Backing::Shared || !v.may_write {
                return Err(Errno::EACCES);
            }
        }
        for (&s, v) in overlapping() {
            // 超出文件末尾的部分碰不得（宿主会发 SIGBUS）
            let from = addr.max(s);
            let to = end.min(s + v.file_len);
            if from < to {
                // SAFETY: [from, to) 在这个 VMA 里，宿主内存由它持有；能获得写权限的共享
                // 映射，宿主映射总是可写的
                unsafe {
                    std::ptr::write_bytes(
                        v.host.as_ptr().add((from - s) as usize),
                        0,
                        (to - from) as usize,
                    )
                };
            }
        }
        Ok(())
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
        // 借来的页在拥有者的宿主内存里
        for (&page, _) in self
            .cow
            .borrow()
            .borrowed
            .range(addr & !(PAGE_SIZE - 1)..end)
        {
            out[((page - (addr & !(PAGE_SIZE - 1))) / PAGE_SIZE) as usize] = true;
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
        // 与 Linux 一样，只有把一个 VMA 拆成三段时才受 max_map_count 限制
        let inside = |a: u64| {
            self.vmas
                .range(..a)
                .next_back()
                .filter(|(s, v)| **s + v.len > a)
        };
        if let (Some((s1, _)), Some((s2, _))) = (inside(addr), inside(end))
            && s1 == s2
        {
            self.room_for(1)?;
        }
        self.split_at(addr);
        self.split_at(end);
        let borrowed = self.cow_unmap(addr, end);
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
        self.give_back(borrowed);
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
        self.room_for(self.splits(&[addr, end]))?;
        self.split_at(addr);
        self.split_at(end);
        // 写时复制的页在下面先拿到新的权限、再去掉写权限：在那之间别的线程不能执行 guest
        // 代码，否则可能写进借出的页
        let cow = self.has_cow(addr, end);
        if cow {
            self.shootdown();
        }
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
        if cow {
            self.cow_protect(addr, end);
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
                // 借出的页借用方还要用，借来的页要换成自己的（之后读到零）
                self.cow_settle(cursor, stop);
                // SAFETY: [cursor, stop) 落在这个 VMA 内，对应的宿主内存是它私有的匿名内存
                unsafe {
                    discard_host(v.host.add((cursor - s) as usize), (stop - cursor) as usize)
                };
            }
            cursor = stop;
        }
        Ok(())
    }

    /// fork：复制出一个新的地址空间。写过的私有匿名映射写时复制（[`Mm::lend`]），没写过
    /// 的内容全是零，子进程用自己的；可写的私有文件映射逐页拷贝（全零的页跳过，只拷贝
    /// 文件末尾之前的部分）。共享映射和只读的私有文件映射与子进程共用宿主内存。调用方
    /// 先让同一地址空间的其他线程停在 guest 代码之外（[`Mm::quiesce`]）。
    pub fn fork(&mut self) -> Result<Rc<RefCell<Mm>>, Errno> {
        let shared = Mm::new().into_shared();
        let mut child = shared.borrow_mut();
        child.loans = self.loans.clone();
        child.files = self.files.clone();
        child.brk_start = self.brk_start;
        child.brk = self.brk;
        child.vdso = self.vdso;
        child.args = self.args;
        child.env = self.env;
        child.exe = self.exe;
        child.mmap_top = self.mmap_top;
        child.start_stack = self.start_stack;
        child.stack_low = self.stack_low;
        let spare: BTreeMap<u64, u64> = self
            .vmas
            .iter()
            .filter(|(_, v)| v.growsdown && v.backing == Backing::Anonymous)
            .map(|(&s, _)| (s, self.spare_below(s)))
            .collect();
        let heap_start = page_up(self.brk_start).unwrap_or(0);
        let is_heap = |s: u64, v: &Vma| {
            s == heap_start && v.backing == Backing::Anonymous && v.len <= HEAP_RESERVE
        };
        // 子进程自己的小映射（数据段、.bss、malloc 的小块）切自同一段宿主内存：shell fork
        // 之后马上 execve，原来每个映射各一次宿主的 mmap 和 munmap。大的、堆和向下增长的
        // 仍然各自一段，解除映射时才能交还宿主
        let pieces: Vec<u64> = self
            .vmas
            .iter()
            .filter(|&(&s, v)| {
                !v.growsdown
                    && !is_heap(s, v)
                    && v.len <= FORK_PIECE_MAX
                    && match v.backing {
                        Backing::Shared => false,
                        Backing::PrivateFile => v.prot & prot::WRITE != 0,
                        Backing::Anonymous => true,
                    }
            })
            .map(|(&s, _)| s)
            .collect();
        let mut carved = Self::carve(&self.vmas, &pieces)?;
        // 写过的私有匿名映射写时复制：先映射好子进程自己的（全零的）宿主内存，再借给它
        let mut cow = Vec::new();
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
                // 堆照样预留宿主地址空间，子进程里接着扩展也是同一个 VMA
                let heap = is_heap(start, v);
                let below = spare.get(&start).copied().unwrap_or(0);
                let (region, host) = if let Some(piece) = carved.remove(&start) {
                    piece
                } else if heap {
                    spare::anonymous(HEAP_RESERVE)?
                } else if below > 0 {
                    // 栈下面预留的宿主内存也照样留着
                    let (region, base) = spare::anonymous(below + v.len)?;
                    // SAFETY: 这段宿主内存有 below + v.len 字节
                    let host = unsafe { NonNull::new_unchecked(base.as_ptr().add(below as usize)) };
                    (region, host)
                } else {
                    Self::new_anonymous(v.len, v.prot & prot::WRITE != 0)?
                };
                let len = v.file_len as usize;
                if v.backing == Backing::Anonymous && v.written.get() {
                    cow.push(start);
                } else if v.backing != Backing::Anonymous {
                    // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
                    unsafe { copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), len, None) };
                }
                (region, host)
            };
            child.insert(start, v.len, v.prot, v.backing, v.may_write, region, host)?;
            let c = child.vmas.get_mut(&start).unwrap();
            c.written.set(v.written.get());
            c.name = v.name.clone();
            c.ino = v.ino;
            c.file_len = v.file_len;
            c.cow = v.cow && share;
            c.file_id = v.file_id;
            c.content = v.content;
            c.file = v.file.clone();
            c.shm = v.shm;
            c.growsdown = v.growsdown;
            child.trim_to_file(start)?;
        }
        let (mut borrowed, mut lent) = (Vec::new(), Vec::new());
        for start in cow {
            // 自己的页表项去掉了写权限：其他线程（已经停在 guest 代码之外）的 TLB 要刷新
            self.dirty |= self.lend(start, &child, &mut borrowed, &mut lent);
        }
        // 都是按地址顺序放进去的，一次建好
        child.cow.get_mut().borrowed = borrowed.into_iter().collect();
        self.cow
            .get_mut()
            .lent
            .append(&mut lent.into_iter().collect());
        drop(child);
        Ok(shared)
    }

    /// fork 时给子进程的小映射（起点是 `pieces`）切出宿主内存：切自同一段，各自按宿主页
    /// 对齐。这一段不计入宿主的提交量（`MAP_NORESERVE`）：每个映射都不超过
    /// [`FORK_PIECE_MAX`]，计不计入只关系到一次申请超过内存总量时失败。返回每个起点的
    /// 那一段和它的宿主地址。
    fn carve(vmas: &BTreeMap<u64, Vma>, pieces: &[u64]) -> Result<Carved, Errno> {
        let page = appine_hal::page_size() as u64;
        let mut out = BTreeMap::new();
        let total: u64 = pieces
            .iter()
            .map(|s| vmas[s].len.next_multiple_of(page))
            .sum();
        if total == 0 {
            return Ok(out);
        }
        let (region, base) = spare::anonymous(total)?;
        let mut at = 0;
        for &s in pieces {
            // SAFETY: 各段按顺序排在这段宿主内存里，at + len 不超过 total
            let host = unsafe { NonNull::new_unchecked(base.as_ptr().add(at as usize)) };
            out.insert(s, (region.clone(), host));
            at += vmas[&s].len.next_multiple_of(page);
        }
        Ok(out)
    }

    /// 给一个和别的地址空间共用宿主内存的私有文件映射复制一份自己的。
    fn unshare(&mut self, start: u64) -> Result<(), Errno> {
        let v = &self.vmas[&start];
        let (region, host) = Self::new_anonymous(v.len, v.prot & prot::WRITE != 0)?;
        // SAFETY: 两段宿主内存都至少有 v.len 字节，并且不重叠
        unsafe { copy_nonzero_pages(v.host.as_ptr(), host.as_ptr(), v.file_len as usize, None) };
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

    /// 有没有能获得写权限的共享映射指向宿主文件 (`dev`, `ino`)（memfd 加 `F_SEAL_WRITE`
    /// 之前检查）。
    pub fn writably_maps(&self, dev: u64, ino: u64) -> bool {
        self.vmas.values().any(|v| {
            v.backing == Backing::Shared
                && v.may_write
                && v.file_id.is_some_and(|(d, i, _)| (d, i) == (dev, ino))
        })
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

    /// 给起点落在 `[start, start + len)` 里的文件映射记下文件的名字和 inode（`/proc/<pid>/maps`
    /// 用）。与 Linux 一样，中间的匿名映射（.bss）没有名字。
    pub fn label(&mut self, start: u64, len: u64, name: Option<Rc<[u8]>>, ino: u64) {
        for (_, v) in self.vmas.range_mut(start..start.saturating_add(len)) {
            if v.file_id.is_some() {
                v.name = name.clone();
                v.ino = ino;
            }
        }
    }

    /// 记下初始的栈指针（加载器使用）。
    pub fn set_start_stack(&mut self, sp: u64) {
        self.start_stack = sp;
    }

    /// `/proc/<pid>/maps` 里标成 `[stack]` 的范围：与 Linux 一样是包含初始栈指针的映射，
    /// 加上它下面接着的、向下增长而且权限相同的 VMA（宿主内存不连续时栈长出来的部分）。
    fn stack_range(&self) -> Option<(u64, u64)> {
        let sp = self.start_stack;
        let (&s, v) = self
            .vmas
            .range(..=sp)
            .next_back()
            .filter(|&(&s, v)| sp != 0 && sp < s + v.len)?;
        let mut low = s;
        for (&b, o) in self.vmas.range(..s).rev() {
            if b + o.len != low || !o.growsdown || o.prot != v.prot {
                break;
            }
            low = b;
        }
        Some((low, s + v.len))
    }

    /// 所有映射，按地址排列；栈、堆和 vDSO 按 Linux 的习惯命名。相邻、权限相同的匿名映射（或者同一个文件里接着的部分）与 Linux 一样合成一段：
    /// 每次匿名 mmap 都是单独的宿主内存，VMA 合不起来，python 跑一会儿就有几百个。
    pub fn regions(&self) -> Vec<Region> {
        let stack = self.stack_range();
        let mut out: Vec<Region> = Vec::new();
        let regions = self.vmas.iter().map(|(&s, v)| {
            let special: Option<&[u8]> = if stack.is_some_and(|(lo, hi)| (lo..hi).contains(&s)) {
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
                file: v.file_id.map(|(_, _, offset)| (offset, v.ino)),
                locked: v.locked,
                may_write: v.may_write,
            }
        });
        for r in regions {
            if let Some(last) = out.last_mut() {
                let joins = match (last.file, r.file) {
                    (None, None) => !last.shared && !r.shared,
                    (Some((off, ino)), Some((next, next_ino))) => {
                        ino == next_ino && off + (last.end - last.start) == next
                    }
                    _ => false,
                };
                if joins
                    && last.end == r.start
                    && (last.prot, last.shared, last.locked) == (r.prot, r.shared, r.locked)
                    && last.may_write == r.may_write
                    && last.name == r.name
                {
                    last.end = r.end;
                    continue;
                }
            }
            out.push(r);
        }
        out
    }

    /// 把 `[addr, addr + len)` 的页调进宿主内存（mlock、`MAP_POPULATE`）。文件映射超出
    /// 文件末尾的部分不碰。
    pub fn populate(&self, addr: u64, len: u64) {
        let end = addr.saturating_add(len);
        let first = self
            .vmas
            .range(..=addr)
            .next_back()
            .map_or(addr, |(&s, _)| s);
        // 与 Linux 一样，没有任何权限的映射不调入
        for (&s, v) in self.vmas.range(first..end).filter(|(_, v)| v.prot != 0) {
            let from = addr.max(s);
            let to = end.min(s + v.file_len);
            if from < to {
                // SAFETY: [from, to) 在这个 VMA 里，宿主内存由它持有，并且在文件末尾之前
                unsafe {
                    appine_hal::populate(
                        v.host.as_ptr().add((from - s) as usize),
                        (to - from) as usize,
                    )
                };
            }
        }
    }

    /// `/proc/<pid>/pagemap` 从 `addr` 起 `pages` 页的表项：驻留（第 63 位），文件页或者
    /// 共享内存（第 61 位），私有匿名页只映射了一次（第 56 位）。与没有特权的进程在
    /// Linux 上看到的一样，不给页框号。
    pub fn pagemap(&self, addr: u64, pages: usize) -> Vec<u64> {
        const PRESENT: u64 = 1 << 63;
        const FILE: u64 = 1 << 61;
        const EXCLUSIVE: u64 = 1 << 56;
        let mut out = vec![0u64; pages];
        let end = addr.saturating_add(pages as u64 * PAGE_SIZE);
        let first = self
            .vmas
            .range(..=addr)
            .next_back()
            .map_or(addr, |(&s, _)| s);
        for (&s, v) in self.vmas.range(first..end) {
            let from = addr.max(s);
            let to = end.min(s + v.len);
            if from >= to {
                continue;
            }
            let bits = if v.backing == Backing::Anonymous {
                PRESENT | EXCLUSIVE
            } else {
                PRESENT | FILE
            };
            // SAFETY: [from, to) 落在这个 VMA 内
            let host = unsafe { v.host.as_ptr().add((from - s) as usize) };
            let resident = appine_hal::page_residency(host, (to - from) as usize);
            let at = ((from - addr) / PAGE_SIZE) as usize;
            for (i, r) in resident.into_iter().enumerate() {
                if r {
                    out[at + i] = bits;
                }
            }
        }
        // 借来的页驻留，但不是只映射了一次
        for (&page, _) in self.cow.borrow().borrowed.range(addr..end) {
            out[((page - addr) / PAGE_SIZE) as usize] = PRESENT;
        }
        out
    }

    /// 映射的总大小（字节）。
    pub fn size(&self) -> u64 {
        self.vmas.values().map(|v| v.len).sum()
    }

    /// 驻留在宿主内存里的 4K 页数。没写过的匿名映射跳过：读它只会映射到零页，Linux 的
    /// RSS 也不算零页。
    ///
    /// 进程退出、execve 时各量一次（`ru_maxrss`），shell 每起一个进程就量两次：
    /// - 宿主上首尾相接的映射（execve 时先后建立的那些大多是）合起来一次 mincore；
    /// - 文件映射里只有宿主 page cache 的页（没写过的私有映射、共享映射）时，驻留多少是
    ///   文件本身的事，与哪个进程映射无关，而 mincore 要在 page cache 里逐页查找：结果按
    ///   文件的一段缓存一会儿（[`FileResidency`]），程序和库的代码段不必每次都查。
    pub fn resident_pages(&self) -> u64 {
        self.rss_plan().measure()
    }

    /// 驻留页数峰值（`ru_maxrss`）的取样：自己的匿名内存不多时不量（[`RssPlan::estimate`]）。
    /// `fresh` 时（fork 之后马上 execve，shell 起进程的常见做法）只算借来的页和缓存着的
    /// 文件映射，不量它自己的匿名内存，那里只有 fork 之后写过的几页。
    pub fn peak_resident_pages(&self, fresh: bool) -> u64 {
        let plan = self.rss_plan();
        if fresh { plan.fixed } else { plan.estimate() }
    }

    /// [`Mm::resident_pages`] 要量的：不用 mincore 就知道的页数，和要 mincore 的宿主范围。
    fn rss_plan(&self) -> RssPlan {
        // 借来的页与 Linux 上写时复制的页一样算在里面
        let mut total = self.cow.borrow().borrowed.len() as u64;
        let mut spans = Vec::with_capacity(self.vmas.len());
        let now = std::time::Instant::now();
        for v in self.vmas.values() {
            let page_cache = match v.backing {
                Backing::Anonymous if !v.written.get() => continue,
                Backing::Anonymous => false,
                Backing::PrivateFile => !v.written.get(),
                Backing::Shared => true,
            };
            let span = (v.host.as_ptr() as usize, v.len as usize);
            match v.file_id {
                Some((dev, ino, offset)) if page_cache => {
                    total += FileResidency::get((dev, ino, offset, v.len), now, || {
                        appine_hal::resident_pages(v.host.as_ptr(), span.1) as u64
                    });
                }
                _ => spans.push(span),
            }
        }
        spans.sort_unstable();
        RssPlan {
            fixed: total,
            spans,
        }
    }

    /// 测试钩子（[`crate::bug`]）：让 `addr` 所在的页在页表里指向一个不能访问的宿主页，
    /// guest 访问它时引擎在宿主上出错。那一页宿主内存不归任何 VMA，故意泄漏。
    #[cfg(any(debug_assertions, feature = "test-hooks"))]
    pub fn break_page(&mut self, addr: u64) -> Result<(), Errno> {
        let host = appine_hal::map_inaccessible(PAGE_SIZE as usize).map_err(Errno)?;
        let page = addr & !(PAGE_SIZE - 1);
        // SAFETY: 访问这页宿主内存正是要模拟的 bug；它从不释放
        unsafe {
            self.mmu
                .map(page, host, PAGE_SIZE, prot::READ | prot::WRITE)
        }
        .map_err(map_err)
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
            if !self.is_free(old_end, grow) {
                return self.brk;
            }
            let grown = self.grow_heap(old_end, grow)
                || self.start_heap(old_end, grow)
                || self
                    .map_anonymous(old_end, grow, prot::READ | prot::WRITE)
                    .is_ok();
            if !grown {
                return self.brk;
            }
        } else if new_end < old_end && self.munmap(new_end, old_end - new_end).is_err() {
            return self.brk;
        }
        self.brk = new;
        new
    }

    /// 堆还没有映射时，在 `start` 映射 `len` 字节并预留 [`HEAP_RESERVE`] 的宿主地址空间。
    fn start_heap(&mut self, start: u64, len: u64) -> bool {
        if start != page_up(self.brk_start).unwrap_or(0) || len > HEAP_RESERVE {
            return false;
        }
        let Ok((region, host)) = Self::new_anonymous(HEAP_RESERVE, false) else {
            return false;
        };
        let rw = prot::READ | prot::WRITE;
        self.insert(start, len, rw, Backing::Anonymous, true, region, host)
            .is_ok()
    }

    /// 结束在 `end` 的堆 VMA 的宿主内存后面还有空余时，把它加长 `grow` 字节。
    fn grow_heap(&mut self, end: u64, grow: u64) -> bool {
        let Some((&s, v)) = self.vmas.range(..end).next_back() else {
            return false;
        };
        let rw = prot::READ | prot::WRITE;
        let compatible = v.backing == Backing::Anonymous
            && v.prot == rw
            && v.locked == self.lock_future
            && v.name.is_none();
        if s + v.len != end || !compatible {
            return false;
        }
        let used = v.host.as_ptr() as usize - v.region.base.as_ptr() as usize + v.len as usize;
        if used + grow as usize > v.region.len {
            return false;
        }
        // 宿主上紧接着的这段不能有别的映射在用（堆的一部分被 mremap 搬走之后仍然属于
        // 同一段宿主内存）
        let (lo, hi) = (v.host.as_ptr() as usize + v.len as usize, grow as usize);
        let taken = self.vmas.values().any(|o| {
            let at = o.host.as_ptr() as usize;
            Arc::ptr_eq(&o.region, &v.region) && at < lo + hi && lo < at + o.len as usize
        });
        if taken {
            return false;
        }
        // SAFETY: [lo, lo + hi) 在这段私有匿名内存里，没有别的映射在用
        let host = unsafe { NonNull::new_unchecked(lo as *mut u8) };
        // 缩小之后又扩展：以前写过的内容要变回零
        unsafe { discard_host(host, hi) };
        if Self::map_pages(&mut self.mmu, end, host, grow, rw).is_err() {
            return false;
        }
        let v = self.vmas.get_mut(&s).unwrap();
        v.len += grow;
        v.file_len += grow;
        v.written.set(true);
        true
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
    pub(crate) fn write_kernel(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        // 直接写 VMA 自己的宿主内存：写时复制的页先换成自己的，与别的地址空间共用宿主
        // 内存的文件映射先复制一份自己的
        let end = page_up(addr.saturating_add(data.len() as u64)).unwrap_or(u64::MAX);
        self.cow_settle(addr & !(PAGE_SIZE - 1), end);
        let first = self
            .vmas
            .range(..=addr)
            .next_back()
            .map_or(addr, |(&s, _)| s);
        let shared: Vec<u64> = self
            .vmas
            .range(first..end)
            .filter(|(_, v)| v.cow)
            .map(|(&s, _)| s)
            .collect();
        for s in shared {
            self.unshare(s)?;
        }
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
            let host = match self.mmu.translate(cur, required_prot) {
                Some(host) => host,
                // 写时复制的页：先换成可写的（Mm::cow_break）
                None if required_prot & prot::WRITE != 0 && self.cow_break(cur) => self
                    .mmu
                    .translate(cur, required_prot)
                    .ok_or(Errno::EFAULT)?,
                None => return Err(Errno::EFAULT),
            };
            f(host.as_ptr(), done, chunk);
            done += chunk;
        }
        Ok(())
    }
}

/// 写时复制（模块文档、设计文档 §5.3）。经由共享借用进行：内核往 guest 内存写的时候
/// 只有 `&Mm`，拥有者收回页时也要改借用方的页表。
impl Mm {
    /// 页 `page` 所在的私有匿名 VMA 和它的起点。
    fn anon_vma(&self, page: u64) -> Option<(u64, &Vma)> {
        let (&s, v) = self.vmas.range(..=page).next_back()?;
        (page < s + v.len && v.backing == Backing::Anonymous).then_some((s, v))
    }

    /// 页 `page` 在自己的 VMA 里的宿主页。
    fn own_page(s: u64, v: &Vma, page: u64) -> NonNull<u8> {
        // SAFETY: page 落在这个 VMA 里，偏移不越过宿主映射
        unsafe { v.host.add((page - s) as usize) }
    }

    /// 页 `page` 写时复制的页表权限：没有写权限。
    fn read_only(v: &Vma) -> u32 {
        mmu_prot(v.prot) & !prot::WRITE
    }

    /// 往 `page` 写之前调用（写缺页、内核写 guest 内存）：借来的页复制一份自己的，借出
    /// 的页先让借用方各自复制。返回 true 表示页表项已经可写；VMA 不可写、不是写时复制
    /// 的页时返回 false。
    pub fn cow_break(&self, page: u64) -> bool {
        let page = page & !(PAGE_SIZE - 1);
        if self
            .anon_vma(page)
            .is_none_or(|(_, v)| v.prot & prot::WRITE == 0)
        {
            return false;
        }
        let (borrowed, lent) = {
            let c = self.cow.borrow();
            (c.borrowed.contains_key(&page), c.lent.contains(&page))
        };
        if borrowed {
            self.materialize(page, true);
        } else if lent {
            self.recall(page);
        }
        borrowed || lent
    }

    /// 借来的页 `page` 换成自己的一份：复制到自己的 VMA 里对应的位置，页表项指向那里。
    /// `notify` 时，还回的是最后一个借用的话让拥有者恢复写权限；拥有者自己收回时不必。
    fn materialize(&self, page: u64, notify: bool) {
        let Some(b) = self.cow.borrow_mut().borrowed.remove(&page) else {
            return;
        };
        if let Some((s, v)) = self.anon_vma(page) {
            let own = Self::own_page(s, v, page);
            // SAFETY: 两边都是一整页，借来的那页由 b.region 持有
            unsafe {
                std::ptr::copy_nonoverlapping(b.host.as_ptr(), own.as_ptr(), PAGE_SIZE as usize)
            };
            if mmu_prot(v.prot) != 0 {
                // SAFETY: 宿主页属于这个 VMA
                let _ = unsafe { self.mmu.map_shared(page, own, PAGE_SIZE, mmu_prot(v.prot)) };
            }
        }
        // 线程的 TLB 里可能还指向借来的页：刷新之后才能还回去，拥有者接着就可能写它
        self.shootdown();
        self.return_loan(b.host, page, notify);
    }

    /// 还回借来的宿主页 `host`（在这里的 guest 地址是 `page`）。
    fn return_loan(&self, host: NonNull<u8>, page: u64, notify: bool) {
        self.return_loans([(page, host)], notify);
    }

    /// 还回一批借来的页（这里的 guest 地址，拥有者的宿主页）。`notify` 时，还回的是最后
    /// 一个借用的页让拥有者恢复写权限，同一个拥有者的一起处理。
    fn return_loans(&self, pages: impl IntoIterator<Item = (u64, NonNull<u8>)>, notify: bool) {
        let mut owners: Vec<(MmRef, Vec<u64>)> = Vec::new();
        {
            let mut loans = self.loans.borrow_mut();
            for (page, host) in pages {
                let key = host.as_ptr() as usize;
                let Some(loan) = loans.get_mut(&key) else {
                    continue;
                };
                if loan.give_back(&self.me, page) {
                    continue;
                }
                if let Some((owner, addr)) = loans.remove(&key).and_then(|l| l.owner)
                    && notify
                {
                    match owners.iter_mut().find(|(o, _)| o.ptr_eq(&owner)) {
                        Some((_, addrs)) => addrs.push(addr),
                        None => owners.push((owner, vec![addr])),
                    }
                }
            }
        }
        // 拥有者正被借用（它自己的操作引起的，比如收回）时不必管：它的页表项下次写缺页
        // 时恢复（Mm::cow_break）
        for (owner, addrs) in owners {
            if let Some(owner) = owner.upgrade()
                && let Ok(owner) = owner.try_borrow()
            {
                owner.repossess(&addrs);
            }
        }
    }

    /// 借出的这些页没有借用方了：恢复写权限（只增加权限，不必刷新 TLB）。
    fn repossess(&self, pages: &[u64]) {
        let mut runs = Runs::default();
        {
            let mut c = self.cow.borrow_mut();
            for &page in pages {
                if c.lent.remove(&page)
                    && let Some((s, v)) = self.anon_vma(page)
                {
                    runs.push(page, Self::own_page(s, v, page));
                }
            }
        }
        for (page, _, len) in runs.runs {
            if let Some((_, v)) = self.anon_vma(page) {
                let _ = self.mmu.protect_shared(page, len, mmu_prot(v.prot));
            }
        }
    }

    /// 拥有者要改借出的页 `page` 之前：每个借用方复制自己的一份，然后恢复写权限。
    fn recall(&self, page: u64) {
        let Some((s, v)) = self.anon_vma(page) else {
            self.cow.borrow_mut().lent.remove(&page);
            return;
        };
        let key = Self::own_page(s, v, page).as_ptr() as usize;
        let loan = self.loans.borrow_mut().remove(&key);
        for (b, addr) in loan.into_iter().flat_map(Loan::borrowers) {
            if let Some(b) = b.upgrade() {
                b.borrow().materialize(addr, false);
            }
        }
        self.repossess(&[page]);
    }

    /// 页表项改指别处之后马上做的 shootdown：先改代数，使用者重新进入 guest 代码之前
    /// 刷新 TLB；正在执行 guest 代码的打断、等它们退出（[`Mm::sync`] 的做法，但调用方
    /// 接着就要用那一页）。
    fn shootdown(&self) {
        if self.users.is_empty() {
            return;
        }
        // 与使用者那边"先标记、再读代数"配对：要么它读到新的代数，要么这里看到它在执行
        self.generation.fetch_add(1, Ordering::SeqCst);
        for u in &self.users {
            if u.in_guest.load(Ordering::SeqCst) {
                // SAFETY: 线程在销毁自己的 Cpu 之前（持有 BKL）从 users 里注销
                unsafe { u.kicker.kick() };
            }
        }
        for u in &self.users {
            while u.in_guest.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
    }

    /// `[start, end)` 里的页不再写时复制，内容保留在自己的宿主内存里：借出的先收回，
    /// 借来的复制一份（mremap 搬走、丢弃、内核直接写宿主内存之前）。
    fn cow_settle(&self, start: u64, end: u64) {
        let (lent, borrowed): (Vec<u64>, Vec<u64>) = {
            let c = self.cow.borrow();
            (
                c.lent.range(start..end).copied().collect(),
                c.borrowed.range(start..end).map(|(&p, _)| p).collect(),
            )
        };
        for page in lent {
            self.recall(page);
        }
        for page in borrowed {
            self.materialize(page, true);
        }
    }

    /// `[start, end)` 要解除映射：借出的先收回；借来的从记录里拿走，等调用方改掉页表项、
    /// 刷新 TLB 之后用 [`Mm::give_back`] 还回去（内容不要了，不必复制）。
    fn cow_unmap(&self, start: u64, end: u64) -> Vec<(u64, Borrowed)> {
        let lent: Vec<u64> = self.cow.borrow().lent.range(start..end).copied().collect();
        for page in lent {
            self.recall(page);
        }
        let mut c = self.cow.borrow_mut();
        let pages: Vec<u64> = c.borrowed.range(start..end).map(|(&p, _)| p).collect();
        pages
            .into_iter()
            .map(|p| (p, c.borrowed.remove(&p).unwrap()))
            .collect()
    }

    /// 还回 [`Mm::cow_unmap`] 拿走的页。
    fn give_back(&self, pages: Vec<(u64, Borrowed)>) {
        if pages.is_empty() {
            return;
        }
        self.shootdown();
        self.return_loans(pages.into_iter().map(|(page, b)| (page, b.host)), true);
    }

    /// mprotect 改了 `[start, end)` 的页表项之后：写时复制的页仍然不能写，借来的页仍然
    /// 指向拥有者的宿主页。
    fn cow_protect(&self, start: u64, end: u64) {
        let c = self.cow.borrow();
        for &page in c.lent.range(start..end) {
            if let Some((_, v)) = self.anon_vma(page)
                && Self::read_only(v) != 0
            {
                let _ = self.mmu.protect_shared(page, PAGE_SIZE, Self::read_only(v));
            }
        }
        for (&page, b) in c.borrowed.range(start..end) {
            if let Some((_, v)) = self.anon_vma(page)
                && Self::read_only(v) != 0
            {
                // SAFETY: 借来的宿主页由 b.region 持有
                let _ = unsafe {
                    self.mmu
                        .map_shared(page, b.host, PAGE_SIZE, Self::read_only(v))
                };
            }
        }
    }

    /// `[start, end)` 里有没有写时复制的页。
    fn has_cow(&self, start: u64, end: u64) -> bool {
        let c = self.cow.borrow();
        c.lent.range(start..end).next().is_some() || c.borrowed.range(start..end).next().is_some()
    }

    /// fork：VMA `start`（私有匿名、写过）里有内容的页借给子进程 `child`，子进程在同一个
    /// 地址的 VMA 已经映射好了自己的（全零的）宿主内存。自己借来的页让子进程向原来的
    /// 拥有者借。子进程借的页按地址顺序放进 `borrowed`，新借出的放进 `lent`，由调用方
    /// 一起建表。返回有没有新借出的页（自己的页表项去掉了写权限，要做 shootdown）。
    fn lend(
        &self,
        start: u64,
        child: &Mm,
        borrowed: &mut Vec<(u64, Borrowed)>,
        lent: &mut Vec<u64>,
    ) -> bool {
        let v = &self.vmas[&start];
        let end = start + v.len;
        // 宿主从没碰过的页一定是零，子进程用自己的就行。小的映射多半都碰过，不为它们读
        // pagemap（一次 pread，fork 持锁时间的三成多），全部借出
        let touched = (v.len > LEND_ALL_MAX)
            .then(|| appine_hal::touched_pages(v.host.as_ptr(), v.len as usize))
            .flatten();
        let ro = Self::read_only(v);
        let cow = self.cow.borrow();
        let mut loans = self.loans.borrow_mut();
        loans.reserve(
            touched
                .as_ref()
                .map_or(0, |t| t.iter().filter(|&&t| t).count()),
        );
        // 自己借来的、已经借出的页按地址顺序和逐页的遍历合并，不必每页查一次
        let mut mine = cow.borrowed.range(start..end).peekable();
        let mut out = cow.lent.range(start..end).peekable();
        let (mut parent_runs, mut child_runs) = (Runs::default(), Runs::default());
        let before = lent.len();
        for (i, page) in (start..end).step_by(PAGE_SIZE as usize).enumerate() {
            let (host, region) = if let Some((_, b)) = mine.next_if(|&(&p, _)| p == page) {
                if let Some(loan) = loans.get_mut(&(b.host.as_ptr() as usize)) {
                    loan.lend((child.me.clone(), page));
                }
                (b.host, b.region.clone())
            } else if touched.as_ref().is_none_or(|t| t[i]) {
                let host = Self::own_page(start, v, page);
                let key = host.as_ptr() as usize;
                if out.next_if(|&&p| p == page).is_some() {
                    if let Some(loan) = loans.get_mut(&key) {
                        loan.lend((child.me.clone(), page));
                    }
                } else {
                    let owner = Some((self.me.clone(), page));
                    loans.insert(key, Loan::new(owner, (child.me.clone(), page)));
                    lent.push(page);
                    parent_runs.push(page, host);
                }
                (host, v.region.clone())
            } else {
                continue;
            };
            child_runs.push(page, host);
            borrowed.push((page, Borrowed { host, region }));
        }
        if ro != 0 {
            for (page, _, len) in parent_runs.runs {
                let _ = self.mmu.protect_shared(page, len, ro);
            }
            for (page, host, len) in child_runs.runs {
                // SAFETY: 借来的宿主页由 Borrowed::region 持有，记在子进程的 borrowed 里
                let _ = unsafe {
                    child
                        .mmu
                        .map_shared(page, NonNull::new_unchecked(host as *mut u8), len, ro)
                };
            }
        }
        lent.len() > before
    }

    /// 地址空间销毁时：私有匿名内存里没有借出的页交还宿主。借出的页留给借用方，它们持有
    /// 那段宿主内存，要等最后一个借用方还回来才释放；同一段宿主内存可能切成了几个 VMA，
    /// 所以每个都要看（没写过的内容全是零、宿主没碰过，不必看）。
    fn discard_unlent(&self, lent: &BTreeSet<u64>) {
        for (&s, v) in self
            .vmas
            .iter()
            .filter(|(_, v)| v.backing == Backing::Anonymous && v.written.get())
        {
            let end = s + v.len;
            let mut cursor = s;
            for page in lent.range(s..end).copied().chain([end]) {
                if page > cursor {
                    // SAFETY: [cursor, page) 落在这个私有匿名 VMA 里，不再有人用
                    unsafe { discard_host(Self::own_page(s, v, cursor), (page - cursor) as usize) };
                }
                cursor = page + PAGE_SIZE;
            }
        }
    }
}

impl Drop for Mm {
    fn drop(&mut self) {
        self.release_cow();
        // SAFETY: 之后不再使用 self.mmu
        let mmu = unsafe { ManuallyDrop::take(&mut self.mmu) };
        // 持有 BKL 时放开之后再释放（release_unmapped）
        if let Some(mmu) = UNMAPPED.with_borrow_mut(|u| match u {
            Some(u) => {
                u.mmus.push(mmu);
                None
            }
            None => Some(mmu),
        }) {
            drop(mmu);
        }
    }
}

impl Mm {
    /// 地址空间销毁时：记下翻译所在的映射，还回借来的页，借出的页留给借用方。
    fn release_cow(&mut self) {
        if let Some(cs) = &self.code_space {
            cs.space.set_sources(&self.code_sources());
        }
        let cow = std::mem::take(self.cow.get_mut());
        // 已经没有线程在用这个地址空间（退出、execve），还回借来的页之前不必刷新 TLB
        self.return_loans(
            cow.borrowed.into_iter().map(|(page, b)| (page, b.host)),
            true,
        );
        if cow.lent.is_empty() {
            return;
        }
        // 借出的页成了孤儿页
        {
            let mut loans = self.loans.borrow_mut();
            for &page in &cow.lent {
                if let Some((s, v)) = self.anon_vma(page)
                    && let Some(loan) =
                        loans.get_mut(&(Self::own_page(s, v, page).as_ptr() as usize))
                {
                    loan.owner = None;
                }
            }
        }
        self.discard_unlent(&cow.lent);
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
/// `touched` 是每页有没有被碰过（[`appine_hal::touched_pages`]），没碰过的页不看。
unsafe fn copy_nonzero_pages(src: *const u8, dst: *mut u8, len: usize, touched: Option<&[bool]>) {
    const PAGE: usize = PAGE_SIZE as usize;
    for (i, off) in (0..len).step_by(PAGE).enumerate() {
        if touched.is_some_and(|t| !t[i]) {
            continue;
        }
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
        assert!(a + 4 * PAGE_SIZE <= mm.mmap_top);
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
        let child = child.borrow();
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
        let mut child = child.borrow_mut();
        let mut buf = [0u8; 4];
        child.read(UserSlice::new(reserved, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"heap");
        child.read(UserSlice::new(text, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"code");
        // PROT_NONE 的页不在页表里，权限加回来之后内容还在
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
        let child = mm.fork().unwrap();
        let mut child = child.borrow_mut();
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
    fn growsdown_mappings_grow_until_the_guard_gap() {
        let mut mm = Mm::new();
        let fixed = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_FIXED;
        let base = 0x4000_0000;
        let top = base + 1024 * PAGE_SIZE;
        mm.mmap(top - PAGE_SIZE, PAGE_SIZE, RW, fixed, None)
            .unwrap();
        mm.set_growsdown(top - PAGE_SIZE);
        // 访问下面隔了几页的地址：一直长到那里，长出来的部分还能接着长
        assert!(mm.grow_down(top - 5 * PAGE_SIZE + 8, u64::MAX));
        mm.write(UserSlice::new(top - 5 * PAGE_SIZE, 2), b"ok")
            .unwrap();
        assert!(mm.grow_down(top - 6 * PAGE_SIZE, u64::MAX));
        // 已经映射的地址、不向下增长的映射下面都不增长
        assert!(!mm.grow_down(top - 6 * PAGE_SIZE, u64::MAX));
        mm.mmap(base, PAGE_SIZE, RW, fixed, None).unwrap();
        assert!(!mm.grow_down(base - PAGE_SIZE, u64::MAX));
        // 离下面那个映射不到 256 页：不增长；它是 PROT_NONE 时不算
        let guard_end = base + PAGE_SIZE + STACK_GUARD_GAP;
        assert!(!mm.grow_down(guard_end - PAGE_SIZE, u64::MAX));
        mm.mprotect(base, PAGE_SIZE, 0).unwrap();
        assert!(mm.grow_down(guard_end - PAGE_SIZE, u64::MAX));
        // fork 之后子进程里的也照样增长
        let child = mm.fork().unwrap();
        let mut child = child.borrow_mut();
        assert!(child.grow_down(guard_end - 2 * PAGE_SIZE, u64::MAX));
    }

    #[test]
    fn stack_grows_in_place_until_its_reservation_runs_out() {
        let mut mm = Mm::new();
        mm.map_stack(2 * PAGE_SIZE, 16 * PAGE_SIZE).unwrap();
        mm.set_start_stack(STACK_TOP - 16);
        mm.write(UserSlice::new(STACK_TOP - 4, 4), b"top!").unwrap();
        // 预留的范围里：还是同一个 VMA，原来的内容不变
        assert!(mm.grow_down(STACK_TOP - 10 * PAGE_SIZE + 100, u64::MAX));
        assert_eq!(mm.vmas.len(), 1);
        assert_eq!(mm.stack_low(), STACK_TOP - 10 * PAGE_SIZE);
        mm.write(UserSlice::new(STACK_TOP - 10 * PAGE_SIZE, 2), b"lo")
            .unwrap();
        let mut buf = [0u8; 4];
        mm.read(UserSlice::new(STACK_TOP - 4, 4), &mut buf).unwrap();
        assert_eq!(&buf, b"top!");
        assert!(mm.grow_down(STACK_TOP - 16 * PAGE_SIZE, u64::MAX));
        assert_eq!(mm.vmas.len(), 1);
        // 超出预留：新的一段，下面再预留到和栈一样大，接着就地增长
        assert!(mm.grow_down(STACK_TOP - 17 * PAGE_SIZE, u64::MAX));
        assert!(mm.grow_down(STACK_TOP - 32 * PAGE_SIZE, u64::MAX));
        assert_eq!(mm.vmas.len(), 2);
        assert!(mm.grow_down(STACK_TOP - 33 * PAGE_SIZE, u64::MAX));
        assert_eq!(mm.vmas.len(), 3);
        // maps 里仍然是一行 [stack]
        let regions = mm.regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(
            (regions[0].start, regions[0].end),
            (STACK_TOP - 33 * PAGE_SIZE, STACK_TOP)
        );
        assert_eq!(regions[0].name.as_deref(), Some(&b"[stack]"[..]));
    }

    #[test]
    fn stack_growth_stops_at_the_limit() {
        let mut mm = Mm::new();
        mm.map_stack(2 * PAGE_SIZE, 4 * PAGE_SIZE).unwrap();
        // 长完之后的大小按分开的几段合起来算
        assert!(!mm.grow_down(STACK_TOP - 9 * PAGE_SIZE, 8 * PAGE_SIZE));
        assert!(mm.grow_down(STACK_TOP - 6 * PAGE_SIZE, 8 * PAGE_SIZE));
        assert!(!mm.grow_down(STACK_TOP - 9 * PAGE_SIZE, 8 * PAGE_SIZE));
        assert!(mm.grow_down(STACK_TOP - 8 * PAGE_SIZE, 8 * PAGE_SIZE));
        assert_eq!(mm.vmas.len(), 3);
        // 上限调高之后接着长
        assert!(mm.grow_down(STACK_TOP - 9 * PAGE_SIZE, 9 * PAGE_SIZE));
    }

    #[test]
    fn fork_keeps_the_reservation_below_the_stack() {
        let mut mm = Mm::new();
        mm.map_stack(2 * PAGE_SIZE, 16 * PAGE_SIZE).unwrap();
        mm.write(UserSlice::new(STACK_TOP - 2, 2), b"hi").unwrap();
        let child = mm.fork().unwrap();
        let mut child = child.borrow_mut();
        assert_eq!(child.stack_low(), STACK_TOP - 2 * PAGE_SIZE);
        assert!(child.grow_down(STACK_TOP - 16 * PAGE_SIZE, u64::MAX));
        assert_eq!(child.vmas.len(), 1);
        let mut buf = [0u8; 2];
        child
            .read(UserSlice::new(STACK_TOP - 2, 2), &mut buf)
            .unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn mmap_area_leaves_room_for_the_stack_limit() {
        let mut mm = Mm::new();
        let default = mm.mmap_top;
        mm.set_layout(STACK_SIZE);
        assert_eq!(mm.mmap_top, default);
        mm.set_layout(4 << 30);
        assert_eq!(mm.mmap_top, STACK_TOP - (4 << 30) - STACK_GUARD_GAP);
        mm.set_layout(u64::MAX);
        assert_eq!(mm.mmap_top, (STACK_TOP - MAX_GAP) & !(PAGE_SIZE - 1));
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

    #[test]
    fn brk_extends_one_vma_and_zeroes_regrown_pages() {
        let mut mm = Mm::new();
        mm.set_brk(0x50_0000);
        for i in 1..=64 {
            assert_eq!(mm.brk(0x50_0000 + i * PAGE_SIZE), 0x50_0000 + i * PAGE_SIZE);
        }
        assert_eq!(mm.vmas.len(), 1);
        mm.write_kernel(0x53_f000, b"old").unwrap();
        mm.brk(0x52_0000);
        mm.brk(0x54_0000);
        assert_eq!(mm.vmas.len(), 1);
        let mut buf = [1u8; 3];
        mm.read(UserSlice::new(0x53_f000, 3), &mut buf).unwrap();
        assert_eq!(buf, [0; 3]);
        // 子进程里接着扩展也还是一个 VMA，内容是复制过来的
        mm.write_kernel(0x50_0000, b"heap").unwrap();
        let child = mm.fork().unwrap();
        let mut buf = [0u8; 4];
        {
            let mut child = child.borrow_mut();
            assert_eq!(child.brk(0x58_0000), 0x58_0000);
            assert_eq!(child.vmas.len(), 1);
            child.read(UserSlice::new(0x50_0000, 4), &mut buf).unwrap();
            assert_eq!(&buf, b"heap");
        }
        // 堆中间的一页被换掉之后，后面那段照样能扩展；子进程借的那页先复制给它
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS | flags::MAP_FIXED;
        mm.mmap(0x50_0000, PAGE_SIZE, 0, anon, None).unwrap();
        child
            .borrow()
            .read(UserSlice::new(0x50_0000, 4), &mut buf)
            .unwrap();
        assert_eq!(&buf, b"heap");
        assert_eq!(mm.brk(0x60_0000), 0x60_0000);
        assert_eq!(mm.vmas.len(), 2);
        let regions = mm.regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[1].name.as_deref(), Some(&b"[heap]"[..]));
    }

    /// 读 `addr` 处的一个字节。
    fn byte(mm: &Rc<RefCell<Mm>>, addr: u64) -> u8 {
        let mut b = [0u8];
        mm.borrow().read(UserSlice::new(addr, 1), &mut b).unwrap();
        b[0]
    }

    fn poke(mm: &Rc<RefCell<Mm>>, addr: u64, b: u8) {
        mm.borrow().write(UserSlice::new(addr, 1), &[b]).unwrap();
    }

    fn writable(mm: &Rc<RefCell<Mm>>, addr: u64) -> bool {
        mm.borrow().mmu().translate(addr, prot::WRITE).is_some()
    }

    #[test]
    fn fork_shares_written_pages_until_either_side_writes() {
        let parent = Mm::new().into_shared();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let a = parent
            .borrow_mut()
            .mmap(0, 4 * PAGE_SIZE, RW, anon, None)
            .unwrap();
        for i in 0..4 {
            poke(&parent, a + i * PAGE_SIZE, b'p');
        }
        let child = parent.borrow_mut().fork().unwrap();
        // 两边的页表都指向父进程的宿主页，都不能写
        let host = |mm: &Rc<RefCell<Mm>>, addr| mm.borrow().mmu().translate(addr, prot::READ);
        assert_eq!(host(&parent, a), host(&child, a));
        assert!(!writable(&parent, a) && !writable(&child, a));
        // 子进程写：复制一份自己的；父进程是唯一的使用者了，马上恢复写权限
        poke(&child, a, b'c');
        assert_eq!((byte(&parent, a), byte(&child, a)), (b'p', b'c'));
        assert!(writable(&parent, a) && writable(&child, a));
        // 父进程写：子进程先复制一份
        poke(&parent, a + PAGE_SIZE, b'q');
        assert_eq!(byte(&child, a + PAGE_SIZE), b'p');
        // 孙进程向子进程借第 0 页，向原来的拥有者借第 2 页
        let grandchild = child.borrow_mut().fork().unwrap();
        assert_eq!(
            host(&grandchild, a + 2 * PAGE_SIZE),
            host(&parent, a + 2 * PAGE_SIZE)
        );
        poke(&parent, a + 2 * PAGE_SIZE, b'r');
        assert_eq!(byte(&child, a + 2 * PAGE_SIZE), b'p');
        assert_eq!(byte(&grandchild, a + 2 * PAGE_SIZE), b'p');
        poke(&child, a, b'd');
        assert_eq!(byte(&grandchild, a), b'c');
        // 拥有者的地址空间销毁：借出的页留给子孙，内容不变，它们照样能写
        drop(parent);
        poke(&grandchild, a + 3 * PAGE_SIZE, b'g');
        assert_eq!(byte(&child, a + 3 * PAGE_SIZE), b'p');
        assert_eq!(byte(&grandchild, a + 3 * PAGE_SIZE), b'g');
        // 子进程解除映射：孙进程借它的页先复制
        child.borrow_mut().munmap(a, 4 * PAGE_SIZE).unwrap();
        assert_eq!(byte(&grandchild, a), b'c');
        assert_eq!(byte(&grandchild, a + PAGE_SIZE), b'p');
        drop(child);
        let g = grandchild.borrow();
        assert!(g.cow.borrow().borrowed.is_empty() && g.cow.borrow().lent.is_empty());
        assert!(g.loans.borrow().is_empty());
    }

    #[test]
    fn copy_on_write_survives_mprotect_discard_and_mremap() {
        let parent = Mm::new().into_shared();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let a = parent
            .borrow_mut()
            .mmap(0, 3 * PAGE_SIZE, RW, anon, None)
            .unwrap();
        for i in 0..3 {
            poke(&parent, a + i * PAGE_SIZE, b'p');
        }
        let child = parent.borrow_mut().fork().unwrap();
        // 去掉权限再加回来（包括 PROT_NONE，那时页不在页表里）：仍然借着、仍然不能写
        for mm in [&parent, &child] {
            for p in [prot::READ, 0, RW] {
                mm.borrow_mut().mprotect(a, 3 * PAGE_SIZE, p).unwrap();
            }
            assert_eq!(byte(mm, a), b'p');
            assert!(!writable(mm, a));
        }
        // 丢弃：另一边的内容不变
        parent.borrow_mut().discard(a, PAGE_SIZE).unwrap();
        child
            .borrow_mut()
            .discard(a + PAGE_SIZE, PAGE_SIZE)
            .unwrap();
        assert_eq!((byte(&parent, a), byte(&child, a)), (0, b'p'));
        assert_eq!(
            (byte(&parent, a + PAGE_SIZE), byte(&child, a + PAGE_SIZE)),
            (b'p', 0)
        );
        // mremap 搬走之后各写各的
        let moved = parent
            .borrow_mut()
            .mremap(
                a + 2 * PAGE_SIZE,
                PAGE_SIZE,
                PAGE_SIZE,
                true,
                Some(0x1000_0000),
            )
            .unwrap();
        poke(&parent, moved, b'q');
        assert_eq!(byte(&child, a + 2 * PAGE_SIZE), b'p');
        poke(&child, a + 2 * PAGE_SIZE, b'c');
        assert_eq!(byte(&parent, moved), b'q');
    }

    #[test]
    fn futex_keys_of_borrowed_pages_are_their_own() {
        let parent = Mm::new().into_shared();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let a = parent
            .borrow_mut()
            .mmap(0, PAGE_SIZE, RW, anon, None)
            .unwrap();
        poke(&parent, a, 1);
        let child = parent.borrow_mut().fork().unwrap();
        // 写时复制之后键就会变：借来的页先换成自己的
        let key = child.borrow().futex_key(a).unwrap();
        assert!(child.borrow().cow.borrow().borrowed.is_empty());
        assert!(key != parent.borrow().futex_key(a).unwrap());
        poke(&child, a, 2);
        assert!(child.borrow().futex_key(a).unwrap() == key);
        assert_eq!(byte(&parent, a), 1);
    }

    #[test]
    fn regions_join_adjacent_anonymous_mappings() {
        let mut mm = Mm::new();
        let anon = flags::MAP_PRIVATE | flags::MAP_ANONYMOUS;
        let a = mm.mmap(0, PAGE_SIZE, RW, anon, None).unwrap();
        let b = mm.mmap(0, PAGE_SIZE, RW, anon, None).unwrap();
        let c = mm.mmap(0, PAGE_SIZE, prot::READ, anon, None).unwrap();
        assert_eq!((a - b, b - c), (PAGE_SIZE, PAGE_SIZE));
        let r = mm.regions();
        assert_eq!(r.len(), 2);
        assert_eq!((r[1].start, r[1].end), (b, a + PAGE_SIZE));
        let shared = flags::MAP_SHARED | flags::MAP_ANONYMOUS;
        mm.mmap(0, PAGE_SIZE, RW, shared, None).unwrap();
        mm.mmap(0, PAGE_SIZE, RW, shared, None).unwrap();
        assert_eq!(mm.regions().len(), 4);
    }
}
