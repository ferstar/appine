/* 引擎内部共享的辅助函数：soft-MMU 快速路径与 guest 访存。 */
#ifndef APPINE_ENGINE_INTERNAL_H
#define APPINE_ENGINE_INTERNAL_H

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "appine/engine.h"

#define TLB_INVALID UINT64_MAX
#define PAGE_MASK (APPINE_PAGE_SIZE - 1)

/* 页表叶子表项 = host 页地址 | 权限位；host 页 4K 对齐，低 12 位空闲 */
#define PTE_PROT_MASK UINT64_C(0x7)

#define LIKELY(x) __builtin_expect(!!(x), 1)
#define UNLIKELY(x) __builtin_expect(!!(x), 0)

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
    struct appine_tlb_entry *e = &cpu->tlb[vpn & (APPINE_TLB_ENTRIES - 1)];
    uint64_t tag = access == APPINE_ACCESS_READ    ? e->tag_read
                   : access == APPINE_ACCESS_WRITE ? e->tag_write
                                                   : e->tag_exec;
    if (LIKELY(tag == vpn))
        return (void *)(uintptr_t)(va + e->addend);
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

enum appine_exit appine_interp_exec(struct appine_cpu *cpu, uint32_t insn);

#endif /* APPINE_ENGINE_INTERNAL_H */
