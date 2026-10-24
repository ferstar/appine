//! 解释器的单元测试：访存、分支、系统指令和 soft-MMU。
//!
//! 数据处理指令由 lockstep 对照硬件测试（tests/lockstep）；M0 的 lockstep
//! 不能执行访存和控制流指令，所以它们在这里用固定编码测试。编码取自 GNU as。

use std::ptr::NonNull;

use appine_engine::{Access, Cpu, Exit, FaultKind, Mmu, PAGE_SIZE, hwcap, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// 两个可读写的数据页映射在 `DATA`，另有一页只读映射在 `RODATA`。
const DATA: u64 = 0x10000;
const RODATA: u64 = 0x20000;
const CODE: u64 = 0x40_0000;

struct Guest {
    // 析构顺序：cpu → mmu → pages
    cpu: Cpu,
    mmu: Mmu,
    pages: Box<[Page]>,
}

impl Guest {
    fn new() -> Self {
        let mut pages: Box<[Page]> = (0..4).map(|_| Page([0; 4096])).collect();
        let mut mmu = Mmu::new();
        let base = NonNull::new(pages.as_mut_ptr().cast::<u8>()).unwrap();
        unsafe {
            mmu.map(DATA, base, 2 * PAGE_SIZE, prot::READ | prot::WRITE)
                .unwrap();
            mmu.map(RODATA, base.add(2 * 4096), PAGE_SIZE, prot::READ)
                .unwrap();
            mmu.map(CODE, base.add(3 * 4096), PAGE_SIZE, prot::READ | prot::EXEC)
                .unwrap();
        }
        let cpu = unsafe { Cpu::new(&mmu) };
        let mut g = Guest { cpu, mmu, pages };
        g.cpu.state_mut().pc = CODE;
        g
    }

    /// guest 地址对应的宿主字节（只支持本测试映射的页）。
    fn bytes(&mut self, addr: u64) -> &mut [u8] {
        let (page, off) = match addr {
            a if (DATA..DATA + 2 * PAGE_SIZE).contains(&a) => {
                ((a - DATA) / PAGE_SIZE, (a - DATA) % PAGE_SIZE)
            }
            a if (RODATA..RODATA + PAGE_SIZE).contains(&a) => (2, a - RODATA),
            a if (CODE..CODE + PAGE_SIZE).contains(&a) => (3, a - CODE),
            _ => panic!("address {addr:#x} not mapped by the test"),
        };
        &mut self.pages[page as usize].0[off as usize..]
    }

    fn put_u64(&mut self, addr: u64, v: u64) {
        self.bytes(addr)[..8].copy_from_slice(&v.to_le_bytes());
    }

    fn get_u64(&mut self, addr: u64) -> u64 {
        u64::from_le_bytes(self.bytes(addr)[..8].try_into().unwrap())
    }

    fn x(&self, r: usize) -> u64 {
        self.cpu.state().x[r]
    }

    fn set_x(&mut self, r: usize, v: u64) {
        self.cpu.state_mut().x[r] = v;
    }

    fn pc(&self) -> u64 {
        self.cpu.state().pc
    }

    /// 执行一条指令，要求正常完成，并且 pc 前进 4。
    fn step(&mut self, insn: u32) {
        let pc = self.pc();
        assert_eq!(self.cpu.exec_insn(insn), None, "insn {insn:#010x}");
        assert_eq!(self.pc(), pc + 4, "insn {insn:#010x}");
    }
}

#[test]
fn load_store_immediate_forms() {
    let mut g = Guest::new();
    g.put_u64(DATA + 8, 0x1122_3344_5566_7788);
    g.set_x(1, DATA);
    g.step(0xf9400420); // ldr x0, [x1, #8]
    assert_eq!(g.x(0), 0x1122_3344_5566_7788);

    g.set_x(1, DATA + 16);
    g.set_x(2, 0xdead_beef);
    g.step(0xf81f8c22); // str x2, [x1, #-8]!
    assert_eq!(g.get_u64(DATA + 8), 0xdead_beef);
    assert_eq!(g.x(1), DATA + 8);

    g.put_u64(DATA, 42);
    g.set_x(1, DATA);
    g.step(0xf8410423); // ldr x3, [x1], #16
    assert_eq!(g.x(3), 42);
    assert_eq!(g.x(1), DATA + 16);

    g.set_x(1, DATA);
    g.set_x(2, 0xabcd);
    g.step(0x79000422); // strh w2, [x1, #2]
    assert_eq!(&g.bytes(DATA)[2..4], &[0xcd, 0xab]);
}

#[test]
fn load_store_pairs() {
    let mut g = Guest::new();
    g.set_x(1, DATA + 32);
    g.set_x(6, 6);
    g.set_x(7, 7);
    g.step(0xa9bf1c26); // stp x6, x7, [x1, #-16]!
    assert_eq!(g.x(1), DATA + 16);
    assert_eq!((g.get_u64(DATA + 16), g.get_u64(DATA + 24)), (6, 7));

    g.set_x(1, DATA);
    g.step(0xa9411424); // ldp x4, x5, [x1, #16]
    assert_eq!((g.x(4), g.x(5)), (6, 7));

    g.bytes(DATA)[..8].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]);
    g.step(0x29401424); // ldp w4, w5, [x1]
    assert_eq!((g.x(4), g.x(5)), (0xffff_ffff, 1));
    g.step(0x69401424); // ldpsw x4, x5, [x1]
    assert_eq!((g.x(4), g.x(5)), (u64::MAX, 1));
}

