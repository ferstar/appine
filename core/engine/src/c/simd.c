/*
 * SIMD&FP（设计文档 §4.2、§4.4）。
 *
 * 数据处理指令：这里只做白名单判定，运算交给原生指令桩。白名单是 ARMv8.0
 * 的基础 FP 和 Advanced SIMD，加上 guest 特性集合里有的 AES/PMULL/SHA1/SHA2/
 * SHA3/SHA512/DotProd/I8MM/FP16/JSCVT；RDM、FHM、FCMA、FRINTTS、BF16、SM3/SM4
 * 等扩展一律拒绝。分组和编码按 ARM ARM 的 "Data Processing -- Scalar
 * Floating-Point and Advanced SIMD" 编码表。
 *
 * 访存指令（LDR/STR/LDP/STP 的 B/H/S/D/Q 形式、LD1–LD4/ST1–ST4）用 C 实现：
 * 先确认要访问的每一页都可访问，再读写，所以失败时不产生任何副作用。
 */
#include "internal.h"

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

/* ---- 原生桩的描述 ---- */

static bool only_v(struct native_desc *d)
{
    *d = (struct native_desc){.fp = 1, .addr_pos = NATIVE_NO_ADDR};
    return true;
}

/* 带一个通用寄存器操作数：pos = 0 表示 Rd（写），pos = 5 表示 Rn（读） */
static bool with_gpr(struct native_desc *d, unsigned pos)
{
    *d = (struct native_desc){.fp = 1, .addr_pos = NATIVE_NO_ADDR, .n = 1};
    d->gpr[0] = (struct native_operand){.pos = (uint8_t)pos, .load = pos != 0, .store = pos == 0};
    return true;
}

/* 半精度的数据处理（FEAT_FP16）：FPHP、ASIMDHP 总是一起宣告（features.c） */
static bool fp16(void)
{
    return appine_has(APPINE_HWCAP_FPHP | APPINE_HWCAP_ASIMDHP);
}

/* FP 指令的 type/size 字段选的精度能不能用：3 是半精度，2 只有 FMOV 的上半部分形式 */
static bool fp_type_ok(unsigned type)
{
    return type <= 1 || (type == 3 && fp16());
}

/* ---- Advanced SIMD：向量 ---- */

static bool vec_three_same(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 15, 11);
    if (opcode >= 0x18) {
        /* FP：size<1> 选择指令，size<0> 是精度；双精度要求 Q = 1 */
        static const uint8_t allowed[2][2] = {{0xdf, 0xc7}, {0xfd, 0x75}};
        if ((size & 1) && !q)
            return false;
        return (allowed[u][size >> 1] >> (opcode - 0x18)) & 1;
    }
    bool q64 = size != 3 || q; /* 64 位元素只能用于 128 位向量 */
    switch (opcode) {
    case 0x00: /* SHADD/UHADD */
    case 0x02: /* SRHADD/URHADD */
    case 0x04: /* SHSUB/UHSUB */
    case 0x0c: /* SMAX/UMAX */
    case 0x0d: /* SMIN/UMIN */
    case 0x0e: /* SABD/UABD */
    case 0x0f: /* SABA/UABA */
    case 0x12: /* MLA/MLS */
    case 0x14: /* SMAXP/UMAXP */
    case 0x15: /* SMINP/UMINP */
        return size != 3;
    case 0x01: /* SQADD/UQADD */
    case 0x05: /* SQSUB/UQSUB */
    case 0x06: /* CMGT/CMHI */
    case 0x07: /* CMGE/CMHS */
    case 0x08: /* SSHL/USHL */
    case 0x09: /* SQSHL/UQSHL */
    case 0x0a: /* SRSHL/URSHL */
    case 0x0b: /* SQRSHL/UQRSHL */
    case 0x10: /* ADD/SUB */
    case 0x11: /* CMTST/CMEQ */
        return q64;
    case 0x03: /* AND/BIC/ORR/ORN、EOR/BSL/BIT/BIF：size 选择具体指令 */
        return true;
    case 0x13: /* MUL / PMUL */
        return u ? size == 0 : size != 3;
    case 0x16: /* SQDMULH/SQRDMULH */
        return size == 1 || size == 2;
    case 0x17: /* ADDP */
        return !u && q64;
    default:
        return false;
    }
}

/* 三寄存器同类型（FP16）：a（bit 23）与 U 一起选指令，表与单、双精度的那组相同 */
static bool vec_three_same_fp16(uint32_t insn)
{
    static const uint8_t allowed[2][2] = {{0xdf, 0xc7}, {0xfd, 0x75}};
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 13, 11);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

/* 两寄存器杂项（FP16）：每一位是 opcode 0x0c–0x1f 里的一条 */
static bool vec_two_reg_misc_fp16(uint32_t insn)
{
    static const uint32_t allowed[2][2] = {
        {0x3f000000, 0x2f00f000}, /* FRINTN/M、FCVTNS/MS/AS、SCVTF；FCM*#0、FABS、FRINTP/Z、FCVTPS/ZS、FRECPE */
        {0x3f000000, 0xae00b000}, /* FRINTA/X、FCVTNU/MU/AU、UCVTF；FCMGE/LE#0、FNEG、FRINTI、FCVTPU/ZU、FRSQRTE、FSQRT */
    };
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 16, 12);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

