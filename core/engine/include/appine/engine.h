/*
 * appine 引擎的对外接口（设计文档 §4、§5、§13.1）。
 *
 * 引擎只负责执行 guest 指令和 soft-MMU 快速路径。遇到 svc、访存失败、未定义
 * 指令、brk 时，appine_cpu_run() 返回一个退出原因，由内核层（Rust）处理后
 * 再次调用 appine_cpu_run() 继续执行。
 *
 * 线程模型：一个 struct appine_cpu 只能由一个线程运行；除 appine_cpu_kick()
 * 外，其余函数都不是线程安全的。多个 cpu 可以共享一个 mmu 并同时运行：页表
 * 允许在被遍历的同时修改，但修改本身必须由调用方串行化。解除映射或降低权限
 * 之后，调用方必须让所有使用该 mmu 的 cpu 调用 appine_cpu_tlb_flush()，并且
 * 在它们都这样做之前不能释放被解除映射的 host 内存。
 */
#ifndef APPINE_ENGINE_H
#define APPINE_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#define APPINE_PAGE_SHIFT 12
#define APPINE_PAGE_SIZE (UINT64_C(1) << APPINE_PAGE_SHIFT)
#define APPINE_VA_BITS 48
#define APPINE_TLB_ENTRIES 4096 /* 数据 TLB，直接映射 */
#define APPINE_ITLB_ENTRIES 512 /* 取指 TLB，只有 Tier 0 用 */

/* 页权限，与 Linux 的 PROT_* 取值一致 */
enum {
    APPINE_PROT_READ = 1u << 0,
    APPINE_PROT_WRITE = 1u << 1,
    APPINE_PROT_EXEC = 1u << 2,
};

/* 访存类型，用于 fault 上报 */
enum appine_access {
    APPINE_ACCESS_READ = 1,
    APPINE_ACCESS_WRITE = 2,
    APPINE_ACCESS_EXEC = 3,
};

enum appine_fault_kind {
    APPINE_FAULT_PAGE = 1,     /* 未映射或权限不符，对应 SIGSEGV */
    APPINE_FAULT_ALIGN = 2,    /* PC 或者要求对齐的数据访问未对齐，对应 SIGBUS */
    APPINE_FAULT_SP_ALIGN = 3, /* 以 SP 为基址时 SP 未对齐（fault_addr = SP），对应 SIGBUS */
};

/* appine_cpu_run() 的退出原因 */
enum appine_exit {
    APPINE_EXIT_NONE = 0,  /* 仅 appine_cpu_exec_insn() 使用：指令正常完成 */
    APPINE_EXIT_SVC = 1,   /* pc 已指向下一条指令，exit_info = imm16 */
    APPINE_EXIT_UNDEF = 2, /* pc 指向该指令，exit_info = 指令编码 */
    APPINE_EXIT_BRK = 3,   /* pc 指向该指令，exit_info = imm16 */
    APPINE_EXIT_FAULT = 4, /* pc 指向出错指令，fault_* 字段有效，指令没有产生副作用 */
    APPINE_EXIT_KICK = 5,  /* 被 appine_cpu_kick() 打断，pending 已清零 */
    APPINE_EXIT_CODE = 6,  /* ic ivau：pc 已指向下一条指令，fault_addr = 其地址（已去掉 tag） */
    /* 引擎自己的 bug 让宿主收到了 SIGSEGV、SIGBUS 或 SIGILL（只在 appine_fault_guard_install()
     * 之后出现）：guest 状态不可信，调用方应当结束这个 guest 进程。详情见
     * appine_host_fault_last()；Tier 1 实例已经作废，appine_dbt_free() 直接释放它 */
    APPINE_EXIT_HOST_FAULT = 7,
};

/*
 * 数据 TLB 表项：host 地址 = guest 地址 + addend。tag 和 addend 相邻存放，
 * Tier 1 生成的代码用一条 ldp 同时取出两者。tag 存的是 guest 页号取反，0 表示
 * 无效：清零的内存就是空的 TLB，不必逐项写入（没碰过的页也就不占内存）。
 */
struct appine_tlb_entry {
    uint64_t tag_read; /* ~guest 页号；0 表示无效 */
    uint64_t addend_read;
    uint64_t tag_write;
    uint64_t addend_write;
};

