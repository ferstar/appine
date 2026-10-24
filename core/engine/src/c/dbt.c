/*
 * Tier 1 原型：同 ISA 直通式 DBT（设计文档 §4.2；M2 性能探针）。
 *
 * 翻译后的代码运行期间的寄存器约定：
 *   - guest X0–X15、X19–X27、X29、X30 放在同号的宿主寄存器里；
 *   - 宿主 X28 指向 struct appine_cpu，宿主 X16–X18 是临时寄存器；
 *   - guest X16、X17、X18、X28 和 SP 存放在 cpu 结构体里（"被占用的寄存器"），
 *     引用它们的指令改写后执行；
 *   - guest NZCV 就是宿主 NZCV，所以插入的代码一律不改动条件标志。
 *
 * 访存指令改写成内联的 TLB 快速路径；TLB 未命中、跨页、SP 未对齐时退出到
 * 分发器，由 Tier 0 执行这一条指令（此时指令还没有产生任何副作用）。直接
 * 分支通过块链接直接跳转，间接分支先查跳转缓存。Tier 0 不认识的指令同样
 * 交给 Tier 0，保证两层的白名单完全一致（§4.4）。
 *
 * 原型限制：
 *   - 翻译结果不会因为 guest 修改代码或映射而失效，调用方必须自行 flush；
 *   - 宿主 X18 是平台寄存器（Android 上是 bionic 给每个线程设的影子调用栈指针），
 *     翻译后的代码把它当临时寄存器用，进入时保存、退出时恢复；
 *   - 代码缓存直接映射为 RWX，正式版改为 §12 的 W^X 双映射。
 */
#include <stddef.h>
#include <stdlib.h>

#include "emit.h"
#include "internal.h"

#if defined(__aarch64__) && defined(__linux__)
#define DBT_SUPPORTED 1
#endif

#ifdef DBT_SUPPORTED

#include <sys/mman.h>

#define BIT(x, n) ((unsigned)(((x) >> (n)) & 1u))
#define FIELD(x, hi, lo) ((unsigned)(((x) >> (lo)) & ((1u << ((hi) - (lo) + 1)) - 1)))

#define CODE_CACHE_BYTES (UINT64_C(64) << 20)
#define MAX_BLOCK_INSNS 64
/* 单条 guest 指令最多展开成约 40 条宿主指令，再加上退出桩 */
#define MAX_BLOCK_WORDS (MAX_BLOCK_INSNS * 64)
#define MAX_STUBS (MAX_BLOCK_INSNS * 2 + 4)
#define MAX_FIXUPS (MAX_BLOCK_INSNS * 4 + 4)
#define TABLE_BITS 16
#define TABLE_SIZE (1u << TABLE_BITS)
#define TABLE_LIMIT (TABLE_SIZE / 4 * 3)
#define JC_BITS 12
#define JC_SIZE (1u << JC_BITS)
#define JC_EMPTY UINT64_C(1) /* 未对齐的 pc 永远不会是合法的查找键 */

/* 宿主寄存器 */
#define CTX 28
#define T0 16
#define T1 17
#define T2 18
#define REG_SP 32 /* 伪寄存器编号：guest SP */

/* 退出原因（从翻译后的代码返回分发器） */
enum { R_CHAIN = 1, R_LOOKUP, R_INTERP, R_SVC, R_KICK };

#define SYSREG_NZCV 0xda10
#define SYSREG_TPIDR_EL0 0xde82

_Static_assert(APPINE_TLB_ENTRIES == 512, "TLB 下标在生成的代码里固定为 9 位");
_Static_assert(sizeof(struct appine_tlb_entry) == 32, "TLB 表项在生成的代码里按 32 字节寻址");
_Static_assert(offsetof(struct appine_cpu, tlb) == 0, "TLB 必须位于 cpu 结构体开头");
_Static_assert(offsetof(struct appine_cpu, dbt_jump_cache) < 32768,
               "cpu 字段必须能被 ldr/str 的 12 位缩放立即数寻址");

struct slot {
    uint64_t pc;
    uint32_t *code;
};

