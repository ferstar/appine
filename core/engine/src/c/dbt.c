/*
 * Tier 1：同 ISA 直通式 DBT（设计文档 §4.2）。
 *
 * 翻译后的代码运行期间的寄存器约定：
 *   - guest X0–X15、X19–X25、X29、X30 放在同号的宿主寄存器里，guest SP 放在宿主 X26；
 *   - 宿主 X28 指向 struct appine_cpu，宿主 X16、X17、X27 是临时寄存器；
 *   - 宿主 X18 是平台寄存器（Android 上是 bionic 给每个线程设的影子调用栈指针），
 *     翻译后的代码不碰它：宿主的信号处理函数随时可能用到它；
 *   - guest X16、X17、X18、X26、X27、X28 存放在 cpu 结构体里（"被占用的寄存器"），
 *     引用它们的指令改写后执行。SP 出现在 17% 的指令里（函数的序言、尾声），X26 在
 *     编译器生成的代码里是用得最少的被调用者保存寄存器（gcc 的 cc1 里不到 SP 的十分之一），
 *     所以把宿主 X26 让给 SP；
 *   - guest NZCV 就是宿主 NZCV，所以插入的代码一律不改动条件标志；
 *   - guest V0–V31、FPCR、FPSR 就是宿主的 V0–V31、FPCR、FPSR，SIMD&FP 数据处理
 *     指令原样执行。
 *
 * 访存指令改写成内联的 TLB 快速路径；TLB 未命中、跨页、SP 未对齐时退出到
 * 分发器，由 Tier 0 执行这一条指令（此时指令还没有产生任何副作用）。直接
 * 分支通过块链接直接跳转，间接分支先查跳转缓存。函数调用压影子返回栈，RET 先看
 * 栈顶（emit_call）。Tier 0 不认识的指令同样
 * 交给 Tier 0，保证两层的白名单完全一致（§4.4）。
 *
 * 翻译结果不会自己失效，见 appine_dbt_invalidate()。代码缓存按 W^X 双映射。
 */
#define _GNU_SOURCE /* -std=c17 下 madvise、mremap 及其标志需要它 */
#include <stddef.h>
#include <stdlib.h>

#include "emit.h"
#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#define DBT_SUPPORTED 1
#endif

#ifdef DBT_SUPPORTED

#include <pthread.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

#define CODE_CACHE_BYTES (UINT64_C(64) << 20)
#define MAX_BLOCK_INSNS 64
/* 单条 guest 指令最多展开成约 40 条宿主指令，再加上退出桩 */
#define MAX_BLOCK_WORDS (MAX_BLOCK_INSNS * 64)
#define MAX_STUBS (MAX_BLOCK_INSNS * 2 + 4)
#define MAX_FIXUPS (MAX_BLOCK_INSNS * 4 + 4)
/* 翻译表从 2^12 项开始，装到四分之三时加倍，最多 2^18 项：表按哈希散开，短命进程的
 * 几百个块要是放进最大的表，每个块都会碰到一页新的内存 */
#define TABLE_MIN_BITS 12
#define TABLE_BITS 18
#define TABLE_SIZE (1u << TABLE_BITS)
#define TABLE_LIMIT (TABLE_SIZE / 4 * 3)
#define JC_BITS 12
#define JC_SIZE (1u << JC_BITS)
#define JC_EMPTY UINT64_C(1) /* 未对齐的 pc 永远不会是合法的查找键 */
/* 进入例程的栈帧：x19–x30、宿主 FPCR、d8–d15 */
#define FRAME_BYTES 176

/* 宿主寄存器 */
#define CTX 28
#define T0 16
#define T1 17
#define T2 27
#define REG_SP 32 /* 伪寄存器编号：guest SP */
#define SP_HOST 26 /* 翻译后的代码运行期间存放 guest SP 的宿主寄存器 */

/* 退出原因（从翻译后的代码返回分发器）。R_CHAIN_FWD 是前向的直接跳转，链接时
 * 跳过目标块入口的 kick 检查：块的起始地址沿前向边严格递增，所以任何循环里至少
 * 有一条边会经过检查 */
enum { R_CHAIN = 1, R_CHAIN_FWD, R_LOOKUP, R_INTERP, R_SVC, R_KICK };

/* 退出桩之外的另一种桩：TLB 未命中时进入回填例程，回填失败再退出（R_INTERP） */
#define STUB_REFILL 0

/* 一个桩最多占的字数：回填桩 4 个字，后面接一个最长 7 个字的退出桩 */
#define MAX_STUB_WORDS 11

/* 块入口的 kick 检查占的字数 */
#define ENTRY_CHECK_WORDS 2

/* 退出原因的低 3 位是 R_*；链接桩在高位带上跳到它的条件分支相对桩的字数（0 表示没有） */
#define REASON_BITS 3
#define REASON_MASK ((1u << REASON_BITS) - 1)
_Static_assert(R_KICK <= REASON_MASK, "退出原因放在低 3 位");
_Static_assert(MAX_BLOCK_WORDS < 1u << (16 - REASON_BITS), "条件分支到桩的距离要放进 movz 的立即数");

#define SYSREG_NZCV 0xda10
#define SYSREG_TPIDR_EL0 0xde82
#define SYSREG_DCZID_EL0 0xd807
#define SYSREG_CNTFRQ_EL0 0xdf00
#define SYSREG_CNTVCT_EL0 0xdf02

/* DC ZVA 的块大小，与 Tier 0 报告的 DCZID_EL0 一致 */
#define DCZ_BLOCK 64
#define DCZID_VALUE 4

#define TLB_BITS 12
#define TLB_OFF offsetof(struct appine_cpu, tlb)
_Static_assert(APPINE_TLB_ENTRIES == 1u << TLB_BITS, "TLB 下标在生成的代码里固定为 12 位");
_Static_assert(sizeof(struct appine_tlb_entry) == 32, "TLB 表项在生成的代码里按 32 字节寻址");
_Static_assert(TLB_OFF % 16 == 0 && TLB_OFF + 16 <= 504, "TLB 必须能被 ldp 的 7 位缩放立即数寻址");
_Static_assert(offsetof(struct appine_cpu, dbt_rs) < TLB_OFF, "dbt_rs 也用 ldr/str 直接读写");
_Static_assert(offsetof(struct appine_cpu, dbt_jump_cache) < TLB_OFF,
               "生成的代码用 ldr/str 直接读写的字段必须位于 TLB 之前");

struct slot {
    uint64_t pc;
    uint32_t *code;
};

/*
 * 影子返回栈：每项是 (guest 返回地址, 宿主落点)，64 项的环形缓冲按自身大小对齐，压栈、
 * 弹栈只改栈顶指针的低 RS_SHIFT 位（bfxil，不碰条件标志）。
 */
#define RS_SHIFT 10
#define RS_ENTRIES ((1u << RS_SHIFT) / 16)

/* 作废翻译时按 guest 页找到其中开始的块（appine_dbt_invalidate） */
#define PAGE_BUCKET_BITS 14
#define INVALIDATE_MAX_PAGES 256 /* 超过这么多页就整体清空 */

struct block {
    uint64_t pc;
    uint32_t *code;  /* 已作废时为 NULL */
    uint32_t next;   /* 同一个桶里的上一个块（下标加 1），0 表示没有 */
    uint32_t words;  /* 翻译时的 guest 指令字在 words 里的位置（recheck 用） */
    uint32_t nguest; /* guest 指令条数 */
};

/* 每个块的 guest 指令字最多占多少字（按块数上限和每块的指令数上限） */
#define WORDS_LIMIT ((size_t)TABLE_LIMIT * MAX_BLOCK_INSNS)
/* 放回池里时最多保留多少个块：再多的实例多半属于 JIT 或大程序，核对的代价不划算 */
#define KEEP_MAX_BLOCKS 32768

struct appine_dbt {
    uint32_t *cache;    /* 执行用的地址；所有代码指针都指向这里 */
    intptr_t rw_delta;  /* 写入用的别名减去执行用的地址；直接映射成 RWX 时为 0 */
    size_t cache_words;
    size_t used_words;
    size_t routine_words;
    uint32_t *enter;
    uint32_t *exit;
    uint32_t *refill[2]; /* TLB 回填例程：读、写 */
    uint32_t *rs_miss;   /* 影子返回栈空项的落点：按 guest x30 查找 */
    uint64_t (*rs)[2];   /* 影子返回栈 */
    struct slot *table; /* guest pc → 翻译结果，开放寻址 */
    unsigned table_bits;
    uint32_t *table_used; /* 按插入顺序记下用过的位置，最多 TABLE_LIMIT 个 */
    size_t table_count;
    struct slot *jc; /* 间接分支的跳转缓存，直接映射 */
    bool jc_dirty;
    struct block *blocks; /* 与 table_used 一一对应，按翻译顺序记录 */
    uint32_t *words;      /* 各块翻译时的 guest 指令字，按需映射 */
    size_t words_used;
    /* 换了地址空间（exec 之后，或者从池里交给另一个线程），下次运行之前要核对 */
    bool stale;
    /* 运行时宿主出了故障（APPINE_EXIT_HOST_FAULT）：各张表可能只改了一半，不再使用 */
    bool poisoned;
    uint32_t *page_head;  /* guest 页的哈希 → blocks 里的链表头（下标加 1） */
    uint64_t generation; /* 每次 flush 加一，用来作废尚未回填的链接 */
    struct appine_dbt_stats stats;
    size_t high_words;       /* 代码缓存用到过的最大字数 */
    struct xlate *xlate;     /* 翻译时的工作区 */
    struct appine_cpu probe; /* 用来询问 Tier 0 是否认识某条指令 */
};

/* ---- 发射器 ---- */

enum fixup_kind { FIX_IMM19, FIX_IMM14 };

struct stub {
    unsigned reason; /* R_* 或 STUB_REFILL；R_LOOKUP 的目标 pc 已经在 T0 里 */
    uint64_t pc;
    uint32_t *addr;
    /* 回填桩：回填成功后重新做 TLB 检查的位置、访问的字节数和方向 */
    uint32_t *retry;
    unsigned bytes;
    bool write;
    /* 跳到这个桩的分支个数和其中一个的位置（发射桩时填写） */
    unsigned sites;
    const uint32_t *branch;
};

struct fixup {
    uint32_t *site;
    enum fixup_kind kind;
    unsigned stub;
};

/* 跳往例程的 b：拷贝翻译模板时要按新位置重新计算偏移 */
enum { TO_EXIT, TO_REFILL_READ, TO_REFILL_WRITE };
#define MAX_RELOCS (MAX_STUBS * 2 + 4)

struct xlate {
    struct appine_dbt *d;
    uint32_t *buf;
    size_t n;
    struct stub stubs[MAX_STUBS];
    unsigned nstubs;
    struct fixup fixups[MAX_FIXUPS];
    unsigned nfixups;
    uint32_t relocs[MAX_RELOCS]; /* 下标 << 2 | TO_* */
    unsigned nrelocs;
};

/* 代码的写入地址（W^X 双映射的可写别名） */
static uint32_t *rw(const struct appine_dbt *d, const uint32_t *p)
{
    return (uint32_t *)((uintptr_t)p + (uintptr_t)d->rw_delta);
}

/* 写入 [start, end) 之后让取指看到新代码 */
static void sync_range(const struct appine_dbt *d, const uint32_t *start, const uint32_t *end)
{
    sync_code((void *)start, (void *)end, d->rw_delta);
}

static void emit(struct xlate *x, uint32_t insn)
{
    if (x->n < MAX_BLOCK_WORDS)
        *rw(x->d, &x->buf[x->n]) = insn;
    x->n++;
}

static uint32_t *here(struct xlate *x)
{
    return &x->buf[x->n];
}

static uint32_t *routine(const struct appine_dbt *d, unsigned to)
{
    return to == TO_EXIT ? d->exit : d->refill[to - TO_REFILL_READ];
}

