//! Tier 1 在地址空间换了之后保留翻译、逐块核对 guest 指令字（`Dbt::recheck`，以及从
//! 池里拿到的实例）。宿主不是 aarch64 Linux 时没有 Tier 1，测试直接通过。

use std::ptr::NonNull;

use appine_engine::{Cpu, Dbt, Exit, Mmu, PAGE_SIZE, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// 两页代码，`CODE` 和紧挨着的下一页。
const CODE: u64 = 0x40_0000;

const SVC: u32 = 0xd400_0001;

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
        let mut pages: Box<[Page]> = (0..2).map(|_| Page([0; 4096])).collect();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(CODE, base, 2 * PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
        }
        let cpu = unsafe { Cpu::new(&mmu) };
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