static bool vec_three_different(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 15, 12);
    switch (opcode) {
    case 0x9: /* SQDMLAL */
    case 0xb: /* SQDMLSL */
    case 0xd: /* SQDMULL */
        return !u && (size == 1 || size == 2);
    case 0xe: /* PMULL：1Q 形式属于 PMULL 扩展 */
        return !u && (size == 0 || (size == 3 && appine_has(APPINE_HWCAP_PMULL)));
    case 0xf:
        return false;
    default: /* ADDL/ADDW/SUBL/SUBW/ADDHN/ABAL/SUBHN/ABDL/MLAL/MLSL/MULL */
        return size != 3;
    }
}

static bool vec_two_reg_misc(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 16, 12), hi = size >> 1;
    bool q64 = size != 3 || q;
    bool fpq = !((size & 1) && !q); /* 双精度向量要求 Q = 1 */
    if (!u) {
        switch (opcode) {
        case 0x00: return size != 3;                /* REV64 */
        case 0x01: return size == 0;                /* REV16 */
        case 0x02: return size != 3;                /* SADDLP */
        case 0x03: return q64;                      /* SUQADD */
        case 0x04: return size != 3;                /* CLS */
        case 0x05: return size == 0;                /* CNT */
        case 0x06: return size != 3;                /* SADALP */
        case 0x07: return q64;                      /* SQABS */
        case 0x08:                                  /* CMGT #0 */
        case 0x09:                                  /* CMEQ #0 */
        case 0x0a:                                  /* CMLT #0 */
        case 0x0b: return q64;                      /* ABS */
        case 0x0c:                                  /* FCMGT #0 */
        case 0x0d:                                  /* FCMEQ #0 */
        case 0x0e:                                  /* FCMLT #0 */
        case 0x0f: return hi && fpq;                /* FABS */
        case 0x12: return size != 3;                /* XTN */
        case 0x14: return size != 3;                /* SQXTN */
        case 0x16:                                  /* FCVTN */
        case 0x17: return !hi;                      /* FCVTL */
        case 0x18:                                  /* FRINTN / FRINTP */
        case 0x19:                                  /* FRINTM / FRINTZ */
        case 0x1a:                                  /* FCVTNS / FCVTPS */
        case 0x1b:                                  /* FCVTMS / FCVTZS */
        case 0x1d: return fpq;                      /* SCVTF / FRECPE */
        case 0x1c: return hi ? size == 2 : fpq;     /* FCVTAS / URECPE */
        default: return false;
        }
    }
    switch (opcode) {
    case 0x00: return size <= 1;                    /* REV32 */
    case 0x02: return size != 3;                    /* UADDLP */
    case 0x03: return q64;                          /* USQADD */
    case 0x04: return size != 3;                    /* CLZ */
    case 0x05: return size <= 1;                    /* NOT / RBIT */
    case 0x06: return size != 3;                    /* UADALP */
    case 0x07: return q64;                          /* SQNEG */
    case 0x08:                                      /* CMGE #0 */
    case 0x09:                                      /* CMLE #0 */
    case 0x0b: return q64;                          /* NEG */
    case 0x0c:                                      /* FCMGE #0 */
    case 0x0d:                                      /* FCMLE #0 */
    case 0x0f: return hi && fpq;                    /* FNEG */
    case 0x12:                                      /* SQXTUN */
    case 0x13:                                      /* SHLL */
    case 0x14: return size != 3;                    /* UQXTN */
    case 0x16: return size == 1;                    /* FCVTXN */
    case 0x18: return !hi && fpq;                   /* FRINTA */
    case 0x19:                                      /* FRINTX / FRINTI */
    case 0x1a:                                      /* FCVTNU / FCVTPU */
    case 0x1b:                                      /* FCVTMU / FCVTZU */
    case 0x1d: return fpq;                          /* UCVTF / FRSQRTE */
    case 0x1c: return hi ? size == 2 : fpq;         /* FCVTAU / URSQRTE */
    case 0x1f: return hi && fpq;                    /* FSQRT */
    default: return false;
    }
}

static bool vec_across_lanes(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned opcode = FIELD(insn, 16, 12);
    if (opcode == 0x0c || opcode == 0x0f) { /* FMAXNMV/FMINNMV、FMAXV/FMINV */
        if (!u) /* 半精度：4H、8H */
            return !(size & 1) && fp16();
        return q && !(size & 1); /* 单精度只有 4S */
    }
    if (size == 3 || (size == 2 && !q))
        return false;
    switch (opcode) {
    case 0x03: /* SADDLV/UADDLV */
    case 0x0a: /* SMAXV/UMAXV */
    case 0x1a: /* SMINV/UMINV */
        return true;
    case 0x1b: /* ADDV */
        return !u;
    default:
        return false;
    }
}

static bool vec_copy(uint32_t insn, struct native_desc *d)
{
    unsigned q = BIT(insn, 30), op = BIT(insn, 29), imm5 = FIELD(insn, 20, 16);
    unsigned imm4 = FIELD(insn, 14, 11);
    if (!(imm5 & 0xf))
        return false;
    unsigned size = (unsigned)__builtin_ctz(imm5);
    if (op) /* INS (element) */
        return q && only_v(d);
    switch (imm4) {
    case 0x0: /* DUP (element) */
        return !(size == 3 && !q) && only_v(d);
    case 0x1: /* DUP (general) */
        return !(size == 3 && !q) && with_gpr(d, 5);
    case 0x3: /* INS (general) */
        return q && with_gpr(d, 5);
    case 0x5: /* SMOV */
        return (q ? size <= 2 : size <= 1) && with_gpr(d, 0);
    case 0x7: /* UMOV */
        return (q ? size == 3 : size <= 2) && with_gpr(d, 0);
    default:
        return false;
    }
}

