/* CPU 生命周期、软件 TLB 和 Tier 0 解释器的主循环。 */
#define _DEFAULT_SOURCE /* -std=c17 下 MAP_ANONYMOUS 需要它 */
#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <sys/mman.h>

#include "internal.h"

/*
 * 直接用 mmap：malloc 的实现可能从堆里分配并逐字节清零（glibc 释放过同样大的块之后
 * 会调高 mmap 的阈值），那样每个线程的 TLB 都会整块占用内存
 */
/*
 * 释放的 cpu 留几个给下一个线程：shell 每起一个进程就要一个，新映射一段、TLB 用到的页
 * 逐页缺页、退出时拆掉，比 appine_cpu_init 只清填过的项贵得多
 */
#define SPARE_CPUS 8
static struct {
    pthread_mutex_t lock;
    struct appine_cpu *cpus[SPARE_CPUS];
    unsigned n;
} spare = {.lock = PTHREAD_MUTEX_INITIALIZER};

struct appine_cpu *appine_cpu_new(struct appine_mmu *mmu)
{
    pthread_mutex_lock(&spare.lock);
    void *p = spare.n ? spare.cpus[--spare.n] : NULL;
    pthread_mutex_unlock(&spare.lock);
    if (!p) {
        p = mmap(NULL, sizeof(struct appine_cpu), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1,
                 0);
        if (p == MAP_FAILED)
            return NULL;
    }
    appine_cpu_init(p, mmu);
    return p;
}

void appine_cpu_free(struct appine_cpu *cpu)
{
    if (!cpu)
        return;
    pthread_mutex_lock(&spare.lock);
    bool keep = spare.n < SPARE_CPUS;
    if (keep)
        spare.cpus[spare.n++] = cpu;
    pthread_mutex_unlock(&spare.lock);
    if (!keep)
        munmap(cpu, sizeof(*cpu));
}

void appine_cpu_init(struct appine_cpu *cpu, struct appine_mmu *mmu)
{
    /* 数据 TLB 只清掉填过的项：新分配的 cpu 是清零的内存，没碰过的页不必写 */
    memset(cpu, 0, offsetof(struct appine_cpu, tlb));
    memset(cpu->v, 0, sizeof(cpu->v));
    cpu->mmu = mmu;
    appine_cpu_tlb_flush(cpu);
}

/* 数据 TLB 有 4096 项，进程频繁 mmap/munmap 时每次都整体清空太贵，只清填充过的项 */
void appine_cpu_tlb_flush(struct appine_cpu *cpu)
{
    for (unsigned w = 0; w < APPINE_TLB_ENTRIES / 64; w++) {
        for (uint64_t bits = cpu->tlb_used[w]; bits; bits &= bits - 1) {
            unsigned i = w * 64 + (unsigned)__builtin_ctzll(bits);
            cpu->tlb[i] = (struct appine_tlb_entry){TLB_INVALID, 0, TLB_INVALID, 0};
        }
        cpu->tlb_used[w] = 0;
    }
    memset(cpu->itlb, 0, sizeof(cpu->itlb));
    _Static_assert(TLB_INVALID == 0, "清零的表项就是无效的");
}

void *appine_tlb_fill(struct appine_cpu *cpu, uint64_t va, uint32_t access)
{
    uintptr_t pte = appine_mmu_lookup_pte(cpu->mmu, va);
    if (!pte)
        return NULL;
    uint32_t prot = (uint32_t)(pte & PTE_PROT_MASK);
    uint32_t need = access == APPINE_ACCESS_READ    ? APPINE_PROT_READ
                    : access == APPINE_ACCESS_WRITE ? APPINE_PROT_WRITE
                                                    : APPINE_PROT_EXEC;
    if (!(prot & need))
        return NULL;

    uint64_t vpn = va >> APPINE_PAGE_SHIFT;
    uintptr_t host_page = pte & ~(uintptr_t)PAGE_MASK;
    uint64_t addend = (uint64_t)host_page - (vpn << APPINE_PAGE_SHIFT);
    if (access == APPINE_ACCESS_EXEC) {
        cpu->itlb[vpn & (APPINE_ITLB_ENTRIES - 1)] = (struct appine_itlb_entry){~vpn, addend};
    } else {
        unsigned idx = (unsigned)(vpn & (APPINE_TLB_ENTRIES - 1));
        cpu->tlb_used[idx / 64] |= UINT64_C(1) << (idx % 64);
        cpu->tlb[idx] = (struct appine_tlb_entry){
            (prot & APPINE_PROT_READ) ? ~vpn : TLB_INVALID,
            addend,
            (prot & APPINE_PROT_WRITE) ? ~vpn : TLB_INVALID,
            addend,
        };
    }
    return (void *)(host_page + (va & PAGE_MASK));
}

static enum appine_exit cpu_run(void *ctx, struct appine_cpu *cpu)
{
    (void)ctx;
    for (;;) {
        /* 先用普通读做快速判断，确实有请求时再原子交换清零，不会吞掉并发到达的 kick */
        if (UNLIKELY(__atomic_load_n(&cpu->pending, __ATOMIC_RELAXED)) &&
            __atomic_exchange_n(&cpu->pending, 0, __ATOMIC_ACQUIRE))
            return APPINE_EXIT_KICK;
        enum appine_exit r = appine_interp_step(cpu);
        if (r != APPINE_EXIT_NONE)
            return r;
    }
}

enum appine_exit appine_cpu_run(struct appine_cpu *cpu)
{
    return appine_guard_run(cpu_run, NULL, cpu);
}

enum appine_exit appine_interp_step(struct appine_cpu *cpu)
{
    uint64_t pc = cpu->pc;
    if (UNLIKELY(pc & 3)) {
        raise_fault(cpu, pc, APPINE_ACCESS_EXEC, APPINE_FAULT_ALIGN);
        return APPINE_EXIT_FAULT;
    }
    void *p = tlb_lookup(cpu, pc, APPINE_ACCESS_EXEC);
    if (UNLIKELY(!p)) {
        raise_fault(cpu, pc, APPINE_ACCESS_EXEC, APPINE_FAULT_PAGE);
        return APPINE_EXIT_FAULT;
    }
    uint32_t insn;
    memcpy(&insn, p, sizeof(insn));
    return appine_interp_exec(cpu, insn);
}

enum appine_exit appine_cpu_exec_insn(struct appine_cpu *cpu, uint32_t insn)
{
    return appine_interp_exec(cpu, insn);
}

static enum appine_exit cpu_step(void *ctx, struct appine_cpu *cpu)
{
    (void)ctx;
    return appine_interp_step(cpu);
}

enum appine_exit appine_cpu_step(struct appine_cpu *cpu)
{
    return appine_guard_run(cpu_step, NULL, cpu);
}

void appine_cpu_kick(struct appine_cpu *cpu)
{
    __atomic_store_n(&cpu->pending, 1, __ATOMIC_RELEASE);
}

int appine_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache,
                            size_t off_tlb, size_t off_v)
{
    return size == sizeof(struct appine_cpu) && off_tlb == offsetof(struct appine_cpu, tlb) &&
           off_v == offsetof(struct appine_cpu, v) &&
           off_x == offsetof(struct appine_cpu, x) && off_pc == offsetof(struct appine_cpu, pc) &&
           off_tpidr == offsetof(struct appine_cpu, tpidr_el0) &&
           off_pending == offsetof(struct appine_cpu, pending) &&
           off_mmu == offsetof(struct appine_cpu, mmu) &&
           off_jump_cache == offsetof(struct appine_cpu, dbt_jump_cache);
}
