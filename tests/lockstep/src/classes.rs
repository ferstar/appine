//! 随机指令生成器。
//!
//! 每个类别按编码格式随机填充字段，并避开会读写 SP、访问内存或改变控制流的
//! 编码，因为这些指令在原生跳板里无法安全执行。PAC/BTI 等 hint 被有意实现为
//! NOP（设计文档 §4.4），与硬件不同，所以也不在这里测。

use crate::rng::Rng;

/// 一个指令类别。
pub struct Class {
    /// 类别名，可用 `--class` 单独运行。
    pub name: &'static str,
    /// 为 true 时，硬件能执行的编码解释器也必须能执行；为 false（模糊类别）时，
    /// 允许解释器比硬件更严格，把白名单外的编码当作未定义指令。
    pub strict: bool,
    /// 生成一条指令编码。
    pub generate: fn(&mut Rng) -> u32,
}

/// 全部指令类别。
pub const CLASSES: &[Class] = &[
    strict("pc_rel", pc_rel),
    strict("add_sub_imm", add_sub_imm),
    strict("logical_imm", logical_imm),
    strict("move_wide", move_wide),
    strict("bitfield", bitfield),
    strict("extract", extract),
    strict("logical_shifted", logical_shifted),
    strict("add_sub_shifted", add_sub_shifted),
    strict("add_sub_extended", add_sub_extended),
    strict("adc_sbc", adc_sbc),
    strict("cond_compare", cond_compare),
    strict("cond_select", cond_select),
    strict("dp_2src", dp_2src),
    strict("dp_1src", dp_1src),
    strict("dp_3src", dp_3src),
    strict("nzcv_move", nzcv_move),
    Class {
        name: "fuzz_dp",
        strict: false,
        generate: fuzz_dp,
    },
];

const fn strict(name: &'static str, generate: fn(&mut Rng) -> u32) -> Class {
    Class {
        name,
        strict: true,
        generate,
    }
}

/// 任意寄存器编号；31 在这些编码里是 XZR。
fn reg(g: &mut Rng) -> u32 {
    g.bits(5)
}

/// 不是 31 的寄存器编号，用于 31 表示 SP 的字段。
fn reg_not_sp(g: &mut Rng) -> u32 {
    g.below(31)
}

fn imm_width(sf: u32) -> u32 {
    if sf == 1 { 6 } else { 5 }
}

// op immlo 10000 immhi Rd
fn pc_rel(g: &mut Rng) -> u32 {
    (g.bit() << 31) | (g.bits(2) << 29) | (0b10000 << 24) | (g.bits(19) << 5) | reg(g)
}

// sf op S 100010 sh imm12 Rn Rd
fn add_sub_imm(g: &mut Rng) -> u32 {
    let s = g.bit();
    let rd = if s == 1 { reg(g) } else { reg_not_sp(g) };
    (g.bit() << 31)
        | (g.bit() << 30)
        | (s << 29)
        | (0b100010 << 23)
        | (g.bit() << 22)
        | (g.bits(12) << 10)
        | (reg_not_sp(g) << 5)
        | rd
}

// sf opc 100100 N immr imms Rn Rd
fn logical_imm(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let opc = g.bits(2);
    let n = if sf == 1 { g.bit() } else { 0 };
    let rd = if opc == 3 { reg(g) } else { reg_not_sp(g) };
    (sf << 31)
        | (opc << 29)
        | (0b100100 << 23)
        | (n << 22)
        | (g.bits(6) << 16)
        | (g.bits(6) << 10)
        | (reg(g) << 5)
        | rd
}

// sf opc 100101 hw imm16 Rd
fn move_wide(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let hw = if sf == 1 { g.bits(2) } else { g.bit() };
    (sf << 31)
        | (g.pick(&[0, 2, 3]) << 29)
        | (0b100101 << 23)
        | (hw << 21)
        | (g.bits(16) << 5)
        | reg(g)
}