static bool vec_modified_imm(uint32_t insn)
{
    unsigned q = BIT(insn, 30), op = BIT(insn, 29), cmode = FIELD(insn, 15, 12);
    if (BIT(insn, 11)) /* o2 = 1：半精度 FMOV（FP16） */
        return !op && cmode == 0xf && fp16();
    return !(op && cmode == 0xf && !q); /* 双精度 FMOV 只有 2D */
}

static bool vec_shift_imm(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), immh = FIELD(insn, 22, 19);
    unsigned opcode = FIELD(insn, 15, 11);
    bool d64 = immh & 8;
    switch (opcode) {
    case 0x00: /* SSHR/USHR */
    case 0x02: /* SSRA/USRA */
    case 0x04: /* SRSHR/URSHR */
    case 0x06: /* SRSRA/URSRA */
    case 0x0a: /* SHL/SLI */
    case 0x0e: /* SQSHL/UQSHL */
        return !(d64 && !q);
    case 0x08: /* SRI */
    case 0x0c: /* SQSHLU */
        return u && !(d64 && !q);
    case 0x10: /* SHRN/SQSHRUN */
    case 0x11: /* RSHRN/SQRSHRUN */
    case 0x12: /* SQSHRN/UQSHRN */
    case 0x13: /* SQRSHRN/UQRSHRN */
    case 0x14: /* SSHLL/USHLL */
        return !d64;
    case 0x1c: /* SCVTF/UCVTF（定点） */
    case 0x1f: /* FCVTZS/FCVTZU（定点）：immh = 001x 是半精度 */
        if ((immh & 0xe) == 0x2)
            return fp16();
        return (immh & 0xc) && !(d64 && !q);
    default:
        return false;
    }
}

/* 三寄存器扩展：DotProd 和 I8MM 的几条；RDM、FCMA、BF16 拒绝 */
static bool vec_three_extra(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    if (size != 2)
        return false;
    switch (FIELD(insn, 14, 11)) {
    case 0x2: /* SDOT/UDOT (vector) */
        return appine_has(APPINE_HWCAP_ASIMDDP);
    case 0x3: /* USDOT (vector) */
        return !u && appine_has2(APPINE_HWCAP2_I8MM);
    case 0x4: /* SMMLA/UMMLA：只有 128 位的形式 */
        return q && appine_has2(APPINE_HWCAP2_I8MM);
    case 0x5: /* USMMLA */
        return q && !u && appine_has2(APPINE_HWCAP2_I8MM);
    default:
        return false;
    }
}

static bool vec_indexed(uint32_t insn)
{
    unsigned q = BIT(insn, 30), u = BIT(insn, 29), size = FIELD(insn, 23, 22);
    unsigned l = BIT(insn, 21), opcode = FIELD(insn, 15, 12);
    bool hs = size == 1 || size == 2;
    switch (opcode) {
    case 0x1: /* FMLA */
    case 0x5: /* FMLS */
    case 0x9: /* FMUL / FMULX */
        if (u && opcode != 0x9)
            return false; /* FCMLA（FCMA） */
        if (size == 0) /* 半精度：H:L:M 选元素，Rm 只有 4 位 */
            return fp16();
        return (size & 2) && !((size & 1) && (l || !q));
    case 0x0: /* MLA */
    case 0x4: /* MLS */
        return u && hs;
    case 0x2: /* SMLAL/UMLAL */
    case 0x6: /* SMLSL/UMLSL */
    case 0xa: /* SMULL/UMULL */
        return hs;
    case 0x3: /* SQDMLAL */
    case 0x7: /* SQDMLSL */
    case 0x8: /* MUL */
    case 0xb: /* SQDMULL */
    case 0xc: /* SQDMULH */
    case 0xd: /* SQRDMULH */
        return !u && hs;
    case 0xe: /* SDOT/UDOT (by element) */
        return size == 2 && appine_has(APPINE_HWCAP_ASIMDDP);
    case 0xf: /* USDOT（size 10）、SUDOT（size 00）(by element)；BFDOT、BFMLAL 拒绝 */
        return !u && (size == 0 || size == 2) && appine_has2(APPINE_HWCAP2_I8MM);
    default: /* RDM、FHM 等 */
        return false;
    }
}

/* ---- Advanced SIMD：标量 ---- */

static bool scalar_three_same(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 15, 11);
    if (opcode >= 0x18) {
        static const uint8_t allowed[2][2] = {{0x98, 0x80}, {0x30, 0x34}};
        return (allowed[u][size >> 1] >> (opcode - 0x18)) & 1;
    }
    switch (opcode) {
    case 0x01: /* SQADD/UQADD */
    case 0x05: /* SQSUB/UQSUB */
    case 0x09: /* SQSHL/UQSHL */
    case 0x0b: /* SQRSHL/UQRSHL */
        return true;
    case 0x06: /* CMGT/CMHI */
    case 0x07: /* CMGE/CMHS */
    case 0x08: /* SSHL/USHL */
    case 0x0a: /* SRSHL/URSHL */
    case 0x10: /* ADD/SUB */
    case 0x11: /* CMTST/CMEQ */
        return size == 3;
    case 0x16: /* SQDMULH/SQRDMULH */
        return size == 1 || size == 2;
    default:
        return false;
    }
}

