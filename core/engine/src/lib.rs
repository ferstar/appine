//! appine 引擎（C 实现）的 Rust 绑定。
//!
//! 引擎负责执行 guest 指令和 soft-MMU 快速路径（设计文档 §4、§5）。内核层用
//! [`Cpu::run`] 驱动执行，再根据返回的 [`Exit`] 处理 syscall、异常等事件。

use std::ffi::{c_int, c_void};
use std::fmt;
use std::mem::offset_of;
use std::ptr::NonNull;
use std::sync::Arc;

/// guest 页大小，固定 4K（设计文档 §5.2）。
pub const PAGE_SIZE: u64 = 4096;
/// guest 虚拟地址位数。
pub const VA_BITS: u32 = 48;
/// 每个 CPU 的软件数据 TLB 项数。
pub const TLB_ENTRIES: usize = 4096;
/// 每个 CPU 的软件取指 TLB 项数。
pub const ITLB_ENTRIES: usize = 512;

/// 页权限，取值与 Linux 的 `PROT_*` 一致。
pub mod prot {
    /// 可读。
    pub const READ: u32 = 1;
    /// 可写。
    pub const WRITE: u32 = 2;
    /// 可执行。
    pub const EXEC: u32 = 4;
}

#[repr(C)]
struct RawMmu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TlbEntry {
    tag_read: u64,
    addend_read: u64,
    tag_write: u64,
    addend_write: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ItlbEntry {
    tag: u64,
    addend: u64,
}

/// guest CPU 状态，逐字段镜像 C 的 `struct appine_cpu`。
#[repr(C)]
pub struct CpuState {
    /// 通用寄存器 X0–X30。
    pub x: [u64; 31],
    /// 栈指针。
    pub sp: u64,
    /// 程序计数器。
    pub pc: u64,
    /// 条件标志，只使用 bit 31..28。
    pub nzcv: u64,
    /// 浮点控制寄存器。
    pub fpcr: u64,
    /// 浮点状态寄存器。
    pub fpsr: u64,
    /// 用户态线程指针。
    pub tpidr_el0: u64,
    /// syscall 重启时用来恢复的原始 X0，由内核层维护。
    pub orig_x0: u64,
    fault_addr: u64,
    fault_access: u32,
    fault_kind: u32,
    exit_info: u32,
    dbt_lookup_seq: u32,
    pending: u64,
    mmu: *mut RawMmu,
    excl_addr: u64,
    excl_value: [u64; 2],
    excl_size: u32,
    reserved2: u32,
    dbt_reason: u64,
    dbt_site: u64,
    dbt_jump_cache: *mut c_void,
    dbt_rs: *mut c_void,
    /// Tier 1 里 `ic ivau` 记下、还没交出来作废的页（0 表示没有）。引擎在 dsb、isb 时以
    /// [`Exit::InvalidateCode`] 交出，因为别的原因退出时由调用方取走并作废。
    pub ivau_page: u64,
    dbt_groups: [u64; 4],
    tlb: [TlbEntry; TLB_ENTRIES],
    itlb: [ItlbEntry; ITLB_ENTRIES],
    /// V0–V31，每个寄存器低 64 位在前。
    pub v: [[u64; 2]; 32],
    tlb_used: [u64; TLB_ENTRIES / 64],
}

unsafe extern "C" {
    fn appine_mmu_new() -> *mut RawMmu;
    fn appine_mmu_free(mmu: *mut RawMmu);
    fn appine_mmu_map(mmu: *mut RawMmu, gva: u64, host: *mut c_void, len: u64, prot: u32) -> c_int;
    fn appine_mmu_unmap(mmu: *mut RawMmu, gva: u64, len: u64) -> c_int;
    fn appine_mmu_protect(mmu: *mut RawMmu, gva: u64, len: u64, prot: u32) -> c_int;
    fn appine_mmu_translate(mmu: *const RawMmu, gva: u64, required_prot: u32) -> *mut c_void;

    fn appine_cpu_new(mmu: *mut RawMmu) -> *mut CpuState;
    fn appine_cpu_free(cpu: *mut CpuState);
    fn appine_cpu_init(cpu: *mut CpuState, mmu: *mut RawMmu);
    fn appine_cpu_tlb_flush(cpu: *mut CpuState);
    fn appine_cpu_run(cpu: *mut CpuState) -> c_int;
    fn appine_cpu_exec_insn(cpu: *mut CpuState, insn: u32) -> c_int;
    fn appine_cpu_kick(cpu: *mut CpuState);
    fn appine_cpu_layout_check(
        size: usize,
        off_x: usize,
        off_pc: usize,
        off_tpidr: usize,
        off_pending: usize,
        off_mmu: usize,
        off_jump_cache: usize,
        off_tlb: usize,
        off_v: usize,
    ) -> c_int;

    fn appine_fault_guard_install();
    fn appine_host_fault_last(out: *mut RawHostFault);

    fn appine_guest_hwcap() -> u64;
    fn appine_guest_midr() -> u64;

    fn appine_dbt_new(key: u64) -> *mut RawDbt;
    fn appine_dbt_trim();
    fn appine_dbt_free(dbt: *mut RawDbt, key: u64);
    fn appine_dbt_set_sources(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn appine_dbt_trust(dbt: *mut RawDbt, s: *const DbtSource, n: usize);
    fn appine_dbt_attach(dbt: *mut RawDbt) -> *mut RawDbtThread;
    fn appine_dbt_detach(t: *mut RawDbtThread);
    fn appine_dbt_flush(t: *mut RawDbtThread);
    fn appine_dbt_recheck(t: *mut RawDbtThread);
    fn appine_dbt_invalidate(t: *mut RawDbtThread, start: u64, len: u64);
    fn appine_dbt_run(t: *mut RawDbtThread, cpu: *mut CpuState) -> c_int;
    fn appine_dbt_get_stats(t: *const RawDbtThread, out: *mut DbtStats);
}

#[repr(C)]
struct RawDbt {
    _private: [u8; 0],
}

#[repr(C)]
struct RawDbtThread {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct RawHostFault {
    signo: i32,
    code: i32,
    addr: u64,
    pc: u64,
}

/// 修改页表时的参数错误或内存不足。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// 地址或长度未按 4K 对齐、越过 48 位地址空间，或权限位非法。
    InvalidArgument,
    /// 分配页表失败。
    OutOfMemory,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::InvalidArgument => f.write_str("invalid mapping argument"),
            MapError::OutOfMemory => f.write_str("out of memory for page tables"),
        }
    }
}

