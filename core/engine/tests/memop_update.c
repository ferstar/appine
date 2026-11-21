/*
 * ARM64 Linux standalone regression. Build with the engine C sources except dbt.c,
 * which is included here to control the interleaving without a runtime test hook.
 *
 * From the repository root on ARM64 Linux (bash):
 *   cc -std=c17 -pthread -Icore/engine/include core/engine/tests/memop_update.c \
 *     core/engine/src/c/{atomic,cpu,features,guard,interp,mmu,native,simd}.c \
 *     -o /tmp/appine-memop-update && /tmp/appine-memop-update
 */
#include "../src/c/dbt.c"

#include <assert.h>

struct update {
    struct appine_dbt *space;
    const struct appine_dbt_memop *op;
};

static void *publish(void *arg)
{
    const struct update *update = arg;
    appine_dbt_set_memops(update->space, update->op, update->op ? 1 : 0);
    return NULL;
}

static void check_update(struct appine_dbt_thread *thread, struct appine_cpu *cpu,
                         const struct appine_dbt_memop *op)
{
    assert(prepare(thread, cpu, cpu->pc));
    struct update update = {thread->d, op};
    pthread_t updater;
    assert(pthread_create(&updater, NULL, publish, &update) == 0);
    assert(pthread_join(updater, NULL) == 0);

    struct syncs sync = {.n = 0};
    struct fresh fresh = {.code = NULL};
    lock_dbt(thread->d, &thread->stats);
    assert(install(thread->d, thread->prep, &sync, &fresh) == STALE);
    assert(sync.n == 0 && fresh.code == NULL);
    unlock_dbt(thread->d);
}

int main(void)
{
    const uint64_t pc = 0x400000;
    _Alignas(APPINE_PAGE_SIZE) uint32_t code[APPINE_PAGE_SIZE / 4] = {0xd65f03c0}; /* RET */
    struct appine_mmu *mmu = appine_mmu_new();
    assert(mmu);
    assert(appine_mmu_map(mmu, pc, code, sizeof(code), APPINE_PROT_READ | APPINE_PROT_EXEC) == 0);
    struct appine_cpu *cpu = appine_cpu_new(mmu);
    assert(cpu);
    cpu->pc = pc;

    struct appine_dbt *space = appine_dbt_new(0, APPINE_DBT_REGS_DEFAULT);
    assert(space);
    struct appine_dbt_thread *thread = appine_dbt_attach(space, 0);
    assert(thread);
    struct appine_dbt_memop op = {.pc = pc, .kind = APPINE_MEMOP_MEMSET};
    check_update(thread, cpu, &op);
    check_update(thread, cpu, NULL);

    appine_dbt_detach(thread);
    appine_dbt_free(space, 0);
    appine_cpu_free(cpu);
    appine_mmu_free(mmu);
    puts("memop registration interleaving passed (add and remove)");
}