/* 标量三寄存器同类型（FP16）：表与单、双精度的那组相同 */
static bool scalar_three_same_fp16(uint32_t insn)
{
    static const uint8_t allowed[2][2] = {{0x98, 0x80}, {0x30, 0x34}};
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 13, 11);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

/* 标量两寄存器杂项（FP16），位的含义同 vec_two_reg_misc_fp16 */
static bool scalar_two_reg_misc_fp16(uint32_t insn)
{
    static const uint32_t allowed[2][2] = {
        {0x3c000000, 0xac007000}, /* FCVTNS/MS/AS、SCVTF；FCM*#0、FCVTPS/ZS、FRECPE、FRECPX */
        {0x3c000000, 0x2c003000}, /* FCVTNU/MU/AU、UCVTF；FCMGE/LE#0、FCVTPU/ZU、FRSQRTE */
    };
    unsigned u = BIT(insn, 29), a = BIT(insn, 23), opcode = FIELD(insn, 16, 12);
    return fp16() && ((allowed[u][a] >> opcode) & 1);
}

static bool scalar_two_reg_misc(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 16, 12);
    unsigned hi = size >> 1;
    switch (opcode) {
    case 0x03: /* SUQADD/USQADD */
    case 0x07: /* SQABS/SQNEG */
        return true;
    case 0x08: /* CMGT/CMGE #0 */
    case 0x09: /* CMEQ/CMLE #0 */
    case 0x0b: /* ABS/NEG */
        return size == 3;
    case 0x0a: /* CMLT #0 */
        return !u && size == 3;
    case 0x0c: /* FCMGT/FCMGE #0 */
    case 0x0d: /* FCMEQ/FCMLE #0 */
        return hi;
    case 0x0e: /* FCMLT #0 */
        return !u && hi;
    case 0x12: /* SQXTUN */
        return u && size != 3;
    case 0x14: /* SQXTN/UQXTN */
        return size != 3;
    case 0x16: /* FCVTXN */
        return u && size == 1;
    case 0x1a: /* FCVTNS/FCVTNU、FCVTPS/FCVTPU */
    case 0x1b: /* FCVTMS/FCVTMU、FCVTZS/FCVTZU */
    case 0x1d: /* SCVTF/UCVTF、FRECPE/FRSQRTE */
        return true;
    case 0x1c: /* FCVTAS/FCVTAU */
        return !hi;
    case 0x1f: /* FRECPX */
        return !u && hi;
    default:
        return false;
    }
}

static bool scalar_pairwise(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), opcode = FIELD(insn, 16, 12);
    if (!u) { /* ADDP；U = 0 的 FP 形式是半精度（FP16），size 的低位必须是 0 */
        if (opcode == 0x1b)
            return size == 3;
        if (size & 1)
            return false;
        bool ok = opcode == 0x0c || opcode == 0x0f || (opcode == 0x0d && size == 0);
        return ok && fp16();
    }
    switch (opcode) {
    case 0x0c: /* FMAXNMP/FMINNMP */
    case 0x0f: /* FMAXP/FMINP */
        return true;
    case 0x0d: /* FADDP */
        return !(size >> 1);
    default:
        return false;
    }
}

static bool scalar_shift_imm(uint32_t insn)
{
    unsigned u = BIT(insn, 29), immh = FIELD(insn, 22, 19), opcode = FIELD(insn, 15, 11);
    bool d64 = immh & 8;
    if (!immh)
        return false;
    switch (opcode) {
    case 0x00: /* SSHR/USHR */
    case 0x02: /* SSRA/USRA */
    case 0x04: /* SRSHR/URSHR */
    case 0x06: /* SRSRA/URSRA */
    case 0x0a: /* SHL/SLI */
        return d64;
    case 0x08: /* SRI */
        return u && d64;
    case 0x0c: /* SQSHLU */
        return u;
    case 0x0e: /* SQSHL/UQSHL */
        return true;
    case 0x10: /* SQSHRUN */
    case 0x11: /* SQRSHRUN */
        return u && !d64;
    case 0x12: /* SQSHRN/UQSHRN */
    case 0x13: /* SQRSHRN/UQRSHRN */
        return !d64;
    case 0x1c: /* SCVTF/UCVTF（定点） */
    case 0x1f: /* FCVTZS/FCVTZU（定点）：immh = 001x 是半精度 */
        if ((immh & 0xe) == 0x2)
            return fp16();
        return immh & 0xc;
    default:
        return false;
    }
}

static bool scalar_indexed(uint32_t insn)
{
    unsigned u = BIT(insn, 29), size = FIELD(insn, 23, 22), l = BIT(insn, 21);
    unsigned opcode = FIELD(insn, 15, 12);
    switch (opcode) {
    case 0x1: /* FMLA */
    case 0x5: /* FMLS */
    case 0x9: /* FMUL / FMULX */
        if (u && opcode != 0x9)
            return false;
        if (size == 0) /* 半精度 */
            return fp16();
        return (size & 2) && !((size & 1) && l);
    case 0x3: /* SQDMLAL */
    case 0x7: /* SQDMLSL */
    case 0xb: /* SQDMULL */
    case 0xc: /* SQDMULH */
    case 0xd: /* SQRDMULH */
        return !u && (size == 1 || size == 2);
    default:
        return false;
    }
}

static bool crypto_sha3(uint32_t insn)
{
    unsigned opcode = FIELD(insn, 14, 12);
    if (opcode <= 3)
        return appine_has(APPINE_HWCAP_SHA1); /* SHA1C/SHA1P/SHA1M/SHA1SU0 */
    return opcode <= 6 && appine_has(APPINE_HWCAP_SHA2); /* SHA256H/SHA256H2/SHA256SU1 */
}

