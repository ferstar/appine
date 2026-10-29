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
 * 访存指令改写成内联的 TLB 快速路径；TLB 未命中、跨页时进入回填例程，回填失败
 * （未映射、没有权限等）和 SP 未对齐时退出到分发器，由 Tier 0 执行这一条指令
 * （此时指令还没有产生任何副作用）。直接
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

/* 一个地址空间的所有线程共用，JIT 负载要放得下十几万个活着的块。块跳往缓存开头的例程、
 * 块之间的链接都是一条 b，偏移范围是 ±128 MB，所以不能更大 */
#define CODE_CACHE_BYTES (UINT64_C(128) << 20)
#define MAX_BLOCK_INSNS 64
_Static_assert(CODE_CACHE_BYTES <= (UINT64_C(128) << 20), "b 的偏移范围");
/* 单条 guest 指令最多展开成约 40 条宿主指令，再加上退出桩 */
#define MAX_BLOCK_WORDS (MAX_BLOCK_INSNS * 64)
#define MAX_STUBS (MAX_BLOCK_INSNS * 2 + 4)
#define MAX_FIXUPS (MAX_BLOCK_INSNS * 4 + 4)
/* 翻译表从 2^12 项开始，装到四分之三时加倍，最多 2^19 项：表按哈希散开，短命进程的
 * 几百个块要是放进最大的表，每个块都会碰到一页新的内存。一个地址空间的所有线程共用
 * 一张表，JIT 负载（JVM）活着的块有十几万个 */
#define TABLE_MIN_BITS 12
#define TABLE_BITS 19
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
 * 有一条边会经过检查。R_UNGROUP 是共用检查的一组访存没通过检查（见 plan_group），
 * 和 R_INTERP 一样由 Tier 0 执行这一条指令 */
enum { R_CHAIN = 1, R_CHAIN_FWD, R_LOOKUP, R_INTERP, R_SVC, R_KICK, R_UNGROUP };

/* 退出桩之外的另一种桩：TLB 未命中时进入回填例程，回填失败再退出（R_INTERP 或 R_UNGROUP） */
#define STUB_REFILL 0

/* 一个桩最多占的字数：链接桩 5 个字（pc 放不进 48 位时 6 个），退出桩 4 个（5 个），回填桩
 * 5 个 */
#define MAX_STUB_WORDS 6

/* 退出桩的数据字里 pc 占低 48 位，高 16 位是退出原因（emit_link_stub） */
#define PACKED_PC_BITS 48

/* 回填桩里的信息字（emit_refill_stub）：低 12 位是字节数减一（共用检查的一组访存最多覆盖
 * GROUP_SPAN 再加一次访问），接着 3 位是回填失败时的退出原因，高 17 位是访问本身相对信息字
 * 的字数（有符号） */
#define REFILL_REASON_SHIFT 12
#define REFILL_RESUME_SHIFT 15
_Static_assert(MAX_BLOCK_WORDS < 1u << (31 - REFILL_RESUME_SHIFT), "访问相对信息字的字数要放进信息字");

/* 块入口的 kick 检查占的字数 */
#define ENTRY_CHECK_WORDS 2

/* 退出原因的低 3 位是 R_*；链接桩在高位带上跳到它的条件分支相对桩的字数（0 表示没有） */
#define REASON_BITS 3
#define REASON_MASK ((1u << REASON_BITS) - 1)
_Static_assert(R_UNGROUP <= REASON_MASK, "退出原因放在低 3 位");
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
 * 翻译表：guest pc → 翻译结果，开放寻址。分发器不持锁查表（table_lookup），所以：插入只用
 * 空位（先写 pc、再写 code），作废只把 pc 改成墓碑——读到旧 code 的线程进入的是作废块的
 * 入口，它跳往转发桩，按原来的 pc 退出；墓碑不复用，扩容或原样重建时去掉。换表之后等
 * 正在查旧表的线程查完才释放它（table_publish）。
 */
struct table {
    unsigned bits;
    unsigned shift; /* 64 - bits：下标是 pc 的哈希右移这么多位 */
    uint64_t mask;  /* 2^bits - 1 */
    struct slot slots[];
};
#define GOLDEN UINT64_C(0x9e3779b97f4a7c15)

/*
 * 影子返回栈：每项是 (guest 返回地址, 宿主落点)，64 项的环形缓冲按自身大小对齐，压栈、
 * 弹栈只改栈顶指针的低 RS_SHIFT 位（bfxil，不碰条件标志）。
 */
#define RS_SHIFT 10
#define RS_ENTRIES ((1u << RS_SHIFT) / 16)

/* 作废翻译时按 guest 页找到其中开始的块（appine_dbt_invalidate） */
#define PAGE_BUCKET_BITS 14
#define INVALIDATE_MAX_PAGES 256 /* 超过这么多页就逐个看所有的块 */

struct block {
    uint64_t pc;
    uint32_t *code;  /* 已作废时为 NULL */
    uint32_t next;   /* 同一个桶里的上一个块（下标加 1），0 表示没有 */
    uint32_t words;  /* 翻译时的 guest 指令字在 words 里的位置（recheck 用） */
    uint16_t nguest; /* guest 指令条数 */
    uint16_t parked; /* 停放着（revive）：入口像作废块一样跳往转发桩，代码还在 */
    uint32_t first;  /* 停放时被改写的那条入口检查之后的指令 */
};

/* 一组访存第二次没通过共用的检查之后，组长的 pc 记在这里，重新翻译时它和后面同一个
 * 块里的访存都不再当组长；记满了从最早的开始覆盖。第一次失败只记进 strikes（按 pc
 * 直接映射） */
#define NOGROUP_MAX 64
#define STRIKES 64

/* 每个块的 guest 指令字最多占多少字（按块数上限和每块的指令数上限） */
#define WORDS_LIMIT ((size_t)TABLE_LIMIT * MAX_BLOCK_INSNS)
/* 放回池里时最多保留多少个块：核对比重新拷贝模板、链接便宜得多（node 一次启动约 5 万个
 * 块），再多的多半是 JIT 生成的代码，下次多半对不上 */
#define KEEP_MAX_BLOCKS 131072

/*
 * 一个地址空间的翻译（代码缓存、翻译表、各种记录），这个地址空间里的所有线程共用
 * （appine_dbt_attach）：多线程的程序（Go 的编译器、JVM）原来每个线程各翻译一遍同样的
 * 代码，翻译、拷贝模板、链接和同步指令缓存都要做好几遍，代码缓存也各占一份。
 *
 * 除了分发器查翻译表，读写都持有 lock。整体清空要等所有线程离开翻译后的代码：发起的
 * 线程打断其他线程，等它们都停在分发器里（parked）或者离开 dbt_run，见 flush_world。
 */
struct appine_dbt {
    uint32_t *cache;    /* 执行用的地址；所有代码指针都指向这里 */
    intptr_t rw_delta;  /* 写入用的别名减去执行用的地址；直接映射成 RWX 时为 0 */
    struct written *written; /* 可写别名里写过的页（note_written）；RWX 时为 NULL */
    /* 所有实例（活着的、池里的）串成的表 pool.all，appine_dbt_unmap_written 遍历它。持有
     * pool.lock 时读写；不在表里时 prev_instance 为 NULL */
    struct appine_dbt *next_instance, **prev_instance;
    size_t cache_words;
    size_t used_words;
    size_t routine_words;
    uint32_t *enter;
    uint32_t *exit;
    uint32_t *exit_stub; /* 退出桩跳往的入口（emit_link_stub），接着就是 exit */
    uint32_t *exit_full; /* 同上，pc 放不进 48 位的 */
    uint32_t *refill[2]; /* TLB 回填例程：读、写 */
    uint32_t *lookup;    /* 跳转缓存没命中时查翻译表的例程（emit_lookup） */
    uint32_t *rs_miss;   /* 影子返回栈空项的落点：按 guest x30 查找 */
    struct table *table; /* 不持锁读取时用 __atomic_load_n */
    uint32_t *table_used; /* 按插入顺序记下用过的位置，最多 TABLE_LIMIT 个 */
    size_t table_count;
    size_t table_occupied; /* 表里不空的位置：有效的块和墓碑 */
    size_t table_dead;     /* 其中的墓碑 */
    struct block *blocks; /* 与 table_used 一一对应，按翻译顺序记录 */
    size_t blocks_dead;   /* 其中已经作废的 */
    size_t nparked;       /* 其中停放着的（revive） */
    uint32_t *words;      /* 各块翻译时的 guest 指令字，按需映射 */
    size_t words_used;
    /* 换了地址空间（exec、fork 之后从池里取出），下次运行之前要核对 */
    bool stale;
    /* 放回池里之前记下的、翻译所在的映射（appine_dbt_set_sources） */
    struct appine_dbt_source sources[APPINE_DBT_SOURCES];
    unsigned nsources;
    /* 下一次核对时不必看的范围 [start, end)（appine_dbt_trust） */
    uint64_t trusted[APPINE_DBT_SOURCES][2];
    unsigned ntrusted;
    /* 上一个使用者的文件映射 [start, end)：核对时其中还没映射的块停放起来（recheck） */
    uint64_t parkable[APPINE_DBT_SOURCES][2];
    unsigned nparkable;
    /* 运行时宿主出了故障（APPINE_EXIT_HOST_FAULT）：各张表可能只改了一半，不再使用 */
    bool poisoned;
    uint32_t *page_head;  /* guest 页的哈希 → blocks 里的链表头（下标加 1） */
    uint32_t *page_parked; /* 同一个哈希：停放着的块数，查表失败时不持锁看一眼（revive） */
    uint64_t nogroup[NOGROUP_MAX];
    unsigned nogroup_count;
    uint64_t strikes[STRIKES];
    /* 每次 flush 加一：作废尚未回填的链接，线程据此清空自己的跳转缓存和影子返回栈 */
    uint64_t generation;
    size_t high_words; /* 代码缓存用到过的最大字数 */

    pthread_mutex_t lock;
    pthread_cond_t flushed; /* 整体清空结束（或者实例作废） */
    struct appine_dbt_thread *threads; /* 挂着的线程 */
    unsigned nthreads;                 /* 它们的个数（原子读） */
    unsigned running;  /* 正在 dbt_run 里的线程数（原子） */
    unsigned parked;   /* 其中停下来等整体清空的（原子） */
    bool flushing;     /* 有线程在等别人停下来整体清空（原子读） */
    bool flush_wanted; /* 要整体清空：调用方要求，或者作废时放不下转发桩（原子读） */
    struct appine_dbt_stats *st; /* 持锁的线程的统计：翻译、作废记在它头上 */
};

/* 一个线程在某个地址空间里运行用的部分：跳转缓存、影子返回栈和统计 */
struct appine_dbt_thread {
    struct appine_dbt *d;
    struct appine_dbt_thread *next; /* d->threads 链表，持有 d->lock 时读写 */
    struct appine_cpu *cpu; /* 正在 dbt_run 里运行的 cpu，否则为 NULL（原子）：整体清空时打断它 */
    uint64_t generation;    /* jc、rs 对应的 d->generation */
    struct slot *jc;        /* 间接分支的跳转缓存，直接映射 */
    bool jc_dirty;
    uint64_t (*rs)[2]; /* 影子返回栈 */
    struct appine_dbt_stats stats;
    /* 不持锁翻译用的工作区（prepare）：翻译过程、生成的代码和结果 */
    struct xlate *xlate;
    uint32_t *scratch;
    struct appine_cpu *probe;
    uint64_t *defined; /* tier0_defined 的结果，按指令字缓存（DEFINED_CACHE 项） */
    struct prep *prep;
};

/* ---- 发射器 ---- */

enum fixup_kind { FIX_IMM19, FIX_IMM14 };

struct stub {
    unsigned reason; /* R_* 或 STUB_REFILL；R_LOOKUP 的目标 pc 已经在 T0 里 */
    uint64_t pc;
    uint32_t *addr;
    /* 回填桩：回填成功后接着执行的位置（TLB 检查之后的访问）、访问的字节数和方向，
     * 失败时的退出原因 */
    uint32_t *resume;
    unsigned bytes;
    bool write;
    unsigned fail;
    /* 跳到这个桩的分支个数和其中一个的位置（发射桩时填写） */
    unsigned sites;
    const uint32_t *branch;
};

struct fixup {
    uint32_t *site;
    enum fixup_kind kind;
    unsigned stub;
};

/*
 * 块的一个直接跳转出口（链接桩）：桩相对块开头的字数、唯一跳到它的条件分支相对块开头的
 * 字数（没有时为 NO_BRANCH）和目标 pc。建块时目标已经翻译过就直接链接（link_block），
 * 省掉第一次走到这里时退出到分发器、再单独同步一次指令缓存（ic ivau 之后的 dsb 要等
 * 作废广播完成，一次约 0.1 微秒）
 */
struct link {
    uint32_t stub; /* LINK_FWD：前向，跳过目标块入口的 kick 检查 */
    uint32_t branch;
    uint64_t pc;
};
#define LINK_FWD (UINT32_C(1) << 31)
#define NO_BRANCH UINT32_MAX

/* 跳往例程的 b：拷贝翻译模板时要按新位置重新计算偏移 */
enum { TO_EXIT, TO_REFILL_READ, TO_REFILL_WRITE, TO_LOOKUP, TO_EXIT_FULL };

/* 一组共用检查的访存最多覆盖多少字节、最多几条；同时最多几组（plan_group） */
#define GROUP_SPAN 512
#define GROUP_MAX 16
#define GROUP_SLOTS 4
_Static_assert(sizeof(((struct appine_cpu *)0)->dbt_groups) == GROUP_SLOTS * 8, "每组一个槽");
#define MAX_RELOCS (MAX_STUBS * 2 + 4)

struct xlate {
    struct appine_dbt *d;
    uint32_t *buf;
    intptr_t wdelta; /* 写入地址减去 buf：翻译进线程的暂存区时为 0，直接写代码缓存时是 rw_delta */
    struct appine_dbt_stats *st; /* 翻译的统计记在谁头上 */
    struct appine_cpu *probe;    /* 用来询问 Tier 0 是否认识某条指令 */
    uint64_t *defined;           /* 它的回答，按指令字缓存 */
    size_t n;
    struct stub stubs[MAX_STUBS];
    unsigned nstubs;
    struct fixup fixups[MAX_FIXUPS];
    unsigned nfixups;
    uint32_t relocs[MAX_RELOCS]; /* 下标 << 3 | TO_* */
    unsigned nrelocs;
    /* 正在翻译的块：起始 pc、guest 指令字、当前指令的下标、块最多能有几条指令 */
    uint64_t pc;
    const uint32_t *guest;
    unsigned idx, limit;
    /* 本块里 SP 已经检查过对齐，之后只按 16 的倍数改过 */
    bool sp_aligned;
    /* 本块里值在翻译时已知的通用寄存器（ADR/ADRP 设下的，和在它上面加的立即数），第
     * r 位对应 Xr，值在 kval 里（konst_update） */
    uint32_t konst;
    uint64_t kval[31];
    /* 共用检查的几组访存（plan_group）：还没翻译的组员。组的宿主基址在 T2 里，或者暂存
     * 在 cpu 的 dbt_groups[组的下标] 里 */
    struct group {
        unsigned n, next;
        unsigned idx[GROUP_MAX];
        int32_t off[GROUP_MAX]; /* 相对组长执行之前的基址 */
        bool reload[GROUP_MAX]; /* 之前有指令用过 T2：先从槽里取回宿主基址 */
        int32_t lo;             /* 整组覆盖的范围，也相对这个基址 */
        unsigned bytes;
        bool write;
        bool spill; /* 有组员要取回：组长把宿主基址存进槽里 */
    } groups[GROUP_SLOTS];
    int t2_group; /* T2 现在是哪一组的宿主基址，-1 表示都不是 */
};

/* 代码的写入地址（W^X 双映射的可写别名） */
static uint32_t *rw(const struct appine_dbt *d, const uint32_t *p)
{
    return (uint32_t *)((uintptr_t)p + (uintptr_t)d->rw_delta);
}

/* 宿主的页大小（4K、16K）：madvise 的范围要按它对齐 */
static size_t host_page_size(void)
{
    static size_t cached;
    size_t page = __atomic_load_n(&cached, __ATOMIC_RELAXED);
    if (!page) {
        page = (size_t)sysconf(_SC_PAGESIZE);
        __atomic_store_n(&cached, page, __ATOMIC_RELAXED);
    }
    return page;
}

/*
 * 宿主的 RSS 按映射数页：代码缓存的同一页经由写入、执行两个视图都碰过就算两次，
 * RssShmem、LMK 看的 RSS 和内存上限量的用量里代码缓存都多算了一倍。所以记下可写视图里
 * 写过的页，内核层的定时器线程每秒拆一次它们的页表项（appine_dbt_unmap_written）：共享
 * 内存上的 MADV_DONTNEED 只拆映射、内容留着，下次写时再缺一次页。按时间拆而不按量拆：
 * JIT 负载每秒要往几千个不同的页里回填链接、作废块，按量拆（每攒 256 KB 拆一次）时这些
 * 页一次次重新缺页，Maven 慢了 3%。不持锁回填链接的线程也写，位图和计数都是原子的；
 * 多记、漏记一页只影响拆的时机。
 */
struct written {
    unsigned pages; /* bits 里置位的页数（原子） */
    bool dropping;  /* 有线程正在拆（原子） */
    uint64_t bits[CODE_CACHE_BYTES / 4096 / 64]; /* 可写视图里写过、还没拆的宿主页 */
};

/* 拆掉可写视图里记下的页 */
static void drop_written(const struct appine_dbt *d)
{
    struct written *w = d->written;
    if (!w || !__atomic_load_n(&w->pages, __ATOMIC_RELAXED) ||
        __atomic_exchange_n(&w->dropping, true, __ATOMIC_ACQUIRE))
        return;
    size_t lo = SIZE_MAX, hi = 0;
    unsigned n = 0;
    for (size_t i = 0; i < sizeof(w->bits) / sizeof(w->bits[0]); i++) {
        uint64_t bits = __atomic_exchange_n(&w->bits[i], 0, __ATOMIC_RELAXED);
        if (!bits)
            continue;
        n += (unsigned)__builtin_popcountll(bits);
        if (lo == SIZE_MAX)
            lo = i * 64 + (size_t)__builtin_ctzll(bits);
        hi = i * 64 + 63 - (size_t)__builtin_clzll(bits);
    }
    if (n) {
        __atomic_sub_fetch(&w->pages, n, __ATOMIC_RELAXED);
        size_t page = host_page_size();
        madvise((char *)rw(d, d->cache) + lo * page, (hi - lo + 1) * page, MADV_DONTNEED);
    }
    __atomic_store_n(&w->dropping, false, __ATOMIC_RELEASE);
}

