/*
 * 独占访问、acquire/release 与 LSE 原子操作（设计文档 §4.5）。
 *
 * LDXR/STXR 不能交给宿主原样执行：两条指令之间我们自己的代码会访问内存，
 * 宿主的独占监视器随时可能被清掉，导致 STXR 永远失败。这里改用"比较并交换"
 * 模拟：LDXR 记下地址、长度和读到的值，STXR 在宿主地址上做 CAS，只有内存值
 * 仍等于记下的值才成功（与 QEMU 的做法相同，允许 ABA）。
 *
 * LSE 原子指令（LDADD、SWP、CAS、CASP 等）本身就是单条指令，交给原生桩在
 * 翻译后的宿主地址上执行，语义和内存序与硬件完全一致。
 */
#include "internal.h"

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

static enum appine_exit undef(struct appine_cpu *c, uint32_t insn)
{
    c->exit_info = insn;
    return APPINE_EXIT_UNDEF;
}

static enum appine_exit next(struct appine_cpu *c)
{
    c->pc += 4;
    return APPINE_EXIT_NONE;
}

static void set_x(struct appine_cpu *c, unsigned r, uint64_t v)
{
    if (r != 31)
        c->x[r] = v;
}

static uint64_t get_x(const struct appine_cpu *c, unsigned r)
{
    return r == 31 ? 0 : c->x[r];
}

/*
 * 取得一次对齐访问的宿主地址。这类指令要求自然对齐，否则是对齐错误；对齐的
 * 访问不会跨页。access 为写时同时要求可读。watch 是这次访问算作哪种硬件观察点
 * （APPINE_WATCH_*，读改写两种都算）。
 */
static void *atomic_target(struct appine_cpu *c, unsigned rn, unsigned bytes, uint32_t access,
                           uint32_t watch)
{
    uint64_t va = rn == 31 ? c->sp : c->x[rn];
    if (rn == 31 && (c->sp & 15)) {
        raise_fault(c, c->sp, access, APPINE_FAULT_SP_ALIGN);
        return NULL;
    }
    va = untag(va);
    if (va & (bytes - 1)) {
        raise_fault(c, va, access, APPINE_FAULT_ALIGN);
        return NULL;
    }
    if (access == APPINE_ACCESS_WRITE && !tlb_lookup(c, va, APPINE_ACCESS_READ)) {
        raise_fault(c, va, APPINE_ACCESS_READ, APPINE_FAULT_PAGE);
        return NULL;
    }
    void *p = tlb_lookup(c, va, access);
    if (!p)
        raise_fault(c, va, access, APPINE_FAULT_PAGE);
    else if (UNLIKELY(watch_hit(c, va, bytes, watch)))
        return raise_fault(c, va, access, APPINE_FAULT_WATCH), NULL;
    return p;
}

static void load_atomic(const void *p, unsigned bytes, bool acquire, uint64_t out[2])
{
    int order = acquire ? __ATOMIC_ACQUIRE : __ATOMIC_RELAXED;
    out[1] = 0;
    switch (bytes) {
    case 1: out[0] = __atomic_load_n((const uint8_t *)p, order); break;
    case 2: out[0] = __atomic_load_n((const uint16_t *)p, order); break;
    case 4: out[0] = __atomic_load_n((const uint32_t *)p, order); break;
    case 8: out[0] = __atomic_load_n((const uint64_t *)p, order); break;
    default: {
#ifdef __aarch64__
        /* 16 字节：用一对独占加载保证单次拷贝原子性，读完立即清掉宿主监视器 */
        uint64_t lo, hi;
        __asm__ volatile("ldaxp %0, %1, [%2]\n\tclrex" : "=&r"(lo), "=&r"(hi) : "r"(p) : "memory");
        out[0] = lo;
        out[1] = hi;
#else
        memcpy(out, p, 16);
#endif
        break;
    }
    }
}