static bool crypto_sha2(uint32_t insn)
{
    unsigned opcode = FIELD(insn, 16, 12);
    if (opcode <= 1)
        return appine_has(APPINE_HWCAP_SHA1); /* SHA1H/SHA1SU1 */
    return opcode == 2 && appine_has(APPINE_HWCAP_SHA2); /* SHA256SU0 */
}

/* ---- 标量浮点 ---- */

static bool fp_int_conversion(uint32_t insn, struct native_desc *d)
{
    unsigned sf = BIT(insn, 31), type = FIELD(insn, 23, 22), rmode = FIELD(insn, 20, 19);
    unsigned opcode = FIELD(insn, 18, 16);
    if (!sf && type == 1 && rmode == 3 && opcode == 6) /* FJCVTZS（JSCVT）：还写 NZCV */
        return appine_has(APPINE_HWCAP_JSCVT) && with_gpr(d, 0);
    if (opcode == 6 || opcode == 7) { /* FMOV（通用寄存器 ↔ 浮点） */
        bool ok = (rmode == 0 && ((!sf && type == 0) || (sf && type == 1))) ||
                  (rmode == 1 && sf && type == 2) ||
                  (rmode == 0 && type == 3 && fp16()); /* Wn/Xn ↔ Hn */
        return ok && with_gpr(d, opcode == 6 ? 0 : 5);
    }
    if (!fp_type_ok(type))
        return false;
    if (opcode <= 1) /* FCVTNS/FCVTPS/FCVTMS/FCVTZS 及无符号形式 */
        return with_gpr(d, 0);
    if (rmode != 0)
        return false;
    if (opcode <= 3) /* SCVTF/UCVTF */
        return with_gpr(d, 5);
    return with_gpr(d, 0); /* FCVTAS/FCVTAU */
}

static bool fp_one_source(uint32_t insn)
{
    unsigned type = FIELD(insn, 23, 22), opcode = FIELD(insn, 20, 15);
    if (type == 3 && !fp16()) /* 没有 FP16 时半精度源操作数只能做 FCVT */
        return opcode == 0x04 || opcode == 0x05;
    if (type == 2)
        return false;
    if (opcode <= 0x03) /* FMOV/FABS/FNEG/FSQRT */
        return true;
    if ((opcode & 0x3c) == 0x04) { /* FCVT：目标精度不能等于源精度，10 是 BFCVT */
        unsigned to = opcode & 3;
        return to != 2 && to != type;
    }
    return opcode >= 0x08 && opcode <= 0x0f && opcode != 0x0d; /* FRINT 系列 */
}

static bool fp_scalar(uint32_t insn, struct native_desc *d)
{
    unsigned type = FIELD(insn, 23, 22);
    if (BIT(insn, 29))
        return false; /* S = 1 */
    if (BIT(insn, 24)) /* FMADD/FMSUB/FNMADD/FNMSUB */
        return !BIT(insn, 31) && fp_type_ok(type) && only_v(d);
    if (!BIT(insn, 21)) { /* 浮点 ↔ 定点 */
        unsigned rmode = FIELD(insn, 20, 19), opcode = FIELD(insn, 18, 16);
        if (!fp_type_ok(type) || (!BIT(insn, 31) && !BIT(insn, 15)))
            return false;
        if (rmode == 0 && (opcode == 2 || opcode == 3))
            return with_gpr(d, 5);
        if (rmode == 3 && opcode <= 1)
            return with_gpr(d, 0);
        return false;
    }
    if (FIELD(insn, 15, 10) == 0)
        return fp_int_conversion(insn, d);
    if (BIT(insn, 31))
        return false; /* 以下各组 M 必须为 0 */
    if (BIT(insn, 14) && FIELD(insn, 13, 10) == 0) /* 1 源操作数 */
        return fp_one_source(insn) && only_v(d);
    if (!fp_type_ok(type))
        return false; /* 其余各组：单精度、双精度，有 FP16 时还有半精度 */
    switch (FIELD(insn, 11, 10)) {
    case 1: /* FCCMP/FCCMPE */
    case 3: /* FCSEL */
        return only_v(d);
    case 2: /* FMUL/FDIV/FADD/FSUB/FMAX/FMIN/FMAXNM/FMINNM/FNMUL */
        return FIELD(insn, 15, 12) <= 8 && only_v(d);
    default:
        break;
    }
    if (BIT(insn, 12)) /* FMOV（立即数） */
        return FIELD(insn, 9, 5) == 0 && only_v(d);
    if (BIT(insn, 13)) /* FCMP/FCMPE */
        return FIELD(insn, 15, 14) == 0 && FIELD(insn, 2, 0) == 0 && only_v(d);
    return false;
}

/* ---- 顶层 ---- */

/* ---- 1100 开头的加密扩展：SHA3（EOR3、BCAX、RAX1、XAR）、SHA512；SM3、SM4 拒绝 ---- */

