//! Integration tests for Tier 1 strip-mined fast-loop matcher extensions (`dbt_loop.c`):
//! 32-bit M_REG induction indices (`UXTW`/`SXTW`), negative constant strides, shifted `ADD`
//! pointer computations, bounded table-lookup `M_REG` streams, `ADDS`/`ADD+CBNZ`/`TBZ`/`TBNZ`
//! loop closers, early-exit conditional side branches, header-tested `while` loops, raised
//! `KICK_BUDGET`, and `ADR`/`ADRP`/`M_LITERAL` inside fast loops.
//!
//! When the host is not aarch64 Linux there is no Tier 1 and the tests pass trivially.

use std::ptr::NonNull;

use appine_engine::{
    Access, Cpu, Dbt, DbtRegs, Exit, FaultKind, Mmu, PAGE_SIZE, WATCH_LOAD, WATCH_STORE, prot,
};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

const CODE: u64 = 0x40_0000;
const STACK: u64 = 0x80_0000;
const DATA: u64 = 0x100_0000;
const SVC: u32 = 0xd400_0001;

struct Guest {
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
}

impl Guest {
    /// Maps 2 code pages at `CODE`, 1 stack page below `STACK`, and 6 data pages at `DATA`:
    /// pages 0..1 contiguous RW, pages 2..3 reversed (non-contiguous) RW, page 4 RW, page 5 RO,
    /// page 6 unmapped.
    fn with_data() -> Self {
        let mut pages: Box<[Page]> = (0..9).map(|_| Page([0; 4096])).collect();
        for (i, page) in pages[3..].iter_mut().enumerate() {
            for (j, byte) in page.0.iter_mut().enumerate() {
                *byte = ((i * 37 + j * 13 + (j >> 4) * 7 + 1) & 0xff) as u8;
            }
        }
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        let rw = prot::READ | prot::WRITE;
        unsafe {
            mmu.map(CODE, base, 2 * PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
            mmu.map(STACK - PAGE_SIZE, base.add(2 * 4096), PAGE_SIZE, rw)
                .unwrap();
            let layout = [
                (0, 3, rw),
                (1, 4, rw),
                (2, 6, rw),
                (3, 5, rw),
                (4, 7, rw),
                (5, 8, prot::READ),
            ];
            for (page, host, p) in layout {
                mmu.map(DATA + page * PAGE_SIZE, base.add(host * 4096), PAGE_SIZE, p)
                    .unwrap();
            }
        }
        let mut cpu = unsafe { Cpu::new(&mmu) };
        cpu.state_mut().sp = STACK;
        Guest { cpu, mmu, pages }
    }

    fn code(&mut self, addr: u64, insns: &[u32]) {
        let off = (addr - CODE) as usize;
        for (i, insn) in insns.iter().enumerate() {
            let at = off + 4 * i;
            self.pages[at / 4096].0[at % 4096..at % 4096 + 4].copy_from_slice(&insn.to_le_bytes());
        }
    }

    fn write_data_bytes(&mut self, addr: u64, bytes: &[u8]) {
        let off = (addr - DATA) as usize;
        for (i, &b) in bytes.iter().enumerate() {
            let p = (off + i) / 4096;
            let host = [3, 4, 6, 5, 7, 8][p];
            self.pages[host].0[(off + i) % 4096] = b;
        }
    }

    fn read_data_u64(&self, addr: u64) -> u64 {
        let off = (addr - DATA) as usize;
        let mut b = [0u8; 8];
        for i in 0..8 {
            let p = (off + i) / 4096;
            let host = [3, 4, 6, 5, 7, 8][p];
            b[i] = self.pages[host].0[(off + i) % 4096];
        }
        u64::from_le_bytes(b)
    }

    fn run_t1(&mut self, dbt: &mut Dbt, pc: u64) -> Exit {
        let _ = &self.mmu;
        self.cpu.state_mut().pc = pc;
        dbt.run(&mut self.cpu)
    }

    fn run_t0(&mut self, pc: u64) -> Exit {
        let _ = &self.mmu;
        self.cpu.state_mut().pc = pc;
        self.cpu.run()
    }
}

fn b_imm(from: u64, to: u64) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x3ff_ffff;
    0x1400_0000 | off
}

fn b_cond(from: u64, to: u64, cond: u32) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    0x5400_0000 | (off << 5) | cond
}

fn cbz(from: u64, to: u64, rt: u32, is64: bool) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    (if is64 { 0xb400_0000 } else { 0x3400_0000 }) | (off << 5) | rt
}

fn cbnz(from: u64, to: u64, rt: u32, is64: bool) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    (if is64 { 0xb500_0000 } else { 0x3500_0000 }) | (off << 5) | rt
}

fn tbz(from: u64, to: u64, rt: u32, bit: u32) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x3fff;
    0x3600_0000 | ((bit >> 5) << 31) | ((bit & 31) << 19) | (off << 5) | rt
}

fn tbnz(from: u64, to: u64, rt: u32, bit: u32) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x3fff;
    0x3700_0000 | ((bit >> 5) << 31) | ((bit & 31) << 19) | (off << 5) | rt
}

fn adr(rd: u32, from: u64, to: u64) -> u32 {
    let off = (to as i64 - from as i64) as u32;
    0x1000_0000 | ((off & 3) << 29) | (((off >> 2) & 0x7_ffff) << 5) | rd
}

fn adrp(rd: u32, from: u64, to: u64) -> u32 {
    let off = (((to & !0xfff) as i64 - (from & !0xfff) as i64) >> 12) as u32;
    0x9000_0000 | ((off & 3) << 29) | (((off >> 2) & 0x7_ffff) << 5) | rd
}

fn ldr_lit_x(rt: u32, from: u64, to: u64) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    0x5800_0000 | (off << 5) | rt
}

fn ldr_lit_w(rt: u32, from: u64, to: u64) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    0x1800_0000 | (off << 5) | rt
}

fn ldr_lit_sw(rt: u32, from: u64, to: u64) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    0x9800_0000 | (off << 5) | rt
}

fn ldr_lit_q(rt: u32, from: u64, to: u64) -> u32 {
    let off = ((to as i64 - from as i64) >> 2) as u32 & 0x7_ffff;
    0x9c00_0000 | (off << 5) | rt
}

