//! Tier 1 在地址空间换了之后保留翻译、逐块核对 guest 指令字（`Dbt::recheck`，以及从
//! 池里拿到的实例），函数返回的预测（影子返回栈），以及跨页的访存。宿主不是 aarch64
//! Linux 时没有 Tier 1，测试直接通过。

use std::ptr::NonNull;

use appine_engine::{Cpu, Dbt, DbtSpace, Exit, FaultKind, Mmu, PAGE_SIZE, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// 两页代码，`CODE` 和紧挨着的下一页；`STACK` 往下是一页栈。
const CODE: u64 = 0x40_0000;
const STACK: u64 = 0x80_0000;
/// [`Guest::with_data`] 映射的数据页从这里开始，见那里的说明。
const DATA: u64 = 0x100_0000;

const SVC: u32 = 0xd400_0001;
const RET: u32 = 0xd65f_03c0;
const STP_FP_LR_PRE: u32 = 0xa9bf_7bfd; // stp x29, x30, [sp, #-16]!
const LDP_FP_LR_POST: u32 = 0xa8c1_7bfd; // ldp x29, x30, [sp], #16
const SUB_X0_1: u32 = 0xd100_0400; // sub x0, x0, #1
const ADD_X1_1: u32 = 0x9100_0421; // add x1, x1, #1
const LDR_X0_X2: u32 = 0xf940_0040; // ldr x0, [x2]
const STR_X3_X4: u32 = 0xf900_0083; // str x3, [x4]
const LDP_X5_X6_X7: u32 = 0xa940_18e5; // ldp x5, x6, [x7]

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
        Self::with_pages(3)
    }

    /// 在 [`Guest::new`] 之外再映射 6 页数据，第 i 页的每个字节都是 `i * 16` 加上页内
    /// 偏移的低 4 位：
    /// `DATA`、`DATA + 1 页` 的宿主页相邻；`+ 2 页`、`+ 3 页` 的宿主页前后颠倒；
    /// `+ 4 页` 可读写，`+ 5 页` 只读，`+ 6 页` 没有映射。
    fn with_data() -> Self {
        let mut g = Self::with_pages(9);
        for (i, page) in g.pages[3..].iter_mut().enumerate() {
            for (j, byte) in page.0.iter_mut().enumerate() {
                *byte = (i * 16 + j % 16) as u8;
            }
        }
        let base = NonNull::new(g.pages.as_mut_ptr().cast::<u8>()).unwrap();
        let rw = prot::READ | prot::WRITE;
        let layout = [
            (0, 3, rw),
            (1, 4, rw),
            (2, 6, rw),
            (3, 5, rw),
            (4, 7, rw),
            (5, 8, prot::READ),
        ];
        for (page, host, p) in layout {
            unsafe {
                g.mmu
                    .map(DATA + page * PAGE_SIZE, base.add(host * 4096), PAGE_SIZE, p)
                    .unwrap();
            }
        }
        g
    }

    fn with_pages(n: usize) -> Self {
        let mut pages: Box<[Page]> = (0..n).map(|_| Page([0; 4096])).collect();
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

    /// 数据第 `page` 页、页内偏移 `off` 处的 8 个字节（不跨页）。
    fn data(&self, page: usize, off: usize) -> u64 {
        let host = [3, 4, 6, 5, 7, 8][page];
        u64::from_le_bytes(self.pages[host].0[off..off + 8].try_into().unwrap())
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
        self.run_at(dbt, CODE)
    }

    /// 从 `pc` 开始执行到 svc，返回 x0。
    fn run_at(&mut self, dbt: &mut Dbt, pc: u64) -> u64 {
        let _ = &self.mmu;
        self.cpu.state_mut().pc = pc;
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
fn pooled_instances_keep_translations_of_the_same_program() {
    // 别的测试不用这个键；池里同一个键的实例只会还给它
    const KEY: u64 = 0x5eed_0001;
    let Some(space) = DbtSpace::new(KEY) else {
        return;
    };
    let mut g = Guest::new();
    g.code(CODE, &[movz_x0(7), SVC]);
    let mut dbt = Dbt::attach(&space).unwrap();
    assert_eq!(g.run(&mut dbt), 7);
    drop((dbt, space));
    // 同一个程序再运行：拿回留下的翻译，核对之后直接用，不再翻译
    let space = DbtSpace::new(KEY).unwrap();
    let mut dbt = Dbt::attach(&space).unwrap();
    assert_eq!(g.run(&mut dbt), 7);
    assert_eq!(dbt.stats().blocks, 0);
    drop((dbt, space));
    // 这期间代码变了：核对时作废，重新翻译
    g.code(CODE, &[movz_x0(8), SVC]);
    let space = DbtSpace::new(KEY).unwrap();
    let mut dbt = Dbt::attach(&space).unwrap();
    assert_eq!(g.run(&mut dbt), 8);
    assert_eq!(dbt.stats().blocks, 1);
    drop((dbt, space));
    // 宿主内存紧张时丢掉池里保留的翻译：再运行要重新翻译（或者拷贝模板）
    appine_engine::trim_dbt_pool();
    let space = DbtSpace::new(KEY).unwrap();
    let mut dbt = Dbt::attach(&space).unwrap();
    assert_eq!(g.run(&mut dbt), 8);
    assert_eq!(dbt.stats().blocks, 1);
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

/// 数据第 `page` 页的第一个字节的地址减去 `before`：从这里开始的访问跨到第 `page` 页。
fn before_page(page: u64, before: u64) -> u64 {
    DATA + page * PAGE_SIZE - before
}

/// 跨页访问的预期值：第 `page - 1` 页末尾的 `before` 个字节接着第 `page` 页开头的字节。
fn crossing(g: &Guest, page: usize, before: usize) -> u64 {
    let lo = g.data(page - 1, 4096 - 8) >> (8 * (8 - before));
    let hi = g.data(page, 0) << (8 * before);
    lo | hi
}

#[test]
fn cross_page_accesses_stay_in_translated_code() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    g.code(CODE, &[LDR_X0_X2, STR_X3_X4, LDP_X5_X6_X7, SVC]);
    // 宿主页相邻：第一次两页都不在 TLB 里，先填进去；第二次只比对表项。都不退给 Tier 0
    for round in 0..2u64 {
        let value = 0x1122_3344_5566_7788 + round;
        let s = g.cpu.state_mut();
        s.x[2] = before_page(1, 4);
        s.x[3] = value;
        s.x[4] = before_page(1, 2);
        s.x[7] = before_page(1, 12);
        let expect = crossing(&g, 1, 4);
        g.run(&mut dbt);
        let s = g.cpu.state();
        assert_eq!(s.x[0], expect);
        assert_eq!(g.data(0, 4096 - 8) >> 48, value & 0xffff);
        assert_eq!(g.data(1, 0) & 0xffff_ffff_ffff, value >> 16);
        assert_eq!(s.x[6], crossing(&g, 1, 4));
        assert_eq!(s.x[5], g.data(0, 4096 - 12));
    }
    assert_eq!(dbt.stats().exits_interp, 0);
}

#[test]
fn cross_page_accesses_to_scattered_host_pages_use_tier0() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    g.code(CODE, &[LDR_X0_X2, SVC]);
    g.cpu.state_mut().x[2] = before_page(3, 4);
    // 宿主上紧接着第 2 页的是第 3 页之前的那一页，必须按页表分别访问
    assert_eq!(g.run(&mut dbt), crossing(&g, 3, 4));
    assert_eq!(dbt.stats().exits_interp, 1);
}

#[test]
fn cross_page_faults_are_precise() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    // 写跨进只读的一页、读跨进没有映射的一页：报告在第二页，第一页不能被写
    let cases = [
        (STR_X3_X4, 4, before_page(5, 4), DATA + 5 * PAGE_SIZE),
        (LDR_X0_X2, 2, before_page(6, 4), DATA + 6 * PAGE_SIZE),
        (LDR_X0_X2, 2, u64::MAX - 3, 0),
    ];
    for (insn, reg, addr, fault) in cases {
        g.code(CODE, &[insn, SVC]);
        dbt.invalidate(CODE, 8);
        let before = g.data(4, 4096 - 8);
        g.cpu.state_mut().x[reg] = addr;
        g.cpu.state_mut().pc = CODE;
        match dbt.run(&mut g.cpu) {
            Exit::Fault { addr, .. } if fault != 0 => assert_eq!(addr, fault),
            Exit::Fault { .. } => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(g.cpu.state().pc, CODE);
        assert_eq!(g.data(4, 4096 - 8), before);
    }
}

const LDR_X1_X2_8: u32 = 0xf940_0441; // ldr x1, [x2, #8]

// 下面几个测试检查是否分组。实例可能来自池子，带着别的测试记下的"这里不再分组"
// （按 pc），所以每个测试的代码放在自己的地址上。

#[test]
fn accesses_with_the_same_base_share_one_check() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::new();
    // 函数的序言和尾声，夹着改写被占用寄存器的指令：一共 6 条访存，只检查一次
    let f = CODE + 0x100;
    g.code(
        f,
        &[
            0xa9bd_7bfd, // stp x29, x30, [sp, #-48]!
            0xa901_47f0, // stp x16, x17, [sp, #16]
            0xf900_17fb, // str x27, [sp, #40]
            0xd280_0010, // movz x16, #0
            0xd280_0011, // movz x17, #0
            0xd280_001b, // movz x27, #0
            0xa941_47f0, // ldp x16, x17, [sp, #16]
            0xf940_17fb, // ldr x27, [sp, #40]
            0xa8c3_7bfd, // ldp x29, x30, [sp], #48
            SVC,
        ],
    );
    let regs = [16, 17, 27, 29, 30];
    for (i, &r) in regs.iter().enumerate() {
        g.cpu.state_mut().x[r] = 0x1111 * (i as u64 + 1);
    }
    g.run_at(&mut dbt, f);
    for (i, &r) in regs.iter().enumerate() {
        assert_eq!(g.cpu.state().x[r], 0x1111 * (i as u64 + 1));
    }
    assert_eq!(g.cpu.state().sp, STACK);
    let s = dbt.stats();
    assert_eq!(s.insns_grouped, 5);
    assert_eq!(s.exits_interp, 0);
}