static bool cas(void *p, unsigned bytes, const uint64_t expected[2], const uint64_t desired[2])
{
    switch (bytes) {
    case 1: {
        uint8_t e = (uint8_t)expected[0];
        return __atomic_compare_exchange_n((uint8_t *)p, &e, (uint8_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 2: {
        uint16_t e = (uint16_t)expected[0];
        return __atomic_compare_exchange_n((uint16_t *)p, &e, (uint16_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 4: {
        uint32_t e = (uint32_t)expected[0];
        return __atomic_compare_exchange_n((uint32_t *)p, &e, (uint32_t)desired[0], false,
                                           __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    }
    case 8: {
        uint64_t e = expected[0];
        return __atomic_compare_exchange_n((uint64_t *)p, &e, desired[0], false, __ATOMIC_SEQ_CST,
                                           __ATOMIC_SEQ_CST);
    }
    default: {
#ifdef __aarch64__
        unsigned ok;
        __asm__ volatile("1: ldaxp x16, x17, [%1]\n\t"
                         "cmp x16, %2\n\t"
                         "ccmp x17, %3, #0, eq\n\t"
                         "b.ne 2f\n\t"
                         "stlxp %w0, %4, %5, [%1]\n\t"
                         "cbnz %w0, 1b\n\t"
                         "mov %w0, #1\n\t"
                         "b 3f\n"
                         "2: clrex\n\t"
                         "mov %w0, #0\n"
                         "3:"
                         : "=&r"(ok)
                         : "r"(p), "r"(expected[0]), "r"(expected[1]), "r"(desired[0]), "r"(desired[1])
                         : "x16", "x17", "cc", "memory");
        return ok;
#else
        if (memcmp(p, expected, 16) != 0)
            return false;
        memcpy(p, desired, 16);
        return true;
#endif
    }
    }
}

/* LDXR/LDAXR/LDXP/LDAXP、STXR/STLXR/STXP/STLXP */
static enum appine_exit exclusive(struct appine_cpu *c, uint32_t insn, bool pair)
{
    unsigned size = FIELD(insn, 31, 30), load = BIT(insn, 22), ordered = BIT(insn, 15);
    unsigned rs = FIELD(insn, 20, 16), rt2 = FIELD(insn, 14, 10), rn = FIELD(insn, 9, 5);
    unsigned rt = insn & 31;
    unsigned esize = 1u << size, bytes = pair ? 2 * esize : esize;
    void *p = atomic_target(c, rn, bytes, load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE,
                            load ? APPINE_WATCH_LOAD : APPINE_WATCH_STORE);
    if (!p)
        return APPINE_EXIT_FAULT;
    uint64_t va = untag(rn == 31 ? c->sp : c->x[rn]);

    if (load) {
        uint64_t v[2];
        load_atomic(p, bytes, ordered, v);
        c->excl_addr = va;
        c->excl_size = bytes;
        c->excl_value[0] = v[0];
        c->excl_value[1] = v[1];
        if (!pair) {
            set_x(c, rt, v[0]);
        } else if (esize == 4) {
            set_x(c, rt, (uint32_t)v[0]);
            set_x(c, rt2, v[0] >> 32);
        } else {
            set_x(c, rt, v[0]);
            set_x(c, rt2, v[1]);
        }
        return next(c);
    }

    uint64_t desired[2] = {get_x(c, rt), 0};
    if (pair && esize == 4)
        desired[0] = (uint32_t)desired[0] | get_x(c, rt2) << 32;
    else if (pair)
        desired[1] = get_x(c, rt2);
    if (esize < 8 && !pair)
        desired[0] &= (UINT64_C(1) << (8 * esize)) - 1;
    bool ok = c->excl_size == bytes && c->excl_addr == va && cas(p, bytes, c->excl_value, desired);
    c->excl_size = 0;
    set_x(c, rs, ok ? 0 : 1);
    return next(c);
}

/* LDAR/STLR */
static enum appine_exit acquire_release(struct appine_cpu *c, uint32_t insn)
{
    unsigned size = FIELD(insn, 31, 30), load = BIT(insn, 22), rn = FIELD(insn, 9, 5);
    unsigned rt = insn & 31, bytes = 1u << size;
    void *p = atomic_target(c, rn, bytes, load ? APPINE_ACCESS_READ : APPINE_ACCESS_WRITE,
                            load ? APPINE_WATCH_LOAD : APPINE_WATCH_STORE);
    if (!p)
        return APPINE_EXIT_FAULT;
    if (load) {
        uint64_t v[2];
        load_atomic(p, bytes, true, v);
        set_x(c, rt, v[0]);
        return next(c);
    }
    uint64_t v = get_x(c, rt);
    switch (bytes) {
    case 1: __atomic_store_n((uint8_t *)p, (uint8_t)v, __ATOMIC_RELEASE); break;
    case 2: __atomic_store_n((uint16_t *)p, (uint16_t)v, __ATOMIC_RELEASE); break;
    case 4: __atomic_store_n((uint32_t *)p, (uint32_t)v, __ATOMIC_RELEASE); break;
    default: __atomic_store_n((uint64_t *)p, v, __ATOMIC_RELEASE); break;
    }
    return next(c);
}

/* LDAPR、LDAPRB、LDAPRH（LRCPC）：照 LDAR 读，acquire 比 RCpc 更强，架构允许 */
static enum appine_exit load_acquire_rcpc(struct appine_cpu *c, uint32_t insn)
{
    unsigned bytes = 1u << FIELD(insn, 31, 30), rt = insn & 31;
    void *p = atomic_target(c, FIELD(insn, 9, 5), bytes, APPINE_ACCESS_READ, APPINE_WATCH_LOAD);
    if (!p)
        return APPINE_EXIT_FAULT;
    uint64_t v[2];
    load_atomic(p, bytes, true, v);
    set_x(c, rt, v[0]);
    return next(c);
}

/* 在翻译后的宿主地址上原样执行一条 LSE 指令 */
static enum appine_exit native_atomic(struct appine_cpu *c, uint32_t insn, unsigned bytes,
                                      const struct native_desc *d)
{
    void *p = atomic_target(c, FIELD(insn, 9, 5), bytes, APPINE_ACCESS_WRITE,
                            APPINE_WATCH_LOAD | APPINE_WATCH_STORE);
    if (!p)
        return APPINE_EXIT_FAULT;
    if (!appine_native_exec(insn, d, c, p))
        return undef(c, insn);
    return next(c);
}

enum appine_exit appine_atomic_exec(struct appine_cpu *c, uint32_t insn)
{
    bool lse = appine_has(APPINE_HWCAP_ATOMICS);
    unsigned size = FIELD(insn, 31, 30);

    if ((insn & 0x3f000000) == 0x08000000) { /* 独占访问、acquire/release、CAS、CASP */
        unsigned o2 = BIT(insn, 23), o1 = BIT(insn, 21), o0 = BIT(insn, 15);
        /*
         * 架构规定"应当全为 1"的字段（SBO）：独占访问和 LDAR/STLR 忽略它们；
         * CAS/CASP 的 Rt2 必须全为 1，否则是未分配的编码。与 LLVM 的解码一致。
         */
        bool rt2_ones = FIELD(insn, 14, 10) == 31;
        if (!o2 && !o1)
            return exclusive(c, insn, false);
        if (!o2 && o1) {
            if (size >= 2)
                return exclusive(c, insn, true);
            /* CASP：Rs、Rt 都必须是偶数编号的寄存器对 */
            unsigned rs = FIELD(insn, 20, 16), rt = insn & 31;
            if (!lse || (rs & 1) || (rt & 1) || !rt2_ones)
                return undef(c, insn);
            struct native_desc d = {.addr_pos = 5, .n = 2};
            d.gpr[0] = (struct native_operand){.pos = 16, .pair = 1, .load = 1, .store = 1};
            d.gpr[1] = (struct native_operand){.pos = 0, .pair = 1, .load = 1};
            return native_atomic(c, insn, 8u << size, &d);
        }
        if (o2 && !o1) /* LDAR/STLR；o0 = 0 的 LDLAR/STLLR 属于 LOR 扩展 */
            return o0 ? acquire_release(c, insn) : undef(c, insn);
        if (!lse || !rt2_ones)
            return undef(c, insn);
        struct native_desc d = {.addr_pos = 5, .n = 2}; /* CAS：Rs 既是比较值也接收旧值 */
        d.gpr[0] = (struct native_operand){.pos = 16, .load = 1, .store = 1};
        d.gpr[1] = (struct native_operand){.pos = 0, .load = 1};
        return native_atomic(c, insn, 1u << size, &d);
    }

    /* LDAPR 在 LSE 那一组里：A = 1、R = 0、Rs 全是 1（LLVM 认的只有这一种）、o3 = 1、opc = 100 */
    if ((insn & 0x3ffffc00) == 0x38bfc000)
        return appine_has(APPINE_HWCAP_LRCPC) ? load_acquire_rcpc(c, insn) : undef(c, insn);
    if ((insn & 0x3f200c00) == 0x38200000 && !BIT(insn, 26)) { /* LD<op>、SWP */
        unsigned o3 = BIT(insn, 15), opc = FIELD(insn, 14, 12);
        if (!lse || (o3 && opc != 0)) /* o3 = 1 且 opc ≠ 0 的另外几种未定义 */
            return undef(c, insn);
        struct native_desc d = {.addr_pos = 5, .n = 2};
        d.gpr[0] = (struct native_operand){.pos = 16, .load = 1};
        d.gpr[1] = (struct native_operand){.pos = 0, .store = 1};
        return native_atomic(c, insn, 1u << size, &d);
    }
    return undef(c, insn);
}