/// Run the same guest program under Tier 0 and Tier 1 with identical initial state, asserting
/// identical exit, GPRs, V0..V7, SP, PC, NZCV, and (optionally) that `fast_loops` incremented.
fn assert_matches_tier0<F>(
    regs: DbtRegs,
    dbt: &mut Dbt,
    pc: u64,
    code_at: &[(u64, &[u32])],
    expect_fast_loop: Option<bool>,
    setup: F,
) -> Guest
where
    F: Fn(&mut Guest),
{
    let mut g0 = Guest::with_data();
    let mut g1 = Guest::with_data();
    for &(addr, insns) in code_at {
        g0.code(addr, insns);
        g1.code(addr, insns);
    }
    setup(&mut g0);
    setup(&mut g1);

    let before_fl = dbt.stats().fast_loops;
    let e0 = g0.run_t0(pc);
    let e1 = g1.run_t1(dbt, pc);
    assert_eq!(e1, e0, "exit mismatch ({regs:?})");
    let s0 = g0.cpu.state();
    let s1 = g1.cpu.state();
    assert_eq!(s1.pc, s0.pc, "pc mismatch ({regs:?})");
    assert_eq!(s1.sp, s0.sp, "sp mismatch ({regs:?})");
    assert_eq!(s1.x, s0.x, "GPR mismatch ({regs:?})");
    assert_eq!(s1.v[..8], s0.v[..8], "SIMD V0..V7 mismatch ({regs:?})");
    assert_eq!(s1.nzcv, s0.nzcv, "NZCV mismatch ({regs:?})");
    for p in 3..9 {
        assert_eq!(
            g1.pages[p].0, g0.pages[p].0,
            "data page {p} mismatch ({regs:?})"
        );
    }
    if let Some(want) = expect_fast_loop {
        if want {
            assert!(
                dbt.stats().fast_loops > before_fl,
                "expected fast loop match at {pc:#x} ({regs:?})"
            );
        } else {
            assert_eq!(
                dbt.stats().fast_loops,
                before_fl,
                "expected fast loop rejection at {pc:#x} ({regs:?})"
            );
        }
    }
    g1
}

#[test]
fn uxtw_sxtw_32bit_mreg_indices_and_wrap() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. UXTW #2 with 32-bit `add w6, w6, #1` + `cmp w6, w7; b.lo` crossing page 0 -> page 1.
        let p1 = CODE + 0x100;
        let code1 = [
            0xb866_582a,            // ldr w10, [x1, w6, uxtw #2]
            0x8b0a_0000,            // add x0, x0, x10
            0x1100_04c6,            // add w6, w6, #1
            0x6b07_00df,            // cmp w6, w7
            b_cond(p1 + 16, p1, 3), // b.lo p1
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p1, &[(p1, &code1)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0xf80; // 32 u32s in page 0, then 68 in page 1
            s.x[6] = 0xdead_beef_0000_0000; // dirty upper 32 bits!
            s.x[7] = 100;
        });

        // 2. UXTW #2 with `add w6, w6, #1` wrapping across 0xffff_ffff -> 0 mid-loop!
        //    Map base at DATA + 0x1000 so index 0xffff_fff8..0xffff_ffff wouldn't be valid unless
        //    we place x1 such that (x1 + 0xffff_fff8*4) is mapped? Instead, use `add w6, w6, #1`
        //    with `UXTW #0` (shift 0) and `sxtw`! For UXTW wrap, map a special page at
        //    DATA + 0x1_0000_0000 so BOTH x1 + 0xffff_fff8 AND x1 + 0 are mapped!
        let p2 = CODE + 0x140;
        let code2 = [
            0x3866_482a,            // ldrb w10, [x1, w6, uxtw]
            0x8b0a_0000,            // add x0, x0, x10
            0x1100_04c6,            // add w6, w6, #1
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p2 + 16, p2, 1), // b.ne p2
            SVC,
        ];
        {
            let mut g0 = Guest::with_data();
            let mut g1 = Guest::with_data();
            // Map high page at DATA + 0x1_0000_0000 - PAGE_SIZE so uxtw(0xffff_fff8) hits it,
            // and DATA + 0 is already mapped so uxtw(0) hits DATA!
            for g in [&mut g0, &mut g1] {
                let host = NonNull::from(&mut g.pages[3]).cast();
                unsafe {
                    g.mmu
                        .map(
                            DATA + 0x1_0000_0000 - PAGE_SIZE,
                            host,
                            PAGE_SIZE,
                            prot::READ | prot::WRITE,
                        )
                        .unwrap();
                }
                g.code(p2, &code2);
                let s = g.cpu.state_mut();
                s.x[0] = 0;
                s.x[1] = DATA;
                s.x[6] = 0xffff_fff8; // 8 bytes before wrap, then wraps to 0!
                s.x[3] = 20;
            }
            let before = dbt.stats().fast_loops;
            assert_eq!(g0.run_t0(p2), Exit::Svc { imm: 0 });
            assert_eq!(g1.run_t1(&mut dbt, p2), Exit::Svc { imm: 0 });
            assert_eq!(g1.cpu.state().x, g0.cpu.state().x, "UXTW wrap ({regs:?})");
            assert_eq!(g1.cpu.state().nzcv, g0.cpu.state().nzcv, "{regs:?}");
            assert!(dbt.stats().fast_loops > before, "{regs:?}");
        }

        // 3. UXTW with pre-access `add w6, w6, #1` at 0xffff_ffff (wraps on iter 0!).
        let p3 = CODE + 0x180;
        let code3 = [
            0x1100_04c6,            // add w6, w6, #1
            0x3866_482a,            // ldrb w10, [x1, w6, uxtw]
            0x8b0a_0000,            // add x0, x0, x10
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p3 + 16, p3, 1), // b.ne p3
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x100;
            s.x[6] = 0xffff_ffff; // iter 0 wraps to 0 -> prologue MUST bail!
            s.x[3] = 16;
        });

        // 4. SXTW #2 starting negative (-32) crossing 0 to +32 (no signed overflow!).
        let p4 = CODE + 0x1c0;
        let code4 = [
            0xb866_d82a,             // ldr w10, [x1, w6, sxtw #2]
            0x8b0a_0000,             // add x0, x0, x10
            0x1100_04c6,             // add w6, w6, #1
            0x6b07_00df,             // cmp w6, w7
            b_cond(p4 + 16, p4, 11), // b.lt p4
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p4, &[(p4, &code4)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x400; // negative offsets -128..0 and positive 0..128 in page 0
            s.x[6] = (-32i32) as u32 as u64;
            s.x[7] = 32;
        });

        // 5. SXTW crossing INT32_MAX -> INT32_MIN mid-loop (prologue must clamp before wrap!).
        let p5 = CODE + 0x200;
        let code5 = [
            0x3866_c82a,            // ldrb w10, [x1, w6, sxtw]
            0x8b0a_0000,            // add x0, x0, x10
            0x1100_04c6,            // add w6, w6, #1
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p5 + 16, p5, 1), // b.ne p5
            SVC,
        ];
        {
            let mut g0 = Guest::with_data();
            let mut g1 = Guest::with_data();
            // Base = 0x1_0000_0000: +0x7fff_fff8 -> 0x1_7fff_fff8, -0x8000_0000 -> 0x0_8000_0000.
            let base_va = 0x1_0000_0000u64;
            for g in [&mut g0, &mut g1] {
                let h0 = NonNull::from(&mut g.pages[3]).cast();
                let h1 = NonNull::from(&mut g.pages[4]).cast();
                unsafe {
                    g.mmu
                        .map(
                            base_va + 0x7fff_f000,
                            h0,
                            PAGE_SIZE,
                            prot::READ | prot::WRITE,
                        )
                        .unwrap();
                    g.mmu
                        .map(
                            base_va - 0x8000_0000,
                            h1,
                            PAGE_SIZE,
                            prot::READ | prot::WRITE,
                        )
                        .unwrap();
                }
                g.code(p5, &code5);
                let s = g.cpu.state_mut();
                s.x[0] = 0;
                s.x[1] = base_va;
                s.x[6] = 0x7fff_fff8; // 8 bytes before INT32_MAX -> INT32_MIN wrap!
                s.x[3] = 16;
            }
            let before = dbt.stats().fast_loops;
            assert_eq!(g0.run_t0(p5), Exit::Svc { imm: 0 });
            assert_eq!(g1.run_t1(&mut dbt, p5), Exit::Svc { imm: 0 });
            assert_eq!(g1.cpu.state().x, g0.cpu.state().x, "SXTW wrap ({regs:?})");
            assert!(dbt.stats().fast_loops > before, "{regs:?}");
        }

        // 6. Stolen 32-bit index register (w16 in Default, w21 in Go) with UXTW #3.
        let idx_s: u32 = match regs {
            DbtRegs::Default => 16,
            DbtRegs::Go => 21,
        };
        let p6 = CODE + 0x240;
        let code6 = [
            0xf860_582a | (idx_s << 16),        // ldr x10, [x1, wS, uxtw #3]
            0x8b0a_0000,                        // add x0, x0, x10
            0x1100_0400 | (idx_s << 5) | idx_s, // add wS, wS, #1
            0xf100_0463,                        // subs x3, x3, #1
            b_cond(p6 + 16, p6, 1),             // b.ne p6
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p6, &[(p6, &code6)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x100;
            s.x[idx_s as usize] = 0;
            s.x[3] = 32;
        });
    }
}