/* 跳往进入/退出例程或回填例程的 b */
static void emit_to_routine(struct xlate *x, unsigned to)
{
    if (x->nrelocs < MAX_RELOCS)
        x->relocs[x->nrelocs] = (uint32_t)x->n << 2 | to;
    x->nrelocs++; /* 溢出由 translate() 检查 */
    emit(x, enc_b((intptr_t)routine(x->d, to) - (intptr_t)here(x)));
}

static void emit_mov_const(struct xlate *x, unsigned rd, uint64_t v)
{
    if (v == 0) {
        emit(x, enc_movz(rd, 0, 0));
        return;
    }
    bool first = true;
    for (unsigned hw = 0; hw < 4; hw++) {
        uint32_t part = (uint32_t)(v >> (16 * hw)) & 0xffff;
        if (!part)
            continue;
        emit(x, first ? enc_movz(rd, part, hw) : enc_movk(rd, part, hw));
        first = false;
    }
}

/* rd = rn + imm，不改动条件标志；要求 |imm| < 2^24，rn 不能是 31 */
static void emit_add_const(struct xlate *x, unsigned rd, unsigned rn, int64_t imm)
{
    if (imm == 0) {
        if (rd != rn)
            emit(x, enc_mov(rd, rn));
        return;
    }
    bool sub = imm < 0;
    uint64_t a = sub ? (uint64_t)-imm : (uint64_t)imm;
    uint32_t hi = (uint32_t)(a >> 12), lo = (uint32_t)(a & 0xfff);
    unsigned src = rn;
    if (hi) {
        emit(x, sub ? enc_sub_imm(rd, src, hi, 1) : enc_add_imm(rd, src, hi, 1));
        src = rd;
    }
    if (lo)
        emit(x, sub ? enc_sub_imm(rd, src, lo, 0) : enc_add_imm(rd, src, lo, 0));
}

static bool is_stolen(unsigned r)
{
    return r == 16 || r == 17 || r == 18 || r == SP_HOST || r == 27 || r == 28;
}

static size_t guest_off(unsigned r)
{
    return r == REG_SP ? offsetof(struct appine_cpu, sp) : offsetof(struct appine_cpu, x) + 8 * r;
}

static void emit_load_guest(struct xlate *x, unsigned host, unsigned guest)
{
    emit(x, enc_ldr_x(host, CTX, guest_off(guest)));
}

static void emit_store_guest(struct xlate *x, unsigned host, unsigned guest)
{
    emit(x, enc_str_x(host, CTX, guest_off(guest)));
}

/* ---- 退出桩 ---- */

/*
 * 退出桩把 T0 = guest pc、T1 = 回填位置（或 0）、T2 = 退出原因交给退出例程。
 * 链接桩的第一条指令是 "adr x17, ."，链接时它会被改写成直接跳往目标块的 b；
 * branch 是唯一跳到这个桩的条件分支，链接时目标在它的偏移范围内就让它直接跳过去。
 */
static void emit_link_stub(struct xlate *x, unsigned reason, uint64_t pc, const uint32_t *branch)
{
    unsigned delta = branch ? (unsigned)(here(x) - branch) : 0;
    emit(x, reason == R_CHAIN || reason == R_CHAIN_FWD ? enc_adr_self(T1) : enc_movz(T1, 0, 0));
    if (reason != R_LOOKUP)
        emit_mov_const(x, T0, pc);
    emit(x, enc_movz(T2, reason | delta << REASON_BITS, 0));
    emit_to_routine(x, TO_EXIT);
}

static void emit_exit_stub(struct xlate *x, unsigned reason, uint64_t pc)
{
    emit_link_stub(x, reason, pc, NULL);
}

/* 发射一条条件跳转（偏移待回填），目标是一个退出桩 */
static void emit_to_stub(struct xlate *x, uint32_t insn, enum fixup_kind kind, unsigned reason, uint64_t pc)
{
    unsigned i;
    for (i = 0; i < x->nstubs; i++) {
        if (x->stubs[i].reason == reason && x->stubs[i].pc == pc)
            break;
    }
    if (i == x->nstubs) {
        if (x->nstubs < MAX_STUBS)
            x->stubs[i] = (struct stub){.reason = reason, .pc = pc};
        x->nstubs++; /* 溢出由 translate() 检查 */
    }
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups++] = (struct fixup){here(x), kind, i};
    else
        x->nfixups++; /* 记录溢出，translate() 会检查 */
    emit(x, insn);
}

/* pc 处的指令直接跳往 target */
static unsigned chain_reason(uint64_t pc, uint64_t target)
{
    return target > pc ? R_CHAIN_FWD : R_CHAIN;
}

/* TLB 未命中时跳往回填桩：每条访存指令一个，不和别的桩合并 */
static void emit_to_refill(struct xlate *x, uint64_t pc, uint32_t *retry, unsigned bytes, bool write)
{
    if (x->nstubs < MAX_STUBS)
        x->stubs[x->nstubs] =
            (struct stub){.reason = STUB_REFILL, .pc = pc, .retry = retry, .bytes = bytes, .write = write};
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups] = (struct fixup){here(x), FIX_IMM19, x->nstubs};
    x->nstubs++;
    x->nfixups++; /* 溢出由 translate() 检查 */
    emit(x, enc_cbnz_x(T1));
}

/*
 * 回填桩：T1 指向桩里的 "b 重试"，T2 = 字节数 - 1，然后进入回填例程。例程成功时
 * 跳回 T1，失败时跳到 T1 + 4，也就是紧接着的退出桩，由 Tier 0 执行这条指令。
 */
static void emit_refill_stub(struct xlate *x, const struct stub *s)
{
    emit(x, enc_adr(T1, 12));
    emit(x, enc_movz(T2, s->bytes - 1, 0));
    emit_to_routine(x, s->write ? TO_REFILL_WRITE : TO_REFILL_READ);
    emit(x, enc_b((intptr_t)s->retry - (intptr_t)here(x)));
    emit_exit_stub(x, R_INTERP, s->pc);
}

static bool fallback(struct xlate *x, uint64_t pc)
{
    x->d->stats.insns_fallback++;
    emit_exit_stub(x, R_INTERP, pc);
    return false;
}

/* 回填 b/cbz/cbnz/tbz/tbnz 的偏移 */
static void patch_branch(const struct appine_dbt *d, uint32_t *site, const uint32_t *target)
{
    uint32_t off = (uint32_t)(target - site), *w = rw(d, site);
    if ((*w & 0x7c000000) == 0x14000000)
        *w |= off & 0x3ffffff;
    else if ((*w & 0x7e000000) == 0x36000000)
        *w |= (off & 0x3fff) << 5;
    else
        *w |= (off & 0x7ffff) << 5;
}

/* 块内的局部跳转；溢出的块不会被使用（translate() 会检查），只是不能写出界 */
static void patch_local(struct xlate *x, uint32_t *site, const uint32_t *target)
{
    if (site < x->buf + MAX_BLOCK_WORDS)
        patch_branch(x->d, site, target);
}

/* ---- 数据处理：原样拷贝或改写被占用的寄存器 ---- */

struct operands {
    unsigned n;
    unsigned pos[4]; /* 字段的起始位 */
    bool sp[4];      /* 31 是否表示 SP */
    bool writes_rd;  /* pos[0] == 0 的 Rd 字段会被写 */
};

static void add_operand(struct operands *o, unsigned pos, bool sp)
{
    o->pos[o->n] = pos;
    o->sp[o->n] = sp;
    o->n++;
}

/* 列出数据处理指令的寄存器字段；Tier 0 已经确认过编码合法 */
static bool dp_operands(uint32_t insn, struct operands *o)
{
    o->n = 0;
    o->writes_rd = true;
    if (FIELD(insn, 28, 26) == 4) { /* 立即数 */
        switch (FIELD(insn, 25, 23)) {
        case 2: /* ADD/SUB (immediate) */
            add_operand(o, 0, !BIT(insn, 29));
            add_operand(o, 5, true);
            return true;
        case 4: /* 逻辑运算 (immediate) */
            add_operand(o, 0, FIELD(insn, 30, 29) != 3);
            add_operand(o, 5, false);
            return true;
        case 5: /* MOVN/MOVZ/MOVK */
            add_operand(o, 0, false);
            return true;
        case 6: /* 位域 */
            add_operand(o, 0, false);
            add_operand(o, 5, false);
            return true;
        case 7: /* EXTR */
            add_operand(o, 0, false);
            add_operand(o, 5, false);
            add_operand(o, 16, false);
            return true;
        default:
            return false;
        }
    }
    unsigned op2 = FIELD(insn, 24, 21);
    if (!BIT(insn, 28)) {
        bool extended = (op2 & 9) == 9;
        add_operand(o, 0, extended && !BIT(insn, 29));
        add_operand(o, 5, extended);
        add_operand(o, 16, false);
        return true;
    }
    switch (op2) {
    case 0: /* ADC/SBC */
    case 4: /* CSEL 系列 */
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        add_operand(o, 16, false);
        return true;
    case 2: /* CCMP/CCMN：没有 Rd，bits 4:0 是 nzcv */
        o->writes_rd = false;
        add_operand(o, 5, false);
        if (!BIT(insn, 11))
            add_operand(o, 16, false);
        return true;
    case 6:
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        if (!BIT(insn, 30))
            add_operand(o, 16, false);
        return true;
    default:
        if (!(op2 & 8))
            return false;
        add_operand(o, 0, false);
        add_operand(o, 5, false);
        add_operand(o, 16, false);
        add_operand(o, 10, false);
        return true;
    }
}

/* 被占用的 guest 寄存器换成临时寄存器：先从 cpu 结构体读入，执行后写回 Rd */
static bool emit_operands(struct xlate *x, uint32_t insn, const struct operands *o)
{
    static const unsigned pool[3] = {T0, T1, T2};
    unsigned guest[3], nmap = 0, rd_host = 0, rd_guest = 0;
    bool rd_stolen = false;
    uint32_t out = insn;
    bool renamed = false;
    for (unsigned i = 0; i < o->n; i++) {
        unsigned r = (insn >> o->pos[i]) & 31;
        if (r == 31 && o->sp[i]) { /* SP 就在宿主寄存器里，换个编号即可 */
            out = (out & ~(31u << o->pos[i])) | SP_HOST << o->pos[i];
            renamed = true;
            continue;
        }
        unsigned g = r;
        if (!is_stolen(g))
            continue;
        unsigned k;
        for (k = 0; k < nmap && guest[k] != g; k++)
            ;
        if (k == nmap) {
            if (nmap == 3)
                return false;
            guest[nmap++] = g;
        }
        out = (out & ~(31u << o->pos[i])) | pool[k] << o->pos[i];
        if (o->pos[i] == 0 && o->writes_rd) {
            rd_stolen = true;
            rd_host = pool[k];
            rd_guest = g;
        }
    }
    if (nmap == 0 && !renamed) {
        emit(x, insn);
        x->d->stats.insns_copied++;
        return true;
    }
    for (unsigned k = 0; k < nmap; k++)
        emit_load_guest(x, pool[k], guest[k]);
    emit(x, out);
    if (rd_stolen)
        emit_store_guest(x, rd_host, rd_guest);
    x->d->stats.insns_remapped++;
    return true;
}

/* ADR/ADRP：结果是编译期常量 */
static void emit_pcrel(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned rd = insn & 31;
    uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
    int64_t off = (int64_t)(imm << 43) >> 43;
    uint64_t v = BIT(insn, 31) ? (pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12) : pc + (uint64_t)off;
    x->d->stats.insns_remapped++;
    if (rd == 31)
        return;
    if (is_stolen(rd)) {
        emit_mov_const(x, T0, v);
        emit_store_guest(x, T0, rd);
    } else {
        emit_mov_const(x, rd, v);
    }
}