/* 经由可写视图写过 [start, end)（执行用的地址） */
static void note_written(const struct appine_dbt *d, const void *start, const void *end)
{
    struct written *w = d->written;
    if (!w || start == end)
        return;
    size_t page = host_page_size();
    size_t first = ((uintptr_t)start - (uintptr_t)d->cache) / page;
    size_t last = ((uintptr_t)end - 1 - (uintptr_t)d->cache) / page;
    for (size_t p = first; p <= last; p++) {
        uint64_t bit = UINT64_C(1) << (p & 63);
        if (!(__atomic_load_n(&w->bits[p / 64], __ATOMIC_RELAXED) & bit) &&
            !(__atomic_fetch_or(&w->bits[p / 64], bit, __ATOMIC_RELAXED) & bit))
            __atomic_add_fetch(&w->pages, 1, __ATOMIC_RELAXED);
    }
}

/* 写入几段代码之后让取指看到它们 */
static void sync_written(const struct appine_dbt *d, void *const *start, void *const *end, unsigned n)
{
    sync_code_ranges(start, end, n, d->rw_delta);
    for (unsigned i = 0; i < n; i++)
        note_written(d, start[i], end[i]);
}

/* 写入 [start, end) 之后让取指看到新代码 */
static void sync_range(const struct appine_dbt *d, const uint32_t *start, const uint32_t *end)
{
    void *s = (void *)start, *e = (void *)end;
    sync_written(d, &s, &e, 1);
}

/*
 * 链接点改写之后不等它生效：改写的线程接着直接进入目标块，用不着新指令；别的线程（和
 * 它自己以后）在看到新指令之前执行的是旧指令，跳往退出例程，由分发器照样找到目标块，
 * 结果一样。缓存维护照做，新指令迟早被看到，只是不等 dsb、不做 isb：java 启动时每个块
 * 要链接一两次，每次完整同步都要等作废广播完成，占了启动时间的两成。
 */
static void publish_link(const struct appine_dbt *d, uint32_t *site, uint32_t *branch)
{
    publish_code_lazily(site, site + 1, d->rw_delta);
    if (branch)
        publish_code_lazily(branch, branch + 1, d->rw_delta);
    note_written(d, site, site + 1);
    if (branch)
        note_written(d, branch, branch + 1);
}

/* 攒着一起同步的几段代码（sync_code_ranges） */
struct syncs {
    void *start[3], *end[3];
    unsigned n;
};

static void sync_add(struct syncs *s, const uint32_t *start, const uint32_t *end)
{
    s->start[s->n] = (void *)start;
    s->end[s->n] = (void *)end;
    s->n++;
}

static void sync_all(const struct appine_dbt *d, struct syncs *s)
{
    if (s->n)
        sync_written(d, s->start, s->end, s->n);
    s->n = 0;
}

static void emit(struct xlate *x, uint32_t insn)
{
    if (x->n < MAX_BLOCK_WORDS)
        *(uint32_t *)((uintptr_t)&x->buf[x->n] + (uintptr_t)x->wdelta) = insn;
    x->n++;
}

static uint32_t *here(struct xlate *x)
{
    return &x->buf[x->n];
}

static uint32_t *routine(const struct appine_dbt *d, unsigned to)
{
    switch (to) {
    case TO_EXIT:
        return d->exit_stub;
    case TO_EXIT_FULL:
        return d->exit_full;
    case TO_LOOKUP:
        return d->lookup;
    default:
        return d->refill[to - TO_REFILL_READ];
    }
}

/* 跳往例程的 b：TO_EXIT 是桩用的退出入口（exit_stub，见 emit_link_stub） */
static void emit_to_routine(struct xlate *x, unsigned to)
{
    if (x->nrelocs < MAX_RELOCS)
        x->relocs[x->nrelocs] = (uint32_t)x->n << 3 | to;
    x->nrelocs++; /* 溢出由 prepare() 检查 */
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
 * 退出桩给退出例程的是 T0 = guest pc、T1 = 回填位置、T2 = 退出原因（emit_routines）。guest pc
 * 是 48 位的地址，拼成立即数要三四条 mov，所以和退出原因一起放在桩末尾的两个数据字里
 * （原因在高 16 位），由例程的桩入口（exit_stub）读出来：
 *
 *     [b 4]   adr T1, 0   b exit_stub   .quad 原因 << 48 | pc
 *
 * 链接桩的第一条指令是跳到下一条的 b，链接时它被改写成直接跳往目标块的 b：别的线程
 * 可能正在执行它，b 改成 b 是架构允许并发修改的（ARM ARM 的 CMODX 规则）。例程把 T1 减 4
 * 交给分发器：链接桩的是它的回填位置，别的桩的分发器不看。branch 是唯一跳到这个桩的
 * 条件分支，链接时目标在它的偏移范围内就让它直接跳过去（只在没有别的线程时，见 dbt_run）。
 * 跳转目标算出来超出 48 位时（guest 跳过去只会出错）原因另用 movz 交给 exit_full。
 */
static void emit_link_stub(struct xlate *x, unsigned reason, uint64_t pc, const uint32_t *branch)
{
    unsigned delta = branch ? (unsigned)(here(x) - branch) : 0;
    if (reason == R_LOOKUP) { /* 目标 pc 已经在 T0 里：先在翻译后的代码里查表 */
        emit_to_routine(x, TO_LOOKUP);
        return;
    }
    if (reason == R_CHAIN || reason == R_CHAIN_FWD)
        emit(x, enc_b(4));
    emit(x, enc_adr_self(T1));
    uint64_t code = reason | delta << REASON_BITS;
    if (pc >> PACKED_PC_BITS == 0) {
        emit_to_routine(x, TO_EXIT);
        pc |= code << PACKED_PC_BITS;
    } else {
        emit(x, enc_movz(T2, (unsigned)code, 0));
        emit_to_routine(x, TO_EXIT_FULL);
    }
    emit(x, (uint32_t)pc);
    emit(x, (uint32_t)(pc >> 32));
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
        x->nstubs++; /* 溢出由 prepare() 检查 */
    }
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups++] = (struct fixup){here(x), kind, i};
    else
        x->nfixups++; /* 记录溢出，prepare() 会检查 */
    emit(x, insn);
}

/* pc 处的指令直接跳往 target */
static unsigned chain_reason(uint64_t pc, uint64_t target)
{
    return target > pc ? R_CHAIN_FWD : R_CHAIN;
}

/* TLB 未命中时跳往回填桩：每条访存指令一个，不和别的桩合并。回填成功后从紧接着的
 * 指令（访问本身）继续 */
static void emit_to_refill(struct xlate *x, uint64_t pc, unsigned bytes, bool write, unsigned fail)
{
    if (x->nstubs < MAX_STUBS)
        x->stubs[x->nstubs] = (struct stub){.reason = STUB_REFILL,
                                            .pc = pc,
                                            .resume = here(x) + 1,
                                            .bytes = bytes,
                                            .write = write,
                                            .fail = fail};
    if (x->nfixups < MAX_FIXUPS)
        x->fixups[x->nfixups] = (struct fixup){here(x), FIX_IMM19, x->nstubs};
    x->nstubs++;
    x->nfixups++; /* 溢出由 prepare() 检查 */
    emit(x, enc_cbnz_x(T1));
}

/*
 * 回填桩：T1 指向信息字，进入回填例程；信息字后面是这条指令的 guest pc。例程成功时 T2 =
 * addend，跳回访问本身；失败时按信息字里的原因带着 pc 退出，由 Tier 0 执行这条指令。
 *
 *     adr T1, 8   b 回填例程   .word 信息   .quad pc
 */
static void emit_refill_stub(struct xlate *x, const struct stub *s)
{
    if (s->bytes > 1u << REFILL_REASON_SHIFT || s->fail >> (REFILL_RESUME_SHIFT - REFILL_REASON_SHIFT))
        abort(); /* 信息字放不下：属于引擎 bug */
    emit(x, enc_adr(T1, 8));
    emit_to_routine(x, s->write ? TO_REFILL_WRITE : TO_REFILL_READ);
    intptr_t resume = s->resume - here(x);
    emit(x, (uint32_t)resume << REFILL_RESUME_SHIFT | s->fail << REFILL_REASON_SHIFT | (s->bytes - 1));
    emit(x, (uint32_t)s->pc);
    emit(x, (uint32_t)(s->pc >> 32));
}

static bool fallback(struct xlate *x, uint64_t pc)
{
    x->st->insns_fallback++;
    emit_exit_stub(x, R_INTERP, pc);
    return false;
}

/* 回填 b/cbz/cbnz/tbz/tbnz 的偏移 */
static void patch_branch(intptr_t wdelta, uint32_t *site, const uint32_t *target)
{
    uint32_t off = (uint32_t)(target - site), *w = (uint32_t *)((uintptr_t)site + (uintptr_t)wdelta);
    if ((*w & 0x7c000000) == 0x14000000)
        *w |= off & 0x3ffffff;
    else if ((*w & 0x7e000000) == 0x36000000)
        *w |= (off & 0x3fff) << 5;
    else
        *w |= (off & 0x7ffff) << 5;
}

/* 块内的局部跳转；溢出的块不会被使用（prepare() 会检查），只是不能写出界 */
static void patch_local(struct xlate *x, uint32_t *site, const uint32_t *target)
{
    if (site < x->buf + MAX_BLOCK_WORDS)
        patch_branch(x->wdelta, site, target);
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
        x->st->insns_copied++;
        return true;
    }
    if (nmap == 3)
        x->t2_group = -1; /* 之后的组员从槽里取回宿主基址（plan_group） */
    for (unsigned k = 0; k < nmap; k++)
        emit_load_guest(x, pool[k], guest[k]);
    emit(x, out);
    if (rd_stolen)
        emit_store_guest(x, rd_host, rd_guest);
    x->st->insns_remapped++;
    return true;
}

/* ADR/ADRP：结果是编译期常量 */
static void emit_pcrel(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned rd = insn & 31;
    uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
    int64_t off = (int64_t)(imm << 43) >> 43;
    uint64_t v = BIT(insn, 31) ? (pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12) : pc + (uint64_t)off;
    x->st->insns_remapped++;
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
 * 表项里，所以这一次比较同时排除了跨页访问。未命中和跨页时进入回填例程，回填失败
 * 时以 `fail` 退出。`in_granule` 表示访问落在一个对齐的 16 字节里，不可能跨页，
 * 可以直接用第一个字节的页。`last_ready` 表示调用方已经把最后一个字节的地址算进了
 * T1（[`emit_address_imm`]）。
 */
static void emit_tlb_check(struct xlate *x, uint64_t pc, unsigned bytes, bool write, bool in_granule,
                           bool last_ready, unsigned fail)
{
    x->t2_group = -1;
    unsigned last = T0;
    if (last_ready) {
        last = T1;
    } else if (bytes > 1 && !in_granule) {
        emit(x, enc_add_imm(T1, T0, bytes - 1, 0));
        last = T1;
    }
    emit(x, enc_ubfx(T1, last, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(T1, CTX, T1, 5));
    emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT)); /* tag 是页号取反 */
    emit_to_refill(x, pc, bytes, write, fail);
}

/*
 * 访问的 guest 地址在翻译时已知（`addr`，T0 里也是它，访问不跨页）：表项的位置是常数，
 * 取表项不必等 T0 算出来（全局变量：ADRP 之后的访存，T0 要等 ADRP 换成的几条 movk）。
 */
static void emit_tlb_check_known(struct xlate *x, uint64_t pc, uint64_t addr, unsigned bytes, bool write,
                                 unsigned fail)
{
    x->t2_group = -1;
    uint64_t off = TLB_OFF + ((addr >> APPINE_PAGE_SHIFT) & (APPINE_TLB_ENTRIES - 1)) * 32 + (write ? 16 : 0);
    int64_t lo = (int64_t)(off & 0xfff);
    uint64_t hi = off - (uint64_t)lo;
    if (lo > 504 && lo >= 4096 - 512) { /* ldp 的偏移是 -512..504 */
        hi += 4096;
        lo -= 4096;
    }
    unsigned b = CTX;
    if (hi) {
        emit(x, enc_add_imm(T1, CTX, (uint32_t)(hi >> 12), 1));
        b = T1;
    }
    if (lo > 504) {
        emit(x, enc_add_imm(T1, b, (uint32_t)lo, 0));
        b = T1;
        lo = 0;
    }
    emit(x, enc_ldp_x(T1, T2, b, (int)lo));
    emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT)); /* tag 是页号取反 */
    emit_to_refill(x, pc, bytes, write, fail);
}

/* 以已知值的寄存器 `rn` 为基址、偏移 `imm` 访问 `bytes` 字节时的 guest 地址；寄存器的值
 * 不知道或者访问跨页时返回 false */
static bool known_address(const struct xlate *x, unsigned rn, int64_t imm, unsigned bytes, uint64_t *addr)
{
    if (rn > 30 || !(x->konst >> rn & 1))
        return false;
    *addr = x->kval[rn] + (uint64_t)imm;
    return ((*addr ^ (*addr + bytes - 1)) >> APPINE_PAGE_SHIFT) == 0;
}

/*
 * T0 = base + imm。要取最后一个字节的页时，T1 = base + imm + bytes - 1 不经过 T0、直接
 * 从基址算：TLB 的下标不必等 T0 算出来，访存的依赖链上少一步（追指针的读每次都走完
 * 整条链），指令数不变。返回 T1 是否已经算好。
 */
static bool emit_address_imm(struct xlate *x, unsigned base, int64_t imm, unsigned bytes, bool in_granule)
{
    int64_t last = imm + (int64_t)bytes - 1;
    bool fold = bytes > 1 && !in_granule && last > -4096 && last < 4096;
    if (fold)
        emit(x, last >= 0 ? enc_add_imm(T1, base, (uint32_t)last, 0) : enc_sub_imm(T1, base, (uint32_t)-last, 0));
    emit_add_const(x, T0, base, imm);
    return fold;
}

/* 访存的字节数 */
static unsigned memop_bytes(const struct memop *m)
{
    return m->structure ? m->bytes : (m->pair ? 2u : 1u) << m->size;
}

/* 以 SP 为基址访存之前检查 SP 的对齐；本块里已经检查过、之后只按 16 的倍数改过时不用 */
static void emit_sp_check(struct xlate *x, uint64_t pc)
{
    if (x->sp_aligned)
        return;
    emit(x, enc_and_low(T1, SP_HOST, 4));
    emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    x->sp_aligned = true;
}

/* SP 改了 `delta` 之后是否仍然对齐 */
static void sp_moved(struct xlate *x, int64_t delta)
{
    if (delta & 15)
        x->sp_aligned = false;
}

static int plan_group(struct xlate *x, const struct memop *lead);
static void emit_group(struct xlate *x, const struct memop *lead, uint64_t pc, int slot);

static void emit_memop(struct xlate *x, const struct memop *m, uint64_t pc)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn;
    int slot = plan_group(x, m);
    if (slot >= 0) {
        emit_group(x, m, pc, slot);
        return;
    }

    /* 1. guest 地址 → T0 */
    bool in_granule = false, last_ready = false, known = false;
    uint64_t addr = 0;
    if (m->mode == M_LITERAL) {
        emit_mov_const(x, T0, (uint64_t)m->imm);
        addr = (uint64_t)m->imm;
        known = ((addr ^ (addr + memop_bytes(m) - 1)) >> APPINE_PAGE_SHIFT) == 0;
    } else {
        unsigned base = rn == REG_SP ? SP_HOST : rn;
        if (rn == REG_SP) { /* SP 必须 16 字节对齐 */
            emit_sp_check(x, pc);
            int64_t off = m->mode == M_POST ? 0 : m->imm;
            in_granule = m->mode != M_REG && (off & 15) + memop_bytes(m) <= 16;
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
            last_ready = emit_address_imm(x, base, 0, memop_bytes(m), in_granule);
            break;
        default:
            known = m->mode == M_OFFSET && known_address(x, m->rn, m->imm, memop_bytes(m), &addr);
            if (known)
                emit_add_const(x, T0, base, m->imm);
            else
                last_ready = emit_address_imm(x, base, m->imm, memop_bytes(m), in_granule);
            break;
        }
    }

    /* 2. TLB */
    if (known)
        emit_tlb_check_known(x, pc, addr, memop_bytes(m), !m->load, R_INTERP);
    else
        emit_tlb_check(x, pc, memop_bytes(m), !m->load, in_granule, last_ready, R_INTERP);

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
        if (rn == REG_SP)
            sp_moved(x, m->wb_reg ? 1 : m->imm);
    }
    x->st->insns_memory++;
}

/* ---- 共用一次 TLB 检查的一组访存 ---- */

/*
 * 同一个块里以同一个寄存器为基址的几条访存（函数的序言和尾声、结构体的几个字段）
 * 共用一次检查：组长检查整组覆盖的范围，之后 T2 = 基址在组长执行之前的值对应的宿主
 * 地址，组员直接用 [T2, #偏移] 访问。组员之间只能有不写基址（按常数加减的除外，记下
 * 累计的偏移）的指令和条件分支，SP 只能按 16 的倍数改变（组长的对齐检查对组员也成立）。
 *
 * 组员之间可以夹着别的访存（别的组、不分组的）和用到 T2 的指令：组长把宿主基址存进
 * cpu 的 dbt_groups[槽]，它们之后的组员先取回来。槽的地址是常数，可以早早取出，组员的
 * 访问不必等 TLB 查表：python 的解释循环里栈帧、代码对象这几个基址的访存互相交错，原来
 * 一个块里十几次检查一次也省不下。一个块执行期间映射不会变（只会回填 TLB），宿主基址
 * 在基址改变之前一直有效。同时最多 GROUP_SLOTS 组。
 *
 * 跨页时回填例程按两页处理，宿主地址不连续时检查失败。条件分支后面的组员不一定会
 * 执行，覆盖的范围里也可能有它们访问不到的页，所以检查失败时以 R_UNGROUP 退出，由
 * Tier 0 执行组长；同一个组长第二次失败时作废这个块，重新翻译时不再分组（ungroup）。
 */