#[test]
fn negative_constant_strides_and_page_start_boundaries() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. Reverse copy across contiguous page boundary (page 1 -> page 0) + TBI tag + FL_CMP_DEC.
        let p1 = CODE + 0x300;
        let code1 = [
            0xf85f_842a,            // ldr x10, [x1], #-8
            0xf81f_844a,            // str x10, [x2], #-8
            0xeb04_003f,            // cmp x1, x4
            b_cond(p1 + 12, p1, 2), // b.hs p1
            SVC,
        ];
        let tag = 0xcd_u64 << 56;
        assert_matches_tier0(regs, &mut dbt, p1, &[(p1, &code1)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[1] = tag | (DATA + PAGE_SIZE + 0x80);
            s.x[2] = tag | (DATA + PAGE_SIZE + 0x100);
            s.x[4] = tag | (DATA + PAGE_SIZE - 0x80);
        });

        // 2. Reverse scan across NON-CONTIGUOUS (reversed host) pages 3 -> 2 with non-pow2 stride (-12).
        let p2 = CODE + 0x340;
        let code2 = [
            0xb85f_442a,            // ldr w10, [x1], #-12
            0x8b0a_0000,            // add x0, x0, x10
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p2 + 12, p2, 1), // b.ne p2
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p2, &[(p2, &code2)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 3 * PAGE_SIZE + 0x60;
            s.x[3] = 24;
        });

        // 3. Upper-page-boundary hazard on negative stride: iter 0 at PAGE_SIZE - 4 reads 8 bytes
        //    crossing into the next page! Prologue must reject (page_off + span > 4096).
        let p3 = CODE + 0x380;
        let code3 = [
            0xf85f_842a,            // ldr x10, [x1], #-8
            0x8b0a_0000,            // add x0, x0, x10
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p3 + 12, p3, 1), // b.ne p3
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            // Page 2 -> Page 3 are non-contiguous host pages; iter 0 crosses 2->3!
            s.x[1] = DATA + 3 * PAGE_SIZE - 4;
            s.x[3] = 10;
        });

        // 4. Precise fault when backward write crosses below page 0 into unmapped page!
        let p4 = CODE + 0x3c0;
        let code4 = [
            0xf81f_844a,           // str x10, [x2], #-8
            0xf100_0463,           // subs x3, x3, #1
            b_cond(p4 + 8, p4, 1), // b.ne p4
            SVC,
        ];
        let g4 = assert_matches_tier0(regs, &mut dbt, p4, &[(p4, &code4)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[2] = DATA + 0x20; // 5 writes succeed (0x20, 0x18, 0x10, 0x08, 0x00), 6th faults at DATA-8!
            s.x[10] = 0x1122_3344_5566_7788;
            s.x[3] = 8;
        });
        assert_eq!(g4.cpu.state().pc, p4, "{regs:?}");
        assert_eq!(g4.cpu.state().x[2], DATA - 8, "{regs:?}");
        assert_eq!(g4.cpu.state().x[3], 3, "{regs:?}");

        // 5. Watchpoint on backward-strided page must NOT be bypassed.
        let mut gw = Guest::with_data();
        gw.code(p2, &code2);
        {
            let s = gw.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x200;
            s.x[3] = 20;
        }
        assert_eq!(gw.run_t1(&mut dbt, p2), Exit::Svc { imm: 0 }); // warm TLB
        gw.cpu.tlb_flush();
        gw.cpu
            .state_mut()
            .set_hw_debug(&[], &[(DATA + 0x1a0, DATA + 0x1a4, WATCH_LOAD)]);
        {
            let s = gw.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x200;
            s.x[3] = 20;
        }
        assert_eq!(
            gw.run_t1(&mut dbt, p2),
            Exit::Fault {
                addr: DATA + 0x1a0,
                access: Access::Read,
                kind: FaultKind::Watchpoint,
            },
            "backward watchpoint ({regs:?})"
        );
    }
}

