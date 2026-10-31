//! 随机测试用例生成器。
//!
//! 每个类别按编码格式随机填充字段，并避开会读写 SP 或改变控制流的编码，因为
//! 这些指令在原生跳板里无法安全执行。访存类用例只访问数据区：生成器给出基址
//! 寄存器相对数据区起点的偏移，由运行器换算成真实地址。PAC/BTI 等 hint 被有意
//! 实现为 NOP（设计文档 §4.4），与硬件不同，所以也不在这里测。

use appine_engine::{hwcap, hwcap2};

use crate::rng::Rng;

/// [`Class::requires`] 里 `AT_HWCAP2` 的位放在高 32 位（`AT_HWCAP` 只用到第 31 位）。
pub const HWCAP2_I8MM: u64 = hwcap2::I8MM << 32;
/// 同上，`HWCAP2_BF16`。
pub const HWCAP2_BF16: u64 = hwcap2::BF16 << 32;
/// 同上，`HWCAP2_FLAGM2`。
pub const HWCAP2_FLAGM2: u64 = hwcap2::FLAGM2 << 32;
/// 同上，`HWCAP2_FRINT`。
pub const HWCAP2_FRINT: u64 = hwcap2::FRINT << 32;
/// 同上，`HWCAP2_DCPODP`。
pub const HWCAP2_DCPODP: u64 = hwcap2::DCPODP << 32;

/// guest 的 `AT_HWCAP` 和左移 32 位的 `AT_HWCAP2`，与 [`Class::requires`] 比较。
pub fn guest_caps() -> u64 {
    appine_engine::guest_hwcap() | appine_engine::guest_hwcap2() << 32
}

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
    /// 只有 guest 特性集合包含这些 HWCAP 位时才运行（HWCAP2 的见 [`HWCAP2_I8MM`]）。
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
    case("same_base", same_base, hwcap::FP | hwcap::ASIMD),
    case("interleaved", interleaved, hwcap::FP | hwcap::ASIMD),
    case("literal", literal, hwcap::FP | hwcap::ASIMD),
    case("pc_page", pc_page, hwcap::FP | hwcap::ASIMD),
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
    case("lrcpc", lrcpc, hwcap::LRCPC),
    case("lrcpc2", lrcpc2, hwcap::ILRCPC),
    case("crc32", crc32, hwcap::CRC32),
    case("dotprod", dotprod, hwcap::ASIMDDP),
    case("sha3_sha512", sha3_sha512, hwcap::SHA3 | hwcap::SHA512),
    case("i8mm", i8mm, HWCAP2_I8MM),
    case("fp16", fp16, hwcap::FPHP | hwcap::ASIMDHP),
    case("jscvt", jscvt, hwcap::JSCVT),
    case("rdm", rdm, hwcap::ASIMDRDM),
    case("fcma", fcma, hwcap::FCMA),
    case("fhm", fhm, hwcap::ASIMDFHM),
    case("bf16", bf16, HWCAP2_BF16),
    case("frint", frint, HWCAP2_FRINT),
    case("flagm", flagm, hwcap::FLAGM),
    case("flagm2", flagm2, HWCAP2_FLAGM2),
    case("sb", sb, hwcap::SB),
    case("dcpop", dcpop, hwcap::DCPOP),
    case("dcpodp", dcpodp, HWCAP2_DCPODP),
    case("dit", dit, hwcap::DIT),
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