impl std::error::Error for MapError {}

fn map_result(rc: c_int) -> Result<(), MapError> {
    match rc {
        0 => Ok(()),
        -2 => Err(MapError::OutOfMemory),
        _ => Err(MapError::InvalidArgument),
    }
}

/// 一个 guest 地址空间的软件页表（设计文档 §5.1）。
pub struct Mmu {
    raw: NonNull<RawMmu>,
}

impl Mmu {
    /// 创建空页表。
    pub fn new() -> Self {
        let raw = unsafe { appine_mmu_new() };
        Mmu {
            raw: NonNull::new(raw).expect("appine_mmu_new: out of memory"),
        }
    }

    /// 把 guest `[gva, gva + len)` 映射到 host `[host, host + len)`，覆盖已有映射。
    ///
    /// # Safety
    ///
    /// `host` 指向的 `len` 字节必须可读写，并在映射存续期间（直到 unmap、
    /// 被覆盖或 `Mmu` 销毁）保持有效。
    pub unsafe fn map(
        &mut self,
        gva: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), MapError> {
        map_result(unsafe {
            appine_mmu_map(self.raw.as_ptr(), gva, host.as_ptr().cast(), len, prot)
        })
    }

    /// 解除 `[gva, gva + len)` 的映射。
    pub fn unmap(&mut self, gva: u64, len: u64) -> Result<(), MapError> {
        map_result(unsafe { appine_mmu_unmap(self.raw.as_ptr(), gva, len) })
    }