#[test]
fn shifted_add_pointer_computation_and_neon_streams() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. Vectorized NEON LDP/STP via `add x9, x1, x6, lsl #4` and `add x10, x2, x6, lsl #4`,
        //    leaving x9 and x10 LIVE at loop exit (epilogue must unshift both aliases!).
        let p1 = CODE + 0x400;
        let code1 = [
            0x8b06_1029,            // add x9, x1, x6, lsl #4
            0x8b06_104a,            // add x10, x2, x6, lsl #4
            0xad40_0520,            // ldp q0, q1, [x9]
            0x4ee1_8400,            // add v0.2d, v0.2d, v1.2d
            0xad00_0540,            // stp q0, q1, [x10]
            0x9100_08c6,            // add x6, x6, #2
            0xeb07_00df,            // cmp x6, x7
            b_cond(p1 + 28, p1, 3), // b.lo p1
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p1, &[(p1, &code1)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[1] = DATA + 0x100;
            s.x[2] = DATA + 0x800;
            s.x[6] = 0;
            s.x[7] = 64; // 32 iters, 1024 B
        });

        // 2. sha256-shaped shifted ADD with negative ldur offset and overwritten temp register.
        let p2 = CODE + 0x440;
        let code2 = [
            0x8b06_0981,            // add x1, x12, x6, lsl #2
            0x8b06_09a3,            // add x3, x13, x6, lsl #2
            0x9100_04c6,            // add x6, x6, #1
            0xb85f_c037,            // ldur w23, [x1, #-4]
            0x0a04_0081,            // and w1, w4, w4 (overwrites x1!)
            0xb85f_c078,            // ldur w24, [x3, #-4]
            0x0b18_02f7,            // add w23, w23, w24
            0x0b17_0000,            // add w0, w0, w23
            0xf101_04df,            // cmp x6, #65
            b_cond(p2 + 36, p2, 1), // b.ne p2
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p2, &[(p2, &code2)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[4] = 0x55aa;
            s.x[6] = 1;
            s.x[12] = DATA + 0x200;
            s.x[13] = DATA + 0x600;
        });

        // 3. LD1/ST1 with extended `add x9, x1, w6, uxtw #4` (32-bit index + NEON structure op).
        let p3 = CODE + 0x480;
        let code3 = [
            0x8b26_5029,            // add x9, x1, w6, uxtw #4
            0x4c40_7120,            // ld1 {v0.16b}, [x9]
            0x4ee0_8421,            // add v1.2d, v1.2d, v0.2d
            0x1100_04c6,            // add w6, w6, #1
            0x6b07_00df,            // cmp w6, w7
            b_cond(p3 + 20, p3, 3), // b.lo p3
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[1] = DATA + 0x200;
            s.x[6] = 0;
            s.x[7] = 48;
            s.v[1] = [0, 0];
        });

        // 4. Pointer opacity violation via shifted ADD: storing derived pointer must be rejected!
        let p4 = CODE + 0x4c0;
        let code4 = [
            0x8b06_0c29,            // add x9, x1, x6, lsl #3
            0xf940_012a,            // ldr x10, [x9]
            0xf900_0049,            // str x9, [x2] (leaks shifted pointer value!)
            0x9100_04c6,            // add x6, x6, #1
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p4 + 20, p4, 1), // b.ne p4
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p4, &[(p4, &code4)], Some(false), |g| {
            let s = g.cpu.state_mut();
            s.x[1] = DATA + 0x100;
            s.x[2] = DATA + 0x400;
            s.x[6] = 0;
            s.x[3] = 8;
        });
    }
}