#[test]
fn sign_extending_loads() {
    let mut g = Guest::new();
    g.set_x(1, DATA);
    g.bytes(DATA)[..4].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
    g.step(0xb9800028); // ldrsw x8, [x1]
    assert_eq!(g.x(8), 0xffff_ffff_ffff_fff0);
    g.bytes(DATA)[0] = 0x80;
    g.step(0x39c00029); // ldrsb w9, [x1]
    assert_eq!(g.x(9), 0xffff_ff80);
}

#[test]
fn register_offset_and_literal() {
    let mut g = Guest::new();
    g.set_x(1, DATA);
    g.set_x(2, 3);
    g.bytes(DATA)[3] = 0x5a;
    g.step(0x3862682a); // ldrb w10, [x1, x2]
    assert_eq!(g.x(10), 0x5a);
    g.put_u64(DATA + 24, 77);
    g.step(0xf862782b); // ldr x11, [x1, x2, lsl #3]
    assert_eq!(g.x(11), 77);

    let literal = g.pc() + 8;
    g.put_u64(literal, 0x1234);
    g.step(0x5800004c); // ldr x12, .+8
    assert_eq!(g.x(12), 0x1234);
}

#[test]
fn access_crossing_a_page_boundary() {
    let mut g = Guest::new();
    let data: Vec<u8> = (1..=8).collect();
    g.bytes(DATA + PAGE_SIZE - 2)[..2].copy_from_slice(&data[..2]);
    g.bytes(DATA + PAGE_SIZE)[..6].copy_from_slice(&data[2..]);
    g.set_x(1, DATA + PAGE_SIZE + 1);
    g.step(0xf85fd020); // ldur x0, [x1, #-3]
    assert_eq!(g.x(0), u64::from_le_bytes(data.try_into().unwrap()));
}

#[test]
fn faults_have_no_side_effects() {
    let mut g = Guest::new();
    // 第二页未映射：整条指令失败，x0 不变
    g.set_x(0, 0x5555);
    g.set_x(1, DATA + 2 * PAGE_SIZE + 1);
    assert_eq!(
        g.cpu.exec_insn(0xf85fd020), // ldur x0, [x1, #-3]
        Some(Exit::Fault {
            addr: DATA + 2 * PAGE_SIZE,
            access: Access::Read,
            kind: FaultKind::Page
        })
    );
    assert_eq!(g.x(0), 0x5555);
    assert_eq!(g.pc(), CODE);

    // 前变址写失败时基址寄存器不回写
    g.set_x(1, 0x5008);
    assert!(matches!(
        g.cpu.exec_insn(0xf81f8c22), // str x2, [x1, #-8]!
        Some(Exit::Fault {
            addr: 0x5000,
            access: Access::Write,
            ..
        })
    ));
    assert_eq!(g.x(1), 0x5008);

    // 只读页不可写
    g.set_x(1, RODATA);
    assert!(matches!(
        g.cpu.exec_insn(0x79000422), // strh w2, [x1, #2]
        Some(Exit::Fault { addr, access: Access::Write, kind: FaultKind::Page }) if addr == RODATA + 2
    ));
}

#[test]
fn sp_based_access_must_be_aligned() {
    let mut g = Guest::new();
    g.cpu.state_mut().sp = DATA + 8;
    assert!(matches!(
        g.cpu.exec_insn(0xf94003e0), // ldr x0, [sp]
        Some(Exit::Fault {
            access: Access::Read,
            kind: FaultKind::StackAlignment,
            ..
        })
    ));
    assert!(matches!(
        g.cpu.exec_insn(0xf90003e0), // str x0, [sp]
        Some(Exit::Fault {
            access: Access::Write,
            kind: FaultKind::StackAlignment,
            ..
        })
    ));
    g.step(0xf98003e0); // prfm pldl1keep, [sp]：预取不检查对齐
    g.cpu.state_mut().sp = DATA + 16;
    g.put_u64(DATA + 16, 9);
    g.step(0xf94003e0);
    assert_eq!(g.x(0), 9);
}