struct appine_dbt {
    uint32_t *cache;
    size_t cache_words;
    size_t used_words;
    size_t routine_words;
    uint32_t *enter;
    uint32_t *exit;
    struct slot *table; /* guest pc → 翻译结果，开放寻址 */
    uint32_t *table_used;
    size_t table_count;
    struct slot *jc; /* 间接分支的跳转缓存，直接映射 */
    bool jc_dirty;
    uint64_t generation; /* 每次 flush 加一，用来作废尚未回填的链接 */
    struct appine_dbt_stats stats;
    struct appine_cpu probe; /* 用来询问 Tier 0 是否认识某条指令 */
};

/* ---- 发射器 ---- */

enum fixup_kind { FIX_IMM19, FIX_IMM14 };

struct stub {
    unsigned reason; /* R_LOOKUP 的目标 pc 已经在 T0 里 */
    uint64_t pc;
    uint32_t *addr;
};

struct fixup {
    uint32_t *site;
    enum fixup_kind kind;
    unsigned stub;
};

struct xlate {
    struct appine_dbt *d;
    uint32_t *buf;
    size_t n;
    struct stub stubs[MAX_STUBS];
    unsigned nstubs;
    struct fixup fixups[MAX_FIXUPS];
    unsigned nfixups;
};

static void emit(struct xlate *x, uint32_t insn)
{
    if (x->n < MAX_BLOCK_WORDS)
        x->buf[x->n] = insn;
    x->n++;
}

static uint32_t *here(struct xlate *x)
{
    return &x->buf[x->n];
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
    return r == 16 || r == 17 || r == 18 || r == 28 || r == REG_SP;
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
 * 链接桩的第一条指令是 "adr x17, ."，链接时它会被改写成直接跳往目标块的 b。
 */
static void emit_exit_stub(struct xlate *x, unsigned reason, uint64_t pc)
{
    emit(x, reason == R_CHAIN ? enc_adr_self(T1) : enc_movz(T1, 0, 0));
    if (reason != R_LOOKUP)
        emit_mov_const(x, T0, pc);
    emit(x, enc_movz(T2, reason, 0));
    emit(x, enc_b((intptr_t)x->d->exit - (intptr_t)here(x)));
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
            x->stubs[i] = (struct stub){reason, pc, NULL};
        x->nstubs++; /* 溢出由 translate() 检查 */
    }
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups++] = (struct fixup){here(x), kind, i};
    else
        x->nfixups++; /* 记录溢出，translate() 会检查 */
    emit(x, insn);
}

