/*
 * Tier 0 的原生指令桩（设计文档 §4.2）：SIMD&FP 数据处理、CRC32 和 LSE 原子
 * 操作直接交给宿主 CPU 执行同一条指令，结果与硬件逐位一致。Tier 0 只负责
 * 白名单判定（§4.4）、地址翻译和寄存器搬运。
 *
 * 每条指令编码对应一个桩，缓存在一块可执行内存里：
 *
 *   void stub(struct appine_cpu *cpu, void *host_addr);
 *
 * 通用寄存器字段被换成宿主 x2–x7（成对的寄存器占两个），地址字段换成 x1；
 * x9、x10 是桩自己的临时寄存器。FP 类的桩还会保存宿主 d8–d15 和 FPCR，
 * 载入 guest 的 FPCR/FPSR/NZCV 和 V0–V31，执行后写回。
 *
 * 缓存由所有线程共享，写满时整体清空。为了不覆盖别的线程正在执行的桩，
 * 查找和执行都在读锁内进行，生成和清空需要写锁。缓存按 W^X 双映射
 * （appine_map_code），在可写的别名里生成桩。
 */
#define _GNU_SOURCE /* -std=c17 下 pthread_rwlock_t、mremap 需要它 */

#include <pthread.h>
#include <stddef.h>
#include <stdlib.h>

#include "emit.h"
#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#define NATIVE_SUPPORTED 1
#endif

#ifdef NATIVE_SUPPORTED

#include <sys/mman.h>

#define CACHE_BYTES (UINT64_C(2) << 20)
#define MAX_STUB_WORDS 96
#define TABLE_BITS 12
#define TABLE_SIZE (1u << TABLE_BITS)

struct slot {
    uint32_t insn;
    native_fn fn;
};

static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_rwlock_t lock = PTHREAD_RWLOCK_INITIALIZER;
static uint32_t *cache;   /* 执行用的地址 */
static intptr_t rw_delta; /* 写入用的别名减去执行用的地址 */
static size_t used_words;
static size_t table_count;
static struct slot table[TABLE_SIZE];

uint32_t *appine_map_code(size_t bytes, intptr_t *delta)
{
    /* 用 mremap 复制共享映射，不依赖 memfd */
    void *w = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (w != MAP_FAILED) {
        void *x = mremap(w, 0, bytes, MREMAP_MAYMOVE);
        if (x != MAP_FAILED && mprotect(x, bytes, PROT_READ | PROT_EXEC) == 0) {
            *delta = (intptr_t)w - (intptr_t)x;
            return x;
        }
        if (x != MAP_FAILED)
            munmap(x, bytes);
        munmap(w, bytes);
    }
    void *c = mmap(NULL, bytes, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *delta = 0;
    return c == MAP_FAILED ? NULL : c;
}

void appine_unmap_code(uint32_t *code, size_t bytes, intptr_t delta)
{
    munmap(code, bytes);
    if (delta)
        munmap((char *)code + delta, bytes);
}

static void init(void)
{
    cache = appine_map_code(CACHE_BYTES, &rw_delta);
}

bool appine_native_available(void)
{
    pthread_once(&once, init);
    return cache != NULL;
}

struct buf {
    uint32_t *p;
    size_t n;
};

static void put(struct buf *b, uint32_t insn)
{
    if (b->n < MAX_STUB_WORDS)
        b->p[b->n] = insn;
    b->n++;
}

/* x9 = cpu + off；off 小于 2^24 */
static void put_add(struct buf *b, unsigned rd, unsigned rn, size_t off)
{
    uint32_t hi = (uint32_t)(off >> 12), lo = (uint32_t)(off & 0xfff);
    unsigned src = rn;
    if (hi) {
        put(b, enc_add_imm(rd, src, hi, 1));
        src = rd;
    }
    if (lo || !hi)
        put(b, enc_add_imm(rd, src, lo, 0));
}

static size_t x_off(unsigned r)
{
    return offsetof(struct appine_cpu, x) + 8 * r;
}

static void generate(struct buf *b, uint32_t insn, const struct native_desc *d)
{
    enum { CPU = 0, ADDR = 1, VBASE = 9, TMP = 10 };
    size_t v_off = offsetof(struct appine_cpu, v);
    uint32_t out = insn;
    unsigned host[3];

    if (d->fp) {
        put(b, enc_stp_d_pre(8, 9, 31, -80));
        put(b, enc_stp_d(10, 11, 31, 16));
        put(b, enc_stp_d(12, 13, 31, 32));
        put(b, enc_stp_d(14, 15, 31, 48));
        put(b, enc_mrs_fpcr(TMP));
        put(b, enc_str_x(TMP, 31, 64));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct appine_cpu, fpcr)));
        put(b, enc_msr_fpcr(TMP));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct appine_cpu, fpsr)));
        put(b, enc_msr_fpsr(TMP));
        put(b, enc_ldr_x(TMP, CPU, offsetof(struct appine_cpu, nzcv)));
        put(b, enc_msr_nzcv(TMP));
        put_add(b, VBASE, CPU, v_off);
        for (unsigned q = 0; q < 32; q += 2)
            put(b, enc_ldp_q(q, q + 1, VBASE, (int)(16 * q)));
    }

    /* 通用寄存器操作数：31 表示 ZR，保持原样 */
    for (unsigned i = 0; i < d->n; i++) {
        const struct native_operand *op = &d->gpr[i];
        unsigned r = (insn >> op->pos) & 31;
        host[i] = 2 + 2 * i;
        if (r == 31)
            continue;
        out = (out & ~(31u << op->pos)) | host[i] << op->pos;
        if (!op->load)
            continue;
        put(b, enc_ldr_x(host[i], CPU, x_off(r)));
        if (op->pair) {
            if (r + 1 < 31)
                put(b, enc_ldr_x(host[i] + 1, CPU, x_off(r + 1)));
            else
                put(b, enc_movz(host[i] + 1, 0, 0));
        }
    }
    if (d->addr_pos != NATIVE_NO_ADDR)
        out = (out & ~(31u << d->addr_pos)) | (uint32_t)ADDR << d->addr_pos;

    put(b, out);

    for (unsigned i = 0; i < d->n; i++) {
        const struct native_operand *op = &d->gpr[i];
        unsigned r = (insn >> op->pos) & 31;
        if (r == 31 || !op->store)
            continue;
        put(b, enc_str_x(host[i], CPU, x_off(r)));
        if (op->pair && r + 1 < 31)
            put(b, enc_str_x(host[i] + 1, CPU, x_off(r + 1)));
    }

    if (d->fp) {
        for (unsigned q = 0; q < 32; q += 2)
            put(b, enc_stp_q(q, q + 1, VBASE, (int)(16 * q)));
        put(b, enc_mrs_fpsr(TMP));
        put(b, enc_str_x(TMP, CPU, offsetof(struct appine_cpu, fpsr)));
        put(b, enc_mrs_nzcv(TMP));
        put(b, enc_str_x(TMP, CPU, offsetof(struct appine_cpu, nzcv)));
        put(b, enc_ldr_x(TMP, 31, 64));
        put(b, enc_msr_fpcr(TMP));
        put(b, enc_ldp_d(14, 15, 31, 48));
        put(b, enc_ldp_d(12, 13, 31, 32));
        put(b, enc_ldp_d(10, 11, 31, 16));
        put(b, enc_ldp_d_post(8, 9, 31, 80));
    }
    put(b, enc_ret());
}

