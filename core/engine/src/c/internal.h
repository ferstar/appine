/* 引擎内部共享的辅助函数：soft-MMU 快速路径与 guest 访存。 */
#ifndef APPINE_ENGINE_INTERNAL_H
#define APPINE_ENGINE_INTERNAL_H

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "appine/engine.h"

#define TLB_INVALID 0 /* TLB 的 tag 是 ~页号，页号不可能全为 1 */
#define PAGE_MASK (APPINE_PAGE_SIZE - 1)

/* 页表叶子表项 = host 页地址 | 权限位；host 页 4K 对齐，低 12 位空闲 */
#define PTE_PROT_MASK UINT64_C(0x7)

#define LIKELY(x) __builtin_expect(!!(x), 1)
#define UNLIKELY(x) __builtin_expect(!!(x), 0)

/*
 * 写入代码之后让取指看到新代码，与 compiler-rt 的 __clear_cache 在 aarch64 上的做法相同。
 * 不用 __builtin___clear_cache：Rust 链接 Android 的动态库时不带 compiler-rt，那个符号
 * 在加载时找不到。代码经由另一个地址写入时（W^X 双映射），数据缓存按写入的地址
 * start + write_delta 清理，指令缓存按执行的地址 start 作废。
 */
#if defined(__aarch64__) && defined(__linux__)
/* 宿主的 CTR_EL0。只读一次：有些内核为了绕过勘误会让 EL0 读 CTR_EL0 陷入内核 */
static inline uint64_t host_ctr(void)
{
    static uint64_t cached;
    uint64_t ctr = __atomic_load_n(&cached, __ATOMIC_RELAXED);
    if (!ctr) { /* bit 31 恒为 1，0 表示还没读过 */
        __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
        __atomic_store_n(&cached, ctr, __ATOMIC_RELAXED);
    }
    return ctr;
}
#endif

static inline void sync_code_ranges(void *const *start, void *const *end, unsigned n,
                                    intptr_t write_delta)
{
#if defined(__aarch64__) && defined(__linux__)
    uint64_t ctr = host_ctr(), addr;
    /* CTR_EL0.IDC：数据缓存不需要清到统一点 */
    if (!((ctr >> 28) & 1)) {
        uint64_t line = UINT64_C(4) << ((ctr >> 16) & 15);
        for (unsigned i = 0; i < n; i++)
            for (addr = (uint64_t)(uintptr_t)start[i] & ~(line - 1); addr < (uint64_t)(uintptr_t)end[i];
                 addr += line)
                __asm__ volatile("dc cvau, %0" ::"r"(addr + (uint64_t)write_delta) : "memory");
    }
    __asm__ volatile("dsb ish" ::: "memory");
    /* CTR_EL0.DIC：指令缓存不需要作废 */
    if (!((ctr >> 29) & 1)) {
        uint64_t line = UINT64_C(4) << (ctr & 15);
        for (unsigned i = 0; i < n; i++)
            for (addr = (uint64_t)(uintptr_t)start[i] & ~(line - 1); addr < (uint64_t)(uintptr_t)end[i];
                 addr += line)
                __asm__ volatile("ic ivau, %0" ::"r"(addr) : "memory");
        /* 这次 dsb 要等作废广播完成，比作废本身贵得多：几段代码合在一起同步，只等一次 */
        __asm__ volatile("dsb ish" ::: "memory");
    }
    __asm__ volatile("isb" ::: "memory");
#else
    (void)write_delta;
    for (unsigned i = 0; i < n; i++)
        __builtin___clear_cache((char *)start[i], (char *)end[i]);
#endif
}

/*
 * 只做缓存维护、不等它完成也不做上下文同步（sync_code_ranges 去掉 dsb、isb）：用于改写
 * 链接点这种"旧指令照样正确、新指令迟一点被看到也没关系"的地方。维护操作总会完成，
 * 之后取指迟早看到新指令；在那之前执行的还是旧指令。
 */
static inline void publish_code_lazily(void *start, void *end, intptr_t write_delta)
{
#if defined(__aarch64__) && defined(__linux__)
    uint64_t ctr = host_ctr(), addr;
    if (!((ctr >> 28) & 1)) {
        uint64_t line = UINT64_C(4) << ((ctr >> 16) & 15);
        for (addr = (uint64_t)(uintptr_t)start & ~(line - 1); addr < (uint64_t)(uintptr_t)end; addr += line)
            __asm__ volatile("dc cvau, %0" ::"r"(addr + (uint64_t)write_delta) : "memory");
    }
    if (!((ctr >> 29) & 1)) {
        uint64_t line = UINT64_C(4) << (ctr & 15);
        for (addr = (uint64_t)(uintptr_t)start & ~(line - 1); addr < (uint64_t)(uintptr_t)end; addr += line)
            __asm__ volatile("ic ivau, %0" ::"r"(addr) : "memory");
    }
#else
    __builtin___clear_cache((char *)start, (char *)end);
    (void)write_delta;
#endif
}