#[test]
fn shared_checks_across_scattered_host_pages_are_undone() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    let f = CODE + 0x200;
    g.code(f, &[LDR_X0_X2, LDR_X1_X2_8, SVC]);
    // 两条访问各自都不跨页，合起来跨到宿主上不相邻的下一页：退给 Tier 0，第二次之后
    // 这个块不再分组
    for (ungrouped, interp) in [(0, 1), (1, 2), (1, 2)] {
        g.cpu.state_mut().x[2] = before_page(3, 8);
        g.run_at(&mut dbt, f);
        assert_eq!(g.cpu.state().x[0], g.data(2, 4096 - 8));
        assert_eq!(g.cpu.state().x[1], g.data(3, 0));
        let s = dbt.stats();
        assert_eq!((s.ungrouped, s.exits_interp), (ungrouped, interp));
    }
    // 别处同样的代码访问宿主上相邻的两页，照样共用一次检查
    let f = CODE + 0x300;
    g.code(f, &[LDR_X0_X2, LDR_X1_X2_8, SVC]);
    g.cpu.state_mut().x[2] = before_page(1, 8);
    g.run_at(&mut dbt, f);
    assert_eq!(g.cpu.state().x[1], g.data(1, 0));
    let s = dbt.stats();
    assert_eq!((s.ungrouped, s.insns_grouped), (1, 2));
}

#[test]
fn shared_checks_cover_accesses_behind_branches() {
    let Some(mut dbt) = Dbt::new() else {
        return;
    };
    let mut g = Guest::with_data();
    // 第二条加载在条件分支之后，要访问的下一页没有映射；x0 是 0 时它不会执行
    let f = CODE + 0x400;
    g.code(f, &[LDR_X0_X2, cbz_x0(f + 4, f + 12), LDR_X1_X2_8, SVC]);
    g.pages[8].0[4096 - 8..].fill(0);
    g.cpu.state_mut().x[1] = 7;
    for ungrouped in [0, 1, 1] {
        g.cpu.state_mut().x[2] = before_page(6, 8);
        g.run_at(&mut dbt, f);
        assert_eq!(g.cpu.state().x[0], 0);
        assert_eq!(g.cpu.state().x[1], 7);
        assert_eq!(dbt.stats().ungrouped, ungrouped);
    }
}