/* ---- 访存：内联 TLB 快速路径 ---- */

enum mem_mode { M_OFFSET, M_PRE, M_POST, M_REG, M_LITERAL };

struct memop {
    enum mem_mode mode;
    bool load, pair, prefetch;
    bool simd;      /* Rt/Rt2 是 SIMD&FP 寄存器 */
    bool structure; /* LD1–LD4/ST1–ST4：final 只缺 Rn */
    bool wb_reg;    /* 回写时加 Xm 而不是 imm */
    unsigned size;  /* 单个元素的 log2 字节数 */
    unsigned bytes; /* structure 时一共访问的字节数 */
    unsigned rt, rt2, rn, rm, option, shift;
    int64_t imm;    /* M_LITERAL 时是绝对地址 */
    uint32_t final; /* 最终访存指令的模板，寄存器字段为 0 */
};

static bool decode_memop(uint32_t insn, uint64_t pc, struct memop *m)
{
    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    *m = (struct memop){.rt = insn & 31, .rn = FIELD(insn, 9, 5), .size = size};
    if (BIT(insn, 26))
        return false;
    if ((insn & 0x3b000000) == 0x18000000) { /* LDR (literal)，bits 31:30 是 opc */
        static const unsigned sizes[3] = {2, 3, 2}, opcs[3] = {1, 1, 2};
        m->mode = M_LITERAL;
        m->imm = (int64_t)(pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4));
        if (size == 3) {
            m->prefetch = true;
            return true;
        }
        m->load = true;
        m->size = sizes[size];
        m->final = enc_ldst_regoff(sizes[size], opcs[size], 0, 0, 0);
        return true;
    }
    if ((insn & 0x3a000000) == 0x28000000) { /* LDP/STP/LDPSW */
        unsigned idx = FIELD(insn, 24, 23);
        m->pair = true;
        m->load = BIT(insn, 22);
        m->rt2 = FIELD(insn, 14, 10);
        m->size = size == 2 ? 3 : 2;
        m->imm = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (1 << m->size);
        m->mode = idx == 1 ? M_POST : idx == 3 ? M_PRE : M_OFFSET;
        m->final = (insn & 0xc0400000) | 0x29000000; /* 有符号偏移形式，偏移为 0 */
        return true;
    }
    if (BIT(insn, 29) != 1 || FIELD(insn, 28, 27) != 3)
        return false;
    m->load = opc != 0;
    m->prefetch = size == 3 && opc == 2;
    m->final = enc_ldst_regoff(size, opc, 0, 0, 0);
    if ((insn & 0x3b000000) == 0x39000000) {
        m->mode = M_OFFSET;
        m->imm = (int64_t)FIELD(insn, 21, 10) << size;
        return true;
    }
    if ((insn & 0x3b200000) == 0x38000000) {
        unsigned op = FIELD(insn, 11, 10);
        m->imm = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
        m->mode = op == 1 ? M_POST : op == 3 ? M_PRE : M_OFFSET;
        return true;
    }
    if ((insn & 0x3b200c00) == 0x38200800) {
        m->mode = M_REG;
        m->rm = FIELD(insn, 20, 16);
        m->option = FIELD(insn, 15, 13);
        m->shift = BIT(insn, 12) ? size : 0;
        return true;
    }
    return false;
}

/* LD1–LD4/ST1–ST4 一共访问的字节数；Tier 0 已经确认过编码合法 */
static unsigned structure_bytes(uint32_t insn)
{
    unsigned q = BIT(insn, 30), opcode = FIELD(insn, 15, 12), size = FIELD(insn, 11, 10);
    if (!BIT(insn, 24)) { /* 多个结构：opcode 对应 rpt × selem 个寄存器 */
        static const uint8_t regs[16] = {[0x0] = 4, [0x2] = 4, [0x4] = 3, [0x6] = 3,
                                         [0x7] = 1, [0x8] = 2, [0xa] = 2};
        return regs[opcode] * (q ? 16u : 8u);
    }
    unsigned selem = ((opcode >> 1 & 1) << 1 | BIT(insn, 21)) + 1, scale = opcode >> 2;
    if (scale == 3) /* LD1R–LD4R */
        scale = size;
    else if (scale == 2 && (size & 1))
        scale = 3;
    return selem << scale;
}

/* SIMD&FP 访存（bit 26 为 1）：和 Tier 0 的 appine_simd_ldst 译码一致 */
static bool decode_simd_memop(uint32_t insn, uint64_t pc, struct memop *m)
{
    unsigned size = FIELD(insn, 31, 30), opc = FIELD(insn, 23, 22);
    *m = (struct memop){.simd = true, .rt = insn & 31, .rn = FIELD(insn, 9, 5)};
    if ((insn & 0xbe000000) == 0x0c000000) { /* LD1–LD4/ST1–ST4 */
        m->structure = true;
        m->load = BIT(insn, 22);
        m->bytes = structure_bytes(insn);
        m->rm = FIELD(insn, 20, 16);
        m->mode = BIT(insn, 23) ? M_POST : M_OFFSET;
        if (m->mode == M_POST) {
            m->wb_reg = m->rm != 31;
            m->imm = m->bytes;
        }
        /* 改成不带回写的形式，Rn 换成宿主地址 */
        m->final = insn & ~(1u << 23 | 31u << 16 | 31u << 5);
        return true;
    }
    if ((insn & 0x3f000000) == 0x1c000000) { /* LDR (literal)：bits 31:30 是 opc */
        m->mode = M_LITERAL;
        m->imm = (int64_t)(pc + (uint64_t)(((int64_t)((uint64_t)FIELD(insn, 23, 5) << 45) >> 45) * 4));
        m->load = true;
        m->size = 2 + size;
        m->final = size == 2 ? enc_ldst_regoff(0, 3, 0, 0, 0) : enc_ldst_regoff(2 + size, 1, 0, 0, 0);
        m->final |= 1u << 26;
        return true;
    }
    if ((insn & 0x3e000000) == 0x2c000000) { /* LDP/STP/LDNP/STNP */
        unsigned idx = FIELD(insn, 24, 23);
        m->pair = true;
        m->load = BIT(insn, 22);
        m->rt2 = FIELD(insn, 14, 10);
        m->size = 2 + size;
        m->imm = ((int64_t)((uint64_t)FIELD(insn, 21, 15) << 57) >> 57) * (1 << m->size);
        m->mode = idx == 1 ? M_POST : idx == 3 ? M_PRE : M_OFFSET;
        m->final = (insn & 0xc0400000) | 0x2d000000; /* 有符号偏移形式，偏移为 0 */
        return true;
    }
    /* 单个寄存器：size = 00 且 opc = 1x 是 128 位的 Q 寄存器 */
    m->load = opc & 1;
    m->size = (size == 0 && (opc & 2)) ? 4 : size;
    m->final = enc_ldst_regoff(size, opc, 0, 0, 0) | 1u << 26;
    if ((insn & 0x3f000000) == 0x3d000000) {
        m->mode = M_OFFSET;
        m->imm = (int64_t)FIELD(insn, 21, 10) << m->size;
        return true;
    }
    if ((insn & 0x3f200000) == 0x3c000000) {
        unsigned op = FIELD(insn, 11, 10);
        m->imm = (int64_t)((uint64_t)FIELD(insn, 20, 12) << 55) >> 55;
        m->mode = op == 1 ? M_POST : op == 3 ? M_PRE : M_OFFSET;
        return true;
    }
    if ((insn & 0x3f200c00) == 0x3c200800) {
        m->mode = M_REG;
        m->rm = FIELD(insn, 20, 16);
        m->option = FIELD(insn, 15, 13);
        m->shift = BIT(insn, 12) ? m->size : 0;
        return true;
    }
    return false;
}

/*
 * T0 = guest 地址时查 TLB，命中后 T2 = addend。取最后一个字节所在页的表项，和
 * 第一个字节所在页比较：TLB 直接映射，跨页时第一页的页号不可能出现在下一页的
 * 表项里，所以这一次比较同时排除了跨页访问。未命中时进入回填例程。
 */
static void emit_tlb_check(struct xlate *x, uint64_t pc, unsigned bytes, bool write)
{
    unsigned last = T0;
    uint32_t *retry = here(x);
    if (bytes > 1) {
        emit(x, enc_add_imm(T1, T0, bytes - 1, 0));
        last = T1;
    }
    emit(x, enc_ubfx(T1, last, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(T1, CTX, T1, 5));
    emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT)); /* tag 是页号取反 */
    emit_to_refill(x, pc, retry, bytes, write);
}

static void emit_memop(struct xlate *x, const struct memop *m, uint64_t pc)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn;

    /* 1. guest 地址 → T0 */
    if (m->mode == M_LITERAL) {
        emit_mov_const(x, T0, (uint64_t)m->imm);
    } else {
        unsigned base = rn == REG_SP ? SP_HOST : rn;
        if (rn == REG_SP) { /* SP 必须 16 字节对齐 */
            emit(x, enc_and_low(T1, SP_HOST, 4));
            emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
        } else if (is_stolen(rn)) {
            emit_load_guest(x, T0, rn);
            base = T0;
        }
        switch (m->mode) {
        case M_REG: {
            unsigned idx = m->rm;
            if (is_stolen(idx)) {
                emit_load_guest(x, T1, idx);
                idx = T1;
            }
            emit(x, enc_add_ext(T0, base, idx, m->option, m->shift));
            break;
        }
        case M_POST:
            if (base != T0)
                emit(x, enc_mov(T0, base));
            break;
        default:
            emit_add_const(x, T0, base, m->imm);
            break;
        }
    }

    /* 2. TLB */
    unsigned bytes = m->structure ? m->bytes : (m->pair ? 2u : 1u) << m->size;
    emit_tlb_check(x, pc, bytes, !m->load);

    /* 3. 访问宿主内存 */
    if (m->structure) {
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        emit(x, m->final | T2 << 5);
    } else if (!m->pair) {
        if (m->simd || !is_stolen(m->rt)) {
            emit(x, m->final | T0 << 16 | T2 << 5 | m->rt);
        } else if (m->load) {
            emit(x, m->final | T0 << 16 | T2 << 5 | T1);
            emit_store_guest(x, T1, m->rt);
        } else {
            emit_load_guest(x, T1, m->rt);
            emit(x, m->final | T0 << 16 | T2 << 5 | T1);
        }
    } else {
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        bool s1 = !m->simd && is_stolen(m->rt), s2 = !m->simd && is_stolen(m->rt2);
        unsigned h1 = s1 ? T0 : m->rt, h2 = s2 ? T1 : m->rt2;
        if (!m->load) {
            if (s1)
                emit_load_guest(x, T0, m->rt);
            if (s2)
                emit_load_guest(x, T1, m->rt2);
        }
        emit(x, m->final | h2 << 10 | T2 << 5 | h1);
        if (m->load) {
            if (s1)
                emit_store_guest(x, T0, m->rt);
            if (s2)
                emit_store_guest(x, T1, m->rt2);
        }
    }

    /* 4. 基址回写 */
    if (m->mode == M_PRE || m->mode == M_POST) {
        unsigned inc = m->rm, dst = rn == REG_SP ? SP_HOST : is_stolen(rn) ? T0 : rn;
        if (m->wb_reg && is_stolen(inc)) {
            emit_load_guest(x, T1, inc);
            inc = T1;
        }
        if (dst == T0)
            emit_load_guest(x, T0, rn);
        if (m->wb_reg)
            emit(x, enc_add_lsl(dst, dst, inc, 0));
        else
            emit_add_const(x, dst, dst, m->imm);
        if (dst == T0)
            emit_store_guest(x, T0, rn);
    }
    x->d->stats.insns_memory++;
}

/* ---- 独占访问、acquire/release、LSE 原子操作（§4.5） ---- */