// sf opc 100110 N immr imms Rn Rd
fn bitfield(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let w = imm_width(sf);
    (sf << 31)
        | (g.pick(&[0, 1, 2]) << 29)
        | (0b100110 << 23)
        | (sf << 22)
        | (g.bits(w) << 16)
        | (g.bits(w) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 00 100111 N 0 Rm imms Rn Rd
fn extract(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (0b100111 << 23)
        | (sf << 22)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf opc 01010 shift N Rm imm6 Rn Rd
fn logical_shifted(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (g.bits(2) << 29)
        | (0b01010 << 24)
        | (g.bits(2) << 22)
        | (g.bit() << 21)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf op S 01011 shift 0 Rm imm6 Rn Rd
fn add_sub_shifted(g: &mut Rng) -> u32 {
    let sf = g.bit();
    (sf << 31)
        | (g.bit() << 30)
        | (g.bit() << 29)
        | (0b01011 << 24)
        | (g.below(3) << 22)
        | (reg(g) << 16)
        | (g.bits(imm_width(sf)) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf op S 01011 00 1 Rm option imm3 Rn Rd
fn add_sub_extended(g: &mut Rng) -> u32 {
    let s = g.bit();
    let rd = if s == 1 { reg(g) } else { reg_not_sp(g) };
    (g.bit() << 31)
        | (g.bit() << 30)
        | (s << 29)
        | (0b01011 << 24)
        | (1 << 21)
        | (reg(g) << 16)
        | (g.bits(3) << 13)
        | (g.below(5) << 10)
        | (reg_not_sp(g) << 5)
        | rd
}

// sf op S 11010000 Rm 000000 Rn Rd
fn adc_sbc(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (g.bit() << 29)
        | (0b11010000 << 21)
        | (reg(g) << 16)
        | (reg(g) << 5)
        | reg(g)
}

// sf op 1 11010010 Rm|imm5 cond imm 0 Rn 0 nzcv
fn cond_compare(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (1 << 29)
        | (0b11010010 << 21)
        | (g.bits(5) << 16)
        | (g.bits(4) << 12)
        | (g.bit() << 11)
        | (reg(g) << 5)
        | g.bits(4)
}

// sf op 0 11010100 Rm cond 0 o2 Rn Rd
fn cond_select(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (g.bit() << 30)
        | (0b11010100 << 21)
        | (reg(g) << 16)
        | (g.bits(4) << 12)
        | (g.bit() << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 0 0 11010110 Rm opcode Rn Rd：UDIV/SDIV/LSLV/LSRV/ASRV/RORV
fn dp_2src(g: &mut Rng) -> u32 {
    (g.bit() << 31)
        | (0b11010110 << 21)
        | (reg(g) << 16)
        | (g.pick(&[2, 3, 8, 9, 10, 11]) << 10)
        | (reg(g) << 5)
        | reg(g)
}

// sf 1 0 11010110 00000 opcode Rn Rd：RBIT/REV16/REV32/REV/CLZ/CLS
fn dp_1src(g: &mut Rng) -> u32 {
    let sf = g.bit();
    let opcode = if sf == 1 {
        g.pick(&[0, 1, 2, 3, 4, 5])
    } else {
        g.pick(&[0, 1, 2, 4, 5])
    };
    (sf << 31) | (1 << 30) | (0b11010110 << 21) | (opcode << 10) | (reg(g) << 5) | reg(g)
}

// sf 00 11011 op31 Rm o0 Ra Rn Rd
fn dp_3src(g: &mut Rng) -> u32 {
    // (只允许 64 位, op31, o0, Ra 必须是 31)
    const FORMS: &[(bool, u32, u32, bool)] = &[
        (false, 0, 0, false), // MADD
        (false, 0, 1, false), // MSUB
        (true, 1, 0, false),  // SMADDL
        (true, 1, 1, false),  // SMSUBL
        (true, 2, 0, true),   // SMULH
        (true, 5, 0, false),  // UMADDL
        (true, 5, 1, false),  // UMSUBL
        (true, 6, 0, true),   // UMULH
    ];
    let (only64, op31, o0, ra_is_31) = g.pick(FORMS);
    let sf = if only64 { 1 } else { g.bit() };
    let ra = if ra_is_31 { 31 } else { reg(g) };
    (sf << 31)
        | (0b11011 << 24)
        | (op31 << 21)
        | (reg(g) << 16)
        | (o0 << 15)
        | (ra << 10)
        | (reg(g) << 5)
        | reg(g)
}

// MRS Xt, NZCV / MSR NZCV, Xt
fn nzcv_move(g: &mut Rng) -> u32 {
    let base = if g.bit() == 1 {
        0xd53b_4200
    } else {
        0xd51b_4200
    };
    base | reg(g)
}

/// 在整个数据处理（立即数、寄存器）编码空间里随机取样，只排除 Rd/Rn 为 31 的编码
/// ——这两个字段在部分指令里表示 SP。
fn fuzz_dp(g: &mut Rng) -> u32 {
    let mut insn = g.next_u64() as u32;
    insn = if g.bit() == 1 {
        (insn & !(0b111 << 26)) | (0b100 << 26) // bits 28:26 = 100：立即数
    } else {
        (insn & !(0b111 << 25)) | (0b101 << 25) // bits 27:25 = 101：寄存器
    };
    let rd = reg_not_sp(g);
    let rn = reg_not_sp(g);
    (insn & !0x3ff) | (rn << 5) | rd
}
