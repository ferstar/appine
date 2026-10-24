//! 随机测试用例生成器。
//!
//! 每个类别按编码格式随机填充字段，并避开会读写 SP 或改变控制流的编码，因为
//! 这些指令在原生跳板里无法安全执行。访存类用例只访问数据区：生成器给出基址
//! 寄存器相对数据区起点的偏移，由运行器换算成真实地址。PAC/BTI 等 hint 被有意
//! 实现为 NOP（设计文档 §4.4），与硬件不同，所以也不在这里测。

use appine_engine::hwcap;

use crate::rng::Rng;

/// 数据区大小（两页），与原生执行端一致。
pub const DATA_BYTES: u64 = 8192;

/// 一个测试用例：最多 4 条指令，以及对部分寄存器的初始设定。
#[derive(Debug, Clone, Default)]
pub struct Case {
    /// 依次执行的指令。
    pub insns: Vec<u32>,
    /// 设为"数据区起点 + 偏移"的寄存器。
    pub pointers: Vec<(u32, i64)>,
    /// 设为指定值的寄存器。
    pub values: Vec<(u32, u64)>,
    /// 设为数据区某个偏移处 8 字节内容的寄存器（用来构造会成功的 CAS）。
    pub loads: Vec<(u32, u64)>,
}

impl Case {
    fn one(insn: u32) -> Self {
        Case {
            insns: vec![insn],
            ..Case::default()
        }
    }
}

/// 生成器需要知道的运行环境。
pub struct Env {
    /// 第一条被测指令在它所在 4K 页内的偏移。
    pub slot_offset: u64,
}

/// 用例的生成方式。
#[derive(Clone, Copy)]
pub enum Gen {
    /// 单条不访存的指令。
    Insn(fn(&mut Rng) -> u32),
    /// 完整的用例。
    Case(fn(&mut Rng, &Env) -> Case),
}

/// 一个用例类别。
pub struct Class {
    /// 类别名，可用 `--class` 单独运行。
    pub name: &'static str,
    /// 为 true 时，硬件能执行的编码解释器也必须能执行；为 false（模糊类别）时，
    /// 允许解释器比硬件更严格，把白名单外的编码当作未定义指令。
    pub strict: bool,
    /// 硬件可能偶发地给出不同结果（独占访问会被中断清掉标记），不一致时重跑硬件。
    pub flaky: bool,
    /// 迭代次数是基准次数的几倍。
    pub weight: u64,
    /// 只有 guest 特性集合包含这些 HWCAP 位时才运行。
    pub requires: u64,
    /// 生成方式。
    pub generate: Gen,
}

/// 全部类别。
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
        flaky: false,
        weight: 1,
        requires: 0,
        generate: Gen::Insn(fuzz_dp),
    },
    case("int_ldst", int_ldst, 0),
    case("int_pair", int_pair, 0),
    case("literal", literal, hwcap::FP | hwcap::ASIMD),
    case("simd_ldst", simd_ldst, hwcap::FP | hwcap::ASIMD),
    case("simd_pair", simd_pair, hwcap::FP | hwcap::ASIMD),
    case("simd_struct", simd_struct, hwcap::FP | hwcap::ASIMD),
    case("simd_lane", simd_lane, hwcap::FP | hwcap::ASIMD),
    case("fp_sysreg", fp_sysreg, hwcap::FP),
    Class {
        name: "exclusive",
        strict: true,
        flaky: true,
        weight: 1,
        requires: 0,
        generate: Gen::Case(exclusive),
    },
    case("lse", lse, hwcap::ATOMICS),
    case("crc32", crc32, hwcap::CRC32),
    Class {
        name: "fuzz_simd_fp",
        strict: false,
        flaky: false,
        weight: 4,
        requires: hwcap::FP | hwcap::ASIMD,
        generate: Gen::Insn(fuzz_simd_fp),
    },
];

const fn strict(name: &'static str, generate: fn(&mut Rng) -> u32) -> Class {
    Class {
        name,
        strict: true,
        flaky: false,
        weight: 1,
        requires: 0,
        generate: Gen::Insn(generate),
    }
}