static bool is_atomic(uint32_t insn);

/* 组员的访问指令：宿主地址 = Rn + off，Rn、Rt 字段为 0。立即数放不下时返回 false */
static bool member_insn(const struct memop *m, int64_t off, uint32_t *out)
{
    unsigned sz = m->size;
    if (m->structure) { /* LD1–LD4/ST1–ST4 没有偏移 */
        *out = m->final;
        return off == 0;
    }
    if (m->pair) {
        if (off & ((1 << sz) - 1) || off < -64 * (1 << sz) || off > 63 * (1 << sz))
            return false;
        *out = m->final | ((uint32_t)(off >> sz) & 0x7f) << 15;
        return true;
    }
    uint32_t common = m->final & 0xc4c00000; /* size、V、opc */
    if (off >= 0 && !(off & ((1 << sz) - 1)) && off >> sz < 4096) { /* 无符号偏移 */
        *out = common | 0x39000000 | (uint32_t)(off >> sz) << 10;
        return true;
    }
    if (off >= -256 && off < 256) { /* LDUR/STUR */
        *out = common | 0x38000000 | ((uint32_t)off & 0x1ff) << 12;
        return true;
    }
    return false;
}

/* 立即数放不下时借来放宿主地址的寄存器：被占用的 Rt 用 T1（成对时用 T0、T1） */
static unsigned member_scratch(const struct memop *m)
{
    return m->pair && !m->simd && is_stolen(m->rt) ? T1 : T0;
}

static bool member_fits(const struct memop *m, int64_t off)
{
    uint32_t insn = 0;
    if (member_insn(m, off, &insn))
        return true;
    return !(m->pair && !m->simd && is_stolen(m->rt) && is_stolen(m->rt2));
}

/* pc 处的访存能不能当组长：记下的组长和它后面一个块的长度以内都不能，它原来的组员
 * 多半也会因为同样的原因通不过检查 */
static bool nogroup(const struct appine_dbt *d, uint64_t pc)
{
    unsigned n = d->nogroup_count < NOGROUP_MAX ? d->nogroup_count : NOGROUP_MAX;
    for (unsigned i = 0; i < n; i++) {
        if (pc - d->nogroup[i] < 4 * MAX_BLOCK_INSNS)
            return true;
    }
    return false;
}

/*
 * 不写基址、不结束块的指令可以夹在组员之间：返回 1，用到 T2 时返回 2，否则返回 0。按常数
 * 加减基址的 ADD/SUB（立即数）把改变量记进 `delta`。
 */
static int keeps_group(uint32_t insn, unsigned rn, int64_t *delta)
{
    unsigned top = FIELD(insn, 28, 25), stolen[3], nstolen = 0;
    struct operands o = {.n = 0};
    if ((top & 7) == 7) { /* SIMD&FP 数据处理：只有通用寄存器操作数要看 */
        struct native_desc nd;
        if (!appine_simd_dp_decode(insn, &nd))
            return false;
        for (unsigned i = 0; i < nd.n; i++) {
            add_operand(&o, nd.gpr[i].pos, false);
            if (nd.gpr[i].store && ((insn >> nd.gpr[i].pos) & 31) == rn && rn != 31)
                return false;
        }
    } else if (top == 0x8 || top == 0x9 || top == 0x5 || top == 0xd) {
        if (FIELD(insn, 28, 24) == 0x10) /* ADR/ADRP：被占用的 Rd 只用 T0 */
            return (insn & 31) != rn || rn == 31;
        if (!dp_operands(insn, &o))
            return false;
        uint32_t op = insn & 0xff800000;
        if ((op == 0x91000000 || op == 0xd1000000) && (insn & 31) == rn && FIELD(insn, 9, 5) == rn) {
            int64_t imm = (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0);
            *delta += op == 0xd1000000 ? -imm : imm;
            return true;
        }
        unsigned rd = insn & 31;
        if (o.writes_rd && (rn == 31 ? rd == 31 && o.sp[0] : rd == rn))
            return false;
    } else {
        return (insn & 0xff000010) == 0x54000000 || /* B.cond */
               (insn & 0x7e000000) == 0x34000000 || /* CBZ/CBNZ */
               (insn & 0x7e000000) == 0x36000000 || /* TBZ/TBNZ */
               (insn & 0xfffff01f) == 0xd503201f || /* HINT */
               (insn & 0xfffff01f) == 0xd503301f;   /* 屏障、CLREX */
    }
    /* 被占用的寄存器依次换成 T0、T1、T2（emit_operands）：三个就用到了 T2 */
    for (unsigned i = 0; i < o.n; i++) {
        unsigned r = (insn >> o.pos[i]) & 31, k;
        if ((r == 31 && o.sp[i]) || !is_stolen(r))
            continue;
        for (k = 0; k < nstolen && stolen[k] != r; k++)
            ;
        if (k == nstolen && nstolen++ == 2)
            return 2;
        stolen[k] = r;
    }
    return 1;
}

/* 访存 `m` 执行时基址比组长执行之前多 `delta`：访问范围的起点（相对组长之前的基址） */
static int64_t member_start(const struct memop *m, int64_t delta)
{
    return delta + (m->mode == M_POST ? 0 : m->imm);
}

/* 从访存 `lead`（块里的第 x->idx 条）开始往后找同一基址的组员；至少有一个组员时在一个
 * 空闲的槽里填好这一组，返回槽的下标，否则返回 -1 */
static int plan_group(struct xlate *x, const struct memop *lead)
{
    int slot = -1;
    for (int i = 0; i < GROUP_SLOTS && slot < 0; i++) {
        if (x->groups[i].next >= x->groups[i].n)
            slot = i;
    }
    if (slot < 0)
        return -1;
    struct group *g = &x->groups[slot];
    unsigned rn = lead->rn;
    uint64_t pc0 = x->pc + 4ull * x->idx;
    g->n = g->next = 0;
    g->spill = false;
    if (lead->mode == M_LITERAL || lead->mode == M_REG || lead->wb_reg || (rn != 31 && is_stolen(rn)) ||
        nogroup(x->d, pc0))
        return -1;
    int64_t delta = 0, lo = member_start(lead, 0), hi = lo + memop_bytes(lead);
    bool write = !lead->load;
    if (!member_fits(lead, lo))
        return -1;
    g->idx[0] = x->idx;
    g->off[0] = (int32_t)lo;
    g->reload[0] = false;
    g->n = 1;
    if (lead->mode != M_OFFSET)
        delta = lead->imm;
    bool overwritten = lead->load && !lead->simd && (lead->rt == rn || (lead->pair && lead->rt2 == rn));
    bool clobbered = false; /* 上一个组员之后有指令用了 T2 */
    for (unsigned j = x->idx + 1; !overwritten && j < x->limit && g->n < GROUP_MAX; j++) {
        uint32_t insn = x->guest[j];
        unsigned top = FIELD(insn, 28, 25);
        if (top != 0x4 && top != 0x6 && top != 0xc && top != 0xe) {
            int keep = keeps_group(insn, rn, &delta);
            if (!keep)
                break;
            clobbered |= keep == 2;
            continue;
        }
        struct memop m;
        uint64_t pc = x->pc + 4ull * j;
        if (is_atomic(insn) || !(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m)))
            break;
        if (m.prefetch) /* 不生成代码 */
            continue;
        if (m.rn != rn) {
            /* 别的基址的访存要用 T2；读进来的值改写了基址就到此为止 */
            if (m.load && !m.simd && (m.rt == rn || (m.pair && m.rt2 == rn)))
                break;
            clobbered = true;
            continue;
        }
        if (m.mode == M_LITERAL || m.mode == M_REG || (rn == 31 && (delta & 15)))
            break;
        int64_t start = member_start(&m, delta), end = start + memop_bytes(&m);
        int64_t nlo = start < lo ? start : lo, nhi = end > hi ? end : hi;
        if (nhi - nlo > GROUP_SPAN || !member_fits(&m, start))
            break;
        lo = nlo;
        hi = nhi;
        write |= !m.load;
        g->idx[g->n] = j;
        g->off[g->n] = (int32_t)start;
        g->reload[g->n] = clobbered;
        g->spill |= clobbered;
        clobbered = false;
        g->n++;
        if (m.mode != M_OFFSET) {
            if (m.wb_reg)
                break;
            delta += m.imm;
        }
        overwritten = m.load && !m.simd && (m.rt == rn || (m.pair && m.rt2 == rn));
    }
    if (g->n < 2) {
        g->n = 0;
        return -1;
    }
    g->next = g->n; /* 组长的检查发射之后才开始 */
    g->lo = (int32_t)lo;
    g->bytes = (unsigned)(hi - lo);
    g->write = write;
    return slot;
}

/* 组员（包括组长）的访问：宿主地址 = T2 + off，然后回写基址 */
static void emit_member(struct xlate *x, const struct memop *m, int64_t off)
{
    unsigned rn = m->rn == 31 ? REG_SP : m->rn, base = rn == REG_SP ? SP_HOST : rn, addr = T2;
    bool s1 = !m->simd && is_stolen(m->rt), s2 = m->pair && !m->simd && is_stolen(m->rt2);
    uint32_t insn = 0;
    if (!member_insn(m, off, &insn)) {
        addr = member_scratch(m);
        emit_add_const(x, addr, T2, off);
        member_insn(m, 0, &insn);
    }
    if (m->structure) {
        emit(x, insn | addr << 5);
    } else if (!m->pair) {
        unsigned rt = s1 ? T1 : m->rt;
        if (s1 && !m->load)
            emit_load_guest(x, T1, m->rt);
        emit(x, insn | addr << 5 | rt);
        if (s1 && m->load)
            emit_store_guest(x, T1, m->rt);
    } else {
        unsigned h1 = s1 ? T0 : m->rt, h2 = s2 ? T1 : m->rt2;
        if (!m->load) {
            if (s1)
                emit_load_guest(x, T0, m->rt);
            if (s2)
                emit_load_guest(x, T1, m->rt2);
        }
        emit(x, insn | h2 << 10 | addr << 5 | h1);
        if (m->load) {
            if (s1)
                emit_store_guest(x, T0, m->rt);
            if (s2)
                emit_store_guest(x, T1, m->rt2);
        }
    }
    if (m->mode == M_PRE || m->mode == M_POST) {
        if (m->wb_reg) {
            unsigned inc = m->rm;
            if (is_stolen(inc)) {
                emit_load_guest(x, T1, inc);
                inc = T1;
            }
            emit(x, enc_add_lsl(base, base, inc, 0));
        } else {
            emit_add_const(x, base, base, m->imm);
        }
        if (rn == REG_SP)
            sp_moved(x, m->wb_reg ? 1 : m->imm);
    }
    x->st->insns_memory++;
}

/* 组长：检查整组覆盖的范围，T2 = 基址的宿主地址（有组员要取回时存进槽里），再做组长
 * 自己的访问 */
