/*
 * guest 可见的 CPU 特性（设计文档 §4.4）：宿主支持 ∩ Tier 0 已实现 ∩ 白名单。
 * HWCAP、ID 寄存器和 /proc/cpuinfo 都从这同一份集合生成，解码器
 * 也按它决定 AES/SHA/CRC32/LSE/DotProd/I8MM/LRCPC/LRCPC2/FP16/JSCVT/RDM/FHM/FCMA/BF16
 * 等可选指令是否合法。
 */
#include <pthread.h>

#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#include <sys/auxv.h>
#define HOST_IS_AARCH64_LINUX 1
#endif

/* Tier 0 实现了的特性。FP、ASIMD 和这些可选扩展都依赖原生指令桩 */
#define IMPLEMENTED                                                                                \
    (APPINE_HWCAP_FP | APPINE_HWCAP_ASIMD | APPINE_HWCAP_AES | APPINE_HWCAP_PMULL |                \
     APPINE_HWCAP_SHA1 | APPINE_HWCAP_SHA2 | APPINE_HWCAP_CRC32 | APPINE_HWCAP_ATOMICS |          \
     APPINE_HWCAP_SHA3 | APPINE_HWCAP_ASIMDDP | APPINE_HWCAP_SHA512 | APPINE_HWCAP_LRCPC |         \
     APPINE_HWCAP_ILRCPC | APPINE_HWCAP_FPHP | APPINE_HWCAP_ASIMDHP | APPINE_HWCAP_JSCVT |       \
     APPINE_HWCAP_ASIMDRDM | APPINE_HWCAP_FCMA | APPINE_HWCAP_ASIMDFHM)
#define IMPLEMENTED2 (APPINE_HWCAP2_I8MM | APPINE_HWCAP2_BF16)

/* SHA3、SHA512 只一起宣告，还要有 SHA2：LLVM 的 sha3 特性同时管这两组，白名单的核对分不开，
 * 实际的 CPU 也总是一起实现 */
#define SHA3_SET (APPINE_HWCAP_SHA2 | APPINE_HWCAP_SHA3 | APPINE_HWCAP_SHA512)

/* 标量和向量的半精度只一起宣告：白名单按一个开关判定，LLVM 的 fullfp16 也是一个特性 */
#define FP16_SET (APPINE_HWCAP_FPHP | APPINE_HWCAP_ASIMDHP)

static pthread_once_t once = PTHREAD_ONCE_INIT;
static uint64_t guest_hwcap, guest_hwcap2;
static uint64_t host_midr;

static void detect(void)
{
    /* ID 寄存器由我们自己模拟（§4.4），与宿主无关 */
    guest_hwcap = APPINE_HWCAP_CPUID;
#ifdef HOST_IS_AARCH64_LINUX
    uint64_t host = getauxval(AT_HWCAP);
    /* 没有 FP 和 ASIMD 的宿主不支持任何 SIMD&FP 指令；原生桩不可用时同样如此 */
    if ((host & APPINE_HWCAP_FP) && (host & APPINE_HWCAP_ASIMD) && appine_native_available()) {
        guest_hwcap |= host & IMPLEMENTED;
        if ((guest_hwcap & SHA3_SET) != SHA3_SET)
            guest_hwcap &= ~(APPINE_HWCAP_SHA3 | APPINE_HWCAP_SHA512);
        if ((guest_hwcap & FP16_SET) != FP16_SET)
            guest_hwcap &= ~FP16_SET;
        /* FHM 的源操作数是半精度，架构上要求有 FP16（LLVM 的 fp16fml 也隐含 fullfp16） */
        if (!(guest_hwcap & APPINE_HWCAP_FPHP))
            guest_hwcap &= ~APPINE_HWCAP_ASIMDFHM;
        /* LRCPC2 包含 LRCPC（ID 寄存器里是同一个字段） */
        if (!(guest_hwcap & APPINE_HWCAP_LRCPC))
            guest_hwcap &= ~APPINE_HWCAP_ILRCPC;
        guest_hwcap2 = getauxval(AT_HWCAP2) & IMPLEMENTED2;
    }
    if (host & APPINE_HWCAP_CPUID)
        __asm__ volatile("mrs %0, midr_el1" : "=r"(host_midr));
#endif
}

