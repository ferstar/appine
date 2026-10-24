//! 原生执行端（oracle.c / oracle.S）的 Rust 封装。

use std::ffi::c_int;
use std::ptr::NonNull;

use crate::State;

/// 数据区大小：两页，访存类用例只能访问这里。
pub const DATA_BYTES: usize = 8192;

#[repr(C)]
struct RawOracle {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn lockstep_oracle_new() -> *mut RawOracle;
    fn lockstep_oracle_free(oracle: *mut RawOracle);
    fn lockstep_oracle_slot(oracle: *const RawOracle) -> u64;
    fn lockstep_oracle_page(oracle: *const RawOracle) -> *const u8;
    fn lockstep_oracle_data(oracle: *const RawOracle) -> *mut u8;
    fn lockstep_oracle_run(
        oracle: *mut RawOracle,
        insns: *const u32,
        n: u32,
        st: *mut State,
    ) -> c_int;
}

/// 一个可以反复执行被测指令的 JIT 页，加上两页数据区。
pub(crate) struct Oracle {
    raw: NonNull<RawOracle>,
}

impl Oracle {
    /// 申请 JIT 页和数据区，并安装信号处理函数。
    pub(crate) fn new() -> Option<Self> {
        NonNull::new(unsafe { lockstep_oracle_new() }).map(|raw| Oracle { raw })
    }

    /// 第一条被测指令所在的地址，即硬件执行时的 PC。
    pub(crate) fn slot_addr(&self) -> u64 {
        unsafe { lockstep_oracle_slot(self.raw.as_ptr()) }
    }

    /// 被测指令所在的那个 4K 页的内容。
    pub(crate) fn slot_page(&self) -> &[u8] {
        let page = unsafe { lockstep_oracle_page(self.raw.as_ptr()) };
        let offset = (self.slot_addr() & !0xfff) - page as u64;
        unsafe { std::slice::from_raw_parts(page.add(offset as usize), 4096) }
    }

    /// 数据区的地址。
    pub(crate) fn data_addr(&self) -> u64 {
        unsafe { lockstep_oracle_data(self.raw.as_ptr()) as u64 }
    }

    /// 数据区的内容。
    pub(crate) fn data(&mut self) -> &mut [u8] {
        let p = unsafe { lockstep_oracle_data(self.raw.as_ptr()) };
        unsafe { std::slice::from_raw_parts_mut(p, DATA_BYTES) }
    }

    /// 在硬件上执行 `insns`。成功时返回 `Ok`，触发信号时返回信号编号。
    pub(crate) fn run(&mut self, insns: &[u32], state: &mut State) -> Result<(), i32> {
        let rc = unsafe {
            lockstep_oracle_run(self.raw.as_ptr(), insns.as_ptr(), insns.len() as u32, state)
        };
        match rc {
            0 => Ok(()),
            -1 => panic!("lockstep oracle: bad case or mprotect failed"),
            sig => Err(sig),
        }
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        unsafe { lockstep_oracle_free(self.raw.as_ptr()) }
    }
}