#[test]
fn bounded_table_lookup_mreg_streams() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. Exact crc32 inner loop with table straddling contiguous 4 KiB page boundary (page 0 -> 1)!
        let p1 = CODE + 0x500;
        let code1 = [
            0x3840_1440,            // ldrb w0, [x2], #1
            0x4a01_0000,            // eor w0, w0, w1
            0x9240_1c00,            // and x0, x0, #0xff
            0xb860_7a80,            // ldr w0, [x20, x0, lsl #2]
            0x4a41_2001,            // eor w1, w0, w1, lsr #8
            0xeb02_007f,            // cmp x3, x2
            b_cond(p1 + 24, p1, 1), // b.ne p1
            SVC,
        ];
        let before_interp = dbt.stats().exits_interp;
        assert_matches_tier0(regs, &mut dbt, p1, &[(p1, &code1)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[1] = 0xffff_ffff;
            s.x[2] = DATA + 0x100;
            s.x[3] = DATA + 0x900; // 2048 bytes
            s.x[20] = DATA + 0xe00; // 1024 B table straddling page 0 -> page 1!
        });
        assert_eq!(dbt.stats().exits_interp, before_interp, "{regs:?}");

        // 2. Table straddling NON-CONTIGUOUS host pages (page 2 -> page 3): dual-endpoint check
        //    must detect addend mismatch and bail cleanly to slow loop with exact result!
        assert_matches_tier0(regs, &mut dbt, p1, &[(p1, &code1)], None, |g| {
            let s = g.cpu.state_mut();
            s.x[1] = 0x1234_5678;
            s.x[2] = DATA + 0x100;
            s.x[3] = DATA + 0x300;
            s.x[20] = DATA + 2 * PAGE_SIZE + 0xe00; // straddles page 2 -> page 3!
        });

        // 3. Direct LDRB + UBFX + 32-bit AND W bounded lookups with UXTW #2 and LSL #3.
        let p3 = CODE + 0x540;
        let code3 = [
            0x3840_1440,            // ldrb w0, [x2], #1  (u_bits = 8)
            0xb860_5a84,            // ldr w4, [x20, w0, uxtw #2] (1024 B)
            0x5302_1c05,            // ubfx w5, w0, #2, #6 (u_bits = 6)
            0xf865_7aa5,            // ldr x5, [x21, x5, lsl #3] (512 B)
            0x8b04_00c6,            // add x6, x6, x4
            0x8b05_00c6,            // add x6, x6, x5
            0xeb02_007f,            // cmp x3, x2
            b_cond(p3 + 28, p3, 1), // b.ne p3
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[2] = DATA + 0x100;
            s.x[3] = DATA + 0x300;
            s.x[6] = 0;
            s.x[20] = DATA + 0x400;
            s.x[21] = DATA + 0x900;
        });

        // 4. Table straddling mapped page 5 into UNMAPPED page 6: if bytes only index low half
        //    (no actual out-of-bounds access), slow loop succeeds with NO false fault!
        let p4 = CODE + 0x580;
        let code4 = [
            0x3840_1440,            // ldrb w0, [x2], #1
            0x1200_1c00,            // and w0, w0, #0xff (u_bits = 8 -> 1024 B table)
            0xb860_7a84,            // ldr w4, [x20, x0, lsl #2]
            0x8b04_00c6,            // add x6, x6, x4
            0xeb02_007f,            // cmp x3, x2
            b_cond(p4 + 20, p4, 1), // b.ne p4
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p4, &[(p4, &code4)], Some(true), |g| {
            g.write_data_bytes(DATA + 0x100, &[0, 1, 2, 3, 4, 5, 6, 7]);
            let s = g.cpu.state_mut();
            s.x[2] = DATA + 0x100;
            s.x[3] = DATA + 0x108;
            s.x[6] = 0;
            // Base at page 5 + 0xf00: offsets 0..255 are mapped, offsets 256..1023 are UNMAPPED!
            s.x[20] = DATA + 5 * PAGE_SIZE + 0xf00;
        });

        // 5. Oversized mask (12-bit mask with lsl #2 = 16 KiB > 4096) must be rejected.
        let p5 = CODE + 0x5c0;
        let code5 = [
            0x3840_1440,            // ldrb w0, [x2], #1
            0x9240_2c00,            // and x0, x0, #0xfff (u_bits = 12)
            0xb860_7a84,            // ldr w4, [x20, x0, lsl #2] (16 KiB > 4096!)
            0x8b04_00c6,            // add x6, x6, x4
            0xeb02_007f,            // cmp x3, x2
            b_cond(p5 + 20, p5, 1), // b.ne p5
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p5, &[(p5, &code5)], Some(false), |g| {
            g.write_data_bytes(DATA + 0x100, &[0, 1, 2, 3, 4, 5, 6, 7]);
            let s = g.cpu.state_mut();
            s.x[2] = DATA + 0x100;
            s.x[3] = DATA + 0x108;
            s.x[6] = 0;
            s.x[20] = DATA + 0x400;
        });
    }
}

#[test]
fn loop_closing_idioms_adds_cbnz_tbz_tbnz() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. ADDS X3, X3, #1 + B.NE (negative counter counting up to 0, ALSO used as M_REG index!).
        let p1 = CODE + 0x600;
        let code1 = [
            0xb863_782a,            // ldr w10, [x1, x3, lsl #2]
            0x8b0a_0000,            // add x0, x0, x10
            0xb100_0463,            // adds x3, x3, #1
            b_cond(p1 + 12, p1, 1), // b.ne p1
            SVC,
        ];
        for cnt in [-64i64, -3, -2, -1] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p1,
                &[(p1, &code1)],
                if cnt == -64 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x400;
                    s.x[3] = cnt as u64;
                },
            );
        }

        // 2. 32-bit ADDS W3, W3, #3 + B.MI (step > 1, crossing 0 from negative -> positive).
        let p2 = CODE + 0x640;
        let code2 = [
            0xb840_442a,            // ldr w10, [x1], #4
            0x8b0a_0000,            // add x0, x0, x10
            0x3100_0c63,            // adds w3, w3, #3
            b_cond(p2 + 12, p2, 4), // b.mi p2
            SVC,
        ];
        for cnt in [-40i32, -5, -4, -3, 0, 5] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p2,
                &[(p2, &code2)],
                if cnt == -40 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[3] = cnt as u32 as u64;
                },
            );
        }

        // 3. ADD X3, X3, #1 + CBNZ X3 (negative counter, must preserve pre-loop NZCV!).
        let p3 = CODE + 0x680;
        let code3 = [
            0xf840_842a, // ldr x10, [x1], #8
            0x8b0a_0000, // add x0, x0, x10
            0x9100_0463, // add x3, x3, #1
            cbnz(p3 + 12, p3, 3, true),
            SVC,
        ];
        assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x100;
            s.x[3] = (-32i64) as u64;
            s.nzcv = 0x5000_0000;
        });

        // 4. SUBS X2, X2, #1 + TBZ X2, #63, loop (continue while >= 0).
        let p4 = CODE + 0x6c0;
        let code4 = [
            0xf840_842a, // ldr x10, [x1], #8
            0x8b0a_0000, // add x0, x0, x10
            0xf100_0442, // subs x2, x2, #1
            tbz(p4 + 12, p4, 2, 63),
            SVC,
        ];
        for cnt in [40i64, 2, 1, 0, -1] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p4,
                &[(p4, &code4)],
                if cnt == 40 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[2] = cnt as u64;
                },
            );
        }

        // 5. 32-bit SUB W2, W2, #2 + TBZ W2, #31, loop (step=2, preserves NZCV).
        let p5 = CODE + 0x700;
        let code5 = [
            0xb840_442a, // ldr w10, [x1], #4
            0x8b0a_0000, // add x0, x0, x10
            0x5100_0842, // sub w2, w2, #2
            tbz(p5 + 12, p5, 2, 31),
            SVC,
        ];
        for cnt in [31i32, 30, 2, 1, 0, -1] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p5,
                &[(p5, &code5)],
                if cnt == 31 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[2] = cnt as u32 as u64;
                    s.nzcv = 0xa000_0000;
                },
            );
        }

        // 6. ADDS X2, X2, #1 + TBNZ X2, #63, loop (continue while < 0).
        let p6 = CODE + 0x740;
        let code6 = [
            0xf840_842a, // ldr x10, [x1], #8
            0x8b0a_0000, // add x0, x0, x10
            0xb100_0442, // adds x2, x2, #1
            tbnz(p6 + 12, p6, 2, 63),
            SVC,
        ];
        for cnt in [-20i64, -3, -2, -1, 0, 5] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p6,
                &[(p6, &code6)],
                if cnt == -20 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[2] = cnt as u64;
                },
            );
        }
    }
}

