//! 原生执行端（oracle.c / oracle.S）的 Rust 封装。

use std::ffi::c_int;
use std::ptr::NonNull;

use crate::State;

#[repr(C)]
struct RawOracle {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn lockstep_oracle_new() -> *mut RawOracle;
    fn lockstep_oracle_free(oracle: *mut RawOracle);
    fn lockstep_oracle_slot(oracle: *const RawOracle) -> u64;
    fn lockstep_oracle_run(oracle: *mut RawOracle, insn: u32, st: *mut State) -> c_int;
}

/// 一个可以反复执行单条指令的 JIT 页。
pub(crate) struct Oracle {
    raw: NonNull<RawOracle>,
}

impl Oracle {
    /// 申请 JIT 页并安装信号处理函数。
    pub(crate) fn new() -> Option<Self> {
        NonNull::new(unsafe { lockstep_oracle_new() }).map(|raw| Oracle { raw })
    }

    /// 被测指令所在的地址，即硬件执行时的 PC。
    pub(crate) fn slot_addr(&self) -> u64 {
        unsafe { lockstep_oracle_slot(self.raw.as_ptr()) }
    }

    /// 在硬件上执行 `insn`。成功时返回 `Ok`，触发信号时返回信号编号。
    pub(crate) fn run(&mut self, insn: u32, state: &mut State) -> Result<(), i32> {
        match unsafe { lockstep_oracle_run(self.raw.as_ptr(), insn, state) } {
            0 => Ok(()),
            -1 => panic!("lockstep oracle: mprotect failed"),
            sig => Err(sig),
        }
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        unsafe { lockstep_oracle_free(self.raw.as_ptr()) }
    }
}