#[test]
fn data_accesses_ignore_the_top_byte() {
    let mut g = Guest::new();
    g.put_u64(DATA + 8, 99);
    g.set_x(1, (0x5a << 56) | DATA);
    g.step(0xf9400420); // ldr x0, [x1, #8]
    assert_eq!(g.x(0), 99);
}

#[test]
fn unimplemented_encodings_are_undefined() {
    let mut g = Guest::new();
    for insn in [
        0x4e421420, // fadd v0.8h, v1.8h, v2.8h：FP16 不在白名单内
        0x4e829420, // sdot v0.4s, v1.16b, v2.16b：DotProd 不在白名单内
        0xb8bfc020, // ldapr w0, [x1]：LRCPC 不在白名单内
        0x00000000, // udf #0
        0xd51bd060, // msr tpidrro_el0, x0：EL0 不可写
    ] {
        assert_eq!(g.cpu.exec_insn(insn), Some(Exit::Undefined { insn }));
        assert_eq!(g.pc(), CODE);
    }
}

/// SIMD&FP 和原生桩只在 aarch64 Linux 宿主上可用（§4.4 的 HWCAP 会如实反映）。
fn has_simd() -> bool {
    let need = hwcap::FP | hwcap::ASIMD;
    appine_engine::guest_hwcap() & need == need
}

#[test]
fn exclusive_store_succeeds_only_after_a_matching_load() {
    let mut g = Guest::new();
    g.put_u64(DATA, 5);
    g.set_x(1, DATA);
    g.set_x(3, 9);
    g.set_x(2, 7);
    g.step(0xc8027c23); // stxr w2, x3, [x1]：没有独占标记，失败
    assert_eq!((g.x(2), g.get_u64(DATA)), (1, 5));

    g.step(0xc85f7c20); // ldxr x0, [x1]
    assert_eq!(g.x(0), 5);
    g.step(0xc8027c23); // stxr w2, x3, [x1]
    assert_eq!((g.x(2), g.get_u64(DATA)), (0, 9));

    g.step(0xc85f7c20); // ldxr x0, [x1]
    g.step(0xd5033f5f); // clrex
    g.step(0xc8027c23); // stxr w2, x3, [x1]：标记已被清除
    assert_eq!(g.x(2), 1);

    g.set_x(1, DATA + 4); // 未对齐：对齐错误
    assert!(matches!(
        g.cpu.exec_insn(0xc85f7c20),
        Some(Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        })
    ));
}