/*
 * 原子访问的地址，与 Tier 0 的 atomic_target 相同：要求自然对齐（所以不会跨页），
 * 以 SP 为基址时 SP 要 16 字节对齐，不满足时交给 Tier 0 报告对齐错误。结束时
 * T0 = guest 地址，T2 = 宿主地址。
 */
static void emit_atomic_address(struct xlate *x, unsigned rn, unsigned bytes, bool write, uint64_t pc)
{
    if (rn == 31) {
        emit(x, enc_mov(T0, SP_HOST));
        emit(x, enc_and_low(T1, T0, 4));
        emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    } else if (is_stolen(rn)) {
        emit_load_guest(x, T0, rn);
    } else {
        emit(x, enc_mov(T0, rn));
    }
    if (bytes > 1) {
        emit(x, enc_and_low(T1, T0, (unsigned)__builtin_ctz(bytes)));
        emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    }
    emit_tlb_check(x, pc, bytes, write);
    emit(x, enc_add_lsl(T2, T2, T0, 0));
    x->d->stats.insns_memory++;
}

struct atomic_operand {
    unsigned pos;
    bool in, out;
};

/* 在宿主地址 T2 上原样执行：被占用的寄存器操作数换成 T0、T1 */
static void emit_on_host_address(struct xlate *x, uint32_t insn, const struct atomic_operand *ops,
                                 unsigned n)
{
    static const unsigned pool[2] = {T0, T1};
    uint32_t out = (insn & ~(31u << 5)) | T2 << 5;
    for (unsigned i = 0; i < n; i++) {
        unsigned r = (insn >> ops[i].pos) & 31;
        if (!is_stolen(r))
            continue;
        out = (out & ~(31u << ops[i].pos)) | pool[i] << ops[i].pos;
        if (ops[i].in)
            emit_load_guest(x, pool[i], r);
    }
    emit(x, out);
    for (unsigned i = 0; i < n; i++) {
        unsigned r = (insn >> ops[i].pos) & 31;
        if (is_stolen(r) && ops[i].out)
            emit_store_guest(x, pool[i], r);
    }
}

/* 写 STXR 的状态：0 成功，1 失败 */
static void emit_status(struct xlate *x, unsigned rs, unsigned v)
{
    if (rs == 31)
        return;
    unsigned h = is_stolen(rs) ? T0 : rs;
    emit(x, enc_movz(h, v, 0));
    if (h == T0)
        emit_store_guest(x, T0, rs);
}

/*
 * LDXR/STXR 改写成比较并交换（与 Tier 0 相同）：LDXR 记下地址、长度和读到的值；
 * STXR 先确认独占标记是同一地址、同一长度，再在宿主地址上用 ldaxr/stlxr 做 CAS，
 * 两条指令之间只有寄存器运算，宿主的独占监视器不会被我们自己的访存清掉。
 */
static bool translate_exclusive(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned size = FIELD(insn, 31, 30), bytes = 1u << size;
    unsigned rs = FIELD(insn, 20, 16), rn = FIELD(insn, 9, 5), rt = insn & 31;
    size_t addr = offsetof(struct appine_cpu, excl_addr), value = offsetof(struct appine_cpu, excl_value),
           len = offsetof(struct appine_cpu, excl_size);

    if (BIT(insn, 22)) { /* LDXR/LDAXR */
        unsigned h = (rt == 31 || is_stolen(rt)) ? T1 : rt;
        emit_atomic_address(x, rn, bytes, false, pc);
        emit(x, BIT(insn, 15) ? enc_ldar(size, h, T2) : enc_ldr_size(size, h, T2));
        emit(x, enc_str_x(T0, CTX, addr));
        emit(x, enc_str_x(h, CTX, value));
        emit(x, enc_movz(T0, bytes, 0));
        emit(x, enc_str_w(T0, CTX, len));
        if (h == T1 && rt != 31)
            emit_store_guest(x, T1, rt);
        return true;
    }

    if (is_stolen(rt)) /* CAS 循环里没有空闲的寄存器放要写的值 */
        return false;
    emit_atomic_address(x, rn, bytes, true, pc);
    uint32_t *mismatch[2], *differ, *done;
    emit(x, enc_ldr_x(T1, CTX, addr));
    emit(x, enc_eor_lsr(T1, T1, T0, 0));
    mismatch[0] = here(x);
    emit(x, enc_cbnz_x(T1));
    emit(x, enc_ldr_w(T1, CTX, len));
    emit(x, enc_sub_imm(T1, T1, bytes, 0));
    mismatch[1] = here(x);
    emit(x, enc_cbnz_x(T1));
    uint32_t *loop = here(x);
    emit(x, enc_ldr_x(T1, CTX, value));
    emit(x, enc_ldaxr(size, T0, T2));
    emit(x, enc_eor_lsr(T0, T0, T1, 0));
    differ = here(x);
    emit(x, enc_cbnz_x(T0));
    emit(x, enc_stlxr(size, T1, rt, T2));
    uint32_t *again = here(x);
    emit(x, enc_cbnz_x(T1));
    patch_local(x, again, loop);
    emit_status(x, rs, 0);
    done = here(x);
    emit(x, enc_b(0));
    patch_local(x, differ, here(x));
    emit(x, enc_clrex());
    patch_local(x, mismatch[0], here(x));
    patch_local(x, mismatch[1], here(x));
    emit_status(x, rs, 1);
    patch_local(x, done, here(x));
    emit(x, enc_str_w(31, CTX, len));
    return true;
}

static bool is_atomic(uint32_t insn)
{
    return (insn & 0x3f000000) == 0x08000000 || ((insn & 0x3f200c00) == 0x38200000 && !BIT(insn, 26));
}

/* 编码已由 tier0_defined 确认合法；返回 false 时还没有发射任何指令 */
static bool translate_atomic(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned size = FIELD(insn, 31, 30), rn = FIELD(insn, 9, 5);
    if ((insn & 0x3f000000) == 0x08000000) {
        unsigned o2 = BIT(insn, 23), o1 = BIT(insn, 21);
        if (!o2 && !o1)
            return translate_exclusive(x, insn, pc);
        if (!o2) { /* LDXP/STXP（size ≥ 2）留给 Tier 0；CASP 的两对寄存器不能被占用 */
            unsigned rs = FIELD(insn, 20, 16), rt = insn & 31;
            if (size >= 2 || is_stolen(rs) || is_stolen(rs + 1) || is_stolen(rt) || is_stolen(rt + 1))
                return false;
            emit_atomic_address(x, rn, 8u << size, true, pc);
            emit(x, (insn & ~(31u << 5)) | T2 << 5);
            return true;
        }
        if (!o1) { /* LDAR/STLR */
            bool load = BIT(insn, 22);
            struct atomic_operand rt = {0, !load, load};
            emit_atomic_address(x, rn, 1u << size, !load, pc);
            emit_on_host_address(x, insn, &rt, 1);
            return true;
        }
        /* CAS：Rs 既是比较值也接收旧值 */
        struct atomic_operand ops[2] = {{16, true, true}, {0, true, false}};
        emit_atomic_address(x, rn, 1u << size, true, pc);
        emit_on_host_address(x, insn, ops, 2);
        return true;
    }
    /* LD<op>、SWP */
    struct atomic_operand ops[2] = {{16, true, false}, {0, false, true}};
    emit_atomic_address(x, rn, 1u << size, true, pc);
    emit_on_host_address(x, insn, ops, 2);
    return true;
}

/* ---- 分支与系统指令 ---- */

static int64_t sext(uint64_t v, unsigned bits)
{
    return (int64_t)(v << (64 - bits)) >> (64 - bits);
}

/*
 * 函数调用（BL、BLR）：宿主 bl 跳过紧随其后的"落点"，宿主的返回地址栈因此记下落点；
 * 落点是链接到 guest 返回地址处的块的链接桩。bl 的目标把 (guest 返回地址, 落点) 压进
 * 影子返回栈，把 guest x30 设成返回地址。调用方接着发射跳往被调函数的代码（T0 不变）。
 *
 * RET 看到栈顶的 guest 地址等于 x30，就弹栈并用宿主 ret 跳回落点，由宿主的返回地址栈
 * 预测；不等时（longjmp、信号处理函数、协程切换）照常查跳转缓存。栈里的每一项都跳往
 * 继续执行它记的 guest 地址的代码（空项跳往 rs_miss），所以 guest 把 x30 设成什么都不会
 * 跳错。
 */
static void emit_call(struct xlate *x, uint64_t pc)
{
    size_t rs = offsetof(struct appine_cpu, dbt_rs);
    uint32_t *site = here(x);
    emit(x, enc_bl(0));
    emit_exit_stub(x, R_CHAIN, pc + 4); /* 落点 */
    patch_local(x, site, here(x));
    emit(x, enc_ldr_x(T1, CTX, rs));
    emit(x, enc_add_imm(T2, T1, 16, 0));
    emit(x, enc_bfxil_low(T1, T2, RS_SHIFT));
    emit(x, enc_str_x(T1, CTX, rs));
    emit_mov_const(x, T2, pc + 4);
    emit(x, enc_stp_x(T2, 30, T1, 0));
    emit(x, enc_mov(30, T2));
}

/* RET：栈顶匹配时弹栈并跳回落点，不匹配时落到后面的跳转缓存查找 */
static void emit_predicted_return(struct xlate *x)
{
    size_t rs = offsetof(struct appine_cpu, dbt_rs);
    emit(x, enc_ldr_x(T1, CTX, rs));
    emit(x, enc_ldp_x(T0, T2, T1, 0));
    emit(x, enc_eor_lsr(T0, T0, 30, 0));
    uint32_t *miss = here(x);
    emit(x, enc_cbnz_x(T0));
    emit(x, enc_sub_imm(T0, T1, 16, 0));
    emit(x, enc_bfxil_low(T1, T0, RS_SHIFT));
    emit(x, enc_str_x(T1, CTX, rs));
    emit(x, enc_ret_reg(T2));
    patch_local(x, miss, here(x));
}

/* 把条件分支的寄存器操作数换成可直接读取的宿主寄存器 */
static uint32_t branch_operand(struct xlate *x, uint32_t insn)
{
    unsigned rt = insn & 31;
    if (!is_stolen(rt))
        return insn;
    emit_load_guest(x, T0, rt);
    return (insn & ~31u) | T0;
}