uint64_t appine_guest_hwcap(void)
{
    pthread_once(&once, detect);
    return guest_hwcap;
}

uint64_t appine_guest_hwcap2(void)
{
    pthread_once(&once, detect);
    return guest_hwcap2;
}

uint64_t appine_guest_midr(void)
{
    pthread_once(&once, detect);
    return host_midr;
}

bool appine_has(uint64_t hwcap)
{
    return (appine_guest_hwcap() & hwcap) == hwcap;
}

bool appine_has2(uint64_t hwcap2)
{
    return (appine_guest_hwcap2() & hwcap2) == hwcap2;
}

/*
 * EL0 读取 ID 寄存器空间（op0=3, op1=0, CRn=0）时返回的值，与 Linux 在用户态
 * 模拟的行为一致：MIDR 原样返回，MPIDR 只有 bit 31，其余只暴露与 HWCAP 对应
 * 的字段。key 是 MRS 编码的 bits 20:5。
 */
bool appine_id_register(unsigned key, uint64_t *value)
{
    unsigned op0 = key >> 14, op1 = (key >> 11) & 7, crn = (key >> 7) & 15;
    unsigned crm = (key >> 3) & 15, op2 = key & 7;
    if (op0 != 3 || op1 != 0 || crn != 0 || crm > 7)
        return false;
    uint64_t h = appine_guest_hwcap(), h2 = appine_guest_hwcap2();
    uint64_t v = 0;
    if (crm == 0) {
        if (op2 == 0)
            v = host_midr;
        else if (op2 == 5)
            v = UINT64_C(1) << 31; /* MPIDR_EL1 */
    } else if (crm == 4 && op2 == 0) { /* ID_AA64PFR0_EL1 */
        v = 1 | 1 << 4;                /* EL0、EL1 只支持 AArch64 */
        /* FP、AdvSIMD：0 是基础集，1 是带半精度，f 是没有 */
        if (!(h & APPINE_HWCAP_FP))
            v |= UINT64_C(0xf) << 16;
        else if (h & APPINE_HWCAP_FPHP)
            v |= UINT64_C(1) << 16;
        if (!(h & APPINE_HWCAP_ASIMD))
            v |= UINT64_C(0xf) << 20;
        else if (h & APPINE_HWCAP_ASIMDHP)
            v |= UINT64_C(1) << 20;
    } else if (crm == 6 && op2 == 0) { /* ID_AA64ISAR0_EL1 */
        if (h & APPINE_HWCAP_PMULL)
            v |= UINT64_C(2) << 4;
        else if (h & APPINE_HWCAP_AES)
            v |= UINT64_C(1) << 4;
        if (h & APPINE_HWCAP_SHA1)
            v |= UINT64_C(1) << 8;
        if (h & APPINE_HWCAP_SHA512)
            v |= UINT64_C(2) << 12;
        else if (h & APPINE_HWCAP_SHA2)
            v |= UINT64_C(1) << 12;
        if (h & APPINE_HWCAP_CRC32)
            v |= UINT64_C(1) << 16;
        if (h & APPINE_HWCAP_ATOMICS)
            v |= UINT64_C(2) << 20;
        if (h & APPINE_HWCAP_ASIMDRDM)
            v |= UINT64_C(1) << 28;
        if (h & APPINE_HWCAP_SHA3)
            v |= UINT64_C(1) << 32;
        if (h & APPINE_HWCAP_ASIMDDP)
            v |= UINT64_C(1) << 44;
        if (h & APPINE_HWCAP_ASIMDFHM)
            v |= UINT64_C(1) << 48;
    } else if (crm == 6 && op2 == 1) { /* ID_AA64ISAR1_EL1 */
        if (h & APPINE_HWCAP_JSCVT)
            v |= UINT64_C(1) << 12;
        if (h & APPINE_HWCAP_FCMA)
            v |= UINT64_C(1) << 16;
        if (h & APPINE_HWCAP_ILRCPC)
            v |= UINT64_C(2) << 20;
        else if (h & APPINE_HWCAP_LRCPC)
            v |= UINT64_C(1) << 20;
        if (h2 & APPINE_HWCAP2_BF16)
            v |= UINT64_C(1) << 44;
        if (h2 & APPINE_HWCAP2_I8MM)
            v |= UINT64_C(1) << 52;
    }
    *value = v;
    return true;
}
