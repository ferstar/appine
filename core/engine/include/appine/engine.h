/*
 * appine 引擎的对外接口（设计文档 §4、§5、§13.1）。
 *
 * 引擎只负责执行 guest 指令和 soft-MMU 快速路径。遇到 svc、访存失败、未定义
 * 指令、brk 时，appine_cpu_run() 返回一个退出原因，由内核层（Rust）处理后
 * 再次调用 appine_cpu_run() 继续执行。
 *
 * 线程模型：一个 struct appine_cpu 只能由一个线程运行；除 appine_cpu_kick()
 * 外，其余函数都不是线程安全的。修改 mmu 映射后，调用方必须对所有使用该 mmu
 * 的 cpu 调用 appine_cpu_tlb_flush()。
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

/* 软件 TLB 表项：host 地址 = guest 地址 + addend */
struct appine_tlb_entry {
    uint64_t tag_read; /* guest 页号；UINT64_MAX 表示无效 */
    uint64_t tag_write;
    uint64_t tag_exec;
    uint64_t addend;
};

struct appine_mmu;

/*
 * guest CPU 状态。字段布局由 Rust 侧逐字段镜像（core/engine/src/lib.rs），
 * 修改时必须同步，并由 appine_cpu_layout_check() 校验。
 */
struct appine_cpu {
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
    uint32_t pending; /* 只能用原子操作访问 */
    struct appine_mmu *mmu;
    struct appine_tlb_entry tlb[APPINE_TLB_ENTRIES];
};

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
int appine_cpu_layout_check(size_t size, size_t off_pc, size_t off_tpidr, size_t off_pending,
                            size_t off_mmu, size_t off_tlb);

#endif /* APPINE_ENGINE_H */
