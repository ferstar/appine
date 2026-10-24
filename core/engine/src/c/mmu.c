/*
 * soft-MMU 页表（设计文档 §5.1）：4 级基数树，每级 512 项，覆盖 48 位 guest
 * 地址空间。中间表项是下一级表的指针，叶子表项是 host 页地址 | 权限位。
 * 只有 TLB 未命中时才会遍历这棵树。
 */
#include <stdbool.h>
#include <stdlib.h>

#include "internal.h"

#define LEVELS 4
#define LEVEL_BITS 9
#define LEVEL_ENTRIES (1u << LEVEL_BITS)

struct appine_mmu {
    uintptr_t *root;
};

static unsigned level_index(uint64_t gva, int level)
{
    unsigned shift = APPINE_PAGE_SHIFT + LEVEL_BITS * (LEVELS - 1 - level);
    return (unsigned)(gva >> shift) & (LEVEL_ENTRIES - 1);
}

static uintptr_t *new_table(void)
{
    return calloc(LEVEL_ENTRIES, sizeof(uintptr_t));
}

static void free_table(uintptr_t *table, int level)
{
    if (level < LEVELS - 1) {
        for (unsigned i = 0; i < LEVEL_ENTRIES; i++) {
            if (table[i])
                free_table((uintptr_t *)table[i], level + 1);
        }
    }
    free(table);
}

/* 返回叶子表项的位置；alloc 为 false 且中间表缺失时返回 NULL */
static uintptr_t *leaf_slot(struct appine_mmu *mmu, uint64_t gva, bool alloc)
{
    uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t *slot = &table[level_index(gva, level)];
        if (!*slot) {
            if (!alloc)
                return NULL;
            uintptr_t *next = new_table();
            if (!next)
                return NULL;
            *slot = (uintptr_t)next;
        }
        table = (uintptr_t *)*slot;
    }
    return &table[level_index(gva, LEVELS - 1)];
}

static bool range_ok(uint64_t gva, uint64_t len)
{
    uint64_t limit = UINT64_C(1) << APPINE_VA_BITS;
    return (gva & PAGE_MASK) == 0 && (len & PAGE_MASK) == 0 && gva <= limit && len <= limit - gva;
}

struct appine_mmu *appine_mmu_new(void)
{
    struct appine_mmu *mmu = calloc(1, sizeof(*mmu));
    if (!mmu)
        return NULL;
    mmu->root = new_table();
    if (!mmu->root) {
        free(mmu);
        return NULL;
    }
    return mmu;
}

void appine_mmu_free(struct appine_mmu *mmu)
{
    if (!mmu)
        return;
    free_table(mmu->root, 0);
    free(mmu);
}

int appine_mmu_map(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot)
{
    uintptr_t h = (uintptr_t)host;
    if (!range_ok(gva, len) || (h & PAGE_MASK) || !h || (prot & ~PTE_PROT_MASK))
        return APPINE_EINVAL;
    for (uint64_t off = 0; off < len; off += APPINE_PAGE_SIZE) {
        uintptr_t *slot = leaf_slot(mmu, gva + off, true);
        if (!slot)
            return APPINE_ENOMEM;
        *slot = (h + off) | prot;
    }
    return APPINE_OK;
}

int appine_mmu_unmap(struct appine_mmu *mmu, uint64_t gva, uint64_t len)
{
    if (!range_ok(gva, len))
        return APPINE_EINVAL;
    for (uint64_t off = 0; off < len; off += APPINE_PAGE_SIZE) {
        uintptr_t *slot = leaf_slot(mmu, gva + off, false);
        if (slot)
            *slot = 0;
    }
    return APPINE_OK;
}

int appine_mmu_protect(struct appine_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot)
{
    if (!range_ok(gva, len) || (prot & ~PTE_PROT_MASK))
        return APPINE_EINVAL;
    for (uint64_t off = 0; off < len; off += APPINE_PAGE_SIZE) {
        uintptr_t *slot = leaf_slot(mmu, gva + off, false);
        if (slot && *slot)
            *slot = (*slot & ~(uintptr_t)PTE_PROT_MASK) | prot;
    }
    return APPINE_OK;
}

uintptr_t appine_mmu_lookup_pte(const struct appine_mmu *mmu, uint64_t gva)
{
    if (!mmu || (gva >> APPINE_VA_BITS))
        return 0;
    const uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t next = table[level_index(gva, level)];
        if (!next)
            return 0;
        table = (const uintptr_t *)next;
    }
    return table[level_index(gva, LEVELS - 1)];
}

void *appine_mmu_translate(const struct appine_mmu *mmu, uint64_t gva, uint32_t required_prot)
{
    uintptr_t pte = appine_mmu_lookup_pte(mmu, gva);
    if (!pte || (pte & required_prot) != required_prot)
        return NULL;
    return (void *)((pte & ~(uintptr_t)PAGE_MASK) + (gva & PAGE_MASK));
}