    /// 修改已映射页的权限。
    pub fn protect(&mut self, gva: u64, len: u64, prot: u32) -> Result<(), MapError> {
        map_result(unsafe { appine_mmu_protect(self.raw.as_ptr(), gva, len, prot) })
    }

    /// 与 [`Mmu::map`] 相同，但只要 `&self`：内核层在写时复制时经由共享借用修改别的地址
    /// 空间的页表。`Mmu` 不能跨线程共享，修改仍然是串行的；别的线程照样可以同时遍历。
    ///
    /// # Safety
    ///
    /// 同 [`Mmu::map`]。
    pub unsafe fn map_shared(
        &self,
        gva: u64,
        host: NonNull<u8>,
        len: u64,
        prot: u32,
    ) -> Result<(), MapError> {
        map_result(unsafe {
            appine_mmu_map(self.raw.as_ptr(), gva, host.as_ptr().cast(), len, prot)
        })
    }

    /// 与 [`Mmu::protect`] 相同，但只要 `&self`（见 [`Mmu::map_shared`]）。
    pub fn protect_shared(&self, gva: u64, len: u64, prot: u32) -> Result<(), MapError> {
        map_result(unsafe { appine_mmu_protect(self.raw.as_ptr(), gva, len, prot) })
    }

    /// 查询 guest 地址对应的 host 指针；`required_prot` 中的权限必须全部具备，传 0 只要求已映射。
    pub fn translate(&self, gva: u64, required_prot: u32) -> Option<NonNull<u8>> {
        NonNull::new(unsafe { appine_mmu_translate(self.raw.as_ptr(), gva, required_prot) }.cast())
    }
}

impl Default for Mmu {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Mmu {
    fn drop(&mut self) {
        unsafe { appine_mmu_free(self.raw.as_ptr()) }
    }
}

/// 访存类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 数据读。
    Read,
    /// 数据写。
    Write,
    /// 取指。
    Exec,
}

/// 访存失败的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// 未映射或权限不符，对应 SIGSEGV。
    Page,
    /// PC 或者要求对齐的数据访问（独占访问、原子操作）未对齐，对应 SIGBUS。
    Alignment,
    /// 以 SP 为基址访存时 SP 未对齐（`addr` 是 SP），对应 SIGBUS。
    StackAlignment,
}

/// [`Cpu::run`] 返回的退出原因：需要内核层介入的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `svc #imm`；pc 已指向下一条指令。
    Svc {
        /// svc 的立即数，Linux 只使用 0。
        imm: u16,
    },
    /// 未定义或不在白名单内的指令；pc 指向该指令。
    Undefined {
        /// 指令编码。
        insn: u32,
    },
    /// `brk #imm`；pc 指向该指令。
    Breakpoint {
        /// brk 的立即数。
        imm: u16,
    },
    /// 访存失败；pc 指向出错指令，该指令没有产生副作用。
    Fault {
        /// 出错的 guest 地址（已去掉 TBI tag）。
        addr: u64,
        /// 访存类型。
        access: Access,
        /// 失败类别。
        kind: FaultKind,
    },
    /// 被其他线程 kick 打断。
    Kick,
    /// `ic ivau`：guest 修改了 `addr` 处的代码；pc 已指向下一条指令。共用这个地址空间的
    /// 每个 [`Dbt`] 都要作废这里的翻译。
    InvalidateCode {
        /// 指令的地址操作数（已去掉 TBI tag）。
        addr: u64,
    },
    /// 引擎自己的 bug 让宿主收到了故障信号（只在 [`install_fault_guard`] 之后出现）：guest
    /// 状态不可信，应当结束这个 guest 进程。用过的 [`Dbt`] 已经作废，析构时直接释放。
    HostFault {
        /// 宿主收到的信号：SIGSEGV、SIGBUS 或 SIGILL。
        signal: i32,
        /// 宿主的 `si_code`。
        code: i32,
        /// 宿主的 `si_addr`。
        addr: u64,
        /// 出错的宿主指令。
        pc: u64,
    },
}