/* 取指 TLB 表项，只有 Tier 0 的取指和 Tier 1 的翻译会用到 */
struct appine_itlb_entry {
    uint64_t tag;
    uint64_t addend;
};

struct appine_mmu;

/*
 * guest CPU 状态。字段布局由 Rust 侧逐字段镜像（core/engine/src/lib.rs），
 * 修改时必须同步，并由 appine_cpu_layout_check() 校验。
 */
struct appine_cpu {
    /* Tier 1 生成的代码用 ldr/str 的立即数偏移直接读写 tlb 之前的字段 */
    uint64_t x[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t nzcv; /* 只使用 bit 31..28 */
    uint64_t fpcr;
    uint64_t fpsr;
    uint64_t tpidr_el0;
    uint64_t orig_x0; /* syscall 重启时恢复 x0（设计文档 §7.3），由内核层维护 */
    uint64_t fault_addr;
    uint32_t fault_access; /* enum appine_access */
    uint32_t fault_kind;   /* enum appine_fault_kind */
    uint32_t exit_info;
    uint32_t dbt_lookup_seq; /* Tier 1：奇数表示正在不持锁地查翻译表（dbt.c 的 table_lookup） */
    uint64_t pending; /* 只能用原子操作访问；64 位是为了让 Tier 1 能用一条 ldr 读到它 */
    struct appine_mmu *mmu;
    /* 独占监视器（LDXR/STXR）：记录的地址、长度和读到的值 */
    uint64_t excl_addr;
    uint64_t excl_value[2];
    uint32_t excl_size; /* 0 表示没有独占标记 */
    uint32_t reserved2;
    /* 以下字段由 Tier 1 内部使用 */
    uint64_t dbt_reason;
    uint64_t dbt_site;
    void *dbt_jump_cache;
    void *dbt_rs; /* 影子返回栈的栈顶（Tier 1 实例里 1 KB 对齐的环形缓冲） */
    /* ic ivau 记下、还没交给调用方作废翻译的页（0 表示没有），dsb、isb 和退出时交出；
     * 也让 tlb 按 16 字节对齐 */
    uint64_t ivau_page;
    /* Tier 1 一个块执行期间暂存几组访存的宿主基址（dbt.c 的 plan_group） */
    uint64_t dbt_groups[4];
    /* Tier 1 以 "ldp [cpu + 下标 * 32 + 偏移]" 寻址表项，所以 tlb 的偏移不能超出 ldp
     * 立即数的范围 */
    struct appine_tlb_entry tlb[APPINE_TLB_ENTRIES];
    struct appine_itlb_entry itlb[APPINE_ITLB_ENTRIES];
    uint64_t v[32][2]; /* V0–V31，低 64 位在前 */
    uint64_t tlb_used[APPINE_TLB_ENTRIES / 64]; /* 填充过的 tlb 项，清空时只处理这些 */
};

/* ---- guest 可见的 CPU 特性（设计文档 §4.4），取值与 Linux 的 HWCAP_* 一致 ---- */

#define APPINE_HWCAP_FP (UINT64_C(1) << 0)
#define APPINE_HWCAP_ASIMD (UINT64_C(1) << 1)
#define APPINE_HWCAP_AES (UINT64_C(1) << 3)
#define APPINE_HWCAP_PMULL (UINT64_C(1) << 4)
#define APPINE_HWCAP_SHA1 (UINT64_C(1) << 5)
#define APPINE_HWCAP_SHA2 (UINT64_C(1) << 6)
#define APPINE_HWCAP_CRC32 (UINT64_C(1) << 7)
#define APPINE_HWCAP_ATOMICS (UINT64_C(1) << 8)
#define APPINE_HWCAP_CPUID (UINT64_C(1) << 11)

/* 宣告给 guest 的 AT_HWCAP：宿主支持 ∩ Tier 0 已实现 ∩ 白名单 */
uint64_t appine_guest_hwcap(void);

/* guest 读到的 MIDR_EL1（宿主不允许读 ID 寄存器时为 0） */
uint64_t appine_guest_midr(void);

/* ---- soft-MMU 页表（设计文档 §5.1）---- */

enum {
    APPINE_OK = 0,
    APPINE_EINVAL = -1, /* 参数未按 4K 对齐、越过 48 位地址空间或权限位非法 */
    APPINE_ENOMEM = -2, /* 分配页表失败；已处理的页保持新映射 */
};

struct appine_mmu *appine_mmu_new(void);
void appine_mmu_free(struct appine_mmu *mmu);

/*
 * 把 guest [gva, gva + len) 映射到 host [host, host + len)，覆盖已有映射。
 * gva、host、len 都必须按 4K 对齐。host 内存由调用方持有，必须在映射存续
 * 期间保持有效。
 */
int appine_mmu_map(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot);
int appine_mmu_unmap(struct appine_mmu *mmu, uint64_t gva, uint64_t len);
/* 只修改已映射页的权限，未映射的页保持不变 */
int appine_mmu_protect(struct appine_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot);

/*
 * 查询一个 guest 地址对应的 host 指针（含页内偏移）。required_prot 中的权限
 * 必须全部具备，否则返回 NULL；传 0 表示只要求已映射。
 */
void *appine_mmu_translate(const struct appine_mmu *mmu, uint64_t gva, uint32_t required_prot);

/* ---- CPU ---- */

/*
 * 分配并初始化一个 cpu（失败时返回 NULL），用 appine_cpu_free() 释放。结构体里的 TLB
 * 有一百多 KB，用 mmap 分配，没用到的页不占内存。
 */
struct appine_cpu *appine_cpu_new(struct appine_mmu *mmu);
void appine_cpu_free(struct appine_cpu *cpu);

/* 清零全部状态并绑定 mmu（可为 NULL，此时所有访存都会失败） */
void appine_cpu_init(struct appine_cpu *cpu, struct appine_mmu *mmu);
void appine_cpu_tlb_flush(struct appine_cpu *cpu);

/* 从 cpu->pc 开始执行，直到需要内核层介入 */
enum appine_exit appine_cpu_run(struct appine_cpu *cpu);

/* 在 cpu->pc 处执行给定的一条指令（不经取指），供 lockstep 测试使用 */
enum appine_exit appine_cpu_exec_insn(struct appine_cpu *cpu, uint32_t insn);

/* 可从任意线程调用：让 appine_cpu_run() 在下一个指令边界返回 APPINE_EXIT_KICK */
void appine_cpu_kick(struct appine_cpu *cpu);

/* ---- 宿主侧的故障兜底（设计文档 §7.4、§12）---- */

/* 宿主收到的故障信号（APPINE_EXIT_HOST_FAULT） */
struct appine_host_fault {
    int32_t signo;
    int32_t code;  /* si_code */
    uint64_t addr; /* si_addr：宿主地址 */
    uint64_t pc;   /* 出错的宿主指令 */
};

/*
 * 装上宿主的 SIGSEGV、SIGBUS、SIGILL 处理函数，可以重复调用。之后 appine_cpu_run()、
 * appine_dbt_run() 运行期间的这些故障变成 APPINE_EXIT_HOST_FAULT；其余的故障先往 stderr
 * 写一行报告，再交给原来的处理方式（Android 上 debuggerd 据此生成 tombstone）。
 */
void appine_fault_guard_install(void);

/* 当前线程最近一次 APPINE_EXIT_HOST_FAULT 的详情 */
void appine_host_fault_last(struct appine_host_fault *out);

/* 校验 Rust 侧镜像的结构体布局，一致时返回 1 */
int appine_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache,
                            size_t off_tlb, size_t off_v);