static inline void sync_code(void *start, void *end, intptr_t write_delta)
{
    sync_code_ranges(&start, &end, 1, write_delta);
}

static inline void sync_icache(void *start, void *end)
{
    sync_code(start, end, 0);
}

/*
 * 分配一块存放生成代码的内存（Tier 1 的代码缓存、原生指令桩），按 W^X 映射（§12）：
 * 同一段共享内存映射两次，执行用返回的地址，写入用它加上 *rw_delta。宿主不允许
 * 双映射时退回 RWX，*rw_delta 为 0。失败时返回 NULL。只在 aarch64 Linux 上可用。
 */
uint32_t *appine_map_code(size_t bytes, intptr_t *rw_delta);
void appine_unmap_code(uint32_t *code, size_t bytes, intptr_t rw_delta);

/* soft-MMU 页表（mmu.c）：Tier 1 的 TLB 回填例程也按这个结构遍历 */
#define MMU_LEVELS 4
#define MMU_LEVEL_BITS 9

struct appine_mmu {
    uintptr_t *root;
};

/* 返回 gva 所在页的叶子表项，未映射时返回 0 */
uintptr_t appine_mmu_lookup_pte(const struct appine_mmu *mmu, uint64_t gva);

/* TLB 未命中：查页表并填充 TLB，失败返回 NULL */
void *appine_tlb_fill(struct appine_cpu *cpu, uint64_t va, uint32_t access);

/* Linux 对 EL0 开启了 TBI：数据访问忽略地址的高 8 位 */
static inline uint64_t untag(uint64_t va)
{
    return va & UINT64_C(0x00ffffffffffffff);
}


static inline void *tlb_lookup(struct appine_cpu *cpu, uint64_t va, uint32_t access)
{
    uint64_t vpn = va >> APPINE_PAGE_SHIFT;
    unsigned idx = (unsigned)(vpn & (APPINE_TLB_ENTRIES - 1));
    uint64_t tag, addend;
    if (access == APPINE_ACCESS_EXEC) {
        tag = cpu->itlb[vpn & (APPINE_ITLB_ENTRIES - 1)].tag;
        addend = cpu->itlb[vpn & (APPINE_ITLB_ENTRIES - 1)].addend;
    } else if (access == APPINE_ACCESS_READ) {
        tag = cpu->tlb[idx].tag_read;
        addend = cpu->tlb[idx].addend_read;
    } else {
        tag = cpu->tlb[idx].tag_write;
        addend = cpu->tlb[idx].addend_write;
    }
    if (LIKELY(tag == ~vpn))
        return (void *)(uintptr_t)(va + addend);
    return appine_tlb_fill(cpu, va, access);
}

static inline bool raise_fault(struct appine_cpu *cpu, uint64_t va, uint32_t access, uint32_t kind)
{
    cpu->fault_addr = va;
    cpu->fault_access = access;
    cpu->fault_kind = kind;
    return false;
}

/*
 * 读写 guest 内存（n ≤ 16）。跨页访问会先确认两页都可访问，再拷贝。
 * 失败时记录 fault 并返回 false，不产生任何副作用。
 */
static inline bool mem_read(struct appine_cpu *cpu, uint64_t va, void *out, unsigned n)
{
    va = untag(va);
    uint64_t off = va & PAGE_MASK;
    void *p0 = tlb_lookup(cpu, va, APPINE_ACCESS_READ);
    if (UNLIKELY(!p0))
        return raise_fault(cpu, va, APPINE_ACCESS_READ, APPINE_FAULT_PAGE);
    if (LIKELY(off + n <= APPINE_PAGE_SIZE)) {
        memcpy(out, p0, n);
        return true;
    }
    unsigned first = (unsigned)(APPINE_PAGE_SIZE - off);
    void *p1 = tlb_lookup(cpu, va + first, APPINE_ACCESS_READ);
    if (UNLIKELY(!p1))
        return raise_fault(cpu, va + first, APPINE_ACCESS_READ, APPINE_FAULT_PAGE);
    memcpy(out, p0, first);
    memcpy((uint8_t *)out + first, p1, n - first);
    return true;
}