/// 一个 guest 硬件线程。
///
/// `Cpu` 持有它所绑定的 [`Mmu`] 的裸指针，调用方必须保证 `Mmu` 比 `Cpu`
/// 活得久；修改 `Mmu` 的映射之后，必须调用 [`Cpu::tlb_flush`]。
pub struct Cpu {
    /// 由 `appine_cpu_new` 分配（mmap），`Drop` 时释放。
    state: NonNull<CpuState>,
}

// SAFETY: 同一时刻只有一个线程运行一个 Cpu（`&mut self`），但它可以在线程之间移交；
// 它指向的 Mmu 由调用方保证存活，并且页表本身允许并发遍历
unsafe impl Send for Cpu {}

/// 从其他线程打断一个 [`Cpu`] 的句柄（[`Cpu::kicker`]）。
#[derive(Clone, Copy)]
pub struct Kicker(NonNull<CpuState>);

// SAFETY: kick 只对 CpuState 的 pending 字段做原子写
unsafe impl Send for Kicker {}
unsafe impl Sync for Kicker {}

impl Kicker {
    /// 让对应的 Cpu 在下一个指令边界（Tier 1 是下一个块入口）返回 [`Exit::Kick`]；
    /// 它当前不在运行时，下一次运行会立即返回。
    ///
    /// # Safety
    ///
    /// 创建这个句柄的 Cpu 必须仍然存活。
    pub unsafe fn kick(&self) {
        unsafe { appine_cpu_kick(self.0.as_ptr()) }
    }
}

impl Cpu {
    /// 创建一个所有寄存器为 0、绑定到 `mmu` 的 CPU。
    ///
    /// # Safety
    ///
    /// `mmu` 必须比返回的 `Cpu` 活得久。
    pub unsafe fn new(mmu: &Mmu) -> Self {
        let state =
            NonNull::new(unsafe { appine_cpu_new(mmu.raw.as_ptr()) }).unwrap_or_else(|| {
                std::alloc::handle_alloc_error(std::alloc::Layout::new::<CpuState>())
            });
        Cpu { state }
    }

    /// 清零全部状态并改为绑定 `mmu`（execve 使用），[`Kicker`] 仍然有效。
    ///
    /// # Safety
    ///
    /// `mmu` 必须比这个 `Cpu` 活得久。
    pub unsafe fn reset(&mut self, mmu: &Mmu) {
        unsafe { appine_cpu_init(self.state.as_ptr(), mmu.raw.as_ptr()) }
    }

    /// 打断这个 Cpu 的句柄。
    pub fn kicker(&self) -> Kicker {
        Kicker(self.state)
    }

    /// guest 寄存器。
    pub fn state(&self) -> &CpuState {
        // SAFETY: state 在 Cpu 存活期间有效，借用规则由 &self 保证
        unsafe { self.state.as_ref() }
    }

    /// 可修改的 guest 寄存器。
    pub fn state_mut(&mut self) -> &mut CpuState {
        // SAFETY: 同上，&mut self 保证独占
        unsafe { self.state.as_mut() }
    }

    /// 按绑定的页表查询 guest 地址对应的 host 指针（[`Mmu::translate`]）；没有绑定页表时
    /// 返回 None。页表允许在别的线程修改它时遍历；指针指向的内存的有效期由调用方保证。
    pub fn translate(&self, gva: u64, required_prot: u32) -> Option<NonNull<u8>> {
        let mmu = self.state().mmu;
        if mmu.is_null() {
            return None;
        }
        // SAFETY: 绑定的 mmu 比 Cpu 活得久（Cpu::new、Cpu::reset）
        NonNull::new(unsafe { appine_mmu_translate(mmu, gva, required_prot) }.cast())
    }

    /// 让软件 TLB 全部失效。
    pub fn tlb_flush(&mut self) {
        unsafe { appine_cpu_tlb_flush(self.state.as_ptr()) }
    }

    /// 从当前 pc 开始执行，直到需要内核层介入。
    pub fn run(&mut self) -> Exit {
        let rc = unsafe { appine_cpu_run(self.state.as_ptr()) };
        self.decode_exit(rc)
            .expect("appine_cpu_run returned EXIT_NONE")
    }

