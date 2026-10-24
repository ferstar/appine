//! appine 引擎（C 实现）的 Rust 绑定。
//!
//! 引擎负责执行 guest 指令和 soft-MMU 快速路径（设计文档 §4、§5）。内核层用
//! [`Cpu::run`] 驱动执行，再根据返回的 [`Exit`] 处理 syscall、异常等事件。

use std::ffi::{c_int, c_void};
use std::fmt;
use std::mem::offset_of;
use std::ptr::NonNull;

/// guest 页大小，固定 4K（设计文档 §5.2）。
pub const PAGE_SIZE: u64 = 4096;
/// guest 虚拟地址位数。
pub const VA_BITS: u32 = 48;
/// 每个 CPU 的软件 TLB 项数。
pub const TLB_ENTRIES: usize = 512;

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
    tlb: [TlbEntry; TLB_ENTRIES],
    itlb: [ItlbEntry; TLB_ENTRIES],
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
    /// V0–V31，每个寄存器低 64 位在前。
    pub v: [[u64; 2]; 32],
    /// 用户态线程指针。
    pub tpidr_el0: u64,
    /// syscall 重启时用来恢复的原始 X0，由内核层维护。
    pub orig_x0: u64,
    fault_addr: u64,
    fault_access: u32,
    fault_kind: u32,
    exit_info: u32,
    reserved: u32,
    pending: u64,
    mmu: *mut RawMmu,
    excl_addr: u64,
    excl_value: [u64; 2],
    excl_size: u32,
    reserved2: u32,
    dbt_reason: u64,
    dbt_site: u64,
    dbt_jump_cache: *mut c_void,
}

unsafe extern "C" {
    fn appine_mmu_new() -> *mut RawMmu;
    fn appine_mmu_free(mmu: *mut RawMmu);
    fn appine_mmu_map(mmu: *mut RawMmu, gva: u64, host: *mut c_void, len: u64, prot: u32) -> c_int;
    fn appine_mmu_unmap(mmu: *mut RawMmu, gva: u64, len: u64) -> c_int;
    fn appine_mmu_protect(mmu: *mut RawMmu, gva: u64, len: u64, prot: u32) -> c_int;
    fn appine_mmu_translate(mmu: *const RawMmu, gva: u64, required_prot: u32) -> *mut c_void;

    fn appine_cpu_init(cpu: *mut CpuState, mmu: *mut RawMmu);
    fn appine_cpu_tlb_flush(cpu: *mut CpuState);
    fn appine_cpu_run(cpu: *mut CpuState) -> c_int;
    fn appine_cpu_exec_insn(cpu: *mut CpuState, insn: u32) -> c_int;
    fn appine_cpu_layout_check(
        size: usize,
        off_x: usize,
        off_pc: usize,
        off_tpidr: usize,
        off_pending: usize,
        off_mmu: usize,
        off_jump_cache: usize,
    ) -> c_int;

    fn appine_guest_hwcap() -> u64;

    fn appine_dbt_new() -> *mut RawDbt;
    fn appine_dbt_free(dbt: *mut RawDbt);
    fn appine_dbt_flush(dbt: *mut RawDbt);
    fn appine_dbt_run(dbt: *mut RawDbt, cpu: *mut CpuState) -> c_int;
    fn appine_dbt_get_stats(dbt: *const RawDbt, out: *mut DbtStats);
}

#[repr(C)]
struct RawDbt {
    _private: [u8; 0],
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
    /// PC 或 SP 未对齐，对应 SIGBUS。
    Alignment,
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
}

/// 一个 guest 硬件线程。
///
/// `Cpu` 持有它所绑定的 [`Mmu`] 的裸指针，调用方必须保证 `Mmu` 比 `Cpu`
/// 活得久；修改 `Mmu` 的映射之后，必须调用 [`Cpu::tlb_flush`]。
pub struct Cpu {
    state: Box<CpuState>,
}