static inline bool mem_write(struct appine_cpu *cpu, uint64_t va, const void *in, unsigned n)
{
    va = untag(va);
    uint64_t off = va & PAGE_MASK;
    void *p0 = tlb_lookup(cpu, va, APPINE_ACCESS_WRITE);
    if (UNLIKELY(!p0))
        return raise_fault(cpu, va, APPINE_ACCESS_WRITE, APPINE_FAULT_PAGE);
    if (LIKELY(off + n <= APPINE_PAGE_SIZE)) {
        memcpy(p0, in, n);
        return true;
    }
    unsigned first = (unsigned)(APPINE_PAGE_SIZE - off);
    void *p1 = tlb_lookup(cpu, va + first, APPINE_ACCESS_WRITE);
    if (UNLIKELY(!p1))
        return raise_fault(cpu, va + first, APPINE_ACCESS_WRITE, APPINE_FAULT_PAGE);
    memcpy(p0, in, first);
    memcpy(p1, (const uint8_t *)in + first, n - first);
    return true;
}

/* 执行一条已经取到的指令 */
/* ---- 宿主故障兜底（guard.c）---- */

/*
 * 上着保险运行 fn(ctx, cpu)：期间宿主的 SIGSEGV、SIGBUS、SIGILL 让它返回
 * APPINE_EXIT_HOST_FAULT（appine_fault_guard_install() 之后才生效）。fn 不能调用 malloc，
 * 拿到的锁都要登记。
 */
enum appine_exit appine_guard_run(enum appine_exit (*fn)(void *, struct appine_cpu *), void *ctx,
                                  struct appine_cpu *cpu);

/* 刚拿到一把锁（rw 为真时是 pthread_rwlock_t，否则是 pthread_mutex_t）：保险期间出了故障
 * 时由 appine_guard_run() 释放 */
void appine_guard_hold(void *lock, bool rw);

/* 马上要释放最近登记的那把锁 */
void appine_guard_drop(void);

/* ---- CPU 特性（features.c）---- */

/* guest 特性集合是否包含 hwcap 里的全部位 */
bool appine_has(uint64_t hwcap);

/* EL0 读取 ID 寄存器空间时的值；key 是 MRS 编码的 bits 20:5，不在该空间时返回 false */
bool appine_id_register(unsigned key, uint64_t *value);

/* ---- 原生指令桩（native.c）---- */

/*
 * 原生桩在宿主 CPU 上执行一条 guest 指令（设计文档 §4.2）：通用寄存器字段被
 * 换成宿主临时寄存器，执行前从 cpu 结构体读入、执行后写回。fp 为真时还会
 * 载入并写回 V0–V31、FPCR、FPSR 和 NZCV。
 */
struct native_operand {
    uint8_t pos;   /* 寄存器字段的起始位 */
    uint8_t pair;  /* 字段表示一对相邻寄存器（CASP） */
    uint8_t load;  /* 执行前读入 guest 值 */
    uint8_t store; /* 执行后写回 */
};

struct native_desc {
    uint8_t fp;
    uint8_t addr_pos; /* 地址寄存器字段的起始位，NATIVE_NO_ADDR 表示没有 */
    uint8_t n;
    struct native_operand gpr[3];
};

#define NATIVE_NO_ADDR 0xff

typedef void (*native_fn)(struct appine_cpu *cpu, void *host_addr);

bool appine_native_available(void);

/*
 * 取得（必要时生成）insn 的原生桩并执行；原生桩不可用时返回 false。同一条
 * insn 的 desc 必须总是相同。可以从多个线程同时调用。
 */
bool appine_native_exec(uint32_t insn, const struct native_desc *desc, struct appine_cpu *cpu,
                        void *host_addr);

/* ---- SIMD&FP（simd.c）与原子访存（atomic.c）---- */

/* SIMD&FP 数据处理指令是否在白名单内；是则填好原生桩的描述 */
bool appine_simd_dp_decode(uint32_t insn, struct native_desc *desc);

/* SIMD&FP 访存（bit 26 为 1 的 load/store 编码） */
enum appine_exit appine_simd_ldst(struct appine_cpu *cpu, uint32_t insn);

/* 独占访问、acquire/release、LSE 原子操作；编码不属于这些类别时返回 UNDEF */
enum appine_exit appine_atomic_exec(struct appine_cpu *cpu, uint32_t insn);

enum appine_exit appine_interp_exec(struct appine_cpu *cpu, uint32_t insn);

/* 从 cpu->pc 取指并执行一条指令（Tier 0 的单步） */
enum appine_exit appine_interp_step(struct appine_cpu *cpu);

#endif /* APPINE_ENGINE_INTERNAL_H */
