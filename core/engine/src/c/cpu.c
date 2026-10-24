/* CPU 生命周期、软件 TLB 和 Tier 0 解释器的主循环。 */
#include <stddef.h>

#include "internal.h"

void appine_cpu_init(struct appine_cpu *cpu, struct appine_mmu *mmu)
{
    memset(cpu, 0, sizeof(*cpu));
    cpu->mmu = mmu;
    appine_cpu_tlb_flush(cpu);
}

void appine_cpu_tlb_flush(struct appine_cpu *cpu)
{
    for (unsigned i = 0; i < APPINE_TLB_ENTRIES; i++) {
        cpu->tlb[i] = (struct appine_tlb_entry){TLB_INVALID, 0, TLB_INVALID, 0};
        cpu->itlb[i] = (struct appine_itlb_entry){TLB_INVALID, 0};
    }
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
    unsigned idx = (unsigned)(vpn & (APPINE_TLB_ENTRIES - 1));
    uintptr_t host_page = pte & ~(uintptr_t)PAGE_MASK;
    uint64_t addend = (uint64_t)host_page - (vpn << APPINE_PAGE_SHIFT);
    if (access == APPINE_ACCESS_EXEC) {
        cpu->itlb[idx] = (struct appine_itlb_entry){vpn, addend};
    } else {
        cpu->tlb[idx] = (struct appine_tlb_entry){
            (prot & APPINE_PROT_READ) ? vpn : TLB_INVALID,
            addend,
            (prot & APPINE_PROT_WRITE) ? vpn : TLB_INVALID,
            addend,
        };
    }
    return (void *)(host_page + (va & PAGE_MASK));
}

enum appine_exit appine_cpu_run(struct appine_cpu *cpu)
{
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

void appine_cpu_kick(struct appine_cpu *cpu)
{
    __atomic_store_n(&cpu->pending, 1, __ATOMIC_RELEASE);
}

int appine_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache)
{
    return size == sizeof(struct appine_cpu) && offsetof(struct appine_cpu, tlb) == 0 &&
           off_x == offsetof(struct appine_cpu, x) && off_pc == offsetof(struct appine_cpu, pc) &&
           off_tpidr == offsetof(struct appine_cpu, tpidr_el0) &&
           off_pending == offsetof(struct appine_cpu, pending) &&
           off_mmu == offsetof(struct appine_cpu, mmu) &&
           off_jump_cache == offsetof(struct appine_cpu, dbt_jump_cache);
}