    /// 在当前 pc 处执行给定的一条指令（不经取指）。正常完成时返回 `None`。
    pub fn exec_insn(&mut self, insn: u32) -> Option<Exit> {
        let rc = unsafe { appine_cpu_exec_insn(self.state.as_ptr(), insn) };
        self.decode_exit(rc)
    }

    pub(crate) fn decode_exit(&self, rc: c_int) -> Option<Exit> {
        let s = self.state();
        Some(match rc {
            0 => return None,
            1 => Exit::Svc {
                imm: s.exit_info as u16,
            },
            2 => Exit::Undefined { insn: s.exit_info },
            3 => Exit::Breakpoint {
                imm: s.exit_info as u16,
            },
            4 => Exit::Fault {
                addr: s.fault_addr,
                access: match s.fault_access {
                    1 => Access::Read,
                    2 => Access::Write,
                    _ => Access::Exec,
                },
                kind: match s.fault_kind {
                    2 => FaultKind::Alignment,
                    3 => FaultKind::StackAlignment,
                    _ => FaultKind::Page,
                },
            },
            5 => Exit::Kick,
            6 => Exit::InvalidateCode { addr: s.fault_addr },
            7 => {
                let mut f = RawHostFault::default();
                unsafe { appine_host_fault_last(&mut f) };
                Exit::HostFault {
                    signal: f.signo,
                    code: f.code,
                    addr: f.addr,
                    pc: f.pc,
                }
            }
            _ => panic!("unknown appine_exit {rc}"),
        })
    }
}

impl Drop for Cpu {
    fn drop(&mut self) {
        unsafe { appine_cpu_free(self.state.as_ptr()) }
    }
}

/// Tier 1 的翻译与运行统计（设计文档 §4.2）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DbtStats {
    /// 翻译的块数。
    pub blocks: u64,
    /// 翻译的 guest 指令数。
    pub insns: u64,
    /// 原样拷贝的指令数。
    pub insns_copied: u64,
    /// 涉及被占用寄存器或 SP、改写后执行的指令数。
    pub insns_remapped: u64,
    /// 改写成内联 TLB 快速路径的访存指令数。
    pub insns_memory: u64,
    /// 交给 Tier 0 执行的指令数（静态计数）。
    pub insns_fallback: u64,
    /// 因块链接退出到分发器的次数（每个链接点只发生一次）。
    pub exits_chain: u64,
    /// 间接分支未命中跳转缓存的次数。
    pub exits_lookup: u64,
    /// 交给 Tier 0 执行一条指令的次数（回填 TLB 失败、SP 未对齐、不支持的指令等）。
    pub exits_interp: u64,
    /// 因 syscall 退出的次数。
    pub exits_svc: u64,
    /// 因 kick 退出的次数。
    pub exits_kick: u64,
    /// 代码缓存被清空的次数。
    pub flushes: u64,
    /// 被 [`Dbt::invalidate`] 作废的块数。
    pub invalidated: u64,
    /// `blocks` 中直接拷贝翻译模板（其他实例翻译过的同一段代码）得到的块数。
    pub blocks_shared: u64,
    /// `insns_memory` 中和同一块里前面以同一寄存器为基址的访存共用一次 TLB 检查的条数。
    pub insns_grouped: u64,
    /// 共用的检查没通过、作废之后按不分组重新翻译的次数。
    pub ungrouped: u64,
}

/// 一个地址空间的 Tier 1 翻译（代码缓存、翻译表），这个地址空间里的所有线程共用：每个
/// 线程经由 [`Dbt::attach`] 得到自己的 [`Dbt`]。最后一个 [`Dbt`] 和这里的引用都没了之后
/// 放回池里，翻译不多时保留下来，下一个使用者运行之前逐块核对。
pub struct DbtSpace {
    raw: NonNull<RawDbt>,
    key: u64,
}

// SAFETY: C 侧用自己的锁保护共享的部分（dbt.c 的 struct appine_dbt）
unsafe impl Send for DbtSpace {}
unsafe impl Sync for DbtSpace {}

