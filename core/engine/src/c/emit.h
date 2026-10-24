/*
 * 宿主 aarch64 指令编码器：Tier 1（dbt.c）和 Tier 0 的原生指令桩（native.c）共用。
 * 每个函数返回一条指令的 32 位编码；立即数的范围和对齐由调用方保证。
 */
#ifndef APPINE_ENGINE_EMIT_H
#define APPINE_ENGINE_EMIT_H

#include <stddef.h>
#include <stdint.h>


static inline uint32_t enc_ldr_x(unsigned rt, unsigned rn, size_t off)
{
    return 0xf9400000u | (uint32_t)(off / 8) << 10 | rn << 5 | rt;
}

static inline uint32_t enc_str_x(unsigned rt, unsigned rn, size_t off)
{
    return 0xf9000000u | (uint32_t)(off / 8) << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_x(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9400000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_x(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9000000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_x_pre(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa9800000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_x_post(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xa8c00000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_mov(unsigned rd, unsigned rm) /* ORR Xd, XZR, Xm */
{
    return 0xaa0003e0u | rm << 16 | rd;
}

static inline uint32_t enc_add_imm(unsigned rd, unsigned rn, uint32_t imm12, unsigned shift12)
{
    return 0x91000000u | shift12 << 22 | imm12 << 10 | rn << 5 | rd;
}

static inline uint32_t enc_sub_imm(unsigned rd, unsigned rn, uint32_t imm12, unsigned shift12)
{
    return 0xd1000000u | shift12 << 22 | imm12 << 10 | rn << 5 | rd;
}

static inline uint32_t enc_movz(unsigned rd, uint32_t imm16, unsigned hw)
{
    return 0xd2800000u | hw << 21 | imm16 << 5 | rd;
}

static inline uint32_t enc_movk(unsigned rd, uint32_t imm16, unsigned hw)
{
    return 0xf2800000u | hw << 21 | imm16 << 5 | rd;
}

/* AND Xd, Xn, #((1 << bits) - 1) */
static inline uint32_t enc_and_low(unsigned rd, unsigned rn, unsigned bits)
{
    return 0x92400000u | (bits - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_ubfx(unsigned rd, unsigned rn, unsigned lsb, unsigned width)
{
    return 0xd3400000u | lsb << 16 | (lsb + width - 1) << 10 | rn << 5 | rd;
}

static inline uint32_t enc_add_lsl(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0x8b000000u | rm << 16 | amount << 10 | rn << 5 | rd;
}

/* EOR Xd, Xn, Xm, LSR #amount */
static inline uint32_t enc_eor_lsr(unsigned rd, unsigned rn, unsigned rm, unsigned amount)
{
    return 0xca000000u | (amount ? 1u << 22 : 0) | rm << 16 | amount << 10 | rn << 5 | rd;
}

static inline uint32_t enc_add_ext(unsigned rd, unsigned rn, unsigned rm, unsigned option, unsigned shift)
{
    return 0x8b200000u | rm << 16 | option << 13 | shift << 10 | rn << 5 | rd;
}

static inline uint32_t enc_ldst_regoff(unsigned size, unsigned opc, unsigned rt, unsigned rn, unsigned rm)
{
    return size << 30 | 0x38206800u | opc << 22 | rm << 16 | rn << 5 | rt;
}

static inline uint32_t enc_b(intptr_t off)
{
    return 0x14000000u | ((uint32_t)(off >> 2) & 0x3ffffff);
}

static inline uint32_t enc_br(unsigned rn)
{
    return 0xd61f0000u | rn << 5;
}

static inline uint32_t enc_cbnz_x(unsigned rt)
{
    return 0xb5000000u | rt;
}

static inline uint32_t enc_tbnz(unsigned rt, unsigned bit)
{
    return 0x37000000u | (bit >> 5) << 31 | (bit & 31) << 19 | rt;
}

static inline uint32_t enc_adr_self(unsigned rd)
{
    return 0x10000000u | rd;
}

/* ---- SIMD&FP 与系统寄存器 ---- */

/* LDP/STP Qt1, Qt2, [Xn, #off]（off 为 16 的倍数，范围 ±1008） */
static inline uint32_t enc_ldp_q(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xad400000u | ((uint32_t)(off / 16) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_q(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0xad000000u | ((uint32_t)(off / 16) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

/* LDP/STP Dt1, Dt2：保存和恢复宿主的被调用者保存寄存器 d8–d15 */
static inline uint32_t enc_stp_d_pre(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d800000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_stp_d(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d000000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_d(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6d400000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_ldp_d_post(unsigned rt, unsigned rt2, unsigned rn, int off)
{
    return 0x6cc00000u | ((uint32_t)(off / 8) & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt;
}

static inline uint32_t enc_mrs_nzcv(unsigned rt)
{
    return 0xd53b4200u | rt;
}

static inline uint32_t enc_msr_nzcv(unsigned rt)
{
    return 0xd51b4200u | rt;
}

static inline uint32_t enc_mrs_fpcr(unsigned rt)
{
    return 0xd53b4400u | rt;
}

static inline uint32_t enc_msr_fpcr(unsigned rt)
{
    return 0xd51b4400u | rt;
}

static inline uint32_t enc_mrs_fpsr(unsigned rt)
{
    return 0xd53b4420u | rt;
}

static inline uint32_t enc_msr_fpsr(unsigned rt)
{
    return 0xd51b4420u | rt;
}

static inline uint32_t enc_ret(void)
{
    return 0xd65f03c0u;
}

#endif /* APPINE_ENGINE_EMIT_H */