/* ---- Tier 1：同 ISA 直通式 DBT（设计文档 §4.2）---- */

struct appine_dbt;

/* 翻译与运行统计 */
struct appine_dbt_stats {
    uint64_t blocks;         /* 翻译的块数 */
    uint64_t insns;          /* 翻译的 guest 指令数 */
    uint64_t insns_copied;   /* 原样拷贝 */
    uint64_t insns_remapped; /* 涉及被占用寄存器或 SP，改写后执行 */
    uint64_t insns_memory;   /* 改写成 TLB 快速路径的访存指令 */
    uint64_t insns_fallback; /* 交给 Tier 0 执行的指令（静态计数） */
    uint64_t exits_chain;    /* 运行时退出到分发器的次数，按原因分类 */
    uint64_t exits_lookup;
    uint64_t exits_interp;
    uint64_t exits_svc;
    uint64_t exits_kick;
    uint64_t flushes;
    uint64_t invalidated;   /* 被 appine_dbt_invalidate() 作废的块 */
    uint64_t blocks_shared; /* blocks 中直接拷贝翻译模板得到的块 */
    uint64_t insns_grouped; /* insns_memory 中和前面的访存共用一次 TLB 检查的 */
    uint64_t ungrouped;     /* 共用的检查没通过、按不分组重新翻译的次数 */
};