static bool crypto_ext(uint32_t insn)
{
    bool sha3 = appine_has(APPINE_HWCAP_SHA3), sha512 = appine_has(APPINE_HWCAP_SHA512);
    if ((insn & 0xffc08000) == 0xce000000) /* EOR3、BCAX（四寄存器，SM3SS1 的 bit 22 是 1） */
        return sha3;
    if ((insn & 0xffe00000) == 0xce800000) /* XAR */
        return sha3;
    if ((insn & 0xffe0f000) == 0xce608000) /* SHA512H、SHA512H2、SHA512SU1；opcode 11 是 RAX1 */
        return FIELD(insn, 11, 10) == 3 ? sha3 : sha512;
    if ((insn & 0xfffffc00) == 0xcec08000) /* SHA512SU0 */
        return sha512;
    return false;
}

bool appine_simd_dp_decode(uint32_t insn, struct native_desc *d)
{
    unsigned op0 = FIELD(insn, 31, 28);
    only_v(d);
    if ((op0 & 0x5) == 0x1) /* x0x1：标量浮点 */
        return fp_scalar(insn, d);
    if ((op0 & 0xd) == 0x5) { /* 01x1：Advanced SIMD 标量，含 SHA */
        if ((insn & 0xffe08c00) == 0x5e000000)
            return crypto_sha3(insn);
        if ((insn & 0xfffe0c00) == 0x5e280800)
            return crypto_sha2(insn);
        if ((insn & 0xdfe08400) == 0x5e000400) /* DUP (element)，标量 */
            return !BIT(insn, 29) && FIELD(insn, 14, 11) == 0 && (FIELD(insn, 19, 16) != 0);
        if ((insn & 0xdf60c400) == 0x5e400400)
            return scalar_three_same_fp16(insn);
        if ((insn & 0xdf7e0c00) == 0x5e780800)
            return scalar_two_reg_misc_fp16(insn);
        if ((insn & 0xdf200400) == 0x5e200400)
            return scalar_three_same(insn);
        if ((insn & 0xdf200c00) == 0x5e200000) /* SQDMLAL/SQDMLSL/SQDMULL */
            return !BIT(insn, 29) && (FIELD(insn, 15, 12) == 0x9 || FIELD(insn, 15, 12) == 0xb ||
                                      FIELD(insn, 15, 12) == 0xd) &&
                   (FIELD(insn, 23, 22) == 1 || FIELD(insn, 23, 22) == 2);
        if ((insn & 0xdf3e0c00) == 0x5e200800)
            return scalar_two_reg_misc(insn);
        if ((insn & 0xdf3e0c00) == 0x5e300800)
            return scalar_pairwise(insn);
        if ((insn & 0xdf800400) == 0x5f000400)
            return scalar_shift_imm(insn);
        if ((insn & 0xdf000400) == 0x5f000000)
            return scalar_indexed(insn);
        return false;
    }
    if ((op0 & 0x9) == 0x0) { /* 0xx0：Advanced SIMD 向量，含 AES */
        if ((insn & 0xfffe0c00) == 0x4e280800) /* AESE/AESD/AESMC/AESIMC */
            return FIELD(insn, 16, 12) >= 4 && FIELD(insn, 16, 12) <= 7 && appine_has(APPINE_HWCAP_AES);
        if ((insn & 0xbfe08c00) == 0x0e000000) /* TBL/TBX */
            return true;
        if ((insn & 0xbf208c00) == 0x0e000800) { /* UZP/TRN/ZIP */
            unsigned opcode = FIELD(insn, 14, 12);
            return (opcode & 3) != 0 && !(FIELD(insn, 23, 22) == 3 && !BIT(insn, 30));
        }
        if ((insn & 0xbfe08400) == 0x2e000000) /* EXT */
            return BIT(insn, 30) || !BIT(insn, 14);
        if ((insn & 0x9fe08400) == 0x0e000400)
            return vec_copy(insn, d);
        if ((insn & 0x9f60c400) == 0x0e400400)
            return vec_three_same_fp16(insn);
        if ((insn & 0x9f7e0c00) == 0x0e780800)
            return vec_two_reg_misc_fp16(insn);
        if ((insn & 0x9f200400) == 0x0e200400)
            return vec_three_same(insn);
        if ((insn & 0x9f208400) == 0x0e008400)
            return vec_three_extra(insn);
        if ((insn & 0x9f200c00) == 0x0e200000)
            return vec_three_different(insn);
        if ((insn & 0x9f3e0c00) == 0x0e200800)
            return vec_two_reg_misc(insn);
        if ((insn & 0x9f3e0c00) == 0x0e300800)
            return vec_across_lanes(insn);
        if ((insn & 0x9ff80400) == 0x0f000400)
            return vec_modified_imm(insn);
        if ((insn & 0x9f800400) == 0x0f000400)
            return vec_shift_imm(insn);
        if ((insn & 0x9f000400) == 0x0f000000)
            return vec_indexed(insn);
        return false;
    }
    if (op0 == 0xc)
        return crypto_ext(insn);
    return false;
}

/* ---- 访存 ---- */

static uint8_t *vreg(struct appine_cpu *c, unsigned r)
{
    return (uint8_t *)c->v[r & 31];
}

static uint64_t gpr(const struct appine_cpu *c, unsigned r)
{
    return r == 31 ? 0 : c->x[r];
}

static uint64_t base_reg(const struct appine_cpu *c, unsigned rn)
{
    return rn == 31 ? c->sp : c->x[rn];
}

static void write_base(struct appine_cpu *c, unsigned rn, uint64_t v)
{
    if (rn == 31)
        c->sp = v;
    else
        c->x[rn] = v;
}

static enum appine_exit undef(struct appine_cpu *c, uint32_t insn)
{
    c->exit_info = insn;
    return APPINE_EXIT_UNDEF;
}