static void emit_group(struct xlate *x, const struct memop *lead, uint64_t pc, int slot)
{
    struct group *g = &x->groups[slot];
    unsigned rn = lead->rn == 31 ? REG_SP : lead->rn, base = rn == REG_SP ? SP_HOST : rn;
    if (rn == REG_SP)
        emit_sp_check(x, pc);
    bool in_granule = rn == REG_SP && (g->lo & 15) + g->bytes <= 16;
    uint64_t addr;
    if (known_address(x, rn, g->lo, g->bytes, &addr)) {
        emit_add_const(x, T0, base, g->lo);
        emit_tlb_check_known(x, pc, addr, g->bytes, g->write, R_UNGROUP);
    } else {
        bool last_ready = emit_address_imm(x, base, g->lo, g->bytes, in_granule);
        emit_tlb_check(x, pc, g->bytes, g->write, in_granule, last_ready, R_UNGROUP);
    }
    emit(x, enc_add_lsl(T2, T2, base, 0));
    if (g->spill)
        emit(x, enc_str_x(T2, CTX, offsetof(struct appine_cpu, dbt_groups) + 8 * (size_t)slot));
    x->t2_group = slot;
    g->next = 1;
    emit_member(x, lead, g->off[0]);
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
        emit_sp_check(x, pc);
    } else if (is_stolen(rn)) {
        emit_load_guest(x, T0, rn);
    } else {
        emit(x, enc_mov(T0, rn));
    }
    if (bytes > 1) {
        emit(x, enc_and_low(T1, T0, (unsigned)__builtin_ctz(bytes)));
        emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
    }
    emit_tlb_check(x, pc, bytes, write, true, false, R_INTERP);
    emit(x, enc_add_lsl(T2, T2, T0, 0));
    x->st->insns_memory++;
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

    emit_atomic_address(x, rn, bytes, true, pc);
    /*
     * 要写的值在被占用的寄存器里（libgcc 的 outline atomics 都这样写：Alpine 上它们不
     * 认 LSE，总是走 LL/SC 的循环）：CAS 循环里 T0、T1、T2 都有用处，借一个这条指令
     * 没用到的 guest 寄存器放它。借来的寄存器先存进 cpu 结构体里它自己的位置（翻译后的
     * 代码运行期间那里没有用），用完恢复；中间不会退出，宿主故障也不从 ucontext 恢复
     * guest 状态（guard.c），所以没有人看得到它被借走。原来整条交给 Tier 0，node 启动时
     * 每次要退出一万多次。
     */
    unsigned v = rt, borrow = 0;
    if (is_stolen(rt)) {
        while (borrow == rn || borrow == rs)
            borrow++;
        emit_store_guest(x, borrow, borrow);
        emit_load_guest(x, borrow, rt);
        v = borrow;
    }
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
    emit(x, enc_stlxr(size, T1, v, T2));
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
    if (v != rt)
        emit_load_guest(x, borrow, borrow);
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
    /* 宿主 x30 是落点，guest 返回地址在它末尾的数据字里（emit_link_stub） */
    if ((pc + 4) >> PACKED_PC_BITS == 0) {
        emit(x, enc_ldur_x(T2, 30, 12));
        emit(x, enc_and_low(T2, T2, PACKED_PC_BITS));
    } else {
        emit(x, enc_ldur_x(T2, 30, 16));
    }
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
            x->st->insns_remapped++;
            return true;
        }
        unsigned op2 = FIELD(insn, 7, 5);
        if (op2 == 4 || op2 == 6) { /* DSB、ISB：先把 ic ivau 记下的页交出去（见 dbt_run） */
            emit(x, enc_ldr_x(T0, CTX, offsetof(struct appine_cpu, ivau_page)));
            emit_to_stub(x, enc_cbnz_x(T0), FIX_IMM19, R_INTERP, pc);
        }
        emit(x, insn);
        x->st->insns_copied++;
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
            x->st->insns_remapped++;
            return true;
        }
        case SYSREG_DCZID_EL0: /* 只读，写入已被 tier0_defined 拒绝 */
            if (rt != 31) {
                unsigned h = is_stolen(rt) ? T0 : rt;
                emit(x, enc_movz(h, DCZID_VALUE, 0));
                if (h == T0)
                    emit_store_guest(x, T0, rt);
            }
            x->st->insns_remapped++;
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
        emit_tlb_check(x, pc, DCZ_BLOCK, true, false, false, R_INTERP);
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        for (int off = 0; off < DCZ_BLOCK; off += 16)
            emit(x, enc_stp_x(31, 31, T2, off));
        x->st->insns_memory++;
        return true;
    }
    /*
     * DC CVAC、DC CVAU、DC CIVAC、IC IVAU：与 Tier 0 一样只按读访问检查地址。ic ivau 不再
     * 每条都退出：记下所在的页（JIT 刷新缓存时一页要执行几十条），等 dsb、isb 时一次交给
     * 调用方作废翻译；已经记着另一页时由 Tier 0 执行这一条，退出时两页一起作废。
     */
    uint32_t cmo = insn & 0xffffffe0;
    if (cmo == 0xd50b7a20 || cmo == 0xd50b7b20 || cmo == 0xd50b7e20 || cmo == 0xd50b7520) {
        unsigned rt = insn & 31;
        if (rt == 31)
            emit(x, enc_movz(T0, 0, 0));
        else if (is_stolen(rt))
            emit_load_guest(x, T0, rt);
        else
            emit(x, enc_mov(T0, rt));
        emit_tlb_check(x, pc, 1, false, true, false, R_INTERP);
        if (cmo == 0xd50b7520) {
            emit(x, enc_and_clear_low(T0, T0, APPINE_PAGE_SHIFT));
            emit(x, enc_ldr_x(T1, CTX, offsetof(struct appine_cpu, ivau_page)));
            emit(x, enc_eor_lsr(T2, T1, T0, 0));
            emit(x, enc_cbz_x(T2) | 3u << 5); /* 同一页：跳过下面两条 */
            emit_to_stub(x, enc_cbnz_x(T1), FIX_IMM19, R_INTERP, pc);
            emit(x, enc_str_x(T0, CTX, offsetof(struct appine_cpu, ivau_page)));
        }
        x->st->insns_memory++;
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

/*
 * 询问 Tier 0 是否认识这条指令（§4.4：两层的白名单必须一致）。回答只取决于指令字，按它
 * 缓存在 `cache` 里（直接映射，每项是 1 << 40 | 指令字 << 1 | 回答）：Tier 0 要把指令
 * 执行一遍才知道，java 启动时翻译的每条指令都问一次，占了两个百分点。
 */
#define DEFINED_CACHE 4096
static bool tier0_defined(struct appine_cpu *probe, uint64_t *cache, uint32_t insn, uint64_t pc)
{
    uint64_t *e = &cache[(insn * UINT32_C(0x9e3779b1)) >> 20];
    if ((*e >> 1) == ((uint64_t)1 << 39 | insn))
        return *e & 1;
    probe->pc = pc;
    probe->sp = 0;
    bool ok = appine_interp_exec(probe, insn) != APPINE_EXIT_UNDEF;
    *e = (uint64_t)1 << 40 | (uint64_t)insn << 1 | ok;
    return ok;
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
static bool translate_insn_body(struct xlate *x, uint32_t insn, uint64_t pc);

/*
 * 这条指令之后哪些寄存器的值已知（xlate.konst）：ADR/ADRP 设下常数，在已知值上加立即数
 * 仍然已知，其余可能写通用寄存器的指令让它们失效。拿不准的一律当作全都写了。
 */
static void konst_update(struct xlate *x, uint32_t insn, uint64_t pc)
{
    unsigned rd = insn & 31, rn = FIELD(insn, 9, 5);
    uint32_t kill = 1u << rd;
    if (FIELD(insn, 28, 24) == 0x10) { /* ADR/ADRP */
        uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
        int64_t off = (int64_t)(imm << 43) >> 43;
        if (rd != 31) {
            x->konst |= 1u << rd;
            x->kval[rd] = BIT(insn, 31) ? (pc & ~UINT64_C(0xfff)) + ((uint64_t)off << 12) : pc + (uint64_t)off;
        }
        return;
    }
    if ((insn & 0xff800000) == 0x91000000 && rd != 31 && rn != 31 && (x->konst >> rn & 1)) {
        /* ADD Xd, Xn, #imm{, lsl #12}（不设标志） */
        x->kval[rd] = x->kval[rn] + ((uint64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0));
        x->konst |= 1u << rd;
        return;
    }
    switch (FIELD(insn, 28, 25)) {
    case 0x8:
    case 0x9:
    case 0x5:
    case 0xd:
    case 0x7:
    case 0xf: /* 数据处理（SIMD 的也可能写通用寄存器，如 UMOV） */
        break;
    case 0x4:
    case 0x6:
    case 0xc:
    case 0xe: {
        struct memop m;
        if (is_atomic(insn)) { /* Rt、Rs，成对的再加一个 */
            unsigned rs = FIELD(insn, 20, 16);
            kill |= 3u << rd | 3u << rs;
            break;
        }
        if (!(BIT(insn, 26) ? decode_simd_memop(insn, pc, &m) : decode_memop(insn, pc, &m))) {
            kill = ~0u;
            break;
        }
        kill = 0;
        if (m.load && !m.simd && !m.prefetch) {
            kill |= 1u << m.rt;
            if (m.pair)
                kill |= 1u << m.rt2;
        }
        if (m.mode == M_PRE || m.mode == M_POST)
            kill |= 1u << m.rn;
        break;
    }
    default:
        /* 条件分支、CBZ、TBZ、HINT、屏障不写寄存器；其余（MRS、BL 等）拿不准 */
        if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000 ||
            (insn & 0x7e000000) == 0x36000000 || (insn & 0xfffff01f) == 0xd503201f ||
            (insn & 0xfffff01f) == 0xd503301f)
            kill = 0;
        else
            kill = ~0u;
        break;
    }
    x->konst &= ~kill;
}

static bool translate_insn(struct xlate *x, uint32_t insn, uint64_t pc)
{
    bool r = translate_insn_body(x, insn, pc);
    konst_update(x, insn, pc);
    return r;
}

static bool translate_insn_body(struct xlate *x, uint32_t insn, uint64_t pc)
{
    if ((FIELD(insn, 28, 25) & 7) == 7)
        return translate_simd_dp(x, insn) || fallback(x, pc);
    if (!tier0_defined(x->probe, x->defined, insn, pc))
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
        if (o.writes_rd && o.sp[0] && (insn & 31) == 31) { /* 写了 SP */
            uint32_t op = insn & 0xff800000;
            if ((op == 0x91000000 || op == 0xd1000000) && FIELD(insn, 9, 5) == 31)
                sp_moved(x, (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0));
            else
                x->sp_aligned = false;
        }
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
        if (m.prefetch)
            return true;
        for (int i = 0; i < GROUP_SLOTS; i++) {
            struct group *g = &x->groups[i];
            if (g->next >= g->n || x->idx != g->idx[g->next])
                continue;
            if (g->reload[g->next]) {
                size_t off = offsetof(struct appine_cpu, dbt_groups) + 8 * (size_t)i;
                emit(x, enc_ldr_x(T2, CTX, off));
                x->t2_group = i;
            } else if (x->t2_group != i) {
                abort(); /* plan_group 保证没标 reload 的组员之前没有指令用过 T2 */
            }
            emit_member(x, &m, g->off[g->next++]);
            x->st->insns_grouped++;
            return true;
        }
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

/* 把条件分支直接指向目标块；超出它的偏移范围时不改（仍然经由桩跳转），返回 false。
 * 不同步指令缓存 */
static bool relink_word(const struct appine_dbt *d, uint32_t *site, const uint32_t *target)
{
    /* 读执行用的地址：在可写别名上读缺页时，宿主会顺带映射附近的十几页（fault-around），
     * 它们没有记在 struct written 里 */
    uint32_t insn = *site;
    bool test = (insn & 0x7e000000) == 0x36000000; /* TBZ/TBNZ：14 位偏移，其余 19 位 */
    intptr_t off = target - site, limit = (intptr_t)1 << (test ? 13 : 18);
    if (off < -limit || off >= limit)
        return false;
    uint32_t mask = (test ? 0x3fffu : 0x7ffffu) << 5;
    *rw(d, site) = (insn & ~mask) | (((uint32_t)off << 5) & mask);
    return true;
}

/*
 * 实例和它的几张表都用 mmap 分配，保证是按需提交的清零页：每个地址空间一个实例、每个
 * 线程一份跳转缓存，表按最大的规模分配，大部分页从来不碰。calloc 不一定这样（glibc 释放过同样大的块
 * 之后会调高 mmap 的阈值，改从堆里分配并逐字节清零）。
 */
static void *zalloc(size_t bytes)
{
    void *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? NULL : p;
}

/* zalloc 得到的内存从头起 `bytes` 字节交还宿主，之后读到的是 0 */
static void release_pages(void *p, size_t bytes)
{
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    if (bytes)
        madvise(p, (bytes + page - 1) & ~(page - 1), MADV_DONTNEED);
}

static void zfree(void *p, size_t bytes)
{
    if (p)
        munmap(p, bytes);
}

#define TABLE_BYTES(bits) (sizeof(struct table) + (sizeof(struct slot) << (bits)))

static size_t table_index(const struct table *t, uint64_t pc)
{
    return (size_t)(((pc >> 2) * GOLDEN) >> t->shift);
}

static size_t table_mask(const struct table *t)
{
    return (size_t)t->mask;
}

/* 分配一张 2^bits 项的空表 */
static struct table *table_new(unsigned bits)
{
    struct table *t = zalloc(TABLE_BYTES(bits));
    if (t) {
        t->bits = bits;
        t->shift = 64 - bits;
        t->mask = (UINT64_C(1) << bits) - 1;
    }
    return t;
}

/* 在 t 里找 pc。持有锁时直接调用；不持锁时经由 table_lookup */
static uint32_t *table_find(const struct table *t, uint64_t pc)
{
    for (size_t i = table_index(t, pc);; i = (i + 1) & table_mask(t)) {
        /* 插入时先写 pc 再写 code：读到 code 不为空，pc 就是对应的那个 */
        uint32_t *code = __atomic_load_n(&t->slots[i].code, __ATOMIC_ACQUIRE);
        if (!code)
            return NULL;
        if (__atomic_load_n(&t->slots[i].pc, __ATOMIC_RELAXED) == pc)
            return code;
    }
}

/*
 * 分发器和查表例程（emit_lookup）不持锁查表：cpu 的 dbt_lookup_seq 为奇数期间，换下来的
 * 旧表不会被释放（table_publish）
 */
static uint32_t *table_lookup(struct appine_dbt *d, struct appine_cpu *c, uint64_t pc)
{
    __atomic_store_n(&c->dbt_lookup_seq, c->dbt_lookup_seq + 1, __ATOMIC_SEQ_CST);
    uint32_t *code = table_find(__atomic_load_n(&d->table, __ATOMIC_SEQ_CST), pc);
    __atomic_store_n(&c->dbt_lookup_seq, c->dbt_lookup_seq + 1, __ATOMIC_RELEASE);
    return code;
}

/* 换上新表，等正在查旧表的线程查完再释放旧表。持有锁 */
static void table_publish(struct appine_dbt *d, struct table *table)
{
    struct table *old = d->table;
    __atomic_store_n(&d->table, table, __ATOMIC_SEQ_CST);
    /* 不在 dbt_run 里的线程不会查表；之后进来的先登记 cpu、再读 table，读到的是新表 */
    for (struct appine_dbt_thread *t = d->threads; t; t = t->next) {
        struct appine_cpu *c = __atomic_load_n(&t->cpu, __ATOMIC_SEQ_CST);
        if (!c)
            continue;
        unsigned seq = __atomic_load_n(&c->dbt_lookup_seq, __ATOMIC_SEQ_CST);
        while ((seq & 1) && __atomic_load_n(&c->dbt_lookup_seq, __ATOMIC_ACQUIRE) == seq)
            __asm__ volatile("yield");
    }
    zfree(old, TABLE_BYTES(old->bits));
}

/*
 * 刚建好、还没同步指令缓存的块（起始 pc 为 `pc`）：目标已经翻译过的出口直接链接，和分发器
 * 里第一次走到时的链接（dbt_run 的 R_CHAIN）一样。改动随整个块一起同步。持有锁
 */
static void link_block(const struct appine_dbt *d, uint32_t *code, uint64_t pc, const struct link *links,
                       unsigned n)
{
    for (unsigned i = 0; i < n; i++) {
        const struct link *l = &links[i];
        uint32_t *target = l->pc == pc ? code : table_find(d->table, l->pc);
        if (!target)
            continue;
        if (l->stub & LINK_FWD)
            target += ENTRY_CHECK_WORDS;
        uint32_t *site = code + (l->stub & ~LINK_FWD);
        *rw(d, site) = enc_b((intptr_t)target - (intptr_t)site);
        if (l->branch != NO_BRANCH)
            relink_word(d, code + l->branch, target);
    }
}

static unsigned page_bucket(uint64_t page)
{
    return (unsigned)((page * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - PAGE_BUCKET_BITS));
}

/* table_used 里的特殊值：这个块已经作废，重建表时没有放进去 */
#define SLOT_NONE UINT32_MAX

/* 把有效的块按原来的插入顺序放进一个 2^bits 项的新表，作废的块（墓碑）不再放进去，
 * table_used 和 blocks 的下标保持对应；分配失败时返回 false，原来的表不变。持有锁 */
static void table_rebuild(struct appine_dbt *d, struct table *table);

static bool table_resize(struct appine_dbt *d, unsigned bits)
{
    struct table *table = table_new(bits);
    if (!table)
        return false;
    table_rebuild(d, table);
    return true;
}

/* 按 blocks 填好新分配的 table 并换上（table_resize） */
static void table_rebuild(struct appine_dbt *d, struct table *table)
{
    size_t occupied = 0;
    for (size_t k = 0; k < d->table_count; k++) {
        const struct block *b = &d->blocks[k];
        if (!b->code || b->parked) {
            d->table_used[k] = SLOT_NONE;
            continue;
        }
        size_t i = table_index(table, b->pc);
        while (table->slots[i].code)
            i = (i + 1) & table_mask(table);
        table->slots[i] = (struct slot){b->pc, b->code};
        d->table_used[k] = (uint32_t)i;
        occupied++;
    }
    d->table_occupied = occupied;
    d->table_dead = 0;
    table_publish(d, table);
}

/* pc 不在表里时才插入，只用空位（见 struct table）。持有锁 */
static void table_insert(struct appine_dbt *d, uint64_t pc, uint32_t *code, const uint32_t *guest,
                         uint32_t nguest)
{
    struct table *t = d->table;
    size_t i = table_index(t, pc);
    while (t->slots[i].code)
        i = (i + 1) & table_mask(t);
    d->table_occupied++;
    __atomic_store_n(&t->slots[i].pc, pc, __ATOMIC_RELAXED);
    __atomic_store_n(&t->slots[i].code, code, __ATOMIC_RELEASE);
    unsigned h = page_bucket(pc >> APPINE_PAGE_SHIFT);
    memcpy(d->words + d->words_used, guest, nguest * 4);
    d->blocks[d->table_count] =
        (struct block){pc, code, d->page_head[h], (uint32_t)d->words_used, (uint16_t)nguest, 0, 0};
    d->words_used += nguest;
    d->table_used[d->table_count++] = (uint32_t)i;
    d->page_head[h] = (uint32_t)d->table_count;
}

/*
 * 从翻译表里删掉 pc，留一个永远不会命中的墓碑。各线程跳转缓存里的旧项不用删：它们指向
 * 作废块的入口，那里跳往转发桩。持有锁
 */
static void table_remove(struct appine_dbt *d, uint64_t pc)
{
    struct table *t = d->table;
    for (size_t i = table_index(t, pc); t->slots[i].code; i = (i + 1) & table_mask(t)) {
        if (t->slots[i].pc == pc) {
            __atomic_store_n(&t->slots[i].pc, JC_EMPTY, __ATOMIC_RELEASE);
            d->table_dead++;
            break;
        }
    }
}

static void clear_jump_cache(struct appine_dbt_thread *t)
{
    for (unsigned i = 0; i < JC_SIZE; i++)
        t->jc[i] = (struct slot){JC_EMPTY, NULL};
    t->jc_dirty = false;
}

/* 影子返回栈全部换成空项：落点所在的代码就要被覆盖了 */
static void rs_reset(struct appine_dbt_thread *t)
{
    for (unsigned i = 0; i < RS_ENTRIES; i++) {
        t->rs[i][0] = 1; /* 未对齐，不会是 RET 的目标；万一匹配，rs_miss 也照样查找 */
        t->rs[i][1] = (uint64_t)(uintptr_t)t->d->rs_miss;
    }
}

/* 代码缓存整体清空过（generation 变了）：线程自己的跳转缓存和影子返回栈指向旧代码 */
static void thread_catch_up(struct appine_dbt_thread *t)
{
    uint64_t g = __atomic_load_n(&t->d->generation, __ATOMIC_ACQUIRE);
    if (t->generation != g) {
        t->generation = g;
        rs_reset(t);
        if (t->jc_dirty)
            clear_jump_cache(t);
    }
}

/*
 * blocks 记满了，但大半是作废的块（JIT 反复改写、重新翻译同一处代码）：去掉它们的记录，
 * 再原样重建翻译表去掉墓碑，不必整体清空。作废块的代码还留在代码缓存里，它们的入口仍然
 * 跳往转发桩。持有锁；分配新表失败时返回 false
 */
static bool compact_blocks(struct appine_dbt *d)
{
    struct table *table = table_new(d->table->bits);
    if (!table)
        return false;
    size_t n = 0, words = 0;
    memset(d->page_head, 0, sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    for (size_t k = 0; k < d->table_count; k++) {
        struct block b = d->blocks[k];
        if (!b.code)
            continue;
        memmove(d->words + words, d->words + b.words, b.nguest * 4);
        b.words = (uint32_t)words;
        words += b.nguest;
        unsigned h = page_bucket(b.pc >> APPINE_PAGE_SHIFT);
        b.next = d->page_head[h];
        d->blocks[n++] = b;
        d->page_head[h] = (uint32_t)n;
    }
    d->table_count = n;
    d->words_used = words;
    d->blocks_dead = 0;
    table_rebuild(d, table);
    return true;
}

/*
 * 丢弃全部翻译。持有锁，并且没有别的线程在翻译后的代码里或者不持锁地查表：它们都停在
 * 分发器里，或者不在 dbt_run 里（flush_world）。
 */
static void flush_locked(struct appine_dbt *d)
{
    if (d->used_words > d->high_words)
        d->high_words = d->used_words;
    struct table *t = d->table;
    for (size_t i = 0; i < d->table_count; i++) {
        if (d->table_used[i] != SLOT_NONE)
            t->slots[d->table_used[i]] = (struct slot){0, NULL};
        d->page_head[page_bucket(d->blocks[i].pc >> APPINE_PAGE_SHIFT)] = 0;
        d->page_parked[page_bucket(d->blocks[i].pc >> APPINE_PAGE_SHIFT)] = 0;
    }
    d->table_count = d->table_occupied = d->table_dead = d->blocks_dead = d->nparked = 0;
    d->words_used = 0;
    d->stale = false;
    d->nsources = d->ntrusted = d->nparkable = 0;
    d->flush_wanted = false;
    d->used_words = d->routine_words;
    __atomic_store_n(&d->generation, d->generation + 1, __ATOMIC_RELEASE);
    d->st->flushes++;
}

/*
 * 让作废的块不再被执行：入口（kick 检查之前和之后两处）改成跳往一个转发桩。转发桩
 * 以 R_CHAIN 退出，回填位置就是旧入口，所以重新翻译之后旧入口直接跳往新的块，
 * 别的块链接到旧入口的跳转不用找出来改写。别的线程可能正在执行这个块：它照样执行到
 * 块尾，和硬件上别的核在上下文同步之前还可能执行旧指令一样。
 *
 * 转发桩要先同步好，入口才能跳过去；每次同步都要等指令缓存的作废广播完成，所以一批块
 * 一起做：retire_add 写好转发桩，retire_flush 同步所有转发桩、改所有入口、再同步一次。
 * 持有锁
 */
#define RETIRE_BATCH 32

struct retire_batch {
    unsigned n;
    struct block *blocks[RETIRE_BATCH];
    bool park[RETIRE_BATCH];
    uint32_t *fwd_start;
};

static void retire_flush(struct appine_dbt *d, struct retire_batch *r)
{
    if (!r->n)
        return;
    void *start[RETIRE_BATCH], *end[RETIRE_BATCH];
    start[0] = r->fwd_start;
    end[0] = d->cache + d->used_words;
    sync_written(d, start, end, 1);
    uint32_t *fwd = r->fwd_start;
    for (unsigned i = 0; i < r->n; i++) {
        uint32_t *code = r->blocks[i]->code;
        if (r->park[i])
            r->blocks[i]->first = code[ENTRY_CHECK_WORDS];
        *rw(d, code) = enc_b((intptr_t)fwd - (intptr_t)code);
        *rw(d, code + ENTRY_CHECK_WORDS) = enc_b(-ENTRY_CHECK_WORDS * 4);
        start[i] = code;
        end[i] = code + ENTRY_CHECK_WORDS + 1;
        fwd += 16;
    }
    sync_written(d, start, end, r->n);
    for (unsigned i = 0; i < r->n; i++) {
        struct block *b = r->blocks[i];
        table_remove(d, b->pc);
        if (r->park[i]) {
            b->parked = 1;
            d->nparked++;
            d->page_parked[page_bucket(b->pc >> APPINE_PAGE_SHIFT)]++;
            continue;
        }
        b->code = NULL;
        d->blocks_dead++;
        d->st->invalidated++;
    }
    r->n = 0;
}

/* 作废停放着的块：它的入口本来就跳往转发桩 */
static void kill_parked(struct appine_dbt *d, struct block *b)
{
    b->parked = 0;
    b->code = NULL;
    d->nparked--;
    d->page_parked[page_bucket(b->pc >> APPINE_PAGE_SHIFT)]--;
    d->blocks_dead++;
    d->st->invalidated++;
}

/* 作废 b：先写好它的转发桩（每个占 16 个字），入口等 retire_flush 一起改。`park` 时只是
 * 停放（revive）。代码缓存放不下转发桩时返回 false */
static bool retire_add(struct appine_dbt *d, struct retire_batch *r, struct block *b, bool park)
{
    if (r->n == RETIRE_BATCH)
        retire_flush(d, r);
    if (d->used_words + 16 > d->cache_words)
        return false;
    struct xlate x = {.d = d, .buf = d->cache + d->used_words, .wdelta = d->rw_delta, .st = d->st};
    emit_mov_const(&x, T1, (uint64_t)(uintptr_t)b->code);
    emit_mov_const(&x, T0, b->pc);
    emit(&x, enc_movz(T2, R_CHAIN, 0));
    emit(&x, enc_b((intptr_t)d->exit - (intptr_t)here(&x)));
    while (x.n < 16)
        emit(&x, enc_b(4)); /* 补齐，转发桩按 16 个字排列 */
    d->used_words += x.n;
    if (!r->n)
        r->fwd_start = x.buf;
    r->park[r->n] = park;
    r->blocks[r->n++] = b;
    return true;
}

static bool block_matches(const struct appine_dbt *d, const struct appine_mmu *mmu,
                          const struct block *b, uint64_t *page, const uint32_t **host);

/*
 * 作废 [start, start + len) 覆盖的各页里开始的块；放不下转发桩或者范围太大时改为请求
 * 整体清空，由下一次进入分发器的线程完成。给了 `mmu` 时只作废 guest 指令字和现在不同的块
 * （翻译只取决于 pc 和指令字，字面量也是运行时才读）：JIT 改写一处代码只 ic ivau 一两行，
 * 而 ic ivau 是按页记下的，同一页里别的方法的块照样能用。HotSpot 不停地改写内联缓存和
 * 方法入口，原来每次都把整页的块作废、重新翻译，Maven 每次构建要作废 31 万个块。拆掉映射、
 * 去掉执行权限之后这一页找不到可执行的映射，照样全部作废。停放着的块不用管，恢复时本来
 * 就要核对。持有锁
 */
static void invalidate_locked(struct appine_dbt *d, const struct appine_mmu *mmu, uint64_t start,
                              uint64_t len)
{
    if (!len)
        return;
    uint64_t end = start + len - 1, first = start >> APPINE_PAGE_SHIFT, last = end >> APPINE_PAGE_SHIFT;
    if (end < start)
        last = UINT64_MAX >> APPINE_PAGE_SHIFT;
    /* 范围很大（拆掉一个库）：逐个看所有的块，比整体清空便宜，别的线程也不必重新翻译。
     * 块不跨页，按页比较 */
    struct retire_batch r = {.n = 0};
    uint64_t seen = 1; /* 未对齐，不会与任何页相同 */
    const uint32_t *host = NULL;
    if (end < start || last - first >= INVALIDATE_MAX_PAGES) {
        for (size_t i = 0; i < d->table_count; i++) {
            struct block *b = &d->blocks[i];
            uint64_t page = b->pc >> APPINE_PAGE_SHIFT;
            if (!b->code || page < first || page > last)
                continue;
            if (b->parked) {
                if (!mmu)
                    kill_parked(d, b);
            } else if (mmu && block_matches(d, mmu, b, &seen, &host)) {
                continue;
            } else if (!retire_add(d, &r, b, false)) {
                goto full;
            }
        }
        retire_flush(d, &r);
        return;
    }
    for (uint64_t page = first; page <= last; page++) {
        for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
            struct block *b = &d->blocks[i - 1];
            if (!b->code || b->pc >> APPINE_PAGE_SHIFT != page)
                continue;
            if (b->parked) {
                if (!mmu)
                    kill_parked(d, b);
            } else if (mmu && block_matches(d, mmu, b, &seen, &host)) {
                continue;
            } else if (!retire_add(d, &r, b, false)) {
                goto full;
            }
        }
    }
    retire_flush(d, &r);
    return;
full:
    retire_flush(d, &r);
    __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
}

/*
 * 组长 `pc` 没通过共用的检查。第一次只记下来：栈还没长到那里这类情况，Tier 0 执行
 * 组员时就会解决。第二次记进 nogroup，作废包含它的块（块不跨页，都在它那一页的桶里），
 * 重新翻译时不再分组。持有锁
 */
static void ungroup(struct appine_dbt *d, uint64_t pc)
{
    uint64_t *strike = &d->strikes[(pc >> 2) % STRIKES];
    if (*strike != pc) {
        *strike = pc;
        return;
    }
    *strike = 0;
    d->nogroup[d->nogroup_count++ % NOGROUP_MAX] = pc;
    d->st->ungrouped++;
    uint64_t page = pc >> APPINE_PAGE_SHIFT;
    struct retire_batch r = {.n = 0};
    for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
        struct block *b = &d->blocks[i - 1];
        if (!b->code || b->pc > pc || pc >= b->pc + 4ull * b->nguest)
            continue;
        if (b->parked) {
            kill_parked(d, b);
        } else if (!retire_add(d, &r, b, false)) {
            __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
            break;
        }
    }
    retire_flush(d, &r);
}

/* ---- 翻译模板：所有实例共享（§4.2） ---- */

/*
 * 一个块的翻译只取决于它的 guest pc 和翻译过的指令字（以及全局的特性集合），与
 * 进程、映射无关。每翻译一个块就把结果存成模板；别的实例（另一个线程、fork 出来的
 * 子进程、又一次 exec 的同一个程序）要翻译同一个 pc 时，先比对模板记下的指令字和
 * 自己内存里的是否相同，相同就把模板拷进自己的代码缓存，只重新计算跳往例程的几条
 * b。块之间的链接在各自的拷贝里回填，所以不同进程在同一地址映射了不同的代码也
 * 互不影响。模板区写满时整体丢弃，因为实例只拷贝模板、从不引用它。
 *
 * 所有进程的所有线程都在用模板，一把锁时 go build 这样并发起编译进程的负载有一成时间
 * 花在锁上，所以按桶分片加锁；分配模板区另用一把只持有片刻的锁，整体丢弃时持有全部的
 * 锁（先分片、后分配，与存模板时的顺序相同）。
 */
#define TEMPLATE_BYTES (UINT64_C(32) << 20)
#define TEMPLATE_BUCKET_BITS 16
#define TEMPLATE_SHARDS 64
/* 同一个 pc 最多保留几个版本：JIT 反复改写同一处代码时只留最新的几个 */
#define TEMPLATES_PER_PC 4

struct template {
    struct template *next;
    uint64_t pc;
    uint64_t key; /* 翻译时用到的分组记录（nogroup_key） */
    uint32_t nguest, nhost, nrelocs, nlinks;
    uint32_t copied, remapped, memory, fallback, grouped; /* 统计，拷贝时累加 */
    uint32_t words[]; /* guest 指令字、宿主指令字、重定位、链接（每个 4 个字，见 struct link） */
};

static struct {
    struct {
        pthread_mutex_t lock;
        char pad[64 - sizeof(pthread_mutex_t) % 64]; /* 各占一个缓存行 */
    } shards[TEMPLATE_SHARDS]; /* 第 i 片管下标模 TEMPLATE_SHARDS 等于 i 的桶 */
    pthread_mutex_t arena_lock;
    uint8_t *arena; /* 第一次使用时映射，写满时清空 */
    size_t used;
    bool full; /* 写满了，等存模板的线程放开分片锁之后清空 */
    struct template *buckets[1u << TEMPLATE_BUCKET_BITS];
} templates = {
    .shards = {[0 ... TEMPLATE_SHARDS - 1] = {.lock = PTHREAD_MUTEX_INITIALIZER}},
    .arena_lock = PTHREAD_MUTEX_INITIALIZER,
};

static unsigned template_index(uint64_t pc)
{
    return (unsigned)(((pc >> 2) * UINT64_C(0x9e3779b97f4a7c15)) >> (64 - TEMPLATE_BUCKET_BITS));
}

static struct template **template_bucket(uint64_t pc)
{
    return &templates.buckets[template_index(pc)];
}

/* pc 所在的桶的锁，同时向 guard 登记 */
static pthread_mutex_t *template_lock(uint64_t pc)
{
    pthread_mutex_t *lock = &templates.shards[template_index(pc) % TEMPLATE_SHARDS].lock;
    pthread_mutex_lock(lock);
    appine_guard_hold(lock, false);
    return lock;
}

static void template_unlock(pthread_mutex_t *lock)
{
    appine_guard_drop();
    pthread_mutex_unlock(lock);
}

/*
 * 从 pc 开始的块翻译时会用到的分组记录（ungroup 记下的、影响到这一页里 pc 之后的
 * 组长）：实例记下的不同，翻译出来的块也可能不同，翻译模板按它区分版本。没有时为 0。
 */
static uint64_t nogroup_key(const struct appine_dbt *d, uint64_t pc)
{
    uint64_t key = 0, end = (pc | PAGE_MASK) + 1;
    unsigned n = d->nogroup_count < NOGROUP_MAX ? d->nogroup_count : NOGROUP_MAX;
    for (unsigned i = 0; i < n; i++) {
        uint64_t q = d->nogroup[i];
        if (q + 4 * MAX_BLOCK_INSNS > pc && q < end)
            key += (q * UINT64_C(0x9e3779b97f4a7c15)) | 1;
    }
    return key;
}

/* 调用方持有 pc 所在的分片的锁 */
static struct template *template_find(uint64_t pc, const uint32_t *guest, uint64_t key)
{
    for (struct template *t = *template_bucket(pc); t; t = t->next) {
        if (t->pc == pc && t->key == key && memcmp(t->words, guest, t->nguest * 4) == 0)
            return t;
    }
    return NULL;
}

/*
 * 准备好、还没装进代码缓存的块（prepare 不持锁翻译，install 持锁装进去）：生成的宿主指令
 * 在线程的暂存区里，跳往例程的 b 的位置记在 relocs，出口记在 links。
 */
struct prep {
    uint64_t pc;
    const uint32_t *guest; /* guest 指令字在宿主内存里的位置：装进去之前再比对一次 */
    uint32_t nguest;
    uint32_t words[MAX_BLOCK_INSNS]; /* 翻译时读到的 guest 指令字 */
    const uint32_t *host;
    uint32_t nhost;
    uint32_t relocs[MAX_RELOCS];
    unsigned nrelocs;
    struct link links[MAX_STUBS];
    unsigned nlinks;
};

/* 有和 `guest`（翻译时读到的指令字）相同的模板时，拷进线程的暂存区记在 p 里 */
static bool template_fetch(uint64_t pc, const uint32_t *guest, uint64_t key, struct prep *p,
                           uint32_t *scratch, struct appine_dbt_stats *st)
{
    pthread_mutex_t *lock = template_lock(pc);
    struct template *t = template_find(pc, guest, key);
    if (t) {
        const uint32_t *host = t->words + t->nguest, *relocs = host + t->nhost;
        memcpy(scratch, host, t->nhost * 4);
        memcpy(p->relocs, relocs, t->nrelocs * 4);
        p->host = scratch;
        p->nhost = t->nhost;
        p->nrelocs = t->nrelocs;
        p->nguest = t->nguest;
        const uint32_t *l = relocs + t->nrelocs;
        for (uint32_t i = 0; i < t->nlinks; i++, l += 4)
            p->links[i] = (struct link){l[0], l[1], (uint64_t)l[3] << 32 | l[2]};
        p->nlinks = t->nlinks;
        st->insns += t->nguest;
        st->insns_copied += t->copied;
        st->insns_remapped += t->remapped;
        st->insns_memory += t->memory;
        st->insns_fallback += t->fallback;
        st->insns_grouped += t->grouped;
        st->blocks_shared++;
    }
    template_unlock(lock);
    return t != NULL;
}

/* 从模板区分配 `bytes` 字节；写满时返回 NULL，并标记等着清空。调用方持有某个分片的锁 */
static void *template_alloc(size_t bytes)
{
    void *p = NULL;
    pthread_mutex_lock(&templates.arena_lock);
    if (!templates.arena) {
        void *m = mmap(NULL, TEMPLATE_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        templates.arena = m == MAP_FAILED ? NULL : m;
    }
    if (templates.arena && templates.used + bytes <= TEMPLATE_BYTES) {
        p = templates.arena + templates.used;
        templates.used += bytes;
    } else if (templates.arena) {
        templates.full = true;
    }
    pthread_mutex_unlock(&templates.arena_lock);
    return p;
}

/* 丢弃所有翻译模板：模板区写满了（`if_full`，别的线程已经清空过时不再清空），或者宿主
 * 出了故障（坏掉的可能是模板本身） */
static void template_clear(bool if_full)
{
    for (unsigned i = 0; i < TEMPLATE_SHARDS; i++)
        pthread_mutex_lock(&templates.shards[i].lock);
    pthread_mutex_lock(&templates.arena_lock);
    if (templates.full || !if_full) {
        memset(templates.buckets, 0, sizeof(templates.buckets));
        templates.used = 0;
        templates.full = false;
    }
    pthread_mutex_unlock(&templates.arena_lock);
    for (unsigned i = TEMPLATE_SHARDS; i-- > 0;)
        pthread_mutex_unlock(&templates.shards[i].lock);
}

/* 把刚翻译好、还没有回填过链接的块存成模板 */
static void template_store(uint64_t pc, const uint32_t *guest, uint32_t nguest, uint64_t key,
                           const struct xlate *x, const struct appine_dbt_stats *before,
                           const struct link *links, unsigned nlinks)
{
    const struct appine_dbt_stats *after = x->st;
    size_t bytes = sizeof(struct template) + 4 * (nguest + x->n + x->nrelocs + 4 * nlinks);
    bytes = (bytes + 7) & ~(size_t)7;
    pthread_mutex_t *lock = template_lock(pc);
    struct template *t = template_find(pc, guest, key) ? NULL : template_alloc(bytes);
    if (t) {
        *t = (struct template){
            .pc = pc,
            .key = key,
            .nguest = nguest,
            .nhost = (uint32_t)x->n,
            .nrelocs = x->nrelocs,
            .nlinks = nlinks,
            .copied = (uint32_t)(after->insns_copied - before->insns_copied),
            .remapped = (uint32_t)(after->insns_remapped - before->insns_remapped),
            .memory = (uint32_t)(after->insns_memory - before->insns_memory),
            .fallback = (uint32_t)(after->insns_fallback - before->insns_fallback),
            .grouped = (uint32_t)(after->insns_grouped - before->insns_grouped),
        };
        memcpy(t->words, guest, 4 * nguest);
        memcpy(t->words + nguest, x->buf, 4 * x->n);
        memcpy(t->words + nguest + x->n, x->relocs, 4 * x->nrelocs);
        uint32_t *l = t->words + nguest + x->n + x->nrelocs;
        for (unsigned i = 0; i < nlinks; i++, l += 4) {
            l[0] = links[i].stub;
            l[1] = links[i].branch;
            l[2] = (uint32_t)links[i].pc;
            l[3] = (uint32_t)(links[i].pc >> 32);
        }
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
    template_unlock(lock);
    if (__atomic_load_n(&templates.full, __ATOMIC_RELAXED))
        template_clear(true);
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

/* install 的返回值：代码缓存或者翻译表满了，要先整体清空（flush_world）；翻译之后 guest
 * 代码被改写过，要重新翻译 */
#define NEED_FLUSH ((uint32_t *)(uintptr_t)1)
#define STALE ((uint32_t *)(uintptr_t)2)

/* install 建好、还没放进翻译表的块：同步了指令缓存之后才能放进去，别的线程不持锁查表 */
struct fresh {
    uint64_t pc;
    uint32_t *code;
    const uint32_t *guest;
    uint32_t nguest;
};

/*
 * 不持锁：把 pc 开始的块翻译进线程的暂存区（或者拷贝模板），结果记在 t->prep。翻译耗时
 * 最长，持锁翻译时 JVM 的主线程常常要等编译线程。guest 指令字先拷一份再翻译，装进去之前
 * 拿它和内存里的比对（install）。取不到指令时返回 false。
 */
static bool prepare(struct appine_dbt_thread *t, struct appine_cpu *c, uint64_t pc)
{
    struct appine_dbt *d = t->d;
    struct prep *p = t->prep;
    /* 块不跨页（遇到页边界就结束），整页的权限相同 */
    const uint32_t *page = NULL;
    if (!(pc & 3))
        page = appine_mmu_translate(c->mmu, pc & ~PAGE_MASK, APPINE_PROT_EXEC);
    if (!page)
        return false;
    unsigned limit = (unsigned)((APPINE_PAGE_SIZE - (pc & PAGE_MASK)) / 4);
    if (limit > MAX_BLOCK_INSNS)
        limit = MAX_BLOCK_INSNS;
    p->pc = pc;
    p->guest = page + (pc & PAGE_MASK) / 4;
    memcpy(p->words, p->guest, limit * 4);
    p->nlinks = 0;
    const uint32_t *guest = p->words;
    /* 不持锁读分组记录：读到旧的只影响分组，检查没通过时会再撤销（ungroup） */
    uint64_t key = d->nogroup_count ? nogroup_key(d, pc) : 0;
    t->stats.blocks++;
    if (template_fetch(pc, guest, key, p, t->scratch, &t->stats))
        return true;

    struct appine_dbt_stats before = t->stats;
    struct xlate *x = t->xlate;
    x->d = d;
    x->buf = t->scratch;
    x->wdelta = 0;
    x->st = &t->stats;
    x->probe = t->probe;
    x->defined = t->defined;
    x->n = 0;
    x->nstubs = 0;
    x->nfixups = 0;
    x->nrelocs = 0;
    x->pc = pc;
    x->guest = guest;
    x->limit = limit;
    x->sp_aligned = false;
    x->konst = 0;
    for (unsigned i = 0; i < GROUP_SLOTS; i++)
        x->groups[i].n = x->groups[i].next = 0;
    x->t2_group = -1;

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
        x->st->insns++;
        x->idx = nguest;
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
        if (link)
            p->links[p->nlinks++] = (struct link){
                (uint32_t)(s->addr - x->buf) | (s->reason == R_CHAIN_FWD ? LINK_FWD : 0),
                s->sites == 1 ? (uint32_t)(s->branch - x->buf) : NO_BRANCH, s->pc};
    }
    for (unsigned i = 0; i < x->nfixups; i++) {
        struct fixup *f = &x->fixups[i];
        intptr_t off = (intptr_t)x->stubs[f->stub].addr - (intptr_t)f->site;
        if (f->kind == FIX_IMM19)
            *f->site |= ((uint32_t)(off >> 2) & 0x7ffff) << 5;
        else
            *f->site |= ((uint32_t)(off >> 2) & 0x3fff) << 5;
    }

    if (x->nrelocs > MAX_RELOCS)
        abort();
    template_store(pc, guest, nguest, key, x, &before, p->links, p->nlinks);
    p->nguest = nguest;
    p->host = x->buf;
    p->nhost = (uint32_t)x->n;
    memcpy(p->relocs, x->relocs, x->nrelocs * 4);
    p->nrelocs = x->nrelocs;
    return true;
}

/*
 * 持锁：把准备好的块（prepare）装进代码缓存，链接已经翻译过的出口。新代码的范围记进
 * `sync`，块记进 `f`，由调用方同步指令缓存之后放进翻译表（table_insert）。
 */
static uint32_t *install(struct appine_dbt *d, const struct prep *p, struct syncs *sync, struct fresh *f)
{
    /* 翻译期间别的线程改写了这段代码（它作废时这个块还不在表里）：重新翻译 */
    if (memcmp(p->guest, p->words, p->nguest * 4) != 0)
        return STALE;
    /* 表里有四分之三不空时，墓碑占一半以上就原样重建，否则加倍；blocks 记满时去掉作废块
     * 的记录，作废的不多就整体清空 */
    if (d->table_count == TABLE_LIMIT &&
        (d->blocks_dead * 8 < d->table_count || !compact_blocks(d)))
        return NEED_FLUSH;
    unsigned bits = d->table->bits;
    if (d->table_occupied >= ((size_t)3 << bits) / 4) {
        bool purge = d->table_dead * 2 >= d->table_occupied || bits == TABLE_BITS;
        if (!table_resize(d, purge ? bits : bits + 1) ||
            d->table_occupied >= ((size_t)3 << d->table->bits) / 4)
            return NEED_FLUSH;
    }
    if (d->used_words + p->nhost > d->cache_words)
        return NEED_FLUSH;
    uint32_t *code = d->cache + d->used_words;
    memcpy(rw(d, code), p->host, p->nhost * 4);
    for (unsigned i = 0; i < p->nrelocs; i++) {
        uint32_t *site = code + (p->relocs[i] >> 3);
        *rw(d, site) = enc_b((intptr_t)routine(d, p->relocs[i] & 7) - (intptr_t)site);
    }
    d->used_words += p->nhost;
    link_block(d, code, p->pc, p->links, p->nlinks);
    perf_map(code, p->nhost * 4, p->pc);
    sync_add(sync, code, code + p->nhost);
    *f = (struct fresh){p->pc, code, p->guest, p->nguest};
    return code;
}

/* 回填例程失败时跳往的位置，最后统一回填：三次填表各 MMU_LEVELS 处，另有三处 */
struct refill_fails {
    uint32_t *site[3 * MMU_LEVELS + 3];
    unsigned n;
};

static void emit_to_fail(struct xlate *x, struct refill_fails *f, uint32_t insn)
{
    f->site[f->n++] = here(x);
    emit(x, insn);
}

/*
 * 按 mmu.c 的结构遍历页表，把地址 `a`（T0 或 x3）所在的页填进 TLB：读写两半的 tag
 * 按叶子表项的权限分别填写，和 Tier 0 的 appine_tlb_fill 一样同时记下 tlb_used。
 * 未映射或没有这次访问要的权限时跳往失败。结束时 x2 = addend；用掉 x0、x1、x4、T2，
 * 保留 x3、T0、T1。
 */
static void emit_fill(struct xlate *x, unsigned a, bool write, struct refill_fails *f)
{
    size_t used = offsetof(struct appine_cpu, tlb_used);
    emit(x, enc_ldr_x(0, CTX, offsetof(struct appine_cpu, mmu)));
    emit(x, enc_ldr_x(0, 0, offsetof(struct appine_mmu, root)));
    /* 中间表项是下一级表的地址；按地址依赖读取，和 mmu.c 的 acquire 读取一样有序 */
    for (unsigned level = 0; level < MMU_LEVELS; level++) {
        emit(x, enc_ubfx(1, a, APPINE_PAGE_SHIFT + MMU_LEVEL_BITS * (MMU_LEVELS - 1 - level),
                         MMU_LEVEL_BITS));
        emit(x, enc_ldr_x_index(0, 0, 1));
        if (level < MMU_LEVELS - 1)
            emit_to_fail(x, f, enc_cbz_x(0));
    }
    /* 叶子表项 = 宿主页地址 | 权限位；未映射的是 0，权限检查一并排除 */
    emit_to_fail(x, f, enc_tbz(0, write ? 1 : 0));
    _Static_assert(APPINE_PROT_READ == 1 && APPINE_PROT_WRITE == 2, "权限位的位置");
    emit(x, enc_and_clear_low(1, a, APPINE_PAGE_SHIFT));
    emit(x, enc_and_clear_low(2, 0, APPINE_PAGE_SHIFT));
    emit(x, enc_sub_reg(2, 2, 1)); /* addend */
    emit(x, enc_ubfx(4, a, APPINE_PAGE_SHIFT, 64 - APPINE_PAGE_SHIFT)); /* 页号 */
    emit(x, enc_ubfx(1, a, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(1, CTX, 1, 5));
    for (unsigned half = 0; half < 2; half++) {
        /* 有权限时 tag = (0 - 1) & ~页号，否则 0 & ~页号 = TLB_INVALID */
        emit(x, enc_ubfx(T2, 0, half, 1));
        emit(x, enc_sub_reg(T2, 31, T2));
        emit(x, enc_bic_reg(T2, T2, 4));
        emit(x, enc_stp_x(T2, 2, 1, (int)(TLB_OFF + 16 * half)));
    }
    /* tlb_used[下标 / 64] |= 1 << (下标 % 64) */
    emit(x, enc_ubfx(1, a, APPINE_PAGE_SHIFT + 6, TLB_BITS - 6));
    emit(x, enc_add_lsl(1, CTX, 1, 3));
    emit_add_const(x, 1, 1, (int64_t)(used & ~(size_t)0xfff));
    emit(x, enc_ldr_x(T2, 1, used & 0xfff));
    emit(x, enc_ubfx(4, a, APPINE_PAGE_SHIFT, 6));
    emit(x, enc_movz(0, 1, 0));
    emit(x, enc_lslv(0, 0, 4));
    emit(x, enc_orr_reg(T2, T2, 0));
    emit(x, enc_str_x(T2, 1, used & 0xfff));
}

/* 在 TLB 里查地址 `a` 所在的页：命中时 `addend` 寄存器 = addend，否则跳往 `miss`
 * （偏移待回填）；用掉 x0 */
static uint32_t *emit_probe(struct xlate *x, unsigned a, unsigned addend, bool write)
{
    emit(x, enc_ubfx(0, a, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(0, CTX, 0, 5));
    emit(x, enc_ldp_x(0, addend, 0, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(0, 0, a, APPINE_PAGE_SHIFT));
    uint32_t *miss = here(x);
    emit(x, enc_cbnz_x(0));
    return miss;
}

/*
 * TLB 回填例程：TLB 未命中或者访问跨页时，在翻译后的代码里处理，不必退出到分发器。
 * 入口：T0 = guest 地址，T1 指向回填桩的信息字（emit_refill_stub）。成功时 T2 = addend，
 * 回到访问本身；失败时按信息字里的原因退出。
 *
 * 不跨页时按页表填好表项。跨页时两页都要有这次访问的权限，宿主地址也要连续（两页
 * 的 addend 相同；同一次宿主映射里的页都是这样），这时整个访问照样在宿主上一次完成：
 * 两页都在 TLB 里时只比对两个表项，不在的先填进去。栈帧跨页的函数每次调用都会这样
 * 访问，所以不能退给 Tier 0。地址带标签、超出地址空间、未映射、没有权限或者宿主地址
 * 不连续时失败。
 *
 * 借用 cpu 结构体里 x0–x4 的位置暂存宿主 x0–x4：翻译后的代码运行期间这些位置
 * 不保存 guest 的值，退出例程会重新写入。
 */
static uint32_t *emit_refill(struct xlate *x, bool write)
{
    size_t x0 = offsetof(struct appine_cpu, x);
    uint32_t *entry = here(x);
    struct refill_fails f = {.n = 0};

    emit(x, enc_stp_x(0, 1, CTX, (int)x0));
    emit(x, enc_stp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_str_x(4, CTX, x0 + 32));
    /* 有观察点（ptrace 设置的）时不按页表填：被观察的页不能进 TLB。交给 Tier 0 执行这一条，
     * 它检查观察点，并按 appine_tlb_fill 的规则填表，没被观察的页之后照常命中 */
    size_t nwatch = offsetof(struct appine_cpu, nwatch);
    emit_add_const(x, 0, CTX, (int64_t)(nwatch & ~(size_t)0xfff));
    emit(x, enc_ldr_w(0, 0, nwatch & 0xfff));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_ldr_w(3, T1, 0));
    emit(x, enc_ubfx(3, 3, 0, REFILL_REASON_SHIFT));
    emit(x, enc_add_lsl(3, T0, 3, 0)); /* 最后一个字节 */
    /* 两头的高位都要是 0：只看 x3 会漏掉加上字节数之后绕回来的地址 */
    emit(x, enc_orr_reg(0, T0, 3));
    emit(x, enc_ubfx(0, 0, APPINE_VA_BITS, 64 - APPINE_VA_BITS));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_eor_lsr(0, T0, 3, 0));
    emit(x, enc_ubfx(0, 0, APPINE_PAGE_SHIFT, 64 - APPINE_PAGE_SHIFT));
    uint32_t *cross = here(x);
    emit(x, enc_cbnz_x(0));

    emit_fill(x, T0, write, &f);
    emit(x, enc_mov(T2, 2));
    uint32_t *done = here(x);
    emit(x, enc_ldr_w(0, T1, 0));
    emit(x, enc_sbfx(0, 0, REFILL_RESUME_SHIFT, 32 - REFILL_RESUME_SHIFT));
    emit(x, enc_add_lsl(T1, T1, 0, 2));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_ldr_x(4, CTX, x0 + 32));
    emit(x, enc_br(T1));

    /* 跨页：x1、x2 = 两页的 addend */
    uint32_t *probe = here(x);
    patch_branch(x->wdelta, cross, probe);
    uint32_t *miss[2];
    miss[0] = emit_probe(x, T0, 1, write);
    miss[1] = emit_probe(x, 3, 2, write);
    emit(x, enc_sub_reg(0, 2, 1));
    emit_to_fail(x, &f, enc_cbnz_x(0));
    emit(x, enc_mov(T2, 1));
    emit(x, enc_b((intptr_t)done - (intptr_t)here(x)));
    for (unsigned i = 0; i < 2; i++) {
        patch_branch(x->wdelta, miss[i], here(x));
        emit_fill(x, i ? 3 : T0, write, &f);
        emit(x, enc_b((intptr_t)probe - (intptr_t)here(x)));
    }

    for (unsigned i = 0; i < f.n; i++)
        patch_branch(x->wdelta, f.site[i], here(x));
    emit(x, enc_ldr_w(0, T1, 0));
    emit(x, enc_ubfx(T2, 0, REFILL_REASON_SHIFT, REFILL_RESUME_SHIFT - REFILL_REASON_SHIFT));
    emit(x, enc_ldur_x(T0, T1, 4));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_ldr_x(4, CTX, x0 + 32));
    emit(x, enc_b((intptr_t)x->d->exit - (intptr_t)here(x)));
    return entry;
}

/* 把 cpu 的 dbt_lookup_seq 加一（用掉 x1、x2），见 table_lookup */
static void emit_seq_step(struct xlate *x)
{
    size_t seq = offsetof(struct appine_cpu, dbt_lookup_seq);
    emit(x, enc_ldr_w(2, CTX, seq));
    emit(x, enc_add_imm(2, 2, 1, 0));
    emit_add_const(x, 1, CTX, (int64_t)seq);
    emit(x, enc_stlr(2, 2, 1));
}

/*
 * 跳转缓存没命中时的查表例程：T0 = guest pc。像 table_lookup 一样不持锁查这个地址空间
 * 共用的翻译表，查到了填进线程的跳转缓存再跳过去，查不到才退出到分发器翻译（R_LOOKUP）。
 * 原来每次没命中都要退出、保存和恢复全部寄存器，JIT 负载一次 Maven 构建有六百多万次。
 * 借用 cpu 结构体里 x0–x3 的位置暂存宿主 x0–x3，见 emit_refill。
 */
static uint32_t *emit_lookup(struct xlate *x, struct appine_dbt *d)
{
    size_t x0 = offsetof(struct appine_cpu, x);
    uint32_t *entry = here(x);
    emit(x, enc_stp_x(0, 1, CTX, (int)x0));
    emit(x, enc_stp_x(2, 3, CTX, (int)x0 + 16));
    /* 序号改成奇数之后再读表：stlr 与随后的 ldar 之间有序 */
    emit_seq_step(x);
    emit_mov_const(x, 0, (uint64_t)(uintptr_t)&d->table);
    emit(x, enc_ldar(3, 0, 0));
    emit(x, enc_ldr_w(2, 0, offsetof(struct table, shift)));
    emit(x, enc_ldr_x(3, 0, offsetof(struct table, mask)));
    emit(x, enc_add_imm(0, 0, offsetof(struct table, slots), 0));
    emit(x, enc_ubfx(T1, T0, 2, 62));
    emit_mov_const(x, T2, GOLDEN);
    emit(x, enc_mul(T1, T1, T2));
    emit(x, enc_lsrv(T1, T1, 2));
    /* 线性探测：x2 = 表项的 pc，T2 = code */
    uint32_t *loop = here(x);
    emit(x, enc_add_lsl(T2, 0, T1, 4));
    emit(x, enc_ldp_x(2, T2, T2, 0));
    uint32_t *miss = here(x);
    emit(x, enc_cbz_x(T2));
    emit(x, enc_eor_lsr(2, 2, T0, 0));
    uint32_t *found = here(x);
    emit(x, enc_cbz_x(2));
    emit(x, enc_add_imm(T1, T1, 1, 0));
    emit(x, enc_and_reg(T1, T1, 3));
    emit(x, enc_b((intptr_t)loop - (intptr_t)here(x)));

    patch_branch(x->wdelta, found, here(x));
    emit_seq_step(x);
    emit(x, enc_ldr_x(0, CTX, offsetof(struct appine_cpu, dbt_jump_cache)));
    emit(x, enc_ubfx(1, T0, 2, JC_BITS));
    emit(x, enc_add_lsl(0, 0, 1, 4));
    emit(x, enc_stp_x(T0, T2, 0, 0));
    emit(x, enc_mov(T1, T2));
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_br(T1));

    patch_branch(x->wdelta, miss, here(x));
    emit_seq_step(x);
    emit(x, enc_ldp_x(0, 1, CTX, (int)x0));
    emit(x, enc_ldp_x(2, 3, CTX, (int)x0 + 16));
    emit(x, enc_movz(T1, 0, 0));
    emit(x, enc_movz(T2, R_LOOKUP, 0));
    emit(x, enc_b((intptr_t)d->exit - (intptr_t)here(x)));
    return entry;
}

/* 进入与退出例程：放在代码缓存开头，所有退出桩都能用一条 b 到达 */
static void emit_routines(struct appine_dbt *d)
{
    static const unsigned pairs[] = {0, 2, 4, 6, 8, 10, 12, 14, 19, 21, 23};
    struct xlate x = {.d = d, .buf = d->cache, .wdelta = d->rw_delta, .st = d->st};
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

    /* 桩用的两个入口：T1 指向桩里的 adr，pc 在桩末尾的数据字里（emit_link_stub） */
    d->exit_full = here(&x);
    emit(&x, enc_ldur_x(T0, T1, 12));
    uint32_t *join = here(&x);
    emit(&x, enc_b(0));
    d->exit_stub = here(&x);
    emit(&x, enc_ldur_x(T0, T1, 8));
    emit(&x, enc_ubfx(T2, T0, PACKED_PC_BITS, 64 - PACKED_PC_BITS));
    emit(&x, enc_and_low(T0, T0, PACKED_PC_BITS));
    patch_branch(x.wdelta, join, here(&x));
    emit(&x, enc_sub_imm(T1, T1, 4, 0));
    /* 退出：T0 = guest pc，T1 = 回填位置（只有链接桩的有用），T2 = 原因 */
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
    d->lookup = emit_lookup(&x, d);

    /* 影子返回栈空项的落点：像间接跳转没命中跳转缓存那样，按 guest x30 查找 */
    d->rs_miss = here(&x);
    emit(&x, enc_mov(T0, 30));
    emit(&x, enc_b((intptr_t)d->lookup - (intptr_t)here(&x)));

    d->routine_words = d->used_words = x.n;
    sync_range(d, d->cache, d->cache + x.n);
}

/*
 * 不再使用的实例（它的地址空间没了）放进一个小池子，给新的地址空间复用：短命进程不必每次
 * 都重新映射代码缓存和各张表，再在第一次写入每一页时缺页。翻译不多时保留下来，按程序
 * （调用方给的键，内核层用可执行文件的 inode）区分：同一个程序再运行时（shell 脚本里反复
 * exec 的 busybox、go build 的一个个 compile、一次次 `node -e`）拿回它留下的翻译，核对之后
 * 接着用；没有同键的就拿一个空的或者新建，不去清空别的程序留下的。几个进程同时跑同一个
 * 程序时各占一个实例，池子要放得下它们。池里保留的代码总量有上限，超出时先去掉最久没用的。
 * 清空了的实例把代码缓存用过的部分标成 MADV_FREE，宿主内存紧张时可以直接回收（之后再写入
 * 只是重新缺页）。线程的部分（跳转缓存、影子返回栈）另有一个池子。
 */
#define POOL_SIZE 16
#define POOL_KEEP_WORDS ((size_t)(64u << 20) / 4) /* 池里保留的代码最多 64 MB */
#define THREAD_POOL_SIZE 8

static struct {
    pthread_mutex_t lock;
    struct pooled {
        struct appine_dbt *d;
        uint64_t key;  /* 保留着哪个程序的翻译；0 表示已经清空 */
        uint64_t when; /* 放进来的先后，去掉最久没用的 */
    } free[POOL_SIZE];
    unsigned n;
    uint64_t clock;
    size_t kept_words; /* 池里保留着的代码 */
    struct appine_dbt_thread *threads[THREAD_POOL_SIZE];
    unsigned nthreads;
    struct appine_dbt *all; /* 所有实例 */
} pool = {.lock = PTHREAD_MUTEX_INITIALIZER};

/* 从池里取出第 i 个。持有池的锁 */
static struct appine_dbt *pool_take(unsigned i)
{
    struct appine_dbt *d = pool.free[i].d;
    if (pool.free[i].key)
        pool.kept_words -= d->used_words - d->routine_words;
    pool.free[i] = pool.free[--pool.n];
    return d;
}

static void release(struct appine_dbt *d)
{
    pthread_mutex_lock(&pool.lock);
    if (d->prev_instance) {
        *d->prev_instance = d->next_instance;
        if (d->next_instance)
            d->next_instance->prev_instance = d->prev_instance;
    }
    pthread_mutex_unlock(&pool.lock);
    if (d->cache)
        appine_unmap_code(d->cache, CODE_CACHE_BYTES, d->rw_delta);
    zfree(d->written, sizeof(*d->written));
    if (d->table)
        zfree(d->table, TABLE_BYTES(d->table->bits));
    zfree(d->table_used, TABLE_LIMIT * sizeof(*d->table_used));
    zfree(d->blocks, TABLE_LIMIT * sizeof(*d->blocks));
    zfree(d->words, WORDS_LIMIT * sizeof(*d->words));
    zfree(d->page_head, sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    zfree(d->page_parked, sizeof(*d->page_parked) << PAGE_BUCKET_BITS);
    pthread_mutex_destroy(&d->lock);
    pthread_cond_destroy(&d->flushed);
    zfree(d, sizeof(*d));
}

/* 没有线程挂着的实例自己的统计（从池里取出、放回池里时的清空和核对） */
static struct appine_dbt_stats spare_stats;

struct appine_dbt *appine_dbt_new(uint64_t key)
{
    pthread_mutex_lock(&pool.lock);
    int best = -1;
    for (unsigned i = 0; i < pool.n; i++) {
        bool same = key && pool.free[i].key == key;
        bool empty = !pool.free[i].key;
        if ((same && (best < 0 || pool.free[best].key != key || pool.free[i].when > pool.free[best].when)) ||
            (empty && best < 0))
            best = (int)i;
    }
    struct appine_dbt *d = best >= 0 ? pool_take((unsigned)best) : NULL;
    pthread_mutex_unlock(&pool.lock);
    if (d)
        return d;

    d = zalloc(sizeof(*d));
    if (!d)
        return NULL;
    pthread_mutex_init(&d->lock, NULL);
    pthread_cond_init(&d->flushed, NULL);
    d->st = &spare_stats;
    d->cache = appine_map_code(CODE_CACHE_BYTES, &d->rw_delta);
    if (d->cache && d->rw_delta)
        d->written = zalloc(sizeof(*d->written));
    d->table = table_new(TABLE_MIN_BITS);
    d->table_used = zalloc(TABLE_LIMIT * sizeof(*d->table_used));
    d->blocks = zalloc(TABLE_LIMIT * sizeof(*d->blocks));
    d->words = zalloc(WORDS_LIMIT * sizeof(*d->words));
    d->page_head = zalloc(sizeof(*d->page_head) << PAGE_BUCKET_BITS);
    d->page_parked = zalloc(sizeof(*d->page_parked) << PAGE_BUCKET_BITS);
    if (!d->cache || (d->rw_delta && !d->written) || !d->table || !d->table_used || !d->blocks ||
        !d->words || !d->page_head || !d->page_parked) {
        release(d);
        return NULL;
    }
    d->cache_words = CODE_CACHE_BYTES / 4;
    emit_routines(d);
    pthread_mutex_lock(&pool.lock);
    d->next_instance = pool.all;
    if (pool.all)
        pool.all->prev_instance = &d->next_instance;
    d->prev_instance = &pool.all;
    pool.all = d;
    pthread_mutex_unlock(&pool.lock);
    return d;
}

/* 放回池里：没有线程挂着，所以可以直接清空 */
void appine_dbt_free(struct appine_dbt *d, uint64_t key)
{
    if (!d)
        return;
    if (d->poisoned) {
        release(d);
        return;
    }
    d->st = &spare_stats;
    size_t words = d->used_words - d->routine_words;
    /* 块不多时保留翻译：下一个同一程序的使用者运行之前核对一遍（recheck），比逐块拷贝模板、
     * 重新链接、同步指令缓存便宜得多 */
    bool keep = key && d->table_count <= KEEP_MAX_BLOCKS && words <= POOL_KEEP_WORDS && !d->flush_wanted;
    /* 超过池里一半的大实例（JVM 一次就有四五十 MB）只在不挤掉别人时保留：否则跑一次 java
     * 就把 node、python 这些常用程序留下的翻译全挤掉了 */
    if (keep && words > POOL_KEEP_WORDS / 2) {
        pthread_mutex_lock(&pool.lock);
        keep = pool.kept_words + words <= POOL_KEEP_WORDS;
        pthread_mutex_unlock(&pool.lock);
    }
    if (keep) {
        d->stale = d->table_count > 0;
    } else {
        key = 0;
        words = 0;
        size_t nblocks = d->table_count, nguest = d->words_used;
        flush_locked(d);
        if (d->table->bits > TABLE_MIN_BITS) /* 下一个使用者多半是个短命进程 */
            table_resize(d, TABLE_MIN_BITS);
        /* 块的记录和指令字按上限预先映射，用过的部分一直占着内存：也交还 */
        release_pages(d->blocks, nblocks * sizeof(*d->blocks));
        release_pages(d->table_used, nblocks * sizeof(*d->table_used));
        release_pages(d->words, nguest * sizeof(*d->words));
        /*
         * 进入/退出例程所在的页保留，其余用过的部分交给宿主回收：私有映射用 MADV_FREE，
         * 内存紧张时才回收；双映射的共享内存不支持它，只能用 MADV_REMOVE 立即释放
         */
        uintptr_t page = host_page_size();
        uintptr_t start = ((uintptr_t)rw(d, d->cache + d->routine_words) + page - 1) & ~(page - 1);
        uintptr_t end = ((uintptr_t)rw(d, d->cache + d->high_words) + page - 1) & ~(page - 1);
        if (end > start)
            madvise((void *)start, end - start, d->rw_delta ? MADV_REMOVE : MADV_FREE);
        d->high_words = 0;
        drop_written(d); /* 例程所在的页、记下的页数 */
    }
    /* 腾出地方：池满了，或者保留的代码超出上限。先去掉空的（重建它们很便宜），再去掉
     * 最久没用的 */
    struct appine_dbt *evicted[POOL_SIZE];
    unsigned nevicted = 0;
    pthread_mutex_lock(&pool.lock);
    while (pool.n && (pool.n == POOL_SIZE || pool.kept_words + words > POOL_KEEP_WORDS)) {
        /* 腾代码的地方时去掉空的没有用 */
        bool need_code = pool.kept_words + words > POOL_KEEP_WORDS;
        int victim = -1;
        for (unsigned i = 0; i < pool.n; i++) {
            bool empty = !pool.free[i].key;
            if (need_code && empty)
                continue;
            if (victim < 0) {
                victim = (int)i;
                continue;
            }
            bool victim_empty = !pool.free[victim].key;
            if (empty != victim_empty ? empty : pool.free[i].when < pool.free[victim].when)
                victim = (int)i;
        }
        if (victim < 0)
            break;
        evicted[nevicted++] = pool_take((unsigned)victim);
    }
    pool.free[pool.n++] = (struct pooled){d, key, ++pool.clock};
    pool.kept_words += words;
    pthread_mutex_unlock(&pool.lock);
    for (unsigned i = 0; i < nevicted; i++)
        release(evicted[i]);
}

/* 拆掉所有实例的代码缓存在可写视图里写过的页（struct written 前面的说明） */
void appine_dbt_unmap_written(void)
{
    pthread_mutex_lock(&pool.lock);
    for (struct appine_dbt *d = pool.all; d; d = d->next_instance)
        drop_written(d);
    pthread_mutex_unlock(&pool.lock);
}

/* 可写视图里写过、还没拆的页一共多少字节：它们在宿主的 RSS 里算了两次 */
uint64_t appine_dbt_written_bytes(void)
{
    uint64_t pages = 0;
    pthread_mutex_lock(&pool.lock);
    for (struct appine_dbt *d = pool.all; d; d = d->next_instance)
        if (d->written)
            pages += __atomic_load_n(&d->written->pages, __ATOMIC_RELAXED);
    pthread_mutex_unlock(&pool.lock);
    return pages * host_page_size();
}

/* 释放池里所有的实例（App 内存紧张时，保留的翻译可以重建） */
void appine_dbt_trim(void)
{
    struct appine_dbt *evicted[POOL_SIZE];
    pthread_mutex_lock(&pool.lock);
    unsigned n = 0;
    while (pool.n)
        evicted[n++] = pool_take(pool.n - 1);
    pthread_mutex_unlock(&pool.lock);
    for (unsigned i = 0; i < n; i++)
        release(evicted[i]);
}

static void release_thread(struct appine_dbt_thread *t)
{
    zfree(t->jc, JC_SIZE * sizeof(*t->jc));
    zfree(t->rs, APPINE_PAGE_SIZE);
    free(t->xlate);
    zfree(t->scratch, MAX_BLOCK_WORDS * sizeof(*t->scratch));
    zfree(t->probe, sizeof(*t->probe));
    zfree(t->defined, DEFINED_CACHE * sizeof(*t->defined));
    free(t->prep);
    zfree(t, sizeof(*t));
}

/* 取得 d 的锁，记下这次的翻译、作废算在谁头上。持锁的时间都很短（装一个块、作废一批块），
 * 先自旋一会儿，免得睡下再被叫醒（虚拟机里一次要几十微秒） */
static void lock_dbt(struct appine_dbt *d, struct appine_dbt_stats *st)
{
    unsigned spin = 0;
    while (pthread_mutex_trylock(&d->lock) != 0) {
        if (++spin == 200) {
            pthread_mutex_lock(&d->lock);
            break;
        }
        __asm__ volatile("yield");
    }
    appine_guard_hold(&d->lock, false);
    d->st = st;
}

static void unlock_dbt(struct appine_dbt *d)
{
    appine_guard_drop();
    pthread_mutex_unlock(&d->lock);
}

struct appine_dbt_thread *appine_dbt_attach(struct appine_dbt *d)
{
    pthread_mutex_lock(&pool.lock);
    struct appine_dbt_thread *t = pool.nthreads ? pool.threads[--pool.nthreads] : NULL;
    pthread_mutex_unlock(&pool.lock);
    if (!t) {
        t = zalloc(sizeof(*t));
        if (!t)
            return NULL;
        t->jc = zalloc(JC_SIZE * sizeof(*t->jc));
        t->rs = zalloc(APPINE_PAGE_SIZE); /* 按页对齐，也就按 1 KB 对齐 */
        t->xlate = malloc(sizeof(*t->xlate));
        t->scratch = zalloc(MAX_BLOCK_WORDS * sizeof(*t->scratch));
        t->probe = zalloc(sizeof(*t->probe)); /* TLB 占一百多 KB，用不到的页不占内存 */
        t->defined = zalloc(DEFINED_CACHE * sizeof(*t->defined));
        t->prep = malloc(sizeof(*t->prep));
        if (!t->jc || !t->rs || !t->xlate || !t->scratch || !t->probe || !t->defined || !t->prep) {
            release_thread(t);
            return NULL;
        }
        appine_cpu_init(t->probe, NULL);
        t->jc_dirty = true;
    }
    t->d = d;
    t->cpu = NULL;
    t->stats = (struct appine_dbt_stats){0};
    if (t->jc_dirty)
        clear_jump_cache(t);
    lock_dbt(d, &t->stats);
    t->generation = d->generation;
    rs_reset(t);
    t->next = d->threads;
    d->threads = t;
    __atomic_store_n(&d->nthreads, d->nthreads + 1, __ATOMIC_RELAXED);
    unlock_dbt(d);
    return t;
}

void appine_dbt_detach(struct appine_dbt_thread *t)
{
    if (!t)
        return;
    struct appine_dbt *d = t->d;
    lock_dbt(d, &spare_stats);
    for (struct appine_dbt_thread **p = &d->threads; *p; p = &(*p)->next) {
        if (*p == t) {
            *p = t->next;
            __atomic_store_n(&d->nthreads, d->nthreads - 1, __ATOMIC_RELAXED);
            break;
        }
    }
    unlock_dbt(d);
    pthread_mutex_lock(&pool.lock);
    bool keep = pool.nthreads < THREAD_POOL_SIZE;
    if (keep)
        pool.threads[pool.nthreads++] = t;
    pthread_mutex_unlock(&pool.lock);
    if (!keep)
        release_thread(t);
}

void appine_dbt_flush(struct appine_dbt_thread *t)
{
    __atomic_store_n(&t->d->flush_wanted, true, __ATOMIC_SEQ_CST);
}

void appine_dbt_recheck(struct appine_dbt_thread *t)
{
    lock_dbt(t->d, &t->stats);
    t->d->stale = t->d->table_count > 0;
    unlock_dbt(t->d);
}

void appine_dbt_invalidate(struct appine_dbt_thread *t, const struct appine_cpu *cpu, uint64_t start,
                           uint64_t len)
{
    lock_dbt(t->d, &t->stats);
    invalidate_locked(t->d, cpu ? cpu->mmu : NULL, start, len);
    unlock_dbt(t->d);
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
 * 大部分都对不上时（exec 了别的程序）请求整体清空。持有锁
 */
/* 块整个落在上一个使用者的某段文件映射里（appine_dbt_trust 记下）：它的页还没映射时值得
 * 停放，以后多半映射回同样的内容；匿名内存里的（JIT 生成的代码）下一次多半对不上 */
static bool block_parkable(const struct appine_dbt *d, const struct block *b)
{
    for (unsigned i = 0; i < d->nparkable; i++) {
        if (b->pc >= d->parkable[i][0] && b->pc + (uint64_t)b->nguest * 4 <= d->parkable[i][1])
            return true;
    }
    return false;
}

/* 块整个落在 appine_dbt_trust 给出的范围里 */
static bool block_trusted(const struct appine_dbt *d, const struct block *b)
{
    for (unsigned i = 0; i < d->ntrusted; i++) {
        if (b->pc >= d->trusted[i][0] && b->pc + (uint64_t)b->nguest * 4 <= d->trusted[i][1])
            return true;
    }
    return false;
}

static void recheck(struct appine_dbt *d, const struct appine_cpu *c)
{
    d->stale = false;
    size_t live = 0, bad = 0, unmapped = 0;
    uint64_t page = 1; /* 未对齐，不会与任何页相同 */
    const uint32_t *host = NULL;
    for (size_t i = 0; i < d->table_count; i++) {
        const struct block *b = &d->blocks[i];
        if (b->code && !b->parked) {
            live++;
            if (!block_trusted(d, b) && !block_matches(d, c->mmu, b, &page, &host)) {
                bool park = !host && block_parkable(d, b);
                bad += !park;
                unmapped += park;
            }
        }
    }
    if (bad * 2 > live) {
        d->ntrusted = d->nparkable = 0;
        d->flush_wanted = true;
        return;
    }
    /* 所在的页还没有映射的块（共享库要等动态链接器加载，这时只有程序和动态链接器）停放
     * 起来，第一次执行到时再核对（revive）：原来都作废了，node 每次启动要从模板重新拷贝
     * 六七千个块 */
    page = 1;
    struct retire_batch r = {.n = 0};
    for (size_t i = 0; (bad || unmapped) && i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (b->code && !b->parked && !block_trusted(d, b) &&
            !block_matches(d, c->mmu, b, &page, &host)) {
            bool park = !host && block_parkable(d, b);
            if (!retire_add(d, &r, b, park)) {
                d->flush_wanted = true;
                break;
            }
            if (park)
                unmapped--;
            else
                bad--;
        }
    }
    retire_flush(d, &r);
    d->ntrusted = d->nparkable = 0;
}

/*
 * 停放的块重新放进翻译表（revive）。停放时留下的墓碑还在（这之后没有重建过表）就原样
 * 恢复：代码没变，不持锁查表的线程读到恢复前后的哪一种都对。插到别的空位的话，没有
 * 记录再指向那个墓碑，整体清空（flush_locked 只清 table_used 记下的位置）之后它还占着
 * 位置却不再计数，反复停放、恢复、清空，表就被占满，插入和查表都找不到空位。持有锁
 */
static void table_relink(struct appine_dbt *d, size_t k)
{
    struct table *t = d->table;
    const struct block *b = &d->blocks[k];
    size_t i = d->table_used[k];
    if (i != SLOT_NONE && t->slots[i].pc == JC_EMPTY && t->slots[i].code == b->code) {
        d->table_dead--;
        __atomic_store_n(&t->slots[i].pc, b->pc, __ATOMIC_RELEASE);
        return;
    }
    i = table_index(t, b->pc);
    while (t->slots[i].code)
        i = (i + 1) & table_mask(t);
    d->table_occupied++;
    __atomic_store_n(&t->slots[i].pc, b->pc, __ATOMIC_RELAXED);
    __atomic_store_n(&t->slots[i].code, b->code, __ATOMIC_RELEASE);
    d->table_used[k] = (uint32_t)i;
}

/*
 * 分发器没查到 pc，而它有停放着的块（recheck）：把同一页里停放着的块一起核对，guest 指令字
 * 和现在的相同的恢复入口、放回表里，不必重新翻译或者拷贝模板；对不上的作废。一页一起做：
 * 每个停放的块第一次执行到时都要经由转发桩退出到这里，一个一个恢复时光是这些退出就把
 * 省下的时间用掉了。指向停放块的链接一直跳往它的入口，停放期间经由转发桩退出到这里，
 * 所以没有核对过的代码不会被执行。表太满时不恢复，由正常的翻译路径扩表。持有锁
 */
/* 恢复的这一批先同步、再放进表里，别的线程才可能跳过去 */
static void revive_flush(struct appine_dbt *d, const uint32_t *idx, unsigned n)
{
    void *start[RETIRE_BATCH], *end[RETIRE_BATCH];
    for (unsigned k = 0; k < n; k++) {
        start[k] = d->blocks[idx[k]].code;
        end[k] = d->blocks[idx[k]].code + ENTRY_CHECK_WORDS + 1;
    }
    sync_written(d, start, end, n);
    for (unsigned k = 0; k < n; k++)
        table_relink(d, idx[k]);
}

static uint32_t *revive(struct appine_dbt *d, const struct appine_cpu *c, uint64_t pc)
{
    uint64_t page = pc >> APPINE_PAGE_SHIFT, p = 1;
    const uint32_t *host = NULL;
    uint32_t *found = NULL, idx[RETIRE_BATCH];
    unsigned n = 0;
    /* 入口检查的第一条对所有块都一样（prepare），不必另外记 */
    uint32_t check = enc_ldr_x(T0, CTX, offsetof(struct appine_cpu, pending));
    for (uint32_t i = d->page_head[page_bucket(page)]; i; i = d->blocks[i - 1].next) {
        struct block *b = &d->blocks[i - 1];
        if (!b->parked || b->pc >> APPINE_PAGE_SHIFT != page)
            continue;
        if (!block_matches(d, c->mmu, b, &p, &host)) {
            kill_parked(d, b);
            continue;
        }
        if (d->table_occupied + n >= ((size_t)3 << d->table->bits) / 4)
            break;
        if (n == RETIRE_BATCH) {
            revive_flush(d, idx, n);
            n = 0;
        }
        *rw(d, b->code) = check;
        *rw(d, b->code + ENTRY_CHECK_WORDS) = b->first;
        b->parked = 0;
        d->nparked--;
        d->page_parked[page_bucket(page)]--;
        idx[n++] = i - 1;
        if (b->pc == pc)
            found = b->code;
    }
    revive_flush(d, idx, n);
    return found;
}

void appine_dbt_set_sources(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    /* 记不下的不记：没记下的映射里的块照常核对 */
    d->nsources = n < APPINE_DBT_SOURCES ? (unsigned)n : APPINE_DBT_SOURCES;
    memcpy(d->sources, s, d->nsources * sizeof(*s));
    pthread_mutex_unlock(&d->lock);
}

void appine_dbt_trust(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    d->ntrusted = 0;
    for (size_t i = 0; d->stale && i < n; i++) {
        for (unsigned j = 0; j < d->nsources; j++) {
            if (!memcmp(&s[i], &d->sources[j], sizeof(*s)) && d->ntrusted < APPINE_DBT_SOURCES) {
                d->trusted[d->ntrusted][0] = s[i].start;
                d->trusted[d->ntrusted][1] = s[i].start + s[i].len;
                d->ntrusted++;
                break;
            }
        }
    }
    /* 记下的只用一次：这个地址空间不再使用时重新记。全部记作可以停放的范围（recheck） */
    for (unsigned j = 0; j < d->nsources; j++) {
        d->parkable[j][0] = d->sources[j].start;
        d->parkable[j][1] = d->sources[j].start + d->sources[j].len;
    }
    d->nparkable = d->stale ? d->nsources : 0;
    d->nsources = 0;
    pthread_mutex_unlock(&d->lock);
}

void appine_dbt_retire_outside(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    pthread_mutex_lock(&d->lock);
    d->st = &spare_stats; /* 上一个持锁的线程可能已经解挂 */
    struct retire_batch r = {.n = 0};
    for (size_t i = 0; i < d->table_count; i++) {
        struct block *b = &d->blocks[i];
        if (!b->code)
            continue;
        bool inside = false;
        for (size_t j = 0; j < n && !inside; j++)
            inside = b->pc - s[j].start < s[j].len;
        if (inside)
            continue;
        if (b->parked) {
            kill_parked(d, b);
        } else if (!retire_add(d, &r, b, false)) {
            __atomic_store_n(&d->flush_wanted, true, __ATOMIC_SEQ_CST);
            break;
        }
    }
    retire_flush(d, &r);
    pthread_mutex_unlock(&d->lock);
}

/* 停下来等别人整体清空完。持有锁，等待期间放开 */
static void park(struct appine_dbt *d)
{
    __atomic_fetch_add(&d->parked, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&d->flushing, __ATOMIC_ACQUIRE) && !d->poisoned)
        pthread_cond_wait(&d->flushed, &d->lock);
    __atomic_fetch_sub(&d->parked, 1, __ATOMIC_SEQ_CST);
}

/*
 * 整体清空：打断其他在 dbt_run 里的线程，等它们都停下来（park）或者离开 dbt_run，再
 * 清空。持有锁进入和返回，等待期间放开锁，别的线程才走得到 park。已经有线程在做时一起
 * 停下等它。
 *
 * 先置 flushing 再看各线程的 cpu，线程那边先登记 cpu、再计入 running、再看 flushing：
 * 要么这里打断得到它，要么它看到 flushing 自己停下。等待时先读 parked 再读 running：
 * 停下的线程在清空结束之前不会离开，这样算出来的"还在跑的"只会多不会少。
 */
static void flush_world(struct appine_dbt *d, struct appine_dbt_thread *self)
{
    if (__atomic_load_n(&d->flushing, __ATOMIC_RELAXED)) {
        park(d);
        return;
    }
    __atomic_store_n(&d->flushing, true, __ATOMIC_SEQ_CST);
    for (struct appine_dbt_thread *t = d->threads; t; t = t->next) {
        struct appine_cpu *c = __atomic_load_n(&t->cpu, __ATOMIC_SEQ_CST);
        if (t != self && c)
            appine_cpu_kick(c);
    }
    unlock_dbt(d);
    for (;;) {
        unsigned parked = __atomic_load_n(&d->parked, __ATOMIC_SEQ_CST);
        unsigned running = __atomic_load_n(&d->running, __ATOMIC_SEQ_CST);
        if (running - parked <= 1)
            break;
        sched_yield();
    }
    lock_dbt(d, &self->stats);
    flush_locked(d);
    __atomic_store_n(&d->flushing, false, __ATOMIC_SEQ_CST);
    pthread_cond_broadcast(&d->flushed);
}

/* guest 改了 `addr` 所在那一页的代码（ic ivau）：作废其中指令字变了的块 */
static void code_changed(struct appine_dbt_thread *t, const struct appine_cpu *c, uint64_t addr)
{
    lock_dbt(t->d, &t->stats);
    invalidate_locked(t->d, c->mmu, addr & ~(uint64_t)PAGE_MASK, APPINE_PAGE_SIZE);
    unlock_dbt(t->d);
}

static enum appine_exit dbt_run(void *ctx, struct appine_cpu *c)
{
    struct appine_dbt_thread *t = ctx;
    struct appine_dbt *d = t->d;
    uint64_t (*enter)(struct appine_cpu *, uint32_t *) =
        (uint64_t(*)(struct appine_cpu *, uint32_t *))(uintptr_t)d->enter;
    uint32_t *patch_site = NULL, *patch_branch_site = NULL;
    uint64_t patch_generation = 0;
    bool patch_forward = false;
    bool fill_jump_cache = false;

    if (UNLIKELY(d->stale)) {
        lock_dbt(d, &t->stats);
        if (d->stale)
            recheck(d, c);
        unlock_dbt(d);
    }
    c->dbt_jump_cache = t->jc;
    t->jc_dirty = true; /* 查表例程会往里填 */
    if (((uintptr_t)c->dbt_rs & ~(uintptr_t)((1u << RS_SHIFT) - 1)) != (uintptr_t)t->rs)
        c->dbt_rs = t->rs;
    for (;;) {
        /* 有人要整体清空：一起停下，或者自己来做 */
        if (UNLIKELY(__atomic_load_n(&d->flushing, __ATOMIC_SEQ_CST) ||
                     __atomic_load_n(&d->flush_wanted, __ATOMIC_SEQ_CST))) {
            lock_dbt(d, &t->stats);
            if (d->flushing)
                park(d);
            else if (d->flush_wanted)
                flush_world(d, t);
            unlock_dbt(d);
            patch_site = NULL;
        }
        thread_catch_up(t);
        if (UNLIKELY(__atomic_load_n(&c->pending, __ATOMIC_RELAXED)) &&
            __atomic_exchange_n(&c->pending, 0, __ATOMIC_ACQUIRE))
            return APPINE_EXIT_KICK;

        uint32_t *code = table_lookup(d, c, c->pc);
        if (!code && __atomic_load_n(&d->page_parked[page_bucket(c->pc >> APPINE_PAGE_SHIFT)],
                                     __ATOMIC_RELAXED)) {
            lock_dbt(d, &t->stats);
            if (!__atomic_load_n(&d->flushing, __ATOMIC_RELAXED) && !d->flush_wanted) {
                code = table_find(d->table, c->pc);
                if (!code)
                    code = revive(d, c, c->pc);
            }
            unlock_dbt(d);
        }
        if (!code) {
            /* 不持锁翻译，持锁装进去 */
            bool fetched = prepare(t, c, c->pc);
            lock_dbt(d, &t->stats);
            if (__atomic_load_n(&d->flushing, __ATOMIC_RELAXED) || d->flush_wanted) {
                unlock_dbt(d);
                continue;
            }
            struct syncs sync = {.n = 0};
            struct fresh f = {.code = NULL};
            /* 只有这一个线程时，新块和链接点一起同步一次；否则新块先同步、放进表里，
             * 别的线程才可能跳过去 */
            bool alone = d->threads == t && !t->next;
            /* 别的线程可能刚装进去同一个块 */
            code = table_find(d->table, c->pc);
            if (!code && fetched) {
                code = install(d, t->prep, &sync, &f);
                if (code == NEED_FLUSH) {
                    flush_world(d, t);
                    unlock_dbt(d);
                    patch_site = NULL;
                    continue;
                }
                if (code == STALE) {
                    unlock_dbt(d);
                    continue;
                }
                if (!alone) {
                    sync_all(d, &sync);
                    table_insert(d, f.pc, f.code, f.guest, f.nguest);
                }
            }
            /* 持锁时表里只有有效的块：回填的目标不会是作废块（它的入口跳往自己） */
            uint32_t *target = code && patch_forward ? code + ENTRY_CHECK_WORDS : code;
            if (code && patch_site && patch_generation == d->generation && target != patch_site) {
                *rw(d, patch_site) = enc_b((intptr_t)target - (intptr_t)patch_site);
                /* 条件分支不在 CMODX 允许并发修改的指令里：有别的线程时不改它 */
                bool branch = patch_branch_site && alone && relink_word(d, patch_branch_site, target);
                if (sync.n) { /* 新块反正要同步，链接点一起 */
                    sync_add(&sync, patch_site, patch_site + 1);
                    if (branch)
                        sync_add(&sync, patch_branch_site, patch_branch_site + 1);
                } else {
                    publish_link(d, patch_site, branch ? patch_branch_site : NULL);
                }
            }
            sync_all(d, &sync);
            if (f.code && alone)
                table_insert(d, f.pc, f.code, f.guest, f.nguest);
            unlock_dbt(d);
            patch_site = NULL;
        } else if (patch_site) {
            /*
             * 目标已经翻译过：不持锁回填链接。b 改成 b 可以并发修改；整体清空要等这个线程
             * 停下，不会同时发生。不持锁查到的可能是刚被作废的块，它不会是回填位置本身以外
             * 的东西：作废块的入口跳往转发桩，转发桩照样退出到这里
             */
            uint32_t *target = patch_forward ? code + ENTRY_CHECK_WORDS : code;
            if (patch_generation == __atomic_load_n(&d->generation, __ATOMIC_ACQUIRE) &&
                target != patch_site) {
                uint32_t b = enc_b((intptr_t)target - (intptr_t)patch_site);
                /* 读执行用的地址，见 relink_word */
                bool again = __atomic_load_n(patch_site, __ATOMIC_RELAXED) == b;
                __atomic_store_n(rw(d, patch_site), b, __ATOMIC_RELAXED);
                bool branch = patch_branch_site && __atomic_load_n(&d->nthreads, __ATOMIC_RELAXED) == 1 &&
                              relink_word(d, patch_branch_site, target);
                if (again) {
                    /* 已经改过、这个线程却还执行了旧指令：完整同步一次 */
                    struct syncs sync = {.n = 0};
                    sync_add(&sync, patch_site, patch_site + 1);
                    if (branch)
                        sync_add(&sync, patch_branch_site, patch_branch_site + 1);
                    sync_all(d, &sync);
                } else {
                    publish_link(d, patch_site, branch ? patch_branch_site : NULL);
                }
            }
            patch_site = NULL;
        }
        if (!code) { /* 无法取指：让 Tier 0 报告精确的 fault */
            enum appine_exit r = appine_interp_step(c);
            if (r != APPINE_EXIT_NONE)
                return r;
            continue;
        }
        if (fill_jump_cache) {
            t->jc[(c->pc >> 2) & (JC_SIZE - 1)] = (struct slot){c->pc, code};
            t->jc_dirty = true;
            fill_jump_cache = false;
        }

        uint64_t reason = enter(c, code);
        switch (reason & REASON_MASK) {
        case R_CHAIN:
        case R_CHAIN_FWD:
            t->stats.exits_chain++;
            patch_site = (uint32_t *)(uintptr_t)c->dbt_site;
            patch_branch_site = reason >> REASON_BITS ? patch_site - (reason >> REASON_BITS) : NULL;
            patch_generation = __atomic_load_n(&d->generation, __ATOMIC_RELAXED);
            patch_forward = (reason & REASON_MASK) == R_CHAIN_FWD;
            break;
        case R_LOOKUP:
            t->stats.exits_lookup++;
            fill_jump_cache = true;
            break;
        case R_UNGROUP:
            /* 有观察点时回填总是失败（emit_refill），不是这一组访存不能共用检查 */
            if (!c->nwatch) {
                lock_dbt(d, &t->stats);
                ungroup(d, c->pc);
                unlock_dbt(d);
            }
            /* fallthrough */
        case R_INTERP: {
            t->stats.exits_interp++;
            enum appine_exit r = appine_interp_step(c);
            /*
             * ic ivau 记下的页（dsb、isb 时；或者 Tier 0 执行了记到另一页的 ic ivau）：翻译由
             * 这个地址空间的线程共用，就地按内容作废一次就行，不必退出到调用方。作废块的入口
             * 跳往转发桩，别的线程下次进入它们时就会看到（硬件上它们在上下文同步之前同样可能
             * 还执行旧指令）。HotSpot 每改一处代码就 ic ivau、dsb 一次，Maven 一次构建 8 万次，
             * 原来每次都要退出、取 BKL、记日志，回来再作废
             */
            if (r == APPINE_EXIT_CODE) {
                code_changed(t, c, c->fault_addr);
                r = APPINE_EXIT_NONE;
            }
            if (r != APPINE_EXIT_NONE)
                return r;
            if (UNLIKELY(c->ivau_page)) {
                code_changed(t, c, c->ivau_page);
                c->ivau_page = 0;
            }
            break;
        }
        case R_SVC:
            t->stats.exits_svc++;
            c->exit_info = 0;
            c->excl_size = 0; /* 与 Tier 0 一样：异常返回清掉独占标记 */
            return APPINE_EXIT_SVC;
        default:
            t->stats.exits_kick++;
            break;
        }
    }
}

enum appine_exit appine_dbt_run(struct appine_dbt_thread *t, struct appine_cpu *c)
{
    struct appine_dbt *d = t->d;
    /* 宿主故障作废了这个实例：其余线程用 Tier 0 跑到进程被结束 */
    if (UNLIKELY(d->poisoned))
        return appine_cpu_run(c);
    /* 先登记 cpu、再计入 running（见 flush_world） */
    __atomic_store_n(&t->cpu, c, __ATOMIC_SEQ_CST);
    __atomic_fetch_add(&d->running, 1, __ATOMIC_SEQ_CST);
    enum appine_exit r = appine_guard_run(dbt_run, t, c);
    if (UNLIKELY(r == APPINE_EXIT_HOST_FAULT)) {
        /* guard 已经放开了持有的锁。停着等清空的线程不能一直等下去 */
        pthread_mutex_lock(&d->lock);
        d->poisoned = true;
        __atomic_store_n(&d->flushing, false, __ATOMIC_SEQ_CST);
        pthread_cond_broadcast(&d->flushed);
        pthread_mutex_unlock(&d->lock);
        template_clear(false);
    }
    __atomic_fetch_sub(&d->running, 1, __ATOMIC_SEQ_CST);
    __atomic_store_n(&t->cpu, NULL, __ATOMIC_RELEASE);
    return r;
}

void appine_dbt_get_stats(const struct appine_dbt_thread *t, struct appine_dbt_stats *out)
{
    *out = t->stats;
}

#else /* !DBT_SUPPORTED */

struct appine_dbt *appine_dbt_new(uint64_t key)
{
    (void)key;
    return NULL;
}

void appine_dbt_free(struct appine_dbt *d, uint64_t key)
{
    (void)d;
    (void)key;
}

void appine_dbt_trim(void)
{
}

void appine_dbt_unmap_written(void)
{
}

uint64_t appine_dbt_written_bytes(void)
{
    return 0;
}

void appine_dbt_set_sources(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

void appine_dbt_trust(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

void appine_dbt_retire_outside(struct appine_dbt *d, const struct appine_dbt_source *s, size_t n)
{
    (void)d;
    (void)s;
    (void)n;
}

struct appine_dbt_thread *appine_dbt_attach(struct appine_dbt *d)
{
    (void)d;
    return NULL;
}

void appine_dbt_detach(struct appine_dbt_thread *t)
{
    (void)t;
}

void appine_dbt_flush(struct appine_dbt_thread *t)
{
    (void)t;
}

void appine_dbt_recheck(struct appine_dbt_thread *t)
{
    (void)t;
}

void appine_dbt_invalidate(struct appine_dbt_thread *t, const struct appine_cpu *cpu, uint64_t start,
                           uint64_t len)
{
    (void)t;
    (void)cpu;
    (void)start;
    (void)len;
}

enum appine_exit appine_dbt_run(struct appine_dbt_thread *t, struct appine_cpu *c)
{
    (void)t;
    return appine_cpu_run(c);
}

void appine_dbt_get_stats(const struct appine_dbt_thread *t, struct appine_dbt_stats *out)
{
    (void)t;
    memset(out, 0, sizeof(*out));
}

#endif