#[test]
fn simd_and_fp_round_trip() {
    if !has_simd() {
        return;
    }
    let mut g = Guest::new();
    g.set_x(1, 0x4045_0000_0000_0000); // 42.0
    g.step(0x9e670020); // fmov d0, x1
    g.step(0x9e660002); // fmov x2, d0
    assert_eq!(g.x(2), 0x4045_0000_0000_0000);
    assert_eq!(g.cpu.state().v[0], [0x4045_0000_0000_0000, 0]);

    let bytes: Vec<u8> = (0..32).collect();
    g.bytes(DATA)[..32].copy_from_slice(&bytes);
    g.set_x(1, DATA);
    g.step(0x4c40a020); // ld1 {v0.16b, v1.16b}, [x1]
    let v = g.cpu.state().v;
    assert_eq!(v[0][0].to_le_bytes(), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(v[1][1].to_le_bytes(), [24, 25, 26, 27, 28, 29, 30, 31]);
}

#[test]
fn id_registers_match_hwcap() {
    let mut g = Guest::new();
    g.step(0xd5380600); // mrs x0, id_aa64isar0_el1
    let isar0 = g.x(0);
    let caps = appine_engine::guest_hwcap();
    assert_eq!(caps & hwcap::CRC32 != 0, (isar0 >> 16) & 0xf == 1);
    assert_eq!(caps & hwcap::ATOMICS != 0, (isar0 >> 20) & 0xf == 2);
    assert_eq!(caps & hwcap::SHA2 != 0, (isar0 >> 12) & 0xf == 1);
}

#[test]
fn branches() {
    let mut g = Guest::new();
    assert_eq!(g.cpu.exec_insn(0x14000002), None); // b .+8
    assert_eq!(g.pc(), CODE + 8);

    g.cpu.state_mut().pc = CODE;
    assert_eq!(g.cpu.exec_insn(0x94000002), None); // bl .+8
    assert_eq!((g.pc(), g.x(30)), (CODE + 8, CODE + 4));

    g.cpu.state_mut().pc = CODE;
    g.set_x(0, 0);
    assert_eq!(g.cpu.exec_insn(0xb4000040), None); // cbz x0, .+8
    assert_eq!(g.pc(), CODE + 8);
    g.set_x(0, 1);
    g.step(0xb4000040);

    g.set_x(0, 0b1000);
    let pc = g.pc();
    assert_eq!(g.cpu.exec_insn(0x37180040), None); // tbnz w0, #3, .+8
    assert_eq!(g.pc(), pc + 8);

    g.cpu.state_mut().nzcv = 0;
    g.step(0x54000040); // b.eq .+8：Z=0，不跳
    g.cpu.state_mut().nzcv = 1 << 30;
    let pc = g.pc();
    assert_eq!(g.cpu.exec_insn(0x54000040), None);
    assert_eq!(g.pc(), pc + 8);

    g.set_x(30, 0x1234_5670);
    assert_eq!(g.cpu.exec_insn(0xd65f03c0), None); // ret
    assert_eq!(g.pc(), 0x1234_5670);

    g.set_x(1, CODE);
    assert_eq!(g.cpu.exec_insn(0xd63f0020), None); // blr x1
    assert_eq!((g.pc(), g.x(30)), (CODE, 0x1234_5674));
}

#[test]
fn exceptions_hints_and_system_registers() {
    let mut g = Guest::new();
    assert_eq!(g.cpu.exec_insn(0xd4000001), Some(Exit::Svc { imm: 0 })); // svc #0
    assert_eq!(g.pc(), CODE + 4);
    assert_eq!(
        g.cpu.exec_insn(0xd4200020),
        Some(Exit::Breakpoint { imm: 1 })
    ); // brk #1
    assert_eq!(g.pc(), CODE + 4);

    g.step(0xd503201f); // nop
    g.set_x(30, 0x4000_1000);
    g.step(0xd503233f); // paciasp：按设计文档 §4.4 当作 NOP
    assert_eq!(g.x(30), 0x4000_1000);
    g.step(0xd5033bbf); // dmb ish

    g.set_x(1, 0xfeed);
    g.step(0xd51bd041); // msr tpidr_el0, x1
    g.step(0xd53bd040); // mrs x0, tpidr_el0
    assert_eq!(g.x(0), 0xfeed);
    assert_eq!(g.cpu.state().tpidr_el0, 0xfeed);
    g.step(0xd53bd060); // mrs x0, tpidrro_el0
    assert_eq!(g.x(0), 0);

    g.step(0xd53b00e5); // mrs x5, dczid_el0：64 字节块，允许 DC ZVA
    assert_eq!(g.x(5), 4);
    g.bytes(DATA + 0x80)[..0x100].fill(0xaa);
    g.set_x(3, DATA + 0xc5);
    g.step(0xd50b7423); // dc zva, x3
    assert!(g.bytes(DATA + 0xc0)[..64].iter().all(|&b| b == 0));
    assert_eq!(g.bytes(DATA + 0xbf)[0], 0xaa);
    assert_eq!(g.bytes(DATA + 0x100)[0], 0xaa);
    g.set_x(3, RODATA + 8);
    assert_eq!(
        g.cpu.exec_insn(0xd50b7423),
        Some(Exit::Fault {
            addr: RODATA,
            access: Access::Write,
            kind: FaultKind::Page
        })
    );
}

#[test]
fn run_fetches_through_the_tlb() {
    let mut g = Guest::new();
    g.bytes(CODE)[..8].copy_from_slice(&[0x40, 0x05, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4]); // movz x0, #42; svc #0
    assert_eq!(g.cpu.run(), Exit::Svc { imm: 0 });
    assert_eq!(g.x(0), 42);

    // pc 未对齐
    g.cpu.state_mut().pc = CODE + 2;
    assert!(matches!(
        g.cpu.run(),
        Exit::Fault {
            kind: FaultKind::Alignment,
            ..
        }
    ));

    // 数据页不可执行
    g.cpu.state_mut().pc = DATA;
    assert_eq!(
        g.cpu.run(),
        Exit::Fault {
            addr: DATA,
            access: Access::Exec,
            kind: FaultKind::Page
        }
    );

    // 解除映射并刷新 TLB 之后，取指失败
    g.mmu.unmap(CODE, PAGE_SIZE).unwrap();
    g.cpu.tlb_flush();
    g.cpu.state_mut().pc = CODE;
    assert!(matches!(
        g.cpu.run(),
        Exit::Fault {
            addr: CODE,
            access: Access::Exec,
            ..
        }
    ));
}