/* 确认 [va, va + len) 的每一页都可以按 access 访问，也没有碰到硬件观察点 */
static bool probe(struct appine_cpu *c, uint64_t va, uint64_t len, uint32_t access)
{
    va = untag(va);
    uint64_t end = va + len;
    for (uint64_t p = va; p < end; p = (p & ~PAGE_MASK) + APPINE_PAGE_SIZE) {
        if (!tlb_lookup(c, p, access))
            return raise_fault(c, p, access, APPINE_FAULT_PAGE);
    }
    uint32_t watch = access == APPINE_ACCESS_READ ? APPINE_WATCH_LOAD : APPINE_WATCH_STORE;
    if (UNLIKELY(watch_hit(c, va, len, watch)))
        return raise_fault(c, va, access, APPINE_FAULT_WATCH);
    return true;
}

/* 以 SP 为基址时要求 SP 16 字节对齐 */
static bool check_sp(struct appine_cpu *c, unsigned rn, uint32_t access)
{
    if (rn == 31 && (c->sp & 15))
        return raise_fault(c, c->sp, access, APPINE_FAULT_SP_ALIGN);
    return true;
}

/* 已经 probe 过的访问，不会失败 */
static void copy_in(struct appine_cpu *c, uint64_t va, void *dst, unsigned n)
{
    (void)mem_read(c, va, dst, n);
}

static void copy_out(struct appine_cpu *c, uint64_t va, const void *src, unsigned n)
{
    (void)mem_write(c, va, src, n);
}

/* LD1–LD4/ST1–ST4（多个结构） */
static enum appine_exit ldst_multiple(struct appine_cpu *c, uint32_t insn)
{
    unsigned q = BIT(insn, 30), load = BIT(insn, 22), post = BIT(insn, 23);
    unsigned rm = FIELD(insn, 20, 16), opcode = FIELD(insn, 15, 12), size = FIELD(insn, 11, 10);
    unsigned rn = FIELD(insn, 9, 5), rt = insn & 31, rpt, selem;
    if (BIT(insn, 21) || (!post && rm != 0))
        return undef(c, insn);
    switch (opcode) {
    case 0x0: rpt = 1, selem = 4; break; /* LD4/ST4 */
    case 0x2: rpt = 4, selem = 1; break; /* LD1/ST1，4 个寄存器 */
    case 0x4: rpt = 1, selem = 3; break; /* LD3/ST3 */
    case 0x6: rpt = 3, selem = 1; break; /* LD1/ST1，3 个寄存器 */
    case 0x7: rpt = 1, selem = 1; break; /* LD1/ST1，1 个寄存器 */
    case 0x8: rpt = 1, selem = 2; break; /* LD2/ST2 */
    case 0xa: rpt = 2, selem = 1; break; /* LD1/ST1，2 个寄存器 */
    default: return undef(c, insn);
    }
    if (size == 3 && !q && selem != 1)
        return undef(c, insn);

    uint32_t access = load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE;
    unsigned esize = 1u << size, elements = (q ? 16u : 8u) >> size;
    uint64_t addr = base_reg(c, rn), total = (uint64_t)rpt * selem * elements * esize;
    if (!check_sp(c, rn, access) || !probe(c, addr, total, access))
        return APPINE_EXIT_FAULT;

    uint8_t regs[4][16] = {{0}};
    uint64_t offs = 0;
    for (unsigned r = 0; r < rpt; r++) {
        for (unsigned e = 0; e < elements; e++) {
            for (unsigned s = 0; s < selem; s++) {
                uint8_t *elem = &regs[r + s][e * esize];
                if (load)
                    copy_in(c, addr + offs, elem, esize);
                else
                    copy_out(c, addr + offs, vreg(c, rt + r + s) + e * esize, esize);
                offs += esize;
            }
        }
    }
    if (load) {
        for (unsigned k = 0; k < rpt * selem; k++)
            memcpy(vreg(c, rt + k), regs[k], 16); /* Q = 0 时高 64 位清零 */
    }
    if (post)
        write_base(c, rn, addr + (rm == 31 ? total : gpr(c, rm)));
    c->pc += 4;
    return APPINE_EXIT_NONE;
}

/* LD1–LD4/ST1–ST4（单个结构）以及 LD1R–LD4R */
static enum appine_exit ldst_single(struct appine_cpu *c, uint32_t insn)
{
    unsigned q = BIT(insn, 30), load = BIT(insn, 22), post = BIT(insn, 23), r = BIT(insn, 21);
    unsigned rm = FIELD(insn, 20, 16), opcode = FIELD(insn, 15, 13), s = BIT(insn, 12);
    unsigned size = FIELD(insn, 11, 10), rn = FIELD(insn, 9, 5), rt = insn & 31;
    unsigned selem = ((opcode & 1) << 1 | r) + 1, scale = opcode >> 1, index = 0;
    bool replicate = false;
    if (!post && rm != 0)
        return undef(c, insn);
    switch (scale) {
    case 0:
        index = q << 3 | s << 2 | size;
        break;
    case 1:
        if (size & 1)
            return undef(c, insn);
        index = q << 2 | s << 1 | size >> 1;
        break;
    case 2:
        if (size & 2)
            return undef(c, insn);
        if (size & 1) {
            if (s)
                return undef(c, insn);
            index = q;
            scale = 3;
        } else {
            index = q << 1 | s;
        }
        break;
    default:
        if (!load || s)
            return undef(c, insn);
        scale = size;
        replicate = true;
        break;
    }