static bool translate_branch_sys(struct xlate *x, uint32_t insn, uint64_t pc)
{
    if ((insn & 0x7c000000) == 0x14000000) { /* B / BL */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 25, 0), 26) * 4);
        if (BIT(insn, 31))
            emit_call(x, pc);
        emit_exit_stub(x, chain_reason(pc, target), target);
        return false;
    }
    if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000) { /* B.cond/CBZ/CBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4);
        /* B.cond 的 bits 4:0 是条件码，原样保留；CBZ/CBNZ 的是寄存器 */
        uint32_t base = BIT(insn, 30) ? insn : branch_operand(x, insn);
        emit_to_stub(x, base & ~(0x7ffffu << 5), FIX_IMM19, chain_reason(pc, target), target);
        return true; /* 不跳转的一侧接着翻译，跳转的一侧是侧出口 */
    }
    if ((insn & 0x7e000000) == 0x36000000) { /* TBZ/TBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 18, 5), 14) * 4);
        uint32_t base = branch_operand(x, insn) & ~(0x3fffu << 5);
        emit_to_stub(x, base, FIX_IMM14, chain_reason(pc, target), target);
        return true;
    }
    if (insn == 0xd4000001) { /* SVC #0 */
        emit_exit_stub(x, R_SVC, pc + 4);
        return false;
    }
    if ((insn & 0xfffff01f) == 0xd503201f) /* HINT：与 Tier 0 一样当作 NOP */
        return true;
    if ((insn & 0xfffff01f) == 0xd503301f) { /* 屏障：白名单已由 Tier 0 确认 */
        if (FIELD(insn, 7, 5) == 2) { /* CLREX：清掉 guest 的独占标记 */
            emit(x, enc_str_w(31, CTX, offsetof(struct appine_cpu, excl_size)));
            x->d->stats.insns_remapped++;
            return true;
        }
        emit(x, insn);
        x->d->stats.insns_copied++;
        return true;
    }
    if ((insn & 0xffd00000) == 0xd5100000) { /* MRS/MSR */
        bool read = BIT(insn, 21);
        unsigned rt = insn & 31;
        switch (FIELD(insn, 20, 5)) {
        case SYSREG_NZCV:
        case SYSREG_CNTFRQ_EL0: /* 计时器只读，写入已被 tier0_defined 拒绝；原样在宿主上读 */
        case SYSREG_CNTVCT_EL0: {
            struct operands o = {.n = 1, .pos = {0}, .sp = {false}, .writes_rd = read};
            return emit_operands(x, insn, &o) || fallback(x, pc);
        }
        case SYSREG_TPIDR_EL0: {
            size_t off = offsetof(struct appine_cpu, tpidr_el0);
            unsigned h = is_stolen(rt) ? T0 : rt;
            if (read) {
                if (rt != 31) {
                    emit(x, enc_ldr_x(h, CTX, off));
                    if (h == T0)
                        emit_store_guest(x, T0, rt);
                }
            } else {
                if (h == T0)
                    emit_load_guest(x, T0, rt);
                emit(x, enc_str_x(h, CTX, off));
            }
            x->d->stats.insns_remapped++;
            return true;
        }
        case SYSREG_DCZID_EL0: /* 只读，写入已被 tier0_defined 拒绝 */
            if (rt != 31) {
                unsigned h = is_stolen(rt) ? T0 : rt;
                emit(x, enc_movz(h, DCZID_VALUE, 0));
                if (h == T0)
                    emit_store_guest(x, T0, rt);
            }
            x->d->stats.insns_remapped++;
            return true;
        default:
            return fallback(x, pc);
        }
    }
    if ((insn & 0xffffffe0) == 0xd50b7420) { /* DC ZVA：清零地址所在的整块，块总在一页之内 */
        unsigned rt = insn & 31;
        if (rt == 31)
            emit(x, enc_movz(T0, 0, 0));
        else if (is_stolen(rt))
            emit_load_guest(x, T0, rt);
        else
            emit(x, enc_mov(T0, rt));
        emit(x, enc_and_clear_low(T0, T0, 6));
        _Static_assert(DCZ_BLOCK == 64, "DC ZVA 的块大小");
        emit_tlb_check(x, pc, DCZ_BLOCK, true);
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        for (int off = 0; off < DCZ_BLOCK; off += 16)
            emit(x, enc_stp_x(31, 31, T2, off));
        x->d->stats.insns_memory++;
        return true;
    }
    uint32_t reg_branch = insn & 0xfffffc1f;
    if (reg_branch == 0xd61f0000 || reg_branch == 0xd63f0000 || reg_branch == 0xd65f0000) {
        unsigned rn = FIELD(insn, 9, 5); /* BR/BLR/RET：目标先放进 T0，再查跳转缓存 */
        if (reg_branch == 0xd65f0000 && rn == 30)
            emit_predicted_return(x);
        if (rn == 31)
            emit(x, enc_movz(T0, 0, 0));
        else if (is_stolen(rn))
            emit_load_guest(x, T0, rn);
        else
            emit(x, enc_mov(T0, rn));
        if (reg_branch == 0xd63f0000)
            emit_call(x, pc);
        emit(x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, dbt_jump_cache)));
        emit(x, enc_ubfx(T2, T0, 2, JC_BITS));
        emit(x, enc_add_lsl(T1, T1, T2, 4));
        emit(x, enc_ldp_x(T2, T1, T1, 0));
        emit(x, enc_eor_lsr(T2, T2, T0, 0));
        emit_to_stub(x, enc_cbnz_x(T2), FIX_IMM19, R_LOOKUP, 0);
        emit(x, enc_br(T1));
        return false;
    }
    return fallback(x, pc);
}

/* 询问 Tier 0 是否认识这条指令（§4.4：两层的白名单必须一致） */
static bool tier0_defined(struct appine_dbt *d, uint32_t insn, uint64_t pc)
{
    d->probe.pc = pc;
    d->probe.sp = 0;
    return appine_interp_exec(&d->probe, insn) != APPINE_EXIT_UNDEF;
}

/*
 * SIMD&FP 数据处理：白名单判定与 Tier 0 共用 appine_simd_dp_decode。V 寄存器
 * 不需要改写，只有通用寄存器操作数可能是被占用的寄存器。不经过 tier0_defined，
 * 免得为每条指令生成一个原生桩。
 */
static bool translate_simd_dp(struct xlate *x, uint32_t insn)
{
    struct native_desc nd;
    if (!appine_has(APPINE_HWCAP_FP | APPINE_HWCAP_ASIMD) || !appine_native_available() ||
        !appine_simd_dp_decode(insn, &nd))
        return false;
    struct operands o = {.n = 0, .writes_rd = false};
    for (unsigned i = 0; i < nd.n; i++) {
        add_operand(&o, nd.gpr[i].pos, false);
        if (nd.gpr[i].pos == 0 && nd.gpr[i].store)
            o.writes_rd = true;
    }
    return emit_operands(x, insn, &o);
}

/* 翻译一条指令；返回 false 表示块到此结束 */
static bool translate_insn(struct xlate *x, uint32_t insn, uint64_t pc)
{
    if ((FIELD(insn, 28, 25) & 7) == 7)
        return translate_simd_dp(x, insn) || fallback(x, pc);
    if (!tier0_defined(x->d, insn, pc))
        return fallback(x, pc);
    switch (FIELD(insn, 28, 25)) {
    case 0x8:
    case 0x9:
        if (FIELD(insn, 28, 24) == 0x10) {
            emit_pcrel(x, insn, pc);
            return true;
        }
        /* fallthrough */
    case 0x5:
    case 0xd: {
        struct operands o;
        if (!dp_operands(insn, &o) || !emit_operands(x, insn, &o))
            return fallback(x, pc);
        return true;
    }
    case 0x4:
    case 0x6:
    case 0xc:
    case 0xe: {
        if (is_atomic(insn))
            return translate_atomic(x, insn, pc) || fallback(x, pc);
        struct memop m;
        if (!(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m)))
            return fallback(x, pc);
        if (!m.prefetch)
            emit_memop(x, &m, pc);
        return true;
    }
    case 0xa:
    case 0xb:
        return translate_branch_sys(x, insn, pc);
    default:
        return fallback(x, pc);
    }
}

/* ---- 翻译表与代码缓存 ---- */

/*
 * 实例和它的几张表都用 mmap 分配，保证是按需提交的清零页：每个 guest 线程一个实例，
 * 表按最大的规模分配，大部分页从来不碰。calloc 不一定这样（glibc 释放过同样大的块
 * 之后会调高 mmap 的阈值，改从堆里分配并逐字节清零）。
 */
static void *zalloc(size_t bytes)
{
    void *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? NULL : p;
}

static void zfree(void *p, size_t bytes)
{
    if (p)
        munmap(p, bytes);
}

#define TABLE_BYTES(bits) (sizeof(struct slot) << (bits))

static size_t table_index(const struct appine_dbt *d, uint64_t pc)
{
    return (size_t)(((pc >> 2) * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - d->table_bits));
}

static size_t table_mask(const struct appine_dbt *d)
{
    return ((size_t)1 << d->table_bits) - 1;
}

static uint32_t *table_lookup(const struct appine_dbt *d, uint64_t pc)
{
    for (size_t i = table_index(d, pc);; i = (i + 1) & table_mask(d)) {
        if (!d->table[i].code)
            return NULL;
        if (d->table[i].pc == pc)
            return d->table[i].code;
    }
}

static unsigned page_bucket(uint64_t page)
{
    return (unsigned)((page * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - PAGE_BUCKET_BITS));
}

/* 放进一个 2^bits 项的新表，表项按原来的插入顺序重新放（墓碑也照样放，table_used
 * 和 blocks 的下标保持对应）；分配失败时返回 false，原来的表不变 */
static bool table_resize(struct appine_dbt *d, unsigned bits)
{
    struct slot *table = zalloc(TABLE_BYTES(bits));
    if (!table)
        return false;
    struct slot *old = d->table;
    unsigned old_bits = d->table_bits;
    d->table = table;
    d->table_bits = bits;
    for (size_t k = 0; k < d->table_count; k++) {
        struct slot s = old[d->table_used[k]];
        size_t i = table_index(d, s.pc);
        while (table[i].code)
            i = (i + 1) & table_mask(d);
        table[i] = s;
        d->table_used[k] = (uint32_t)i;
    }
    zfree(old, TABLE_BYTES(old_bits));
    return true;
}

static void table_insert(struct appine_dbt *d, uint64_t pc, uint32_t *code, const uint32_t *guest,
                         uint32_t nguest)
{
    size_t i = table_index(d, pc);
    while (d->table[i].code)
        i = (i + 1) & table_mask(d);
    d->table[i] = (struct slot){pc, code};
    unsigned h = page_bucket(pc >> APPINE_PAGE_SHIFT);
    memcpy(d->words + d->words_used, guest, nguest * 4);
    d->blocks[d->table_count] =
        (struct block){pc, code, d->page_head[h], (uint32_t)d->words_used, nguest};
    d->words_used += nguest;
    d->table_used[d->table_count++] = (uint32_t)i;
    d->page_head[h] = (uint32_t)d->table_count;
}

/* 从翻译表和跳转缓存里删掉 pc。翻译表开放寻址，删掉的位置留一个永远不会命中的墓碑 */
static void table_remove(struct appine_dbt *d, uint64_t pc)
{
    for (size_t i = table_index(d, pc); d->table[i].code; i = (i + 1) & table_mask(d)) {
        if (d->table[i].pc == pc) {
            d->table[i].pc = JC_EMPTY;
            break;
        }
    }
    struct slot *j = &d->jc[(pc >> 2) & (JC_SIZE - 1)];
    if (j->pc == pc)
        *j = (struct slot){JC_EMPTY, NULL};
}

static void clear_jump_cache(struct appine_dbt *d)
{
    for (unsigned i = 0; i < JC_SIZE; i++)
        d->jc[i] = (struct slot){JC_EMPTY, NULL};
    d->jc_dirty = false;
}

/* 影子返回栈全部换成空项：落点所在的代码就要被覆盖了 */
static void rs_reset(struct appine_dbt *d)
{
    for (unsigned i = 0; i < RS_ENTRIES; i++) {
        d->rs[i][0] = 1; /* 未对齐，不会是 RET 的目标；万一匹配，rs_miss 也照样查找 */
        d->rs[i][1] = (uint64_t)(uintptr_t)d->rs_miss;
    }
}

void appine_dbt_flush(struct appine_dbt *d)
{
    if (d->used_words > d->high_words)
        d->high_words = d->used_words;
    for (size_t i = 0; i < d->table_count; i++) {
        d->table[d->table_used[i]] = (struct slot){0, NULL};
        d->page_head[page_bucket(d->blocks[i].pc >> APPINE_PAGE_SHIFT)] = 0;
    }
    d->table_count = 0;
    d->words_used = 0;
    d->stale = false;
    rs_reset(d);
    if (d->jc_dirty)
        clear_jump_cache(d);
    d->used_words = d->routine_words;
    d->generation++;
    d->stats.flushes++;
}

/*
 * 让作废的块不再被执行：入口（kick 检查之前和之后两处）改成跳往一个转发桩。转发桩
 * 以 R_CHAIN 退出，回填位置就是旧入口，所以重新翻译之后旧入口直接跳往新的块，
 * 别的块链接到旧入口的跳转不用找出来改写。代码缓存放不下转发桩时返回 false。
 */