#[test]
fn early_exit_side_branches_exact_state_and_nzcv() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. sort-shaped counted loop with B.CC early exit + live alias + stolen regs:
        //    test both full completion AND early exit at iter 0, 1, 5, 40!
        let p1 = CODE + 0x800;
        let fail1 = CODE + 0x840;
        let stolen_r: u32 = match regs {
            DbtRegs::Default => 28,
            DbtRegs::Go => 25,
        };
        let code1 = [
            0xaa00_03e5 | (stolen_r << 16), // mov x5, xS (live alias BEFORE side exit!)
            0x2a01_03e2,                    // mov w2, w1
            0xb940_00a1,                    // ldr w1, [x5]
            0x6b02_003f,                    // cmp w1, w2
            b_cond(p1 + 16, fail1, 3),      // b.lo fail1 (early side exit!)
            0x9100_1000 | (stolen_r << 5) | stolen_r, // add xS, xS, #4
            0xeb00_00ff | (stolen_r << 16), // cmp x7, xS
            b_cond(p1 + 28, p1, 1),         // b.ne p1
            SVC,
        ];
        let fail1_code = [
            0xd280_0540, // mov x0, #42 (at fail1, NZCV from cmp w1, w2 must be exact!)
            SVC,
        ];
        for bad_at in [None, Some(0usize), Some(1), Some(5), Some(40)] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p1,
                &[(p1, &code1), (fail1, &fail1_code)],
                if bad_at.is_none() { Some(true) } else { None },
                |g| {
                    for i in 0..64usize {
                        let v: u32 = if bad_at == Some(i) {
                            0
                        } else {
                            (i as u32) * 10 + 5
                        };
                        g.write_data_bytes(DATA + 0x100 + (i * 4) as u64, &v.to_le_bytes());
                    }
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = 1;
                    s.x[stolen_r as usize] = DATA + 0x100;
                    s.x[7] = DATA + 0x100 + 64 * 4;
                },
            );
        }

        // 2. lz-shaped CBZ side exit AFTER loop-closing CMP (`cmp x0, #64; ...; cbz w1, exit; b.ne`):
        //    exit block immediately reads NZCV via `cset x9, eq`!
        let p2 = CODE + 0x880;
        let exit2 = CODE + 0x8c0;
        let code2 = [
            0x9100_0400,                   // add x0, x0, #1
            0x8b00_0042,                   // add x2, x2, x0? wait: x2 = x4 + x0
            0xf101_001f,                   // cmp x0, #64 (MUST NOT be omitted because cbz follows!)
            0x381f_f041,                   // sturb w1, [x2, #-1]
            0x3860_6861,                   // ldrb w1, [x3, x0]
            cbz(p2 + 20, exit2, 1, false), // cbz w1, exit2
            b_cond(p2 + 24, p2, 1),        // b.ne p2
            SVC,
        ];
        // Fix insn 1 to `add x2, x4, x0` (`0x8b00_0082`)
        let mut code2 = code2;
        code2[1] = 0x8b00_0082;
        let exit2_code = [
            0x9a9f_17e9, // cset x9, eq  (reads NZCV set by `cmp x0, #64`!)
            SVC,
        ];
        for zero_at in [None, Some(10usize), Some(64)] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p2,
                &[(p2, &code2), (exit2, &exit2_code)],
                if zero_at.is_none() { Some(true) } else { None },
                |g| {
                    let mut buf = [0x41u8; 80];
                    if let Some(pos) = zero_at {
                        buf[pos] = 0;
                    }
                    g.write_data_bytes(DATA + 0x200, &buf);
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = 0x41;
                    s.x[3] = DATA + 0x200;
                    s.x[4] = DATA + 0x400;
                    s.x[9] = 99;
                },
            );
        }

        // 3. Multi-target early exits (3 distinct targets: CBZ, B.EQ, TBNZ) + stolen regs + borrowed regs!
        let p3 = CODE + 0x900;
        let t_a = CODE + 0x960;
        let t_b = CODE + 0x980;
        let t_c = CODE + 0x9a0;
        let code3 = [
            0xb840_442a,                 // ldr w10, [x1], #4
            cbz(p3 + 4, t_a, 10, false), // target A if w10 == 0
            0x7101_ed5f,                 // cmp w10, #123
            b_cond(p3 + 12, t_b, 0),     // target B if w10 == 123
            tbnz(p3 + 16, t_c, 10, 7),   // target C if bit 7 set
            0x8b0a_0000,                 // add x0, x0, x10
            0xf100_0463,                 // subs x3, x3, #1
            b_cond(p3 + 28, p3, 1),      // b.ne p3
            SVC,
        ];
        let ta_code = [0xd280_0148, SVC]; // mov x8, #10
        let tb_code = [0x9a9f_17e9, 0xd280_0288, SVC]; // cset x9, eq; mov x8, #20
        let tc_code = [0xd280_03c8, SVC]; // mov x8, #30
        for trigger in [
            None,
            Some((5usize, 0u32)),
            Some((12, 123)),
            Some((18, 0x81)),
        ] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p3,
                &[
                    (p3, &code3),
                    (t_a, &ta_code),
                    (t_b, &tb_code),
                    (t_c, &tc_code),
                ],
                if trigger.is_none() { Some(true) } else { None },
                |g| {
                    for i in 0..32usize {
                        let v: u32 = match trigger {
                            Some((pos, val)) if pos == i => val,
                            _ => (i as u32) + 1,
                        };
                        g.write_data_bytes(DATA + 0x100 + (i * 4) as u64, &v.to_le_bytes());
                    }
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[3] = 32;
                    s.x[8] = 0;
                    s.x[9] = 0;
                    s.nzcv = 0x8000_0000;
                },
            );
        }

        // 4. Unsafe mid-iteration alias created AFTER a side exit must be REJECTED (on iter 0 x5 is
        //    unshifted, on iter 1+ x5 would be shifted!).
        let p4 = CODE + 0x9c0;
        let e4 = CODE + 0xa00;
        let code4 = [
            0xb840_442a,                // ldr w10, [x1], #4
            cbz(p4 + 4, e4, 10, false), // side exit BEFORE `mov x5, x1`!
            0xaa01_03e5,                // mov x5, x1
            0xf100_0463,                // subs x3, x3, #1
            b_cond(p4 + 16, p4, 1),     // b.ne p4
            SVC,
        ];
        let e4_code = [SVC];
        assert_matches_tier0(
            regs,
            &mut dbt,
            p4,
            &[(p4, &code4), (e4, &e4_code)],
            Some(false),
            |g| {
                g.write_data_bytes(DATA + 0x100, &[0, 0, 0, 0, 1, 0, 0, 0]);
                let s = g.cpu.state_mut();
                s.x[1] = DATA + 0x100;
                s.x[3] = 8;
                s.x[5] = 0xdead_beef;
            },
        );
    }
}