const fn case(name: &'static str, generate: fn(&mut Rng, &Env) -> Case, requires: u64) -> Class {
    Class {
        name,
        strict: true,
        flaky: false,
        weight: 1,
        requires,
        generate: Gen::Case(generate),
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

/// 0–30 中不在 `avoid` 里的寄存器。
fn reg_except(g: &mut Rng, avoid: &[u32]) -> u32 {
    loop {
        let r = g.below(31);
        if !avoid.contains(&r) {
            return r;
        }
    }
}

/// 0–31 中不在 `avoid` 里的寄存器；31 在这些编码里是 ZR。
fn reg_or_zr(g: &mut Rng, avoid: &[u32]) -> u32 {
    loop {
        let r = g.bits(5);
        if !avoid.contains(&r) {
            return r;
        }
    }
}

/// 一次访问 `len` 字节的目标偏移，按 `align` 对齐，保证落在数据区内（可以跨页）。
fn target(g: &mut Rng, len: u64, align: u64) -> i64 {
    let slots = (DATA_BYTES - len) / align + 1;
    (g.below(slots as u32) as u64 * align) as i64
}

fn signed(g: &mut Rng, bits: u32) -> i64 {
    g.bits(bits) as i64 - (1 << (bits - 1))
}

/// 整数 LDR/STR/LDRS*：无符号偏移、非缩放、前变址、后变址、寄存器偏移。
fn int_ldst(g: &mut Rng, _: &Env) -> Case {
    const OPCS: [&[u32]; 4] = [&[0, 1, 2, 3], &[0, 1, 2, 3], &[0, 1, 2], &[0, 1]];
    let size = g.bits(2);
    let opc = g.pick(OPCS[size as usize]);
    let rn = reg_except(g, &[]);
    let rt = reg_or_zr(g, &[rn]);
    single_reg(g, 0x3800_0000, size, opc, size, rn, rt, 1 << size)
}

/// 单寄存器访存的公共部分；`base` 是 V = 0 或 1 的编码基值，`scale` 是 log2 字节数。
#[allow(clippy::too_many_arguments)]
fn single_reg(
    g: &mut Rng,
    base: u32,
    size: u32,
    opc: u32,
    scale: u32,
    rn: u32,
    rt: u32,
    n: u64,
) -> Case {
    let mut c = Case::default();
    let t = target(g, n, 1);
    let fixed = (size << 30) | base | (opc << 22) | (rn << 5) | rt;
    let (insn, start) = match g.below(5) {
        0 => {
            let imm = g.below(64);
            (
                fixed | 0x0100_0000 | (imm << 10),
                t - ((imm as i64) << scale),
            )
        }
        1..=3 => {
            let op = g.pick(&[0, 1, 3]);
            let imm9 = signed(g, 9);
            let start = if op == 1 { t } else { t - imm9 };
            (fixed | ((imm9 as u32 & 0x1ff) << 12) | (op << 10), start)
        }
        _ => {
            let rm = reg_except(g, &[rn]);
            let option = g.pick(&[2, 3, 6, 7]);
            let s = g.bit();
            let index = g.below(32) as u64;
            c.values.push((rm, index));
            let off = (index << (s * scale)) as i64;
            (
                fixed | 0x0020_0800 | (rm << 16) | (option << 13) | (s << 12),
                t - off,
            )
        }
    };
    c.insns.push(insn);
    c.pointers.push((rn, start));
    c
}

/// 整数 LDP/STP/LDPSW/LDNP/STNP。
fn int_pair(g: &mut Rng, _: &Env) -> Case {
    let opc = g.pick(&[0, 1, 2]);
    let load = if opc == 1 { 1 } else { g.bit() };
    let n = if opc == 2 { 8 } else { 4 };
    let idx = g.bits(2); /* opc = 01 且 idx = 00（LDNPSW）是未分配编码，两边都应拒绝 */
    let rn = reg_except(g, &[]);
    let rt = reg_or_zr(g, &[rn]);
    let rt2 = if load == 1 {
        reg_or_zr(g, &[rn, rt])
    } else {
        reg_or_zr(g, &[rn])
    };
    pair(g, 0x2800_0000, opc, load, idx, n, rn, rt, rt2)
}

#[allow(clippy::too_many_arguments)]
fn pair(
    g: &mut Rng,
    base: u32,
    opc: u32,
    load: u32,
    idx: u32,
    n: u64,
    rn: u32,
    rt: u32,
    rt2: u32,
) -> Case {
    let imm7 = signed(g, 7);
    let t = target(g, 2 * n, 1);
    let start = if idx == 1 { t } else { t - imm7 * n as i64 };
    Case {
        insns: vec![
            (opc << 30)
                | base
                | (idx << 23)
                | (load << 22)
                | ((imm7 as u32 & 0x7f) << 15)
                | (rt2 << 10)
                | (rn << 5)
                | rt,
        ],
        pointers: vec![(rn, start)],
        ..Case::default()
    }
}

/// LDR (literal)：整数和 SIMD&FP 两种，目标在被测指令所在的 4K 页内。
/// 解释器一侧在第 5 个字放的是 svc，与硬件不同，目标要避开它。
fn literal(g: &mut Rng, env: &Env) -> Case {
    let simd = g.bit() == 1;
    let opc = g.below(3);
    let n: u64 = if simd {
        4 << opc
    } else {
        [4, 8, 4][opc as usize]
    };
    let svc = env.slot_offset as i64 + 16;
    let target = loop {
        let t = g.below(((4096 - n) / 4 + 1) as u32) as i64 * 4;
        if t + n as i64 <= svc || t >= svc + 4 {
            break t;
        }
    };
    let imm19 = (target - env.slot_offset as i64) / 4;
    let base = if simd { 0x1c00_0000 } else { 0x1800_0000 };
    Case::one((opc << 30) | base | ((imm19 as u32 & 0x7ffff) << 5) | g.bits(5))
}

/// SIMD&FP 单寄存器 LDR/STR（B/H/S/D/Q）。
fn simd_ldst(g: &mut Rng, _: &Env) -> Case {
    const FORMS: [(u32, u32); 10] = [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (1, 0),
        (1, 1),
        (2, 0),
        (2, 1),
        (3, 0),
        (3, 1),
    ];
    let (size, opc) = g.pick(&FORMS);
    let scale = if size == 0 && opc >= 2 { 4 } else { size };
    let rn = reg_except(g, &[]);
    let rt = g.bits(5);
    single_reg(g, 0x3c00_0000, size, opc, scale, rn, rt, 1 << scale)
}

/// SIMD&FP LDP/STP/LDNP/STNP（S/D/Q）。
fn simd_pair(g: &mut Rng, _: &Env) -> Case {
    let opc = g.below(3);
    let load = g.bit();
    let rn = reg_except(g, &[]);
    let rt = g.bits(5);
    let mut rt2 = g.bits(5);
    if load == 1 && rt2 == rt {
        rt2 = (rt + 1) % 32;
    }
    let idx = g.bits(2);
    pair(g, 0x2c00_0000, opc, load, idx, 4 << opc, rn, rt, rt2)
}

/// 后变址形式：Rm = 31 表示按传输的字节数，否则加上一个通用寄存器。
fn post_index(g: &mut Rng, c: &mut Case, rn: u32) -> u32 {
    if g.bit() == 1 {
        return 31;
    }
    let rm = reg_except(g, &[rn]);
    c.values.push((rm, g.next_u64() & 0xfff));
    rm
}

/// LD1–LD4/ST1–ST4（多个结构），含后变址。
fn simd_struct(g: &mut Rng, _: &Env) -> Case {
    const FORMS: [(u32, u64, u64); 7] = [
        (0x0, 1, 4),
        (0x2, 4, 1),
        (0x4, 1, 3),
        (0x6, 3, 1),
        (0x7, 1, 1),
        (0x8, 1, 2),
        (0xa, 2, 1),
    ];
    let (opcode, rpt, selem) = g.pick(&FORMS);
    let q = g.bit();
    let mut size = g.bits(2);
    if size == 3 && q == 0 && selem != 1 {
        size = 2;
    }
    let total = rpt * selem * if q == 1 { 16 } else { 8 };
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    let mut insn =
        (q << 30) | (g.bit() << 22) | (opcode << 12) | (size << 10) | (rn << 5) | g.bits(5);
    if g.bit() == 1 {
        insn |= 0x0c80_0000 | (post_index(g, &mut c, rn) << 16);
    } else {
        insn |= 0x0c00_0000;
    }
    c.insns.push(insn);
    c.pointers.push((rn, target(g, total, 1)));
    c
}

/// LD1–LD4/ST1–ST4（单个结构）和 LD1R–LD4R，含后变址。
fn simd_lane(g: &mut Rng, _: &Env) -> Case {
    let selem = g.below(4) + 1;
    let kind = g.below(5);
    let (q, s, size, scale_bits, esize) = match kind {
        0 => {
            let i = g.bits(4);
            (i >> 3, (i >> 2) & 1, i & 3, 0, 1)
        }
        1 => {
            let i = g.bits(3);
            (i >> 2, (i >> 1) & 1, (i & 1) << 1, 1, 2)
        }
        2 => {
            let i = g.bits(2);
            (i >> 1, i & 1, 0, 2, 4)
        }
        3 => (g.bit(), 0, 1, 2, 8),
        _ => {
            let size = g.bits(2);
            (g.bit(), 0, size, 3, 1 << size)
        }
    };
    let load = if kind == 4 { 1 } else { g.bit() };
    let opcode = (scale_bits << 1) | ((selem - 1) >> 1);
    let r = (selem - 1) & 1;
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    let mut insn = (q << 30)
        | (load << 22)
        | (r << 21)
        | (opcode << 13)
        | (s << 12)
        | (size << 10)
        | (rn << 5)
        | g.bits(5);
    if g.bit() == 1 {
        insn |= 0x0d80_0000 | (post_index(g, &mut c, rn) << 16);
    } else {
        insn |= 0x0d00_0000;
    }
    c.insns.push(insn);
    c.pointers.push((rn, target(g, selem as u64 * esize, 1)));
    c
}

/// MRS/MSR FPCR、FPSR；写入的值只含 ARMv8.0 定义的位。
fn fp_sysreg(g: &mut Rng, _: &Env) -> Case {
    let rt = g.bits(5);
    let mut c = Case::default();
    let insn = match g.below(4) {
        0 => {
            c.values.push((rt, g.next_u64() & 0x07c0_0000));
            0xd51b_4400
        }
        1 => 0xd53b_4400,
        2 => {
            c.values.push((rt, g.next_u64() & 0x0800_009f));
            0xd51b_4420
        }
        _ => 0xd53b_4420,
    };
    c.insns.push(insn | rt);
    c
}

/// 独占访问：LDXR+STXR、LDXP+STXP、CLREX+STXR（必然失败）、LDAR/STLR。
fn exclusive(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let (acquire, release) = (g.bit(), g.bit());
    let mut c = Case::default();
    match g.below(5) {
        0 | 1 => {
            let size = g.bits(2);
            let rt = reg_or_zr(g, &[rn]);
            let ws = reg_except(g, &[rn]);
            let rt2 = reg_or_zr(g, &[rn, ws]);
            c.insns = vec![
                (size << 30) | 0x085f_7c00 | (acquire << 15) | (rn << 5) | rt,
                (size << 30) | 0x0800_7c00 | (ws << 16) | (release << 15) | (rn << 5) | rt2,
            ];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        2 => {
            let size = 2 + g.bit();
            let rt = reg_or_zr(g, &[rn]);
            let rt2 = reg_or_zr(g, &[rn, rt]);
            let ws = reg_except(g, &[rn]);
            let (st1, st2) = (reg_or_zr(g, &[rn, ws]), reg_or_zr(g, &[rn, ws]));
            c.insns = vec![
                (size << 30) | 0x087f_0000 | (acquire << 15) | (rt2 << 10) | (rn << 5) | rt,
                (size << 30)
                    | 0x0820_0000
                    | (ws << 16)
                    | (release << 15)
                    | (st2 << 10)
                    | (rn << 5)
                    | st1,
            ];
            let n = 2u64 << size;
            c.pointers.push((rn, target(g, n, n)));
        }
        3 => {
            let size = g.bits(2);
            let ws = reg_except(g, &[rn]);
            let rt = reg_or_zr(g, &[rn, ws]);
            c.insns = vec![
                0xd503_3f5f, // clrex
                (size << 30) | 0x0800_7c00 | (ws << 16) | (release << 15) | (rn << 5) | rt,
            ];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        _ => {
            let size = g.bits(2);
            let base = if g.bit() == 1 {
                0x08df_fc00
            } else {
                0x089f_fc00
            };
            c.insns = vec![(size << 30) | base | (rn << 5) | reg_or_zr(g, &[rn])];
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
    }
    c
}

/// LSE：LD<op>、SWP、CAS、CASP。CAS 有一半概率让比较值等于内存值，这样会真正写入。
fn lse(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let (a, r) = (g.bit(), g.bit());
    let mut c = Case::default();
    match g.below(4) {
        0 | 1 => {
            let size = g.bits(2);
            let op = g.below(9);
            let (o3, opc) = if op == 8 { (1, 0) } else { (0, op) };
            let rs = reg_or_zr(g, &[rn]);
            let rt = reg_or_zr(g, &[rn]);
            c.insns.push(
                (size << 30)
                    | 0x3820_0000
                    | (a << 23)
                    | (r << 22)
                    | (rs << 16)
                    | (o3 << 15)
                    | (opc << 12)
                    | (rn << 5)
                    | rt,
            );
            c.pointers.push((rn, target(g, 1 << size, 1 << size)));
        }
        2 => {
            let size = g.bits(2);
            let rs = reg_except(g, &[rn]);
            let t = target(g, 1 << size, 1 << size);
            if g.bit() == 1 {
                c.loads.push((rs, t as u64));
            }
            c.insns.push(
                (size << 30)
                    | 0x08a0_7c00
                    | (a << 22)
                    | (rs << 16)
                    | (r << 15)
                    | (rn << 5)
                    | reg_or_zr(g, &[rn]),
            );
            c.pointers.push((rn, t));
        }
        _ => {
            let size = g.bit();
            let esize = 4u64 << size;
            let even = |g: &mut Rng| loop {
                let x = g.below(15) * 2;
                if x != rn && x + 1 != rn {
                    return x;
                }
            };
            let rs = even(g);
            let rt = even(g);
            let t = target(g, 2 * esize, 2 * esize);
            if g.bit() == 1 {
                c.loads.push((rs, t as u64));
                c.loads.push((rs + 1, t as u64 + esize));
            }
            c.insns.push(
                (size << 30) | 0x0820_7c00 | (a << 22) | (rs << 16) | (r << 15) | (rn << 5) | rt,
            );
            c.pointers.push((rn, t));
        }
    }
    c
}

/// CRC32B/H/W/X、CRC32CB/H/W/X。
fn crc32(g: &mut Rng, _: &Env) -> Case {
    let sz = g.bits(2);
    let sf = (sz == 3) as u32;
    Case::one(
        (sf << 31)
            | 0x1ac0_4000
            | (reg(g) << 16)
            | (g.bit() << 12)
            | (sz << 10)
            | (reg(g) << 5)
            | reg(g),
    )
}

/// 整个 SIMD&FP 数据处理编码空间（bits 27:25 = 111）。白名单本身由 LLVM 对照
/// 检查（whitelist.rs），这里检查被执行的指令的语义和寄存器搬运。
fn fuzz_simd_fp(g: &mut Rng) -> u32 {
    g.next_u64() as u32 | 0x0e00_0000
}