/// 以同一个寄存器为基址的 2–4 条访存（Tier 1 让它们共用一次 TLB 检查）：整数和
/// SIMD&FP 的单寄存器、成对访问，带回写，中间可以有按常数加减基址的 ADD/SUB，最后
/// 一条可以是把基址覆盖掉的加载。覆盖的范围可以跨页。
fn same_base(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    /* 基址离数据区两头都足够远：每条指令最多移动基址 512 字节、偏移最多 512 字节 */
    let start = 2560 + g.below(DATA_BYTES as u32 - 2 * 2560 - 16) as i64;
    let mut c = Case {
        pointers: vec![(rn, start)],
        ..Case::default()
    };
    for _ in 0..2 + g.below(3) {
        let rt = reg_or_zr(g, &[rn]);
        let insn = match g.below(6) {
            0 => {
                /* LDR/STR Wt|Xt 无符号偏移；加载时偶尔覆盖基址，之后不再用它 */
                let size = 2 + g.bit();
                let load = g.bit();
                let rt = if load == 1 && g.below(4) == 0 { rn } else { rt };
                let insn = size << 30 | 0x3900_0000 | load << 22 | g.below(32) << 10 | rn << 5 | rt;
                if rt == rn {
                    c.insns.push(insn);
                    break;
                }
                insn
            }
            1 => {
                /* LDUR/STUR、后变址、前变址，各种大小 */
                let op = g.pick(&[0, 1, 3]);
                g.bits(2) << 30
                    | 0x3800_0000
                    | g.bit() << 22
                    | (signed(g, 9) as u32 & 0x1ff) << 12
                    | op << 10
                    | rn << 5
                    | rt
            }
            2 => {
                /* LDP/STP Xt：后变址、有符号偏移、前变址 */
                let load = g.bit();
                let rt2 = if load == 1 {
                    reg_or_zr(g, &[rn, rt])
                } else {
                    reg_or_zr(g, &[rn])
                };
                2 << 30
                    | 0x2800_0000
                    | (1 + g.below(3)) << 23
                    | load << 22
                    | (signed(g, 7) as u32 & 0x7f) << 15
                    | rt2 << 10
                    | rn << 5
                    | rt
            }
            3 => {
                /* LDR/STR Qt 无符号偏移 */
                0x3d80_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | g.bits(5)
            }
            4 => {
                /* LDP/STP Dt 有符号偏移 */
                let load = g.bit();
                let rt = g.bits(5);
                let rt2 = loop {
                    let r = g.bits(5);
                    if load == 0 || r != rt {
                        break r;
                    }
                };
                1 << 30
                    | 0x2d00_0000
                    | load << 22
                    | (signed(g, 7) as u32 & 0x7f) << 15
                    | rt2 << 10
                    | rn << 5
                    | rt
            }
            _ => {
                /* ADD/SUB Xn, Xn, #imm */
                0x9100_0000 | g.bit() << 30 | g.below(64) << 10 | rn << 5 | rn
            }
        };
        c.insns.push(insn);
    }
    c
}

/// 两个基址交错的访存（Tier 1 让同一基址的共用一次检查，中间夹着别的访存时组员从槽里
/// 取回宿主基址），中间还可能夹一条用三个被占用寄存器的运算（翻译时要用 T2）。两个基址
/// 指向的范围可以重叠。
fn interleaved(g: &mut Rng, _: &Env) -> Case {
    let a = reg_except(g, &[]);
    let b = reg_except(g, &[a]);
    let mut start = || 2560 + g.below(DATA_BYTES as u32 - 2 * 2560 - 16) as i64;
    let mut c = Case {
        pointers: vec![(a, start()), (b, start())],
        ..Case::default()
    };
    let pattern = g.pick(&[b"abab", b"abaa", b"aaba", b"axab"]);
    for &p in pattern {
        let insn = match p {
            b'a' => access_on(g, a, &[a, b]),
            b'b' => access_on(g, b, &[a, b]),
            _ => {
                /* ADD Xd, Xn, Xm：三个被占用的寄存器，不写两个基址 */
                let stolen = [16, 17, 18, 26, 27, 28];
                let pick = |g: &mut Rng, used: &[u32]| loop {
                    let r = g.pick(&stolen);
                    if !used.contains(&r) {
                        break r;
                    }
                };
                let rd = pick(g, &[a, b]);
                let rn = pick(g, &[rd]);
                let rm = pick(g, &[rd, rn]);
                0x8b00_0000 | rm << 16 | rn << 5 | rd
            }
        };
        c.insns.push(insn);
    }
    c
}