#[test]
fn header_tested_while_loops() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. Canonical Go-SSA header-tested while loop:
        //    `cmp x4, x5; b.ge exit; ldr x10, [x1, x4, lsl #3]; add x0, x0, x10; add x4, x4, #1; b loop`
        let p1 = CODE + 0xa80;
        let e1 = CODE + 0xac0;
        let code1 = [
            0xeb05_009f,            // cmp x4, x5
            b_cond(p1 + 4, e1, 10), // b.ge e1
            0xf864_782a,            // ldr x10, [x1, x4, lsl #3]
            0x8b0a_0000,            // add x0, x0, x10
            0x9100_0484,            // add x4, x4, #1
            b_imm(p1 + 20, p1),     // b p1
        ];
        let e1_code = [SVC];
        for (start, end) in [
            (0i64, 600i64),
            (0, 0),
            (5, 3),
            (0, 1),
            (0, 2),
            (0, 3),
            (-10, 10),
        ] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p1,
                &[(p1, &code1), (e1, &e1_code)],
                if end == 600 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x200;
                    s.x[4] = start as u64;
                    s.x[5] = end as u64;
                },
            );
        }

        // 2. Header-tested while loop WITH inner bounds-check side exit (Go slice pattern!):
        let p2 = CODE + 0xb00;
        let e2 = CODE + 0xb40;
        let panic2 = CODE + 0xb50;
        let code2 = [
            0xeb05_009f,                // cmp x4, x5
            b_cond(p2 + 4, e2, 10),     // b.ge e2 (header exit)
            0xeb06_009f,                // cmp x4, x6 (bounds check)
            b_cond(p2 + 12, panic2, 2), // b.hs panic2 (side exit!)
            0xf864_782a,                // ldr x10, [x1, x4, lsl #3]
            0x8b0a_0000,                // add x0, x0, x10
            0x9100_0484,                // add x4, x4, #1
            b_imm(p2 + 28, p2),         // b p2
        ];
        let e2_code = [SVC];
        let panic2_code = [0xd280_1fe0, SVC]; // mov x0, #0xff
        for cap in [64u64, 15, 0] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p2,
                &[(p2, &code2), (e2, &e2_code), (panic2, &panic2_code)],
                if cap == 64 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[4] = 0;
                    s.x[5] = 40;
                    s.x[6] = cap;
                },
            );
        }

        // 3. Header-tested CBZ + SUB countdown (`cbz x3, exit; ...; sub x3, x3, #1; b loop`).
        let p3 = CODE + 0xb80;
        let e3 = CODE + 0xbc0;
        let code3 = [
            cbz(p3, e3, 3, true),
            0xf840_842a,        // ldr x10, [x1], #8
            0x8b0a_0000,        // add x0, x0, x10
            0xd100_0463,        // sub x3, x3, #1
            b_imm(p3 + 16, p3), // b p3
        ];
        let e3_code = [SVC];
        for n in [40u64, 2, 1, 0] {
            assert_matches_tier0(
                regs,
                &mut dbt,
                p3,
                &[(p3, &code3), (e3, &e3_code)],
                if n == 40 { Some(true) } else { None },
                |g| {
                    let s = g.cpu.state_mut();
                    s.x[0] = 0;
                    s.x[1] = DATA + 0x100;
                    s.x[3] = n;
                    s.nzcv = 0x3000_0000;
                },
            );
        }

        // 4. Header-tested decrementing pointer loop (`cmp x1, x2; b.lo exit; ldr [x1], #-8; b loop`).
        let p4 = CODE + 0xc00;
        let e4 = CODE + 0xc40;
        let code4 = [
            0xeb02_003f,           // cmp x1, x2
            b_cond(p4 + 4, e4, 3), // b.lo e4 (continue while x1 >=u x2)
            0xf85f_842a,           // ldr x10, [x1], #-8
            0x8b0a_0000,           // add x0, x0, x10
            b_imm(p4 + 16, p4),    // b p4
        ];
        let e4_code = [SVC];
        assert_matches_tier0(
            regs,
            &mut dbt,
            p4,
            &[(p4, &code4), (e4, &e4_code)],
            Some(true),
            |g| {
                let s = g.cpu.state_mut();
                s.x[0] = 0;
                s.x[1] = DATA + 0x400;
                s.x[2] = DATA + 0x200;
            },
        );

        // 5. Header-tested loop where `cset` between `cmp` and `b.ge` reads the header CMP's NZCV:
        //    header CMP must NOT be omitted so `cset` sees exact flags on every iteration!
        let p5 = CODE + 0xc60;
        let e5 = CODE + 0xca0;
        let code5 = [
            0xeb05_009f,            // cmp x4, x5
            0x9a9f_a7e9,            // cset x9, lt (reads NZCV from cmp BEFORE b.ge!)
            b_cond(p5 + 8, e5, 10), // b.ge e5
            0xf864_782a,            // ldr x10, [x1, x4, lsl #3]
            0x8b0a_0000,            // add x0, x0, x10
            0x8b09_0000,            // add x0, x0, x9
            0x9100_0484,            // add x4, x4, #1
            b_imm(p5 + 28, p5),     // b p5
        ];
        let e5_code = [SVC];
        assert_matches_tier0(
            regs,
            &mut dbt,
            p5,
            &[(p5, &code5), (e5, &e5_code)],
            Some(true),
            |g| {
                let s = g.cpu.state_mut();
                s.x[0] = 0;
                s.x[1] = DATA + 0x100;
                s.x[4] = 0;
                s.x[5] = 32;
                s.nzcv = 0x6000_0000; // Z=1, C=1 (lt is false before loop!)
            },
        );
    }
}