static size_t slot_index(uint32_t insn)
{
    return (size_t)((insn * UINT32_C(0x9e3779b1)) >> (32 - TABLE_BITS));
}

/* 持有锁时调用：查找已生成的桩 */
static native_fn lookup(uint32_t insn)
{
    size_t i = slot_index(insn);
    while (table[i].fn && table[i].insn != insn)
        i = (i + 1) & (TABLE_SIZE - 1);
    return table[i].fn;
}

/* 持有写锁时调用：生成桩，缓存写满时先整体清空 */
static void insert(uint32_t insn, const struct native_desc *desc)
{
    if (table_count >= TABLE_SIZE / 2 || used_words + MAX_STUB_WORDS > CACHE_BYTES / 4) {
        memset(table, 0, sizeof(table));
        table_count = 0;
        used_words = 0;
    }
    size_t i = slot_index(insn);
    while (table[i].fn)
        i = (i + 1) & (TABLE_SIZE - 1);
    uint32_t *code = cache + used_words;
    struct buf b = {(uint32_t *)((char *)code + rw_delta), 0}; /* 桩里没有相对跳转 */
    generate(&b, insn, desc);
    if (b.n > MAX_STUB_WORDS)
        abort(); /* 上界估计错误：属于引擎 bug */
    sync_code((void *)code, (void *)(code + b.n), rw_delta);
    used_words += b.n;
    table[i] = (struct slot){insn, (native_fn)(uintptr_t)code};
    table_count++;
}

bool appine_native_exec(uint32_t insn, const struct native_desc *desc, struct appine_cpu *cpu,
                        void *host_addr)
{
    if (!appine_native_available())
        return false;
    /* 桩在宿主地址上访存：地址错了（引擎的 bug）时宿主的故障兜底要释放这把锁 */
    for (;;) {
        pthread_rwlock_rdlock(&lock);
        appine_guard_hold(&lock, true);
        native_fn fn = lookup(insn);
        if (fn) {
            fn(cpu, host_addr);
            appine_guard_drop();
            pthread_rwlock_unlock(&lock);
            return true;
        }
        appine_guard_drop();
        pthread_rwlock_unlock(&lock);
        /* 换成写锁后重新查找：其他线程可能已经生成了同一个桩 */
        pthread_rwlock_wrlock(&lock);
        appine_guard_hold(&lock, true);
        if (!lookup(insn))
            insert(insn, desc);
        appine_guard_drop();
        pthread_rwlock_unlock(&lock);
    }
}

#else /* !NATIVE_SUPPORTED */

bool appine_native_available(void)
{
    return false;
}

bool appine_native_exec(uint32_t insn, const struct native_desc *desc, struct appine_cpu *cpu,
                        void *host_addr)
{
    (void)insn;
    (void)desc;
    (void)cpu;
    (void)host_addr;
    return false;
}

#endif