/// 以 `rn` 为基址的一条访存（same_base 的几种形式），不写 `keep` 里的寄存器（回写基址
/// 除外）。
fn access_on(g: &mut Rng, rn: u32, keep: &[u32]) -> u32 {
    let rt = reg_or_zr(g, keep);
    match g.below(5) {
        0 => {
            let size = 2 + g.bit();
            size << 30 | 0x3900_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | rt
        }
        1 => {
            let op = g.pick(&[0, 1, 3]);
            g.bits(2) << 30
                | 0x3800_0000
                | g.bit() << 22
                | (signed(g, 9) as u32 & 0x1ff) << 12
                | op << 10
                | rn << 5
                | rt
        }
        2 => {
            let load = g.bit();
            let rt2 = loop {
                let r = reg_or_zr(g, keep);
                if load == 0 || r != rt {
                    break r;
                }
            };
            2 << 30
                | 0x2800_0000
                | (1 + g.below(3)) << 23
                | load << 22
                | (signed(g, 7) as u32 & 0x7f) << 15
                | rt2 << 10
                | rn << 5
                | rt
        }
        3 => 0x3d80_0000 | g.bit() << 22 | g.below(32) << 10 | rn << 5 | g.bits(5),
        _ => 0x9100_0000 | g.bit() << 30 | g.below(64) << 10 | rn << 5 | rn,
    }
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

/// 以 ADRP 算出的基址（可能再加一个立即数）读被测指令所在的页：Tier 1 翻译时就知道
/// 地址，表项的位置是常数（dbt.c 的 konst）。中间可能夹一条改掉基址的指令（MOV、或者读
/// 进基址本身），之后的访问不能再当作已知地址。
fn pc_page(g: &mut Rng, env: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let mut c = Case::default();
    c.insns.push(0x9000_0000 | rn); // ADRP Xn, 本页
    let mut base = 0i64;
    if g.bit() == 1 {
        let k = g.below(500) as i64 * 8; // 后面至少还有几个 16 字节的位置
        c.insns.push(0x9100_0000 | (k as u32) << 10 | rn << 5 | rn); // ADD Xn, Xn, #k
        base = k;
    }
    let svc = env.slot_offset as i64 + 16;
    // 访问 n 字节：LDR X/W/H/B、LDP X、LDR Q，偏移按 n 对齐、不超过 `max`（LDP 的立即数
    // 只有 7 位），落在本页、不碰 SVC
    let pick = |g: &mut Rng, base: i64, n: i64, max: i64| loop {
        let off = g.below(((4096 - base - n) / n).min(max / n) as u32 + 1) as i64 * n;
        let t = base + off;
        if t + n <= 4096 && (t + n <= svc || t >= svc + 4) {
            break off;
        }
    };
    let access = |g: &mut Rng, rn: u32, base: i64| -> u32 {
        match g.below(4) {
            0 => {
                let size = g.below(4);
                let n = 1i64 << size;
                let off = pick(g, base, n, 4095 * n);
                (size << 30)
                    | 0x3940_0000
                    | ((off / n) as u32) << 10
                    | rn << 5
                    | reg_or_zr(g, &[rn])
            }
            1 => {
                let off = pick(g, base, 16, 504);
                let t1 = reg_or_zr(g, &[rn]);
                let t2 = reg_or_zr(g, &[rn, t1]);
                0xa940_0000 | ((off / 8) as u32) << 15 | t2 << 10 | rn << 5 | t1
            }
            _ => {
                let off = pick(g, base, 16, 4095 * 16);
                0x3dc0_0000 | ((off / 16) as u32) << 10 | rn << 5 | g.bits(5)
            }
        }
    };
    match g.below(4) {
        0 => {
            // MOV Xn, Xm：基址改指向数据区，不再是已知值
            let rm = reg_except(g, &[rn]);
            c.pointers.push((rm, 0));
            c.insns.push(0xaa00_03e0 | rm << 16 | rn);
            let off = (g.below(512) * 8) as i64;
            c.insns
                .push(0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | g.bits(5));
        }
        1 => {
            // 读进基址本身（LDR Xn, [Xn, #off]）
            let off = pick(g, base, 8, 4095 * 8);
            c.insns
                .push(0xf940_0000 | ((off / 8) as u32) << 10 | rn << 5 | rn);
        }
        _ => {
            c.insns.push(access(g, rn, base));
            if c.insns.len() < 4 && g.bit() == 1 {
                c.insns.push(access(g, rn, base));
            }
        }
    }
    c
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

/// LDAPR、LDAPRB、LDAPRH（32、64 位），基址指向数据区里对齐的地方。
fn lrcpc(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let size = g.bits(2);
    let mut c = Case::default();
    c.insns
        .push((size << 30) | 0x38bf_c000 | (rn << 5) | reg_or_zr(g, &[rn]));
    c.pointers.push((rn, target(g, 1 << size, 1 << size)));
    c
}

/// LRCPC2 的 STLUR*、LDAPUR*、LDAPURS*：基址加 9 位偏移指向数据区里对齐的地方。
fn lrcpc2(g: &mut Rng, _: &Env) -> Case {
    let rn = reg_except(g, &[]);
    let size = g.bits(2);
    let bytes = 1u32 << size;
    // size 11 没有带符号扩展的读，size 10 没有扩展到 32 位的
    let opc = match size {
        3 => g.below(2),
        2 => g.below(3),
        _ => g.below(4),
    };
    let off = (g.below(512 / bytes) as i64 - (256 / bytes) as i64) * bytes as i64;
    let rt = if opc == 0 {
        reg_or_zr(g, &[])
    } else {
        reg_or_zr(g, &[rn])
    };
    let mut c = Case::default();
    c.insns.push(
        (size << 30) | 0x1900_0000 | (opc << 22) | ((off as u32 & 0x1ff) << 12) | (rn << 5) | rt,
    );
    c.pointers
        .push((rn, target(g, bytes.into(), bytes.into()) - off));
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
/// SDOT、UDOT 的向量形式和按元素形式（2S、4S）。
fn dotprod(g: &mut Rng, _: &Env) -> Case {
    let q_u = (g.bit() << 30) | (g.bit() << 29);
    let rm = reg(g);
    let rn_rd = (reg(g) << 5) | reg(g);
    Case::one(if g.bit() == 1 {
        0x0e80_9400 | q_u | (rm << 16) | rn_rd
    } else {
        // 按元素：H:L 选 Vm 的第几个 32 位元素，M:Rm 是完整的寄存器号
        0x0f80_e000 | q_u | (g.bit() << 21) | (rm << 16) | (g.bit() << 11) | rn_rd
    })
}

/// SHA3 的 EOR3、BCAX、XAR、RAX1，SHA512 的 SHA512H、SHA512H2、SHA512SU0、SHA512SU1。
fn sha3_sha512(g: &mut Rng, _: &Env) -> Case {
    let rn_rd = (reg(g) << 5) | reg(g);
    let rm = reg(g) << 16;
    Case::one(match g.bits(3) {
        0 => 0xce00_0000 | rm | (reg(g) << 10) | rn_rd, // EOR3
        1 => 0xce20_0000 | rm | (reg(g) << 10) | rn_rd, // BCAX
        2 => 0xce80_0000 | rm | (g.bits(6) << 10) | rn_rd, // XAR
        3 => 0xce60_8c00 | rm | rn_rd,                  // RAX1
        4 => 0xcec0_8000 | rn_rd,                       // SHA512SU0
        // SHA512H、SHA512H2、SHA512SU1
        k => 0xce60_8000 | ((k - 5) << 10) | rm | rn_rd,
    })
}

/// I8MM：SMMLA、UMMLA、USMMLA（只有 128 位）、USDOT 的向量形式，USDOT、SUDOT 的按元素形式。
fn i8mm(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let rn_rd = (reg(g) << 5) | reg(g);
    let rm = reg(g) << 16;
    let lane = (g.bit() << 21) | (g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x4e80_a400 | rm | rn_rd,                // SMMLA
        1 => 0x6e80_a400 | rm | rn_rd,                // UMMLA
        2 => 0x4e80_ac00 | rm | rn_rd,                // USMMLA
        3 | 4 => 0x0e80_9c00 | q | rm | rn_rd,        // USDOT (vector)
        5 | 6 => 0x0f80_f000 | q | lane | rm | rn_rd, // USDOT (by element)
        _ => 0x0f00_f000 | q | lane | rm | rn_rd,     // SUDOT (by element)
    })
}

/// FP16：向量和标量的三寄存器、两寄存器杂项，按元素，跨元素，标量浮点的各组（算术、
/// 乘加、比较、条件选择、与整数互转、FMOV），向量的 FMOV 立即数。按表只挑有定义的编码。
fn fp16(g: &mut Rng, _: &Env) -> Case {
    // (U, a, opcode) 的位表，与 simd.c 的相同：三寄存器 opcode 0–7，杂项 opcode 0x0c–0x1f
    const THREE: [[u32; 2]; 2] = [[0xdf, 0xc7], [0xfd, 0x75]];
    const SCALAR_THREE: [[u32; 2]; 2] = [[0x98, 0x80], [0x30, 0x34]];
    const MISC: [[u32; 2]; 2] = [[0x3f00_0000, 0x2f00_f000], [0x3f00_0000, 0xae00_b000]];
    const SCALAR_MISC: [[u32; 2]; 2] = [[0x3c00_0000, 0xac00_7000], [0x3c00_0000, 0x2c00_3000]];
    let pick = |g: &mut Rng, table: &[[u32; 2]; 2], width: u32| loop {
        let (u, a, op) = (g.bit(), g.bit(), g.bits(width));
        if (table[u as usize][a as usize] >> op) & 1 == 1 {
            return (u << 29) | (a << 23) | op;
        }
    };
    let q = g.bit() << 30;
    let (rd, rn, rm) = (reg(g), reg(g) << 5, reg(g) << 16);
    Case::one(match g.bits(4) {
        0 | 1 => {
            let f = pick(g, &THREE, 3);
            0x0e40_0400 | q | (f & !7) | ((f & 7) << 11) | rm | rn | rd
        }
        2 => {
            let f = pick(g, &MISC, 5);
            0x0e78_0800 | q | (f & !31) | ((f & 31) << 12) | rn | rd
        }
        3 => {
            let f = pick(g, &SCALAR_THREE, 3);
            0x5e40_0400 | (f & !7) | ((f & 7) << 11) | rm | rn | rd
        }
        4 => {
            let f = pick(g, &SCALAR_MISC, 5);
            0x5e78_0800 | (f & !31) | ((f & 31) << 12) | rn | rd
        }
        // FMLA、FMLS、FMUL、FMULX（按元素）：H:L:M 选元素，Rm 只有 4 位
        5 => {
            let op = [0x0f00_1000, 0x0f00_5000, 0x0f00_9000, 0x2f00_9000][g.bits(2) as usize];
            let lane = (g.bit() << 11) | (g.bit() << 21) | (g.bit() << 20);
            op | q | lane | ((reg(g) & 15) << 16) | rn | rd
        }
        // FMAXNMV、FMAXV、FMINNMV、FMINV（opcode 0x0c、0x0f）
        6 => 0x0e30_c800 | q | (g.bit() << 23) | (g.bit() * 0x3000) | rn | rd,
        // 标量的 FMUL、FDIV、FADD、FSUB、FMAX、FMIN、FMAXNM、FMINNM、FNMUL
        7 | 8 => 0x1ee0_0800 | rm | ((g.next_u64() % 9) as u32) << 12 | rn | rd,
        // FMADD、FMSUB、FNMADD、FNMSUB
        9 => 0x1fc0_0000 | (g.bit() << 21) | rm | (g.bit() << 15) | (reg(g) << 10) | rn | rd,
        // FCMP、FCMPE（和 0 比）、FCCMP、FCSEL
        10 => {
            let opc = g.bits(2);
            0x1ee0_2000 | if opc & 1 == 1 { 0 } else { rm } | rn | (opc << 3)
        }
        11 => 0x1ee0_0400 | rm | (g.bits(4) << 12) | rn | (g.bit() << 4) | g.bits(4),
        12 => 0x1ee0_0c00 | rm | (g.bits(4) << 12) | rn | rd,
        // FMOV、FABS、FNEG、FSQRT、FCVT（到单、双精度）、FRINT*
        13 => {
            let op = [0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 14, 15][(g.next_u64() % 13) as usize];
            0x1ee0_4000 | (op << 15) | rn | rd
        }
        // 与 W/X 互转：FCVT*S/U、SCVTF/UCVTF、FMOV
        14 => {
            let (rmode, op) = [
                (0, 0),
                (1, 0),
                (2, 1),
                (3, 0),
                (0, 2),
                (0, 3),
                (0, 4),
                (0, 5),
                (0, 6),
                (0, 7),
            ][(g.next_u64() % 10) as usize];
            (g.bit() << 31) | 0x1ee0_0000 | (rmode << 19) | (op << 16) | rn | rd
        }
        // 向量的 FMOV 立即数（半精度）
        _ => 0x0f00_fc00 | q | (g.bits(3) << 16) | (g.bits(5) << 5) | rd,
    })
}

/// FJCVTZS：输入是随机的双精度位型（含 NaN、无穷、超出范围的），比对 W 和 NZCV。
fn jscvt(g: &mut Rng, _: &Env) -> Case {
    Case::one(0x1e7e_0000 | (reg(g) << 5) | reg(g))
}

/// RDM：SQRDMLAH、SQRDMLSH 的向量、标量形式和按元素形式（H、S 两种元素）。
fn rdm(g: &mut Rng, _: &Env) -> Case {
    let size = (1 + g.bit()) << 22;
    let sub = g.bit(); // 0 是 SQRDMLAH，1 是 SQRDMLSH
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    // 按元素：H 元素时 H:L:M 选元素、Rm 只有 4 位；S 元素时 H:L 选元素、M:Rm 是寄存器号
    let lane = (g.bit() << 21) | (g.bit() << 20) | ((reg(g) & 15) << 16) | (g.bit() << 11);
    Case::one(match g.bits(2) {
        0 => 0x2e00_8400 | (g.bit() << 30) | size | rm | (sub << 11) | rn_rd,
        1 => 0x7e00_8400 | size | rm | (sub << 11) | rn_rd,
        2 => 0x2f00_d000 | (sub << 13) | (g.bit() << 30) | size | lane | rn_rd,
        _ => 0x7f00_d000 | (sub << 13) | size | lane | rn_rd,
    })
}

/// FCMA：FCMLA（向量、按元素，四种旋转）、FCADD（90、270）。半精度、单精度、双精度（只有 128 位）。
fn fcma(g: &mut Rng, _: &Env) -> Case {
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    let size = 1 + (g.next_u64() % 3) as u32;
    let q = if size == 3 { 1 } else { g.bit() };
    Case::one(match g.bits(2) {
        0 | 1 => 0x2e00_c400 | (q << 30) | (size << 22) | rm | (g.bits(2) << 11) | rn_rd,
        2 => 0x2e00_e400 | (q << 30) | (size << 22) | rm | (g.bit() << 12) | rn_rd,
        // 按元素：半精度按 H:L 选一对（64 位时 H = 0），单精度按 H 选（只有 128 位，L = 0）
        _ => {
            let half = g.bit() == 1;
            let q = if half { g.bit() } else { 1 };
            let h = if half && q == 0 { 0 } else { g.bit() };
            let l = if half { g.bit() } else { 0 };
            let size = if half { 1 } else { 2 };
            0x2f00_1000
                | (q << 30)
                | (size << 22)
                | (l << 21)
                | rm
                | (g.bits(2) << 13)
                | (h << 11)
                | rn_rd
        }
    })
}

/// FHM：FMLAL、FMLSL、FMLAL2、FMLSL2 的向量和按元素形式（H:L:M 选元素，Rm 只有 4 位）。
fn fhm(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let rn_rd = (reg(g) << 5) | reg(g);
    let lane = (g.bit() << 21) | (g.bit() << 20) | ((reg(g) & 15) << 16) | (g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x0e20_ec00 | q | (reg(g) << 16) | rn_rd, // FMLAL
        1 => 0x0ea0_ec00 | q | (reg(g) << 16) | rn_rd, // FMLSL
        2 => 0x2e20_cc00 | q | (reg(g) << 16) | rn_rd, // FMLAL2
        3 => 0x2ea0_cc00 | q | (reg(g) << 16) | rn_rd, // FMLSL2
        4 => 0x0f80_0000 | q | lane | rn_rd,           // FMLAL (by element)
        5 => 0x0f80_4000 | q | lane | rn_rd,           // FMLSL (by element)
        6 => 0x2f80_8000 | q | lane | rn_rd,           // FMLAL2 (by element)
        _ => 0x2f80_c000 | q | lane | rn_rd,           // FMLSL2 (by element)
    })
}

/// BF16：BFDOT、BFMLALB/T（向量和按元素）、BFMMLA、BFCVT、BFCVTN/BFCVTN2。
fn bf16(g: &mut Rng, _: &Env) -> Case {
    let q = g.bit() << 30;
    let (rn_rd, rm) = ((reg(g) << 5) | reg(g), reg(g) << 16);
    // BFDOT 按 H:L 选一对、M:Rm 是寄存器号；BFMLAL 按 H:L:M 选元素、Rm 只有 4 位
    let (l, m, h) = (g.bit() << 21, g.bit() << 20, g.bit() << 11);
    Case::one(match g.bits(3) {
        0 => 0x2e40_fc00 | q | rm | rn_rd,         // BFDOT
        1 => 0x6e40_ec00 | rm | rn_rd,             // BFMMLA
        2 => 0x2ec0_fc00 | q | rm | rn_rd,         // BFMLALB、BFMLALT
        3 => 0x0f40_f000 | q | l | rm | h | rn_rd, // BFDOT (by element)
        4 => 0x0fc0_f000 | q | l | m | (rm & 0xf_0000) | h | rn_rd, // BFMLALB/T (by element)
        5 => 0x1e63_4000 | rn_rd,                  // BFCVT
        _ => 0x0ea1_6800 | q | rn_rd,              // BFCVTN、BFCVTN2
    })
}

/// FRINTTS：FRINT32Z/X、FRINT64Z/X 的标量（单、双精度）和向量形式（2S、4S、2D）。X 的形式按
/// 随机的 FPCR.RMode 舍入。
fn frint(g: &mut Rng, _: &Env) -> Case {
    let (rn_rd, op) = ((reg(g) << 5) | reg(g), g.bits(2));
    Case::one(if g.bit() == 1 {
        0x1e20_4000 | (g.bit() << 22) | ((0x10 | op) << 15) | rn_rd
    } else {
        let sz = g.bit();
        let q = if sz == 1 { 1 } else { g.bit() };
        let (u, opcode) = (op & 1, 0x1e | (op >> 1));
        0x0e20_0800 | (q << 30) | (u << 29) | (sz << 22) | (opcode << 12) | rn_rd
    })
}

/// FlagM：CFINV、RMIF（随机的移位和掩码）、SETF8、SETF16；NZCV 的初值是随机的。
fn flagm(g: &mut Rng, _: &Env) -> Case {
    Case::one(match g.bits(2) {
        0 => 0xd500_401f,
        1 => 0xba00_0400 | (g.bits(6) << 15) | (reg(g) << 5) | g.bits(4),
        _ => 0x3a00_080d | (g.bit() << 14) | (reg(g) << 5),
    })
}

/// FlagM2：XAFLAG、AXFLAG。
fn flagm2(g: &mut Rng, _: &Env) -> Case {
    Case::one(0xd500_401f | ((1 + g.bit()) << 5))
}

/// SB（推测执行屏障）。
fn sb(_: &mut Rng, _: &Env) -> Case {
    Case::one(0xd503_30ff)
}

/// DC CVAP、DC CVADP：地址指向数据区里任意字节。
fn dc_persist(g: &mut Rng, insn: u32) -> Case {
    let rt = reg_except(g, &[]);
    let mut c = Case::one(insn | rt);
    c.pointers.push((rt, target(g, 1, 1)));
    c
}

fn dcpop(g: &mut Rng, _: &Env) -> Case {
    dc_persist(g, 0xd50b_7c20)
}

fn dcpodp(g: &mut Rng, _: &Env) -> Case {
    dc_persist(g, 0xd50b_7d20)
}

/// DIT：先用 MSR DIT, #imm（任意 CRm）或者 MSR DIT, Xn（随机值）设，再用 MRS 读回；原生
/// 一侧上一个用例留下的 DIT 不影响结果。
fn dit(g: &mut Rng, _: &Env) -> Case {
    let (rt, rn) = (reg(g), reg(g));
    let set = if g.bit() == 1 {
        0xd503_405f | (g.bits(4) << 8)
    } else {
        0xd51b_42a0 | rn
    };
    Case {
        insns: vec![set, 0xd53b_42a0 | rt],
        ..Case::default()
    }
}

fn fuzz_simd_fp(g: &mut Rng) -> u32 {
    g.next_u64() as u32 | 0x0e00_0000
}