static bool retire_block(struct appine_dbt *d, struct block *b)
{
    if (d->used_words + 16 > d->cache_words)
        return false;
    struct xlate x = {.d = d, .buf = d->cache + d->used_words};
    emit_mov_const(&x, T1, (uint64_t)(uintptr_t)b->code);
    emit_mov_const(&x, T0, b->pc);
    emit(&x, enc_movz(T2, R_CHAIN, 0));
    emit(&x, enc_b((intptr_t)d->exit - (intptr_t)here(&x)));
    sync_range(d, x.buf, x.buf + x.n);
    d->used_words += x.n;
    *rw(d, b->code) = enc_b((intptr_t)x.buf - (intptr_t)b->code);
    *rw(d, b->code + ENTRY_CHECK_WORDS) = enc_b(-ENTRY_CHECK_WORDS * 4);
    sync_range(d, b->code, b->code + ENTRY_CHECK_WORDS + 1);
    table_remove(d, b->pc);
    b->code = NULL;
    d->stats.invalidated++;
    return true;
}

void appine_dbt_invalidate(struct appine_dbt *d, uint64_t start, uint64_t len)
{
    if (!len)
        return;
    uint64_t end = start + len - 1, first = start >> APPINE_PAGE_SHIFT, last = end >> APPINE_PAGE_SHIFT;
    if (end < start || last - first >= INVALIDATE_MAX_PAGES) {
        appine_dbt_flush(d);
        return;
    }
    for (uint64_t page = first; page <= last; page++) {
        for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
            struct block *b = &d->blocks[i - 1];
            if (b->code && b->pc >> APPINE_PAGE_SHIFT == page && !retire_block(d, b)) {
                appine_dbt_flush(d);
                return;
            }
        }
    }
}

/* ---- 翻译模板：所有实例共享（§4.2） ---- */

/*
 * 一个块的翻译只取决于它的 guest pc 和翻译过的指令字（以及全局的特性集合），与
 * 进程、映射无关。每翻译一个块就把结果存成模板；别的实例（另一个线程、fork 出来的
 * 子进程、又一次 exec 的同一个程序）要翻译同一个 pc 时，先比对模板记下的指令字和
 * 自己内存里的是否相同，相同就把模板拷进自己的代码缓存，只重新计算跳往例程的几条
 * b。块之间的链接在各自的拷贝里回填，所以不同进程在同一地址映射了不同的代码也
 * 互不影响。模板区写满时整体丢弃，因为实例只拷贝模板、从不引用它。
 */
#define TEMPLATE_BYTES (UINT64_C(32) << 20)
#define TEMPLATE_BUCKET_BITS 16
/* 同一个 pc 最多保留几个版本：JIT 反复改写同一处代码时只留最新的几个 */
#define TEMPLATES_PER_PC 4

struct template {
    struct template *next;
    uint64_t pc;
    uint32_t nguest, nhost, nrelocs;
    uint32_t copied, remapped, memory, fallback; /* 统计，拷贝时累加 */
    uint32_t words[];                            /* guest 指令字、宿主指令字、重定位 */
};

static struct {
    pthread_mutex_t lock;
    uint8_t *arena; /* 第一次使用时映射，写满时清空 */
    size_t used;
    struct template *buckets[1u << TEMPLATE_BUCKET_BITS];
} templates = {.lock = PTHREAD_MUTEX_INITIALIZER};

static struct template **template_bucket(uint64_t pc)
{
    return &templates.buckets[((pc >> 2) * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - TEMPLATE_BUCKET_BITS)];
}

/* 调用方持有锁 */
static struct template *template_find(uint64_t pc, const uint32_t *guest)
{
    for (struct template *t = *template_bucket(pc); t; t = t->next) {
        if (t->pc == pc && memcmp(t->words, guest, t->nguest * 4) == 0)
            return t;
    }
    return NULL;
}

/* 按模板在代码缓存的当前位置生成一个块，`nguest` 是它的 guest 指令条数；没有合适的
 * 模板时返回 NULL */
static uint32_t *template_copy(struct appine_dbt *d, uint64_t pc, const uint32_t *guest,
                               uint32_t *nguest)
{
    uint32_t *code = d->cache + d->used_words;
    pthread_mutex_lock(&templates.lock);
    appine_guard_hold(&templates.lock, false);
    struct template *t = template_find(pc, guest);
    if (t) {
        const uint32_t *host = t->words + t->nguest, *relocs = host + t->nhost;
        memcpy(rw(d, code), host, t->nhost * 4);
        for (uint32_t i = 0; i < t->nrelocs; i++) {
            uint32_t *site = code + (relocs[i] >> 2);
            *rw(d, site) = enc_b((intptr_t)routine(d, relocs[i] & 3) - (intptr_t)site);
        }
        d->used_words += t->nhost;
        d->stats.insns += t->nguest;
        d->stats.insns_copied += t->copied;
        d->stats.insns_remapped += t->remapped;
        d->stats.insns_memory += t->memory;
        d->stats.insns_fallback += t->fallback;
        d->stats.blocks_shared++;
        *nguest = t->nguest;
    }
    appine_guard_drop();
    pthread_mutex_unlock(&templates.lock);
    return t ? code : NULL;
}

/* 把刚翻译好、还没有回填过链接的块存成模板 */
static void template_store(uint64_t pc, const uint32_t *guest, uint32_t nguest, const struct xlate *x,
                           const struct appine_dbt_stats *before)
{
    const struct appine_dbt_stats *after = &x->d->stats;
    size_t bytes = sizeof(struct template) + 4 * (nguest + x->n + x->nrelocs);
    bytes = (bytes + 7) & ~(size_t)7;
    pthread_mutex_lock(&templates.lock);
    appine_guard_hold(&templates.lock, false);
    if (!templates.arena) {
        void *p = mmap(NULL, TEMPLATE_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        templates.arena = p == MAP_FAILED ? NULL : p;
    }
    if (templates.arena && !template_find(pc, guest)) {
        if (templates.used + bytes > TEMPLATE_BYTES) {
            memset(templates.buckets, 0, sizeof(templates.buckets));
            templates.used = 0;
        }
        struct template *t = (struct template *)(templates.arena + templates.used);
        templates.used += bytes;
        *t = (struct template){
            .pc = pc,
            .nguest = nguest,
            .nhost = (uint32_t)x->n,
            .nrelocs = x->nrelocs,
            .copied = (uint32_t)(after->insns_copied - before->insns_copied),
            .remapped = (uint32_t)(after->insns_remapped - before->insns_remapped),
            .memory = (uint32_t)(after->insns_memory - before->insns_memory),
            .fallback = (uint32_t)(after->insns_fallback - before->insns_fallback),
        };
        memcpy(t->words, guest, 4 * nguest);
        memcpy(t->words + nguest, x->buf, 4 * x->n);
        memcpy(t->words + nguest + x->n, x->relocs, 4 * x->nrelocs);
        struct template **b = template_bucket(pc);
        t->next = *b;
        *b = t;
        /* 新的在前面：同一个 pc 超出上限的旧版本从链上摘掉（空间等模板区清空时回收） */
        unsigned versions = 0;
        for (struct template **p = &t->next; *p;) {
            if ((*p)->pc == pc && ++versions >= TEMPLATES_PER_PC)
                *p = (*p)->next;
            else
                p = &(*p)->next;
        }
    }
    appine_guard_drop();
    pthread_mutex_unlock(&templates.lock);
}

/* 丢弃所有翻译模板（宿主出了故障之后：坏掉的可能是模板本身） */
static void template_clear(void)
{
    pthread_mutex_lock(&templates.lock);
    memset(templates.buckets, 0, sizeof(templates.buckets));
    templates.used = 0;
    pthread_mutex_unlock(&templates.lock);
}

/*
 * 剖析用：环境变量 APPINE_PERF_MAP 存在时，把每个块在代码缓存里的位置写进
 * /tmp/perf-<pid>.map，perf report 据此把样本记到 guest pc 上（g_<pc>）。没有这个
 * 变量时只多一次判断。
 */
static void perf_map(const void *code, size_t bytes, uint64_t pc)
{
    static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    static FILE *file;
    static int enabled = -1;
    if (__atomic_load_n(&enabled, __ATOMIC_RELAXED) == 0)
        return;
    pthread_mutex_lock(&lock);
    if (__atomic_load_n(&enabled, __ATOMIC_RELAXED) < 0) {
        int on = getenv("APPINE_PERF_MAP") != NULL;
        if (on) {
            char path[64];
            snprintf(path, sizeof path, "/tmp/perf-%d.map", (int)getpid());
            file = fopen(path, "w");
        }
        __atomic_store_n(&enabled, on, __ATOMIC_RELAXED);
    }
    if (file) {
        fprintf(file, "%lx %zx g_%lx\n", (unsigned long)(uintptr_t)code, bytes,
                (unsigned long)pc);
        fflush(file);
    }
    pthread_mutex_unlock(&lock);
}

static uint32_t *translate(struct appine_dbt *d, struct appine_cpu *c, uint64_t pc)
{
    /* 块不跨页（遇到页边界就结束），整页的权限相同 */
    const uint32_t *page = NULL;
    if (!(pc & 3))
        page = appine_mmu_translate(c->mmu, pc & ~PAGE_MASK, APPINE_PROT_EXEC);
    if (!page)
        return NULL;
    if (d->table_count >= ((size_t)3 << d->table_bits) / 4 &&
        (d->table_bits == TABLE_BITS || !table_resize(d, d->table_bits + 1)))
        appine_dbt_flush(d);
    if (d->used_words + MAX_BLOCK_WORDS > d->cache_words)
        appine_dbt_flush(d);

    const uint32_t *guest = page + (pc & PAGE_MASK) / 4;
    uint32_t shared_nguest = 0;
    uint32_t *code = template_copy(d, pc, guest, &shared_nguest);
    if (code) {
        perf_map(code, (size_t)(d->cache + d->used_words - code) * 4, pc);
        sync_range(d, code, d->cache + d->used_words);
        table_insert(d, pc, code, guest, shared_nguest);
        d->stats.blocks++;
        return code;
    }

    struct appine_dbt_stats before = d->stats;
    struct xlate *x = d->xlate;
    x->d = d;
    x->buf = d->cache + d->used_words;
    x->n = 0;
    x->nstubs = 0;
    x->nfixups = 0;
    x->nrelocs = 0;

    /* 块入口检查 kick，保证链接起来的循环也能被打断 */
    emit(x, enc_ldr_x(T0, CTX, offsetof(struct appine_cpu, pending)));
    emit_to_stub(x, enc_cbnz_x(T0), FIX_IMM19, R_KICK, pc);
    _Static_assert(ENTRY_CHECK_WORDS == 2, "块入口检查的长度");

    uint32_t nguest = 0;
    for (;; nguest++) {
        uint64_t va = pc + 4ull * nguest;
        if (nguest == MAX_BLOCK_INSNS || (nguest > 0 && (va & PAGE_MASK) == 0)) {
            emit_exit_stub(x, R_CHAIN_FWD, va);
            break;
        }
        d->stats.insns++;
        if (!translate_insn(x, guest[nguest], va)) {
            nguest++;
            break;
        }
    }

    /* 上界估计错误属于引擎 bug：在写出界之前就停下 */
    if (x->nstubs > MAX_STUBS || x->nfixups > MAX_FIXUPS ||
        x->n + (size_t)x->nstubs * MAX_STUB_WORDS > MAX_BLOCK_WORDS)
        abort();
    for (unsigned i = 0; i < x->nstubs; i++)
        x->stubs[i].sites = 0;
    for (unsigned k = 0; k < x->nfixups; k++) {
        struct stub *s = &x->stubs[x->fixups[k].stub];
        s->branch = x->fixups[k].site;
        s->sites++;
    }
    for (unsigned i = 0; i < x->nstubs; i++) {
        struct stub *s = &x->stubs[i];
        s->addr = here(x);
        if (s->reason == STUB_REFILL) {
            emit_refill_stub(x, s);
            continue;
        }
        bool link = s->reason == R_CHAIN || s->reason == R_CHAIN_FWD;
        emit_link_stub(x, s->reason, s->pc, link && s->sites == 1 ? s->branch : NULL);
    }
    for (unsigned i = 0; i < x->nfixups; i++) {
        struct fixup *f = &x->fixups[i];
        intptr_t off = (intptr_t)x->stubs[f->stub].addr - (intptr_t)f->site;
        if (f->kind == FIX_IMM19)
            *rw(d, f->site) |= ((uint32_t)(off >> 2) & 0x7ffff) << 5;
        else
            *rw(d, f->site) |= ((uint32_t)(off >> 2) & 0x3fff) << 5;
    }

    if (x->nrelocs > MAX_RELOCS)
        abort();
    template_store(pc, guest, nguest, x, &before);
    code = x->buf;
    size_t words = x->n;
    perf_map(code, words * 4, pc);
    sync_range(d, code, code + words);
    d->used_words += words;
    table_insert(d, pc, code, guest, nguest);
    d->stats.blocks++;
    return code;
}

/*
 * TLB 回填例程：在翻译后的代码里按 mmu.c 的结构遍历页表，填好表项后回到访存
 * 指令重新检查，不必退出到分发器。入口：T0 = guest 地址，T1 和 T2 见
 * emit_refill_stub。跨页、地址带标签、未映射或没有权限时失败。
 *
 * 借用 cpu 结构体里 x0–x3 的位置暂存宿主 x0–x3：翻译后的代码运行期间这些位置
 * 不保存 guest 的值，退出例程会重新写入。和 Tier 0 的 appine_tlb_fill 一样，
 * 同时记下 tlb_used，读写两半的 tag 按叶子表项的权限分别填写。
 */
static uint32_t *emit_refill(struct xlate *x, bool write)
{
    size_t x0 = offsetof(struct appine_cpu, x), used = offsetof(struct appine_cpu, tlb_used);
    uint32_t *fail[8], *entry = here(x);
    unsigned nfail = 0;

    emit(x, enc_stp_x(0, 1, CTX, (int)x0));
    emit(x, enc_stp_x(2, 3, CTX, (int)x0 + 16));
    /* 跨页：页内偏移 + 字节数 - 1 超出这一页 */
    emit(x, enc_and_low(0, T0, APPINE_PAGE_SHIFT));
    emit(x, enc_add_lsl(0, 0, T2, 0));
    fail[nfail++] = here(x);
    emit(x, enc_tbnz(0, APPINE_PAGE_SHIFT));
    emit(x, enc_ubfx(0, T0, APPINE_VA_BITS, 64 - APPINE_VA_BITS));
    fail[nfail++] = here(x);
    emit(x, enc_cbnz_x(0));
    emit(x, enc_ldr_x(0, CTX, offsetof(struct appine_cpu, mmu)));
    emit(x, enc_ldr_x(0, 0, offsetof(struct appine_mmu, root)));
    /* 中间表项是下一级表的地址；按地址依赖读取，和 mmu.c 的 acquire 读取一样有序 */
    for (unsigned level = 0; level < MMU_LEVELS; level++) {
        emit(x, enc_ubfx(1, T0, APPINE_PAGE_SHIFT + MMU_LEVEL_BITS * (MMU_LEVELS - 1 - level),
                         MMU_LEVEL_BITS));
        emit(x, enc_ldr_x_index(0, 0, 1));
        if (level < MMU_LEVELS - 1) {
            fail[nfail++] = here(x);
            emit(x, enc_cbz_x(0));
        }
    }
    /* 叶子表项 = 宿主页地址 | 权限位；未映射的是 0，权限检查一并排除 */
    fail[nfail++] = here(x);
    emit(x, enc_tbz(0, write ? 1 : 0));
    _Static_assert(APPINE_PROT_READ == 1 && APPINE_PROT_WRITE == 2, "权限位的位置");
    emit(x, enc_and_clear_low(1, T0, APPINE_PAGE_SHIFT));
    emit(x, enc_and_clear_low(T2, 0, APPINE_PAGE_SHIFT));
    emit(x, enc_sub_reg(T2, T2, 1)); /* addend */
    emit(x, enc_ubfx(2, T0, APPINE_PAGE_SHIFT, 64 - APPINE_PAGE_SHIFT)); /* 页号 */
    emit(x, enc_ubfx(1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(1, CTX, 1, 5));
    for (unsigned half = 0; half < 2; half++) {
        /* 有权限时 tag = (0 - 1) & ~页号，否则 0 & ~页号 = TLB_INVALID */
        emit(x, enc_ubfx(3, 0, half, 1));
        emit(x, enc_sub_reg(3, 31, 3));
        emit(x, enc_bic_reg(3, 3, 2));
        emit(x, enc_stp_x(3, T2, 1, (int)(TLB_OFF + 16 * half)));
    }
    /* tlb_used[下标 / 64] |= 1 << (下标 % 64) */
    emit(x, enc_ubfx(1, T0, APPINE_PAGE_SHIFT + 6, TLB_BITS - 6));
    emit(x, enc_add_lsl(1, CTX, 1, 3));
    emit_add_const(x, 1, 1, (int64_t)(used & ~(size_t)0xfff));
    emit(x, enc_ldr_x(3, 1, used & 0xfff));
    emit(x, enc_ubfx(2, T0, APPINE_PAGE_SHIFT, 6));
    emit(x, enc_movz(0, 1, 0));
    emit(x, enc_lslv(0, 0, 2));
    emit(x, enc_orr_reg(3, 3, 0));
    emit(x, enc_str_x(3, 1, used & 0xfff));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_br(T1));

    for (unsigned i = 0; i < nfail; i++)
        patch_branch(x->d, fail[i], here(x));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_add_imm(T1, T1, 4, 0));
    emit(x, enc_br(T1));
    return entry;
}