impl Cpu {
    /// 创建一个所有寄存器为 0、绑定到 `mmu` 的 CPU。
    ///
    /// # Safety
    ///
    /// `mmu` 必须比返回的 `Cpu` 活得久。
    pub unsafe fn new(mmu: &Mmu) -> Self {
        // CpuState 只含整数和裸指针，全零是合法值；直接在堆上清零分配，避免 16KB 的栈拷贝
        let layout = std::alloc::Layout::new::<CpuState>();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<CpuState>();
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        let mut state = unsafe { Box::from_raw(ptr) };
        unsafe { appine_cpu_init(&mut *state, mmu.raw.as_ptr()) };
        Cpu { state }
    }

    /// guest 寄存器。
    pub fn state(&self) -> &CpuState {
        &self.state
    }

    /// 可修改的 guest 寄存器。
    pub fn state_mut(&mut self) -> &mut CpuState {
        &mut self.state
    }

    /// 让软件 TLB 全部失效。
    pub fn tlb_flush(&mut self) {
        unsafe { appine_cpu_tlb_flush(&mut *self.state) }
    }

    /// 从当前 pc 开始执行，直到需要内核层介入。
    pub fn run(&mut self) -> Exit {
        let rc = unsafe { appine_cpu_run(&mut *self.state) };
        self.decode_exit(rc)
            .expect("appine_cpu_run returned EXIT_NONE")
    }

    /// 在当前 pc 处执行给定的一条指令（不经取指）。正常完成时返回 `None`。
    pub fn exec_insn(&mut self, insn: u32) -> Option<Exit> {
        let rc = unsafe { appine_cpu_exec_insn(&mut *self.state, insn) };
        self.decode_exit(rc)
    }

    pub(crate) fn decode_exit(&self, rc: c_int) -> Option<Exit> {
        let s = &self.state;
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
                kind: if s.fault_kind == 2 {
                    FaultKind::Alignment
                } else {
                    FaultKind::Page
                },
            },
            5 => Exit::Kick,
            _ => panic!("unknown appine_exit {rc}"),
        })
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
    /// 交给 Tier 0 执行一条指令的次数（TLB 未命中、跨页、不支持的指令）。
    pub exits_interp: u64,
    /// 因 syscall 退出的次数。
    pub exits_svc: u64,
    /// 因 kick 退出的次数。
    pub exits_kick: u64,
    /// 代码缓存被清空的次数。
    pub flushes: u64,
}

/// Tier 1：同 ISA 直通式 DBT 的一个实例（代码缓存、翻译表、跳转缓存）。
///
/// 原型限制：翻译结果不会因为 guest 修改代码或映射而失效，修改之后必须调用
/// [`Dbt::flush`]。只支持 aarch64 Linux 宿主。
pub struct Dbt {
    raw: NonNull<RawDbt>,
}

impl Dbt {
    /// 创建实例；宿主不是 aarch64 Linux 时返回 `None`。
    pub fn new() -> Option<Self> {
        NonNull::new(unsafe { appine_dbt_new() }).map(|raw| Dbt { raw })
    }

    /// 与 [`Cpu::run`] 语义相同，但在翻译后的代码里执行。
    pub fn run(&mut self, cpu: &mut Cpu) -> Exit {
        let rc = unsafe { appine_dbt_run(self.raw.as_ptr(), &mut *cpu.state) };
        cpu.decode_exit(rc)
            .expect("appine_dbt_run returned EXIT_NONE")
    }

    /// 丢弃全部翻译结果。
    pub fn flush(&mut self) {
        unsafe { appine_dbt_flush(self.raw.as_ptr()) }
    }

    /// 统计数据。
    pub fn stats(&self) -> DbtStats {
        let mut stats = DbtStats::default();
        unsafe { appine_dbt_get_stats(self.raw.as_ptr(), &mut stats) };
        stats
    }
}

impl Drop for Dbt {
    fn drop(&mut self) {
        unsafe { appine_dbt_free(self.raw.as_ptr()) }
    }
}

/// 宣告给 guest 的 `AT_HWCAP`：宿主支持 ∩ Tier 0 已实现 ∩ 白名单（设计文档 §4.4）。
pub fn guest_hwcap() -> u64 {
    unsafe { appine_guest_hwcap() }
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