/// 翻译所在的一段映射（[`DbtSpace::set_sources`]）：guest 地址范围，和能断定内容相同的
/// 身份（文件和它的版本）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbtSource {
    pub start: u64,
    pub len: u64,
    pub id: [u64; 5],
}

impl DbtSpace {
    /// [`DbtSpace::set_sources`] 最多记下几段。
    pub const SOURCES: usize = 16;

    /// 创建（或者从池里取出）一个实例；宿主不是 aarch64 Linux 时返回 `None`。`key` 区分
    /// 程序（0 表示不区分）：池里有同一个键留下的翻译时优先拿回它，用完放回池里时也按它
    /// 记下。
    pub fn new(key: u64) -> Option<Arc<Self>> {
        NonNull::new(unsafe { appine_dbt_new(key) }).map(|raw| Arc::new(DbtSpace { raw, key }))
    }

    /// 地址空间不再使用了（翻译要放回池里）：记下翻译所在的映射，最多 [`Self::SOURCES`]
    /// 段。调用方保证身份相同的映射内容相同。
    pub fn set_sources(&self, sources: &[DbtSource]) {
        unsafe { appine_dbt_set_sources(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }

    /// 从池里拿到的翻译在新的地址空间第一次运行之前：新地址空间里与记下的
    /// （[`Self::set_sources`]）完全相同的映射里的块不必逐块核对。
    pub fn trust(&self, sources: &[DbtSource]) {
        unsafe { appine_dbt_trust(self.raw.as_ptr(), sources.as_ptr(), sources.len()) }
    }
}

/// 释放池里保留的 Tier 1 翻译：没有人在用，宿主内存紧张时（Android 的 `onTrimMemory`）
/// 可以丢掉，下次运行同一个程序时重新拷贝模板。
pub fn trim_dbt_pool() {
    unsafe { appine_dbt_trim() }
}

impl Drop for DbtSpace {
    fn drop(&mut self) {
        // 挂着的 Dbt 都持有 Arc，走到这里时它们都已经解挂
        unsafe { appine_dbt_free(self.raw.as_ptr(), self.key) }
    }
}

/// Tier 1：同 ISA 直通式 DBT 里一个线程的部分（跳转缓存、影子返回栈、统计），挂在一个
/// [`DbtSpace`] 上。
///
/// 翻译结果不会自己失效：guest 用 `ic ivau` 声明修改了代码（[`Exit::InvalidateCode`]），
/// 或者可执行的映射被拆除、改了权限之后，必须调用 [`Dbt::invalidate`] 或
/// [`Dbt::flush`]（经由挂在同一个 [`DbtSpace`] 上的任意一个线程调用一次）。只支持
/// aarch64 Linux 宿主。
pub struct Dbt {
    raw: NonNull<RawDbtThread>,
    space: Arc<DbtSpace>,
}

// SAFETY: 线程的部分只由持有这个 Dbt 的线程使用，共享的部分由 C 侧加锁
unsafe impl Send for Dbt {}

impl Dbt {
    /// 创建一个只有这个线程用的实例；宿主不是 aarch64 Linux 时返回 `None`。
    pub fn new() -> Option<Self> {
        Self::attach(&DbtSpace::new(0)?)
    }

    /// 挂到 `space` 上。
    pub fn attach(space: &Arc<DbtSpace>) -> Option<Self> {
        NonNull::new(unsafe { appine_dbt_attach(space.raw.as_ptr()) }).map(|raw| Dbt {
            raw,
            space: space.clone(),
        })
    }

    /// 挂着的实例。
    pub fn space(&self) -> &Arc<DbtSpace> {
        &self.space
    }

    /// 与 [`Cpu::run`] 语义相同，但在翻译后的代码里执行。
    pub fn run(&mut self, cpu: &mut Cpu) -> Exit {
        let rc = unsafe { appine_dbt_run(self.raw.as_ptr(), cpu.state.as_ptr()) };
        cpu.decode_exit(rc)
            .expect("appine_dbt_run returned EXIT_NONE")
    }

    /// 丢弃全部翻译：要等所有线程离开翻译后的代码，由下一次 [`Dbt::run`] 完成。
    pub fn flush(&mut self) {
        unsafe { appine_dbt_flush(self.raw.as_ptr()) }
    }

    /// 要换一个地址空间运行（exec 之后）：保留翻译，下次运行之前逐块核对 guest 指令字，
    /// 对不上的作废，大部分对不上时整体清空。比 [`Dbt::flush`] 之后重新拷贝翻译模板便宜。
    pub fn recheck(&mut self) {
        unsafe { appine_dbt_recheck(self.raw.as_ptr()) }
    }

    /// 作废 `[start, start + len)` 覆盖的各页里开始的块；范围很大时等同于 [`Dbt::flush`]。
    pub fn invalidate(&mut self, start: u64, len: u64) {
        unsafe { appine_dbt_invalidate(self.raw.as_ptr(), start, len) }
    }

    /// 这个线程的统计：它退出的次数，以及它做的翻译、作废。
    pub fn stats(&self) -> DbtStats {
        let mut stats = DbtStats::default();
        unsafe { appine_dbt_get_stats(self.raw.as_ptr(), &mut stats) };
        stats
    }
}

impl Drop for Dbt {
    fn drop(&mut self) {
        unsafe { appine_dbt_detach(self.raw.as_ptr()) }
    }
}

/// 装上宿主侧的故障兜底（设计文档 §7.4、§12），可以重复调用：之后 [`Cpu::run`]、
/// [`Dbt::run`] 运行期间宿主收到的 SIGSEGV、SIGBUS、SIGILL 变成 [`Exit::HostFault`]，
/// 只影响当时运行的那个 guest；其余的故障先往 stderr 写一行报告，再交给原来的处理方式
/// （Android 上 debuggerd 据此生成 tombstone）。
pub fn install_fault_guard() {
    unsafe { appine_fault_guard_install() }
}

/// 宣告给 guest 的 `AT_HWCAP`：宿主支持 ∩ Tier 0 已实现 ∩ 白名单（设计文档 §4.4）。
pub fn guest_hwcap() -> u64 {
    unsafe { appine_guest_hwcap() }
}

/// guest 读到的 `MIDR_EL1`（`/proc/cpuinfo` 用）；宿主不允许读 ID 寄存器时为 0。
pub fn guest_midr() -> u64 {
    unsafe { appine_guest_midr() }
}

/// HWCAP 位，取值与 Linux 的 `HWCAP_*` 一致。
pub mod hwcap {
    /// 浮点。
    pub const FP: u64 = 1 << 0;
    /// Advanced SIMD。
    pub const ASIMD: u64 = 1 << 1;
    /// AES 指令。
    pub const AES: u64 = 1 << 3;
    /// PMULL 的 1Q 形式。
    pub const PMULL: u64 = 1 << 4;
    /// SHA1 指令。
    pub const SHA1: u64 = 1 << 5;
    /// SHA256 指令。
    pub const SHA2: u64 = 1 << 6;
    /// CRC32 指令。
    pub const CRC32: u64 = 1 << 7;
    /// LSE 原子指令。
    pub const ATOMICS: u64 = 1 << 8;
    /// EL0 可以读取 ID 寄存器。
    pub const CPUID: u64 = 1 << 11;
}

/// 校验 [`CpuState`] 与 C 结构体的布局是否一致。
pub fn layout_matches() -> bool {
    unsafe {
        appine_cpu_layout_check(
            size_of::<CpuState>(),
            offset_of!(CpuState, x),
            offset_of!(CpuState, pc),
            offset_of!(CpuState, tpidr_el0),
            offset_of!(CpuState, pending),
            offset_of!(CpuState, mmu),
            offset_of!(CpuState, dbt_jump_cache),
            offset_of!(CpuState, tlb),
            offset_of!(CpuState, v),
        ) == 1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn layout_matches_c() {
        assert!(super::layout_matches());
    }
}