/* 进入与退出例程：放在代码缓存开头，所有退出桩都能用一条 b 到达 */
static void emit_routines(struct appine_dbt *d)
{
    static const unsigned pairs[] = {0, 2, 4, 6, 8, 10, 12, 14, 19, 21, 23};
    struct xlate x = {.d = d, .buf = d->cache};
    size_t x0 = offsetof(struct appine_cpu, x), v0 = offsetof(struct appine_cpu, v);

    /* uint64_t enter(struct appine_cpu *cpu, uint32_t *code)：保存被调用者保存的寄存器
     * （含 d8–d15）和宿主 FPCR。栈帧布局：x29/x30、x27–x19 的五对、空着的 8 字节、
     * 宿主 FPCR（+104）、d8–d15（+112） */
    d->enter = here(&x);
    emit(&x, enc_stp_x_pre(29, 30, 31, -FRAME_BYTES));
    emit(&x, enc_stp_x(27, 28, 31, 16));
    emit(&x, enc_stp_x(25, 26, 31, 32));
    emit(&x, enc_stp_x(23, 24, 31, 48));
    emit(&x, enc_stp_x(21, 22, 31, 64));
    emit(&x, enc_stp_x(19, 20, 31, 80));
    for (unsigned r = 8; r < 16; r += 2)
        emit(&x, enc_stp_d(r, r + 1, 31, (int)(112 + 8 * (r - 8))));
    emit(&x, enc_mrs_fpcr(T1));
    emit(&x, enc_str_x(T1, 31, 104));
    emit(&x, enc_mov(CTX, 0));
    emit(&x, enc_mov(T0, 1));
    /* guest 的 FP 状态在翻译后的代码里一直放在宿主寄存器里（§4.3） */
    emit(&x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, fpcr)));
    emit(&x, enc_msr_fpcr(T1));
    emit(&x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, fpsr)));
    emit(&x, enc_msr_fpsr(T1));
    emit(&x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, nzcv)));
    emit(&x, enc_msr_nzcv(T1));
    emit_add_const(&x, T1, CTX, (int64_t)v0);
    for (unsigned q = 0; q < 32; q += 2)
        emit(&x, enc_ldp_q(q, q + 1, T1, (int)(16 * q)));
    emit_add_const(&x, T1, CTX, (int64_t)x0);
    for (unsigned i = 0; i < sizeof(pairs) / sizeof(pairs[0]); i++)
        emit(&x, enc_ldp_x(pairs[i], pairs[i] + 1, T1, (int)(8 * pairs[i])));
    emit(&x, enc_ldr_x(25, T1, 8 * 25));
    emit(&x, enc_ldp_x(29, 30, T1, 8 * 29));
    emit(&x, enc_ldr_x(SP_HOST, CTX, offsetof(struct appine_cpu, sp)));
    emit(&x, enc_br(T0));

    /* 退出：T0 = guest pc，T1 = 回填位置或 0，T2 = 原因 */
    d->exit = here(&x);
    emit(&x, enc_str_x(T0, CTX, offsetof(struct appine_cpu, pc)));
    emit(&x, enc_str_x(T1, CTX, offsetof(struct appine_cpu, dbt_site)));
    emit(&x, enc_str_x(T2, CTX, offsetof(struct appine_cpu, dbt_reason)));
    emit(&x, enc_mrs_nzcv(T0));
    emit(&x, enc_str_x(T0, CTX, offsetof(struct appine_cpu, nzcv)));
    emit_add_const(&x, T1, CTX, (int64_t)x0);
    for (unsigned i = 0; i < sizeof(pairs) / sizeof(pairs[0]); i++)
        emit(&x, enc_stp_x(pairs[i], pairs[i] + 1, T1, (int)(8 * pairs[i])));
    emit(&x, enc_str_x(25, T1, 8 * 25));
    emit(&x, enc_stp_x(29, 30, T1, 8 * 29));
    emit(&x, enc_str_x(SP_HOST, CTX, offsetof(struct appine_cpu, sp)));
    emit_add_const(&x, T1, CTX, (int64_t)v0);
    for (unsigned q = 0; q < 32; q += 2)
        emit(&x, enc_stp_q(q, q + 1, T1, (int)(16 * q)));
    emit(&x, enc_mrs_fpsr(T0));
    emit(&x, enc_str_x(T0, CTX, offsetof(struct appine_cpu, fpsr)));
    emit(&x, enc_ldr_x(T0, 31, 104));
    emit(&x, enc_msr_fpcr(T0));
    emit(&x, enc_mov(0, T2));
    for (unsigned r = 8; r < 16; r += 2)
        emit(&x, enc_ldp_d(r, r + 1, 31, (int)(112 + 8 * (r - 8))));
    emit(&x, enc_ldp_x(19, 20, 31, 80));
    emit(&x, enc_ldp_x(21, 22, 31, 64));
    emit(&x, enc_ldp_x(23, 24, 31, 48));
    emit(&x, enc_ldp_x(25, 26, 31, 32));
    emit(&x, enc_ldp_x(27, 28, 31, 16));
    emit(&x, enc_ldp_x_post(29, 30, 31, FRAME_BYTES));
    emit(&x, enc_ret());

    d->refill[0] = emit_refill(&x, false);
    d->refill[1] = emit_refill(&x, true);

    /* 影子返回栈空项的落点：像间接跳转没命中跳转缓存那样，按 guest x30 退出查找 */
    d->rs_miss = here(&x);
    emit(&x, enc_mov(T0, 30));
    emit(&x, enc_movz(T1, 0, 0));
    emit(&x, enc_movz(T2, R_LOOKUP, 0));
    emit(&x, enc_b((intptr_t)d->exit - (intptr_t)here(&x)));

    d->routine_words = d->used_words = x.n;
    sync_range(d, d->cache, d->cache + x.n);
}

/*
 * 退出的线程留下的实例清空后放进一个小池子，给新线程复用：短命进程不必每次都重新
 * 映射代码缓存和各张表，再在第一次写入每一页时缺页。池里的实例把代码缓存用过的
 * 部分标成 MADV_FREE，宿主内存紧张时可以直接回收（之后再写入只是重新缺页）。
 */
