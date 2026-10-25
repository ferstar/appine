//! Tier 1 在地址空间换了之后保留翻译、逐块核对 guest 指令字（`Dbt::recheck`，以及从
//! 池里拿到的实例），以及函数返回的预测（影子返回栈）。宿主不是 aarch64 Linux 时没有
//! Tier 1，测试直接通过。

use std::ptr::NonNull;

use appine_engine::{Cpu, Dbt, Exit, FaultKind, Mmu, PAGE_SIZE, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// 两页代码，`CODE` 和紧挨着的下一页；`STACK` 往下是一页栈。
const CODE: u64 = 0x40_0000;
const STACK: u64 = 0x80_0000;

const SVC: u32 = 0xd400_0001;
const RET: u32 = 0xd65f_03c0;
const STP_FP_LR_PRE: u32 = 0xa9bf_7bfd; // stp x29, x30, [sp, #-16]!
const LDP_FP_LR_POST: u32 = 0xa8c1_7bfd; // ldp x29, x30, [sp], #16
const SUB_X0_1: u32 = 0xd100_0400; // sub x0, x0, #1
const ADD_X1_1: u32 = 0x9100_0421; // add x1, x1, #1

fn bl(from: u64, to: u64) -> u32 {
    0x9400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
}

fn cbz_x0(from: u64, to: u64) -> u32 {
    0xb400_0000 | ((((to as i64 - from as i64) / 4) as u32 & 0x7_ffff) << 5)
}

fn adr_x30(from: u64, to: u64) -> u32 {
    let off = (to as i64 - from as i64) as u32;
    0x1000_001e | (off & 3) << 29 | ((off >> 2) & 0x7_ffff) << 5
}

fn movz_x0(imm: u16) -> u32 {
    0xd280_0000 | (imm as u32) << 5
}

fn b(from: u64, to: u64) -> u32 {
    0x1400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
}

struct Guest {
    // 析构顺序：cpu → mmu → pages
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
}

impl Guest {
    fn new() -> Self {
        let mut pages: Box<[Page]> = (0..3).map(|_| Page([0; 4096])).collect();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(CODE, base, 2 * PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
            mmu.map(
                STACK - PAGE_SIZE,
                base.add(2 * 4096),
                PAGE_SIZE,
                prot::READ | prot::WRITE,
            )
            .unwrap();
        }
        let mut cpu = unsafe { Cpu::new(&mmu) };
        cpu.state_mut().sp = STACK;
        Guest { cpu, mmu, pages }
    }

    /// 把指令写到 `addr`（只支持本测试映射的两页）。
    fn code(&mut self, addr: u64, insns: &[u32]) {
        let off = (addr - CODE) as usize;
        for (i, insn) in insns.iter().enumerate() {
            let at = off + 4 * i;
            self.pages[at / 4096].0[at % 4096..at % 4096 + 4].copy_from_slice(&insn.to_le_bytes());
        }
    }

    /// 从 `CODE` 开始执行到 svc，返回 x0。
    fn run(&mut self, dbt: &mut Dbt) -> u64 {
        let _ = &self.mmu;
        self.cpu.state_mut().pc = CODE;
        assert!(matches!(dbt.run(&mut self.cpu), Exit::Svc { imm: 0 }));
        self.cpu.state().x[0]
    }
}

#[test]
fn recheck_after_exec_runs_the_new_code() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(1), SVC]);
    assert_eq!(g.run(&mut dbt), 1);
    // 同一个地址换成了另一个程序（exec），核对之后执行的是新代码
    g.code(CODE, &[movz_x0(2), SVC]);
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 2);
    // 代码没变时核对之后照样可用，不用重新翻译
    let blocks = dbt.stats().blocks;
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 2);
    assert_eq!(dbt.stats().blocks, blocks);
}

#[test]
fn pooled_instances_are_checked_before_use() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(3), SVC]);
    assert_eq!(g.run(&mut dbt), 3);
    // 实例回到池里，下一个线程拿到它时这里已经是别的代码
    drop(dbt);
    g.code(CODE, &[movz_x0(4), SVC]);
    let mut dbt = Dbt::new().unwrap();
    assert_eq!(g.run(&mut dbt), 4);
}

#[test]
fn recheck_retires_only_blocks_that_changed() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let second = CODE + PAGE_SIZE;
    g.code(CODE, &[b(CODE, second)]);
    g.code(second, &[movz_x0(5), SVC]);
    assert_eq!(g.run(&mut dbt), 5);
    // 只改第二页：第一页的块和它到第二页的链接保留，第二页的块作废后重新翻译
    g.code(second, &[movz_x0(6), SVC]);
    let before = dbt.stats();
    dbt.recheck();
    assert_eq!(g.run(&mut dbt), 6);
    let after = dbt.stats();
    assert_eq!(after.invalidated - before.invalidated, 1);
    assert_eq!(after.blocks - before.blocks, 1);
}

#[test]
fn returns_follow_x30_even_when_the_prediction_is_wrong() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let f = CODE + 0x100;
    let other = CODE + 0x200;
    g.code(CODE, &[bl(CODE, f), movz_x0(7), SVC]);
    // 普通的调用和返回：返回到调用之后
    g.code(f, &[RET]);
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(g.run(&mut dbt), 7);
    // 被调函数改了 x30（longjmp 那样）：影子栈顶对不上，按 x30 返回
    g.code(f, &[adr_x30(f, other), RET]);
    g.code(other, &[movz_x0(9), SVC]);
    dbt.invalidate(f, 4);
    assert_eq!(g.run(&mut dbt), 9);
}

#[test]
fn returning_to_the_empty_entry_marker_is_still_checked() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    // 影子栈的空项记的 guest 地址是 1：x30 等于它时照样按 x30 返回，得到 pc 对齐错误
    g.code(CODE, &[0xd280_003e, RET]); // movz x30, #1; ret
    g.cpu.state_mut().pc = CODE;
    assert!(matches!(
        dbt.run(&mut g.cpu),
        Exit::Fault {
            addr: 1,
            kind: FaultKind::Alignment,
            ..
        }
    ));
}

#[test]
fn deep_recursion_wraps_the_shadow_stack() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    let f = CODE + 0x100;
    // main: x0 = 200; bl f; svc。f 递归 x0 层，每层返回时 x1 加一：超过影子栈的 64 项之后
    // 栈底的项被覆盖，更深的返回只能按 x30 查找，结果必须一样
    g.code(CODE, &[movz_x0(200), bl(CODE + 4, f), SVC]);
    let done = f + 6 * 4;
    g.code(
        f,
        &[
            cbz_x0(f, done),
            STP_FP_LR_PRE,
            SUB_X0_1,
            bl(f + 12, f),
            LDP_FP_LR_POST,
            ADD_X1_1,
            RET,
        ],
    );
    for _ in 0..2 {
        g.cpu.state_mut().x[1] = 0;
        assert_eq!(g.run(&mut dbt), 0);
        assert_eq!(g.cpu.state().x[1], 200);
        assert_eq!(g.cpu.state().sp, STACK);
    }
}