#[test]
fn adr_adrp_m_literal_and_raised_kick_budget() {
    for regs in [DbtRegs::Default, DbtRegs::Go] {
        let Some(mut dbt) = Dbt::with_regs(regs) else {
            return;
        };

        // 1. ADR + ADRP + read-only M_LITERAL (W, X, SW, Q) inside a fast loop (incl. stolen Rd!).
        let p1 = CODE + 0xd00;
        let lit_pool = CODE + 0xd80; // 16-byte aligned in same code page
        let stolen_r: u32 = match regs {
            DbtRegs::Default => 17,
            DbtRegs::Go => 22,
        };
        let code1 = [
            0xf840_842a,                           // ldr x10, [x1], #8
            adr(11, p1 + 4, lit_pool),             // adr x11, lit_pool
            adrp(stolen_r, p1 + 8, DATA),          // adrp xS, DATA
            ldr_lit_w(12, p1 + 12, lit_pool),      // ldr w12, lit_pool+0
            ldr_lit_sw(13, p1 + 16, lit_pool + 4), // ldrsw x13, lit_pool+4
            ldr_lit_x(14, p1 + 20, lit_pool + 8),  // ldr x14, lit_pool+8
            ldr_lit_q(2, p1 + 24, lit_pool + 16),  // ldr q2, lit_pool+16
            0x8b0a_0000,                           // add x0, x0, x10
            0x8b0b_0000,                           // add x0, x0, x11
            0x8b00_0000 | (stolen_r << 16),        // add x0, x0, xS
            0x8b0c_0000,                           // add x0, x0, x12
            0x8b0d_0000,                           // add x0, x0, x13
            0x8b0e_0000,                           // add x0, x0, x14
            0x4ee2_8421,                           // add v1.2d, v1.2d, v2.2d
            0xf100_0463,                           // subs x3, x3, #1
            b_cond(p1 + 60, p1, 1),                // b.ne p1
            SVC,
        ];
        let lits = [
            0x1234_5678u32,
            0xffff_ff80u32, // -128 when sign-extended by ldrsw!
            0xabcd_ef01u32,
            0x0102_0304u32,
            0x1111_2222u32,
            0x3333_4444u32,
            0x5555_6666u32,
            0x7777_8888u32,
        ];
        assert_matches_tier0(
            regs,
            &mut dbt,
            p1,
            &[(p1, &code1), (lit_pool, &lits)],
            Some(true),
            |g| {
                let s = g.cpu.state_mut();
                s.x[0] = 0;
                s.x[1] = DATA + 0x100;
                s.x[3] = 32;
                s.v[1] = [0, 0];
            },
        );

        // 2. Watchpoint on literal pool address must NOT be bypassed by M_LITERAL fast loop!
        let mut gw = Guest::with_data();
        gw.code(p1, &code1);
        gw.code(lit_pool, &lits);
        gw.cpu
            .state_mut()
            .set_hw_debug(&[], &[(lit_pool, lit_pool + 4, WATCH_LOAD)]);
        {
            let s = gw.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA + 0x100;
            s.x[3] = 16;
            s.v[1] = [0, 0];
        }
        assert_eq!(
            gw.run_t1(&mut dbt, p1),
            Exit::Fault {
                addr: lit_pool,
                access: Access::Read,
                kind: FaultKind::Watchpoint,
            },
            "M_LITERAL watchpoint ({regs:?})"
        );

        // 3. Raised KICK_BUDGET (up to 4096 iters on a 3-insn loop): run 6000 iterations over a
        //    2-page contiguous span (4096 + 1904) and verify exact sum + prompt kick!
        let p3 = CODE + 0xe00;
        let code3 = [
            0x3840_142a,            // ldrb w10, [x1], #1
            0x8b0a_0000,            // add x0, x0, x10
            0xf100_0463,            // subs x3, x3, #1
            b_cond(p3 + 12, p3, 1), // b.ne p3
            SVC,
        ];
        let g3 = assert_matches_tier0(regs, &mut dbt, p3, &[(p3, &code3)], Some(true), |g| {
            let s = g.cpu.state_mut();
            s.x[0] = 0;
            s.x[1] = DATA;
            s.x[3] = 6000;
        });
        assert_eq!(g3.cpu.state().x[1], DATA + 6000, "{regs:?}");

        // Verify store watchpoint on fast loop still triggers cleanly.
        let p4 = CODE + 0xe40;
        let code4 = [
            0x3800_142a,           // strb w10, [x1], #1
            0xf100_0463,           // subs x3, x3, #1
            b_cond(p4 + 8, p4, 1), // b.ne p4
            SVC,
        ];
        let mut gsw = Guest::with_data();
        gsw.code(p4, &code4);
        gsw.cpu
            .state_mut()
            .set_hw_debug(&[], &[(DATA + 0x50, DATA + 0x51, WATCH_STORE)]);
        {
            let s = gsw.cpu.state_mut();
            s.x[1] = DATA;
            s.x[10] = 0xab;
            s.x[3] = 200;
        }
        assert_eq!(
            gsw.run_t1(&mut dbt, p4),
            Exit::Fault {
                addr: DATA + 0x50,
                access: Access::Write,
                kind: FaultKind::Watchpoint,
            },
            "store watchpoint ({regs:?})"
        );
        // Also verify prompt Exit::Kick on the 3-insn raised-budget loop.
        gsw.cpu.state_mut().set_hw_debug(&[], &[]);
        gsw.cpu.tlb_flush();
        {
            let s = gsw.cpu.state_mut();
            s.x[1] = DATA;
            s.x[10] = 0xab;
            s.x[3] = 4000;
        }
        let kicker = gsw.cpu.kicker();
        unsafe { kicker.kick() };
        assert_eq!(gsw.run_t1(&mut dbt, p4), Exit::Kick, "{regs:?}");
        assert_eq!(gsw.read_data_u64(DATA) & 0xff, 0xab, "{regs:?}");
    }
}