#define POOL_SIZE 2

static struct {
    pthread_mutex_t lock;
    struct appine_dbt *free[POOL_SIZE];
    unsigned n;
} pool = {.lock = PTHREAD_MUTEX_INITIALIZER};

static void release(struct appine_dbt *d)
{
    if (d->cache)
        appine_unmap_code(d->cache, CODE_CACHE_BYTES, d->rw_delta);
    zfree(d->table, TABLE_BYTES(d->table_bits));
    zfree(d->table_used, TABLE_LIMIT * sizeof(*d->table_used));
    zfree(d->jc, JC_SIZE * sizeof(*d->jc));
    zfree(d->blocks, TABLE_LIMIT * sizeof(*d->blocks));
    zfree(d->words, WORDS_LIMIT * sizeof(*d->words));
    zfree(d->rs, APPINE_PAGE_SIZE);
    zfree(d->page_head, sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    free(d->xlate);
    zfree(d, sizeof(*d));
}

struct appine_dbt *appine_dbt_new(void)
{
    pthread_mutex_lock(&pool.lock);
    struct appine_dbt *d = pool.n ? pool.free[--pool.n] : NULL;
    pthread_mutex_unlock(&pool.lock);
    if (d) {
        d->stats = (struct appine_dbt_stats){0};
        return d;
    }

    d = zalloc(sizeof(*d));
    if (!d)
        return NULL;
    d->cache = appine_map_code(CODE_CACHE_BYTES, &d->rw_delta);
    d->table_bits = TABLE_MIN_BITS;
    d->table = zalloc(TABLE_BYTES(TABLE_MIN_BITS));
    d->table_used = zalloc(TABLE_LIMIT * sizeof(*d->table_used));
    d->jc = zalloc(JC_SIZE * sizeof(*d->jc));
    d->blocks = zalloc(TABLE_LIMIT * sizeof(*d->blocks));
    d->words = zalloc(WORDS_LIMIT * sizeof(*d->words));
    d->rs = zalloc(APPINE_PAGE_SIZE); /* 按页对齐，也就按 1 KB 对齐 */
    d->page_head = zalloc(sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    d->xlate = malloc(sizeof(*d->xlate));
    if (!d->cache || !d->table || !d->table_used || !d->jc || !d->blocks || !d->words || !d->rs ||
        !d->page_head || !d->xlate) {
        release(d);
        return NULL;
    }
    d->cache_words = CODE_CACHE_BYTES / 4;
    clear_jump_cache(d);
    appine_cpu_init(&d->probe, NULL);
    emit_routines(d);
    rs_reset(d);
    return d;
}

void appine_dbt_free(struct appine_dbt *d)
{
    if (!d)
        return;
    if (d->poisoned) {
        release(d);
        return;
    }
    pthread_mutex_lock(&pool.lock);
    bool keep = pool.n < POOL_SIZE;
    pthread_mutex_unlock(&pool.lock);
    if (!keep) {
        release(d);
        return;
    }
    /*
     * 块不多时保留翻译：下一个使用者（fork 出来的子进程、又一次 exec 的同一个程序）
     * 多半运行同样的代码，运行之前核对一遍（recheck）比逐块拷贝模板便宜得多
     */
    if (d->table_count <= KEEP_MAX_BLOCKS) {
        d->stale = true;
        pthread_mutex_lock(&pool.lock);
        if (pool.n < POOL_SIZE)
            pool.free[pool.n++] = d;
        else
            keep = false;
        pthread_mutex_unlock(&pool.lock);
        if (!keep)
            release(d);
        return;
    }
    appine_dbt_flush(d);
    if (d->table_bits > TABLE_MIN_BITS) /* 下一个使用者多半是个短命进程 */
        table_resize(d, TABLE_MIN_BITS);
    /*
     * 进入/退出例程所在的页保留，其余用过的部分交给宿主回收：私有映射用 MADV_FREE，
     * 内存紧张时才回收；双映射的共享内存不支持它，只能用 MADV_REMOVE 立即释放
     */
    uintptr_t page = APPINE_PAGE_SIZE;
    uintptr_t start = ((uintptr_t)rw(d, d->cache + d->routine_words) + page - 1) & ~(page - 1);
    uintptr_t end = ((uintptr_t)rw(d, d->cache + d->high_words) + page - 1) & ~(page - 1);
    if (end > start)
        madvise((void *)start, end - start, d->rw_delta ? MADV_REMOVE : MADV_FREE);
    d->high_words = 0;
    pthread_mutex_lock(&pool.lock);
    if (pool.n < POOL_SIZE)
        pool.free[pool.n++] = d;
    else
        keep = false;
    pthread_mutex_unlock(&pool.lock);
    if (!keep)
        release(d);
}

/* 把条件分支直接指向目标块；超出它的偏移范围时仍然经由桩跳转 */
static void relink_branch(const struct appine_dbt *d, uint32_t *site, const uint32_t *target)
{
    uint32_t *w = rw(d, site);
    bool test = (*w & 0x7e000000) == 0x36000000; /* TBZ/TBNZ：14 位偏移，其余 19 位 */
    intptr_t off = target - site, limit = (intptr_t)1 << (test ? 13 : 18);
    if (off < -limit || off >= limit)
        return;
    uint32_t mask = (test ? 0x3fffu : 0x7ffffu) << 5;
    *w = (*w & ~mask) | (((uint32_t)off << 5) & mask);
    sync_range(d, site, site + 1);
}

/* 块翻译时的 guest 指令字和现在 `mmu` 里同一地址的是否相同；`page`/`host` 记着上一次
 * 查到的页 */
static bool block_matches(const struct appine_dbt *d, const struct appine_mmu *mmu,
                          const struct block *b, uint64_t *page, const uint32_t **host)
{
    uint64_t p = b->pc & ~PAGE_MASK;
    if (p != *page) {
        *page = p;
        *host = appine_mmu_translate(mmu, p, APPINE_PROT_EXEC);
    }
    return *host && memcmp(*host + (b->pc & PAGE_MASK) / 4, d->words + b->words, b->nguest * 4) == 0;
}

/*
 * 实例换了地址空间之后、第一次运行之前：逐块核对 guest 指令字，对不上的作废。和拷贝
 * 翻译模板时的核对是同一件事，只是一次做完，省掉逐块拷贝、重新链接和同步指令缓存。
 * 大部分都对不上时（exec 了别的程序）整体清空。
 */
static void recheck(struct appine_dbt *d, const struct appine_cpu *c)
{
    d->stale = false;
    size_t live = 0, bad = 0;
    uint64_t page = 1; /* 未对齐，不会与任何页相同 */
    const uint32_t *host = NULL;
    for (size_t i = 0; i < d->table_count; i++) {
        const struct block *b = &d->blocks[i];
        if (b->code) {
            live++;
            bad += !block_matches(d, c->mmu, b, &page, &host);
        }
    }
    if (bad * 2 > live) {
        appine_dbt_flush(d);
        return;
    }
    page = 1;
    for (size_t i = 0; bad && i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (b->code && !block_matches(d, c->mmu, b, &page, &host)) {
            if (!retire_block(d, b)) {
                appine_dbt_flush(d);
                return;
            }
            bad--;
        }
    }
}

void appine_dbt_recheck(struct appine_dbt *d)
{
    d->stale = d->table_count > 0;
}

static enum appine_exit dbt_run(void *ctx, struct appine_cpu *c)
{
    struct appine_dbt *d = ctx;
    uint64_t (*enter)(struct appine_cpu *, uint32_t *) =
        (uint64_t(*)(struct appine_cpu *, uint32_t *))(uintptr_t)d->enter;
    uint32_t *patch_site = NULL, *patch_branch_site = NULL;
    uint64_t patch_generation = 0;
    bool patch_forward = false;
    bool fill_jump_cache = false;

    if (UNLIKELY(d->stale))
        recheck(d, c);
    c->dbt_jump_cache = d->jc;
    if (((uintptr_t)c->dbt_rs & ~(uintptr_t)((1u << RS_SHIFT) - 1)) != (uintptr_t)d->rs)
        c->dbt_rs = d->rs;
    for (;;) {
        if (UNLIKELY(__atomic_load_n(&c->pending, __ATOMIC_RELAXED)) &&
            __atomic_exchange_n(&c->pending, 0, __ATOMIC_ACQUIRE))
            return APPINE_EXIT_KICK;

        uint32_t *code = table_lookup(d, c->pc);
        if (!code)
            code = translate(d, c, c->pc);
        if (!code) { /* 无法取指：让 Tier 0 报告精确的 fault */
            enum appine_exit r = appine_interp_step(c);
            if (r != APPINE_EXIT_NONE)
                return r;
            patch_site = NULL;
            continue;
        }
        if (patch_site && patch_generation == d->generation) {
            uint32_t *target = patch_forward ? code + ENTRY_CHECK_WORDS : code;
            *rw(d, patch_site) = enc_b((intptr_t)target - (intptr_t)patch_site);
            sync_range(d, patch_site, patch_site + 1);
            if (patch_branch_site)
                relink_branch(d, patch_branch_site, target);
        }
        patch_site = NULL;
        if (fill_jump_cache) {
            d->jc[(c->pc >> 2) & (JC_SIZE - 1)] = (struct slot){c->pc, code};
            d->jc_dirty = true;
            fill_jump_cache = false;
        }

        uint64_t reason = enter(c, code);
        switch (reason & REASON_MASK) {
        case R_CHAIN:
        case R_CHAIN_FWD:
            d->stats.exits_chain++;
            patch_site = (uint32_t *)(uintptr_t)c->dbt_site;
            patch_branch_site = reason >> REASON_BITS ? patch_site - (reason >> REASON_BITS) : NULL;
            patch_generation = d->generation;
            patch_forward = (reason & REASON_MASK) == R_CHAIN_FWD;
            break;
        case R_LOOKUP:
            d->stats.exits_lookup++;
            fill_jump_cache = true;
            break;
        case R_INTERP: {
            d->stats.exits_interp++;
            enum appine_exit r = appine_interp_step(c);
            if (r != APPINE_EXIT_NONE)
                return r;
            break;
        }
        case R_SVC:
            d->stats.exits_svc++;
            c->exit_info = 0;
            c->excl_size = 0; /* 与 Tier 0 一样：异常返回清掉独占标记 */
            return APPINE_EXIT_SVC;
        default:
            d->stats.exits_kick++;
            break;
        }
    }
}

enum appine_exit appine_dbt_run(struct appine_dbt *d, struct appine_cpu *c)
{
    enum appine_exit r = appine_guard_run(dbt_run, d, c);
    if (UNLIKELY(r == APPINE_EXIT_HOST_FAULT)) {
        d->poisoned = true;
        template_clear();
    }
    return r;
}

void appine_dbt_get_stats(const struct appine_dbt *d, struct appine_dbt_stats *out)
{
    *out = d->stats;
}

#else /* !DBT_SUPPORTED */

struct appine_dbt *appine_dbt_new(void)
{
    return NULL;
}

void appine_dbt_free(struct appine_dbt *d)
{
    (void)d;
}

void appine_dbt_flush(struct appine_dbt *d)
{
    (void)d;
}

void appine_dbt_recheck(struct appine_dbt *d)
{
    (void)d;
}

void appine_dbt_invalidate(struct appine_dbt *d, uint64_t start, uint64_t len)
{
    (void)d;
    (void)start;
    (void)len;
}

enum appine_exit appine_dbt_run(struct appine_dbt *d, struct appine_cpu *c)
{
    (void)d;
    return appine_cpu_run(c);
}

void appine_dbt_get_stats(const struct appine_dbt *d, struct appine_dbt_stats *out)
{
    (void)d;
    memset(out, 0, sizeof(*out));
}

#endif
