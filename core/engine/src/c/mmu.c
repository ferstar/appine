/*
 * soft-MMU 页表（设计文档 §5.1）：4 级基数树，每级 512 项，覆盖 48 位 guest
 * 地址空间。中间表项是下一级表的指针，叶子表项是 host 页地址 | 权限位。
 * 只有 TLB 未命中时才会遍历这棵树。
 *
 * 表项按需建立：内核层只给小的映射预先建好，大的等 guest 第一次访问时一次补一张叶子表
 * （appine_mmu_fill），所以页表的大小随碰过的内存增长，而不是随映射的大小。遍历一段
 * 范围时，没有中间表的地方按缺的那一级整段跳过（一项覆盖 2 MB、1 GB 或 512 GB）。
 *
 * 并发：修改只由内核层在持有 BKL 时进行，而共享同一地址空间的其他线程可能
 * 同时在遍历（TLB 未命中）。所以表项一律用原子操作读写：新建的中间表以
 * release 语义挂上，遍历时以 acquire 语义读取。中间表在 mmu 销毁前不会释放。
 */
#include <stdbool.h>
#include <stdlib.h>

#include "internal.h"

#define LEVELS MMU_LEVELS
#define LEVEL_BITS MMU_LEVEL_BITS
#define LEVEL_ENTRIES (1u << LEVEL_BITS)
/* 一张叶子表覆盖的字节数 */
#define LEAF_SPAN ((uint64_t)APPINE_PAGE_SIZE << LEVEL_BITS)

static unsigned level_shift(int level)
{
    return APPINE_PAGE_SHIFT + LEVEL_BITS * (LEVELS - 1 - level);
}

static unsigned level_index(uint64_t gva, int level)
{
    return (unsigned)(gva >> level_shift(level)) & (LEVEL_ENTRIES - 1);
}

static uintptr_t *new_table(void)
{
    return calloc(LEVEL_ENTRIES, sizeof(uintptr_t));
}

static uintptr_t load_entry(const uintptr_t *slot)
{
    return __atomic_load_n(slot, __ATOMIC_ACQUIRE);
}

static void store_entry(uintptr_t *slot, uintptr_t v)
{
    __atomic_store_n(slot, v, __ATOMIC_RELEASE);
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

/*
 * gva 所在的叶子表。中间表缺失时：alloc 为真就新建；否则返回 NULL，并把 *hole_end 设为
 * 从 gva 起整段没有表项的范围的末端（按缺失的那一级对齐，不超过 2^48）。alloc 时分配
 * 失败也返回 NULL
 */
static uintptr_t *leaf_table(const struct appine_mmu *mmu, uint64_t gva, bool alloc,
                             uint64_t *hole_end)
{
    uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t *slot = &table[level_index(gva, level)];
        uintptr_t next = load_entry(slot);
        if (!next) {
            if (!alloc) {
                unsigned shift = level_shift(level);
                *hole_end = ((gva >> shift) + 1) << shift;
                return NULL;
            }
            uintptr_t *fresh = new_table();
            if (!fresh)
                return NULL;
            next = (uintptr_t)fresh;
            store_entry(slot, next);
        }
        table = (uintptr_t *)next;
    }
    return table;
}

/* gva 所在叶子表覆盖范围的末端，不超过 end */
static uint64_t leaf_end(uint64_t gva, uint64_t end)
{
    uint64_t e = (gva | (LEAF_SPAN - 1)) + 1;
    return e < end ? e : end;
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

/* map 和 fill：only_empty 时已有表项（包括权限为 0 的）保持不变 */
static int store_range(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len,
                       uint32_t prot, bool only_empty)
{
    uintptr_t h = (uintptr_t)host;
    if (!range_ok(gva, len) || (h & PAGE_MASK) || !h || (prot & ~PTE_PROT_MASK))
        return APPINE_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, true, NULL);
        if (!table)
            return APPINE_ENOMEM;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += APPINE_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            if (!only_empty || !load_entry(slot))
                store_entry(slot, (h + (va - gva)) | prot);
        }
    }
    return APPINE_OK;
}

int appine_mmu_map(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot)
{
    return store_range(mmu, gva, host, len, prot, false);
}

int appine_mmu_fill(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot)
{
    return store_range(mmu, gva, host, len, prot, true);
}

int appine_mmu_unmap(struct appine_mmu *mmu, uint64_t gva, uint64_t len)
{
    if (!range_ok(gva, len))
        return APPINE_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += APPINE_PAGE_SIZE)
            store_entry(&table[level_index(va, LEVELS - 1)], 0);
    }
    return APPINE_OK;
}

int appine_mmu_protect(struct appine_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot)
{
    if (!range_ok(gva, len) || (prot & ~PTE_PROT_MASK))
        return APPINE_EINVAL;
    uint64_t end = gva + len;
    for (uint64_t va = gva; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += APPINE_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            uintptr_t old = load_entry(slot);
            if (old)
                store_entry(slot, (old & ~(uintptr_t)PTE_PROT_MASK) | prot);
        }
    }
    return APPINE_OK;
}

int appine_mmu_move(struct appine_mmu *mmu, uint64_t from, uint64_t to, uint64_t len)
{
    if (!range_ok(from, len) || !range_ok(to, len) || (from < to + len && to < from + len))
        return APPINE_EINVAL;
    uint64_t end = from + len;
    for (uint64_t va = from; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += APPINE_PAGE_SIZE) {
            uintptr_t *slot = &table[level_index(va, LEVELS - 1)];
            uintptr_t v = load_entry(slot);
            if (!v)
                continue;
            uint64_t dst = to + (va - from), unused;
            uintptr_t *target = leaf_table(mmu, dst, true, &unused);
            if (!target)
                return APPINE_ENOMEM;
            store_entry(&target[level_index(dst, LEVELS - 1)], v);
            store_entry(slot, 0);
        }
    }
    return APPINE_OK;
}

uint64_t appine_mmu_next_present(const struct appine_mmu *mmu, uint64_t gva, uint64_t end)
{
    uint64_t limit = UINT64_C(1) << APPINE_VA_BITS;
    if (end > limit)
        end = limit;
    for (uint64_t va = gva & ~(uint64_t)PAGE_MASK; va < end;) {
        uintptr_t *table = leaf_table(mmu, va, false, &va);
        if (!table)
            continue;
        for (uint64_t stop = leaf_end(va, end); va < stop; va += APPINE_PAGE_SIZE) {
            if (load_entry(&table[level_index(va, LEVELS - 1)]))
                return va;
        }
    }
    return end;
}

uintptr_t appine_mmu_lookup_pte(const struct appine_mmu *mmu, uint64_t gva)
{
    if (!mmu || (gva >> APPINE_VA_BITS))
        return 0;
    const uintptr_t *table = mmu->root;
    for (int level = 0; level < LEVELS - 1; level++) {
        uintptr_t next = load_entry(&table[level_index(gva, level)]);
        if (!next)
            return 0;
        table = (const uintptr_t *)next;
    }
    return load_entry(&table[level_index(gva, LEVELS - 1)]);
}

void *appine_mmu_translate(const struct appine_mmu *mmu, uint64_t gva, uint32_t required_prot)
{
    uintptr_t pte = appine_mmu_lookup_pte(mmu, gva);
    /* 权限为 0 的表项（PROT_NONE 的页留着宿主页）不能访问，传 0 时也不算已映射 */
    if (!(pte & PTE_PROT_MASK) || (pte & required_prot) != required_prot)
        return NULL;
    return (void *)((pte & ~(uintptr_t)PAGE_MASK) + (gva & PAGE_MASK));
}
