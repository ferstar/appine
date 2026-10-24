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
#define APPINE_TLB_ENTRIES 512

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
    APPINE_FAULT_PAGE = 1,  /* 未映射或权限不符，对应 SIGSEGV */
    APPINE_FAULT_ALIGN = 2, /* PC 或 SP 未对齐，对应 SIGBUS */
};

/* appine_cpu_run() 的退出原因 */
enum appine_exit {
    APPINE_EXIT_NONE = 0,  /* 仅 appine_cpu_exec_insn() 使用：指令正常完成 */
    APPINE_EXIT_SVC = 1,   /* pc 已指向下一条指令，exit_info = imm16 */
    APPINE_EXIT_UNDEF = 2, /* pc 指向该指令，exit_info = 指令编码 */
    APPINE_EXIT_BRK = 3,   /* pc 指向该指令，exit_info = imm16 */
    APPINE_EXIT_FAULT = 4, /* pc 指向出错指令，fault_* 字段有效，指令没有产生副作用 */
    APPINE_EXIT_KICK = 5,  /* 被 appine_cpu_kick() 打断，pending 已清零 */
};

/*
 * 数据 TLB 表项：host 地址 = guest 地址 + addend。tag 和 addend 相邻存放，
 * Tier 1 生成的代码用一条 ldp 同时取出两者。
 */
struct appine_tlb_entry {
    uint64_t tag_read; /* guest 页号；UINT64_MAX 表示无效 */
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
    /* 数据 TLB 必须位于偏移 0：Tier 1 生成的代码以 "cpu + 下标 * 32" 直接寻址表项 */
    struct appine_tlb_entry tlb[APPINE_TLB_ENTRIES];
    struct appine_itlb_entry itlb[APPINE_TLB_ENTRIES];
    uint64_t x[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t nzcv; /* 只使用 bit 31..28 */
    uint64_t fpcr;
    uint64_t fpsr;
    uint64_t v[32][2]; /* V0–V31，低 64 位在前 */
    uint64_t tpidr_el0;
    uint64_t orig_x0; /* syscall 重启时恢复 x0（设计文档 §7.3），由内核层维护 */
    uint64_t fault_addr;
    uint32_t fault_access; /* enum appine_access */
    uint32_t fault_kind;   /* enum appine_fault_kind */
    uint32_t exit_info;
    uint32_t reserved;
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

/* 清零全部状态并绑定 mmu（可为 NULL，此时所有访存都会失败） */
void appine_cpu_init(struct appine_cpu *cpu, struct appine_mmu *mmu);
void appine_cpu_tlb_flush(struct appine_cpu *cpu);

/* 从 cpu->pc 开始执行，直到需要内核层介入 */
enum appine_exit appine_cpu_run(struct appine_cpu *cpu);

/* 在 cpu->pc 处执行给定的一条指令（不经取指），供 lockstep 测试使用 */
enum appine_exit appine_cpu_exec_insn(struct appine_cpu *cpu, uint32_t insn);

/* 可从任意线程调用：让 appine_cpu_run() 在下一个指令边界返回 APPINE_EXIT_KICK */
void appine_cpu_kick(struct appine_cpu *cpu);

/* 校验 Rust 侧镜像的结构体布局，一致时返回 1 */
int appine_cpu_layout_check(size_t size, size_t off_x, size_t off_pc, size_t off_tpidr,
                            size_t off_pending, size_t off_mmu, size_t off_jump_cache);

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
};

/*
 * 创建一个 Tier 1 实例（代码缓存、翻译表、跳转缓存）。一个实例只能由一个
 * 线程使用。宿主不是 aarch64 Linux 时返回 NULL。
 *
 * 原型限制：翻译结果不会因为 guest 修改代码或映射而失效，调用方必须在
 * 这种情况下调用 appine_dbt_flush()。
 */
struct appine_dbt *appine_dbt_new(void);
void appine_dbt_free(struct appine_dbt *dbt);
void appine_dbt_flush(struct appine_dbt *dbt);

/* 与 appine_cpu_run() 语义相同，但在翻译后的代码里执行 */
enum appine_exit appine_dbt_run(struct appine_dbt *dbt, struct appine_cpu *cpu);
void appine_dbt_get_stats(const struct appine_dbt *dbt, struct appine_dbt_stats *out);

#endif /* APPINE_ENGINE_H */