static bool fallback(struct xlate *x, uint64_t pc)
{
    x->d->stats.insns_fallback++;
    emit_exit_stub(x, R_INTERP, pc);
    return false;
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
    for (unsigned i = 0; i < o->n; i++) {
        unsigned r = (insn >> o->pos[i]) & 31;
        unsigned g = (r == 31 && o->sp[i]) ? REG_SP : r;
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
    if (nmap == 0) {
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
    unsigned size; /* 单个元素的 log2 字节数 */
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

static void emit_memop(struct xlate *x, const struct memop *m, uint64_t pc)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn;

    /* 1. guest 地址 → T0 */
    if (m->mode == M_LITERAL) {
        emit_mov_const(x, T0, (uint64_t)m->imm);
    } else {
        unsigned base = rn;
        if (is_stolen(rn)) {
            emit_load_guest(x, T0, rn);
            if (rn == REG_SP) { /* SP 必须 16 字节对齐 */
                emit(x, enc_and_low(T1, T0, 4));
                emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
            }
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

    /* 2. 跨页检查 */
    unsigned bytes = (m->pair ? 2u : 1u) << m->size;
    if (bytes > 1) {
        emit(x, enc_and_low(T1, T0, 12));
        emit(x, enc_add_imm(T1, T1, bytes - 1, 0));
        emit_to_stub(x, enc_tbnz(T1, 12), FIX_IMM14, R_INTERP, pc);
    }

    /* 3. TLB：T1 = tag，T2 = addend */
    emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, 9));
    emit(x, enc_add_lsl(T1, CTX, T1, 5));
    emit(x, enc_ldp_x(T1, T2, T1, m->load ? 0 : 16));
    emit(x, enc_eor_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
    emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);

    /* 4. 访问宿主内存 */
    if (!m->pair) {
        if (!is_stolen(m->rt)) {
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
        bool s1 = is_stolen(m->rt), s2 = is_stolen(m->rt2);
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

    /* 5. 基址回写 */
    if (m->mode == M_PRE || m->mode == M_POST) {
        if (is_stolen(rn)) {
            emit_load_guest(x, T0, rn);
            emit_add_const(x, T0, T0, m->imm);
            emit_store_guest(x, T0, rn);
        } else {
            emit_add_const(x, rn, rn, m->imm);
        }
    }
    x->d->stats.insns_memory++;
}

/* ---- 分支与系统指令 ---- */

static int64_t sext(uint64_t v, unsigned bits)
{
    return (int64_t)(v << (64 - bits)) >> (64 - bits);
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
        if (BIT(insn, 31))
            emit_mov_const(x, 30, pc + 4);
        emit_exit_stub(x, R_CHAIN, pc + (uint64_t)(sext(FIELD(insn, 25, 0), 26) * 4));
        return false;
    }
    if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000) { /* B.cond/CBZ/CBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 23, 5), 19) * 4);
        /* B.cond 的 bits 4:0 是条件码，原样保留；CBZ/CBNZ 的是寄存器 */
        uint32_t base = BIT(insn, 30) ? insn : branch_operand(x, insn);
        emit_to_stub(x, base & ~(0x7ffffu << 5), FIX_IMM19, R_CHAIN, target);
        emit_exit_stub(x, R_CHAIN, pc + 4);
        return false;
    }
    if ((insn & 0x7e000000) == 0x36000000) { /* TBZ/TBNZ */
        uint64_t target = pc + (uint64_t)(sext(FIELD(insn, 18, 5), 14) * 4);
        uint32_t base = branch_operand(x, insn) & ~(0x3fffu << 5);
        emit_to_stub(x, base, FIX_IMM14, R_CHAIN, target);
        emit_exit_stub(x, R_CHAIN, pc + 4);
        return false;
    }
    if (insn == 0xd4000001) { /* SVC #0 */
        emit_exit_stub(x, R_SVC, pc + 4);
        return false;
    }
    if ((insn & 0xfffff01f) == 0xd503201f) /* HINT：与 Tier 0 一样当作 NOP */
        return true;
    if ((insn & 0xfffff01f) == 0xd503301f) { /* 屏障：白名单已由 Tier 0 确认 */
        emit(x, insn);
        x->d->stats.insns_copied++;
        return true;
    }
    if ((insn & 0xffd00000) == 0xd5100000) { /* MRS/MSR */
        bool read = BIT(insn, 21);
        unsigned rt = insn & 31;
        switch (FIELD(insn, 20, 5)) {
        case SYSREG_NZCV: {
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
        default:
            return fallback(x, pc);
        }
    }
    uint32_t reg_branch = insn & 0xfffffc1f;
    if (reg_branch == 0xd61f0000 || reg_branch == 0xd63f0000 || reg_branch == 0xd65f0000) {
        unsigned rn = FIELD(insn, 9, 5); /* BR/BLR/RET：目标先放进 T0，再查跳转缓存 */
        if (rn == 31)
            emit(x, enc_movz(T0, 0, 0));
        else if (is_stolen(rn))
            emit_load_guest(x, T0, rn);
        else
            emit(x, enc_mov(T0, rn));
        if (reg_branch == 0xd63f0000)
            emit_mov_const(x, 30, pc + 4);
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

/* 翻译一条指令；返回 false 表示块到此结束 */
static bool translate_insn(struct xlate *x, uint32_t insn, uint64_t pc)
{
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
        struct memop m;
        if (!decode_memop(insn, pc, &m))
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

static size_t table_index(uint64_t pc)
{
    return (size_t)(((pc >> 2) * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - TABLE_BITS));
}

static uint32_t *table_lookup(const struct appine_dbt *d, uint64_t pc)
{
    for (size_t i = table_index(pc);; i = (i + 1) & (TABLE_SIZE - 1)) {
        if (!d->table[i].code)
            return NULL;
        if (d->table[i].pc == pc)
            return d->table[i].code;
    }
}

static void table_insert(struct appine_dbt *d, uint64_t pc, uint32_t *code)
{
    size_t i = table_index(pc);
    while (d->table[i].code)
        i = (i + 1) & (TABLE_SIZE - 1);
    d->table[i] = (struct slot){pc, code};
    d->table_used[d->table_count++] = (uint32_t)i;
}

static void clear_jump_cache(struct appine_dbt *d)
{
    for (unsigned i = 0; i < JC_SIZE; i++)
        d->jc[i] = (struct slot){JC_EMPTY, NULL};
    d->jc_dirty = false;
}

void appine_dbt_flush(struct appine_dbt *d)
{
    for (size_t i = 0; i < d->table_count; i++)
        d->table[d->table_used[i]] = (struct slot){0, NULL};
    d->table_count = 0;
    if (d->jc_dirty)
        clear_jump_cache(d);
    d->used_words = d->routine_words;
    d->generation++;
    d->stats.flushes++;
}

static uint32_t *translate(struct appine_dbt *d, struct appine_cpu *c, uint64_t pc)
{
    if ((pc & 3) || !appine_mmu_translate(c->mmu, pc, APPINE_PROT_EXEC))
        return NULL;
    if (d->used_words + MAX_BLOCK_WORDS > d->cache_words || d->table_count >= TABLE_LIMIT)
        appine_dbt_flush(d);

    struct xlate *x = malloc(sizeof(*x));
    if (!x)
        return NULL;
    x->d = d;
    x->buf = d->cache + d->used_words;
    x->n = 0;
    x->nstubs = 0;
    x->nfixups = 0;

    /* 块入口检查 kick，保证链接起来的循环也能被打断 */
    emit(x, enc_ldr_x(T0, CTX, offsetof(struct appine_cpu, pending)));
    emit_to_stub(x, enc_cbnz_x(T0), FIX_IMM19, R_KICK, pc);

    for (unsigned i = 0;; i++) {
        uint64_t va = pc + 4ull * i;
        uint32_t *insn = NULL;
        if (i < MAX_BLOCK_INSNS && (i == 0 || (va & PAGE_MASK) != 0))
            insn = appine_mmu_translate(c->mmu, va, APPINE_PROT_EXEC);
        if (!insn) {
            emit_exit_stub(x, R_CHAIN, va);
            break;
        }
        d->stats.insns++;
        if (!translate_insn(x, *insn, va))
            break;
    }

    /* 上界估计错误属于引擎 bug：在写出界之前就停下 */
    if (x->nstubs > MAX_STUBS || x->nfixups > MAX_FIXUPS ||
        x->n + (size_t)x->nstubs * 8 > MAX_BLOCK_WORDS)
        abort();
    for (unsigned i = 0; i < x->nstubs; i++) {
        x->stubs[i].addr = here(x);
        emit_exit_stub(x, x->stubs[i].reason, x->stubs[i].pc);
    }
    for (unsigned i = 0; i < x->nfixups; i++) {
        struct fixup *f = &x->fixups[i];
        intptr_t off = (intptr_t)x->stubs[f->stub].addr - (intptr_t)f->site;
        if (f->kind == FIX_IMM19)
            *f->site |= ((uint32_t)(off >> 2) & 0x7ffff) << 5;
        else
            *f->site |= ((uint32_t)(off >> 2) & 0x3fff) << 5;
    }

    uint32_t *code = x->buf;
    size_t words = x->n;
    free(x);
    sync_icache((void *)code, (void *)(code + words));
    d->used_words += words;
    table_insert(d, pc, code);
    d->stats.blocks++;
    return code;
}

/* 进入与退出例程：放在代码缓存开头，所有退出桩都能用一条 b 到达 */
static void emit_routines(struct appine_dbt *d)
{
    static const unsigned pairs[] = {0, 2, 4, 6, 8, 10, 12, 14, 19, 21, 23, 25};
    struct xlate x = {.d = d, .buf = d->cache};
    size_t x0 = offsetof(struct appine_cpu, x);

    /* uint64_t enter(struct appine_cpu *cpu, uint32_t *code)：保存被调用者保存的寄存器
     * 和平台寄存器 X18 */
    d->enter = here(&x);
    emit(&x, enc_stp_x_pre(29, 30, 31, -112));
    emit(&x, enc_str_x(18, 31, 96));
    emit(&x, enc_stp_x(27, 28, 31, 16));
    emit(&x, enc_stp_x(25, 26, 31, 32));
    emit(&x, enc_stp_x(23, 24, 31, 48));
    emit(&x, enc_stp_x(21, 22, 31, 64));
    emit(&x, enc_stp_x(19, 20, 31, 80));
    emit(&x, enc_mov(CTX, 0));
    emit(&x, enc_mov(T0, 1));
    emit(&x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, nzcv)));
    emit(&x, enc_msr_nzcv(T1));
    emit_add_const(&x, T1, CTX, (int64_t)x0);
    for (unsigned i = 0; i < sizeof(pairs) / sizeof(pairs[0]); i++)
        emit(&x, enc_ldp_x(pairs[i], pairs[i] + 1, T1, (int)(8 * pairs[i])));
    emit(&x, enc_ldr_x(27, T1, 8 * 27));
    emit(&x, enc_ldp_x(29, 30, T1, 8 * 29));
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
    emit(&x, enc_str_x(27, T1, 8 * 27));
    emit(&x, enc_stp_x(29, 30, T1, 8 * 29));
    emit(&x, enc_mov(0, T2));
    emit(&x, enc_ldp_x(19, 20, 31, 80));
    emit(&x, enc_ldp_x(21, 22, 31, 64));
    emit(&x, enc_ldp_x(23, 24, 31, 48));
    emit(&x, enc_ldp_x(25, 26, 31, 32));
    emit(&x, enc_ldp_x(27, 28, 31, 16));
    emit(&x, enc_ldr_x(18, 31, 96));
    emit(&x, enc_ldp_x_post(29, 30, 31, 112));
    emit(&x, enc_ret());

    d->routine_words = d->used_words = x.n;
    sync_icache((void *)d->cache, (void *)(d->cache + x.n));
}

struct appine_dbt *appine_dbt_new(void)
{
    struct appine_dbt *d = calloc(1, sizeof(*d));
    if (!d)
        return NULL;
    void *cache = mmap(NULL, CODE_CACHE_BYTES, PROT_READ | PROT_WRITE | PROT_EXEC,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    d->table = calloc(TABLE_SIZE, sizeof(*d->table));
    d->table_used = calloc(TABLE_SIZE, sizeof(*d->table_used));
    d->jc = calloc(JC_SIZE, sizeof(*d->jc));
    if (cache == MAP_FAILED || !d->table || !d->table_used || !d->jc) {
        if (cache != MAP_FAILED)
            munmap(cache, CODE_CACHE_BYTES);
        free(d->table);
        free(d->table_used);
        free(d->jc);
        free(d);
        return NULL;
    }
    d->cache = cache;
    d->cache_words = CODE_CACHE_BYTES / 4;
    clear_jump_cache(d);
    appine_cpu_init(&d->probe, NULL);
    emit_routines(d);
    return d;
}

void appine_dbt_free(struct appine_dbt *d)
{
    if (!d)
        return;
    munmap(d->cache, CODE_CACHE_BYTES);
    free(d->table);
    free(d->table_used);
    free(d->jc);
    free(d);
}

enum appine_exit appine_dbt_run(struct appine_dbt *d, struct appine_cpu *c)
{
    uint64_t (*enter)(struct appine_cpu *, uint32_t *) =
        (uint64_t(*)(struct appine_cpu *, uint32_t *))(uintptr_t)d->enter;
    uint32_t *patch_site = NULL;
    uint64_t patch_generation = 0;
    bool fill_jump_cache = false;

    c->dbt_jump_cache = d->jc;
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
            *patch_site = enc_b((intptr_t)code - (intptr_t)patch_site);
            sync_icache((void *)patch_site, (void *)(patch_site + 1));
        }
        patch_site = NULL;
        if (fill_jump_cache) {
            d->jc[(c->pc >> 2) & (JC_SIZE - 1)] = (struct slot){c->pc, code};
            d->jc_dirty = true;
            fill_jump_cache = false;
        }

        switch (enter(c, code)) {
        case R_CHAIN:
            d->stats.exits_chain++;
            patch_site = (uint32_t *)(uintptr_t)c->dbt_site;
            patch_generation = d->generation;
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
            return APPINE_EXIT_SVC;
        default:
            d->stats.exits_kick++;
            break;
        }
    }
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