/*
 * 一个地址空间的 Tier 1 翻译（代码缓存、翻译表）：这个地址空间里的所有线程共用，每个
 * 线程用 appine_dbt_attach() 挂上去得到自己的部分（跳转缓存、影子返回栈、统计），再用
 * 它运行。宿主不是 aarch64 Linux 时返回 NULL。appine_dbt_free() 在所有线程都解挂之后
 * 调用；实例放回池里，翻译不多时保留下来，下一个使用者运行之前逐块核对。`key` 区分程序
 * （0 表示不区分）：appine_dbt_new() 优先拿回同一个键留下的翻译。
 *
 * 翻译结果不会自己失效：guest 用 ic ivau 声明修改了代码（APPINE_EXIT_CODE），
 * 或者可执行的映射被拆除、改了权限之后，调用方要调用 appine_dbt_invalidate() 或
 * appine_dbt_flush()（对地址空间调用一次即可，经由任意一个挂着的线程）。
 */
struct appine_dbt *appine_dbt_new(uint64_t key);
void appine_dbt_free(struct appine_dbt *dbt, uint64_t key);
/* 释放池里保留的翻译（宿主内存紧张时） */
void appine_dbt_trim(void);

/*
 * 翻译所在的一段映射：guest 地址范围，和调用方给出的、能断定内容相同的身份（文件和它的
 * 版本）。身份相同的两段映射内容一定相同，由调用方保证。
 */
struct appine_dbt_source {
    uint64_t start, len;
    uint64_t id[5];
};

/* 地址空间不再使用之前（翻译要放回池里）：记下翻译所在的映射，最多 APPINE_DBT_SOURCES 段 */
#define APPINE_DBT_SOURCES 16
void appine_dbt_set_sources(struct appine_dbt *dbt, const struct appine_dbt_source *s, size_t n);
/*
 * 从池里拿到的实例在新的地址空间第一次运行之前：新地址空间里与记下的完全相同的映射里的块
 * 不必逐块核对（execve 时程序和动态链接器的代码段，核对它们要把每一页都读一遍）
 */
void appine_dbt_trust(struct appine_dbt *dbt, const struct appine_dbt_source *s, size_t n);

struct appine_dbt_thread;
struct appine_dbt_thread *appine_dbt_attach(struct appine_dbt *dbt);
void appine_dbt_detach(struct appine_dbt_thread *t);

/* 丢弃全部翻译：要等所有线程离开翻译后的代码，所以只是请求，由下一次进入 appine_dbt_run()
 * 的线程完成 */
void appine_dbt_flush(struct appine_dbt_thread *t);

/* 要换一个地址空间运行（exec 之后）：保留翻译，下次运行之前逐块核对 guest 指令字，
 * 对不上的作废。从池里拿到的实例也是这样处理的。 */
void appine_dbt_recheck(struct appine_dbt_thread *t);

/* 作废 [start, start + len) 覆盖的各页里开始的块；范围很大时等同于 appine_dbt_flush() */
void appine_dbt_invalidate(struct appine_dbt_thread *t, uint64_t start, uint64_t len);

/* 与 appine_cpu_run() 语义相同，但在翻译后的代码里执行 */
enum appine_exit appine_dbt_run(struct appine_dbt_thread *t, struct appine_cpu *cpu);
/* 这个线程的统计：它退出的次数，以及它做的翻译、作废 */
void appine_dbt_get_stats(const struct appine_dbt_thread *t, struct appine_dbt_stats *out);

#endif /* APPINE_ENGINE_H */