    uint32_t access = load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE;
    unsigned esize = 1u << scale;
    uint64_t addr = base_reg(c, rn), total = (uint64_t)selem * esize;
    if (!check_sp(c, rn, access) || !probe(c, addr, total, access))
        return APPINE_EXIT_FAULT;

    for (unsigned k = 0; k < selem; k++) {
        uint8_t *v = vreg(c, rt + k);
        uint64_t va = addr + (uint64_t)k * esize;
        if (replicate) {
            uint8_t elem[8], value[16] = {0};
            copy_in(c, va, elem, esize);
            for (unsigned i = 0; i < (q ? 16u : 8u); i += esize)
                memcpy(value + i, elem, esize);
            memcpy(v, value, 16);
        } else if (load) {
            copy_in(c, va, v + index * esize, esize);
        } else {
            copy_out(c, va, v + index * esize, esize);
        }
    }
    if (post)
        write_base(c, rn, addr + (rm == 31 ? total : gpr(c, rm)));
    c->pc += 4;
    return APPINE_EXIT_NONE;
}

/* 一个 SIMD&FP 寄存器整体读写 n 字节（n ≤ 16），读入时高位清零 */
static void load_v(struct appine_cpu *c, unsigned rt, uint64_t va, unsigned n)
{
    uint8_t value[16] = {0};
    copy_in(c, va, value, n);
    memcpy(vreg(c, rt), value, 16);
}

enum appine_exit appine_simd_ldst(struct appine_cpu *c, uint32_t insn)
{
    if (!appine_has(APPINE_HWCAP_FP | APPINE_HWCAP_ASIMD))
        return undef(c, insn);
    if ((insn & 0xbf000000) == 0x0c000000)
        return ldst_multiple(c, insn);
    if ((insn & 0xbf000000) == 0x0d000000)
        return ldst_single(c, insn);

    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    unsigned rt = insn & 31, rn = FIELD(insn, 9, 5);

    if ((insn & 0x3f000000) == 0x1c000000) { /* LDR (literal)：bits 31:30 是 opc */
        if (size == 3)
            return undef(c, insn);
        unsigned n = 4u << size;
        uint64_t va = c->pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4);
        if (!probe(c, va, n, APPINE_ACCESS_READ))
            return APPINE_EXIT_FAULT;
        load_v(c, rt, va, n);
        c->pc += 4;
        return APPINE_EXIT_NONE;
    }

    if ((insn & 0x3e000000) == 0x2c000000) { /* LDP/STP/LDNP/STNP */
        unsigned idx = FIELD(insn, 24, 23), load = BIT(insn, 22), rt2 = FIELD(insn, 14, 10);
        if (size == 3)
            return undef(c, insn);
        unsigned n = 4u << size;
        uint32_t access = load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE;
        uint64_t base = base_reg(c, rn);
        int64_t off = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (int64_t)n;
        uint64_t va = idx == 1 ? base : base + (uint64_t)off;
        if (!check_sp(c, rn, access) || !probe(c, va, 2 * n, access))
            return APPINE_EXIT_FAULT;
        if (load) {
            load_v(c, rt, va, n);
            load_v(c, rt2, va + n, n);
        } else {
            copy_out(c, va, vreg(c, rt), n);
            copy_out(c, va + n, vreg(c, rt2), n);
        }
        if (idx == 1 || idx == 3)
            write_base(c, rn, base + (uint64_t)off);
        c->pc += 4;
        return APPINE_EXIT_NONE;
    }

    /* 单个寄存器：size = 00 且 opc = 1x 是 128 位的 Q 寄存器 */
    unsigned scale, load = opc & 1;
    if (size == 0 && (opc & 2))
        scale = 4;
    else if (opc & 2)
        return undef(c, insn);
    else
        scale = size;
    unsigned n = 1u << scale;
    uint32_t access = load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE;
    uint64_t base = base_reg(c, rn), va, wb = 0;
    bool writeback = false;

    if ((insn & 0x3f000000) == 0x3d000000) { /* 无符号偏移 */
        va = base + ((uint64_t)FIELD(insn, 21, 10) << scale);
    } else if ((insn & 0x3f200000) == 0x3c000000) { /* 9 位立即数 */
        unsigned op = FIELD(insn, 11, 10);
        uint64_t off = (uint64_t)((int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55);
        if (op == 2)
            return undef(c, insn); /* 非特权形式没有 SIMD&FP 版本 */
        va = op == 1 ? base : base + off;
        writeback = op != 0;
        wb = base + off;
    } else if ((insn & 0x3f200c00) == 0x3c200800) { /* 寄存器偏移 */
        unsigned option = FIELD(insn, 15, 13), shift = BIT(insn, 12) ? scale : 0;
        uint64_t m = gpr(c, FIELD(insn, 20, 16));
        if (!(option & 2))
            return undef(c, insn);
        switch (option) {
        case 2: m = (uint32_t)m; break;
        case 6: m = (uint64_t)(int64_t)(int32_t)m; break;
        default: break;
        }
        va = base + (m << shift);
    } else {
        return undef(c, insn);
    }

    if (!check_sp(c, rn, access) || !probe(c, va, n, access))
        return APPINE_EXIT_FAULT;
    if (load)
        load_v(c, rt, va, n);
    else
        copy_out(c, va, vreg(c, rt), n);
    if (writeback)
        write_base(c, rn, wb);
    c->pc += 4;
    return APPINE_EXIT_NONE;
}
