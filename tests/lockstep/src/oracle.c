/*
 * lockstep 的原生执行端：在宿主 CPU 上执行被测指令（最多 4 条），作为解释器的
 * 标准答案（设计文档 §13.3）。只支持 Linux aarch64 宿主。
 *
 * 被测指令如果在硬件上触发信号（多数是 SIGILL），信号处理函数会 siglongjmp
 * 回 lockstep_oracle_run()，并把信号编号作为返回值。访存类的被测指令只能访问
 * lockstep_oracle_data() 返回的两页数据区。
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

struct lockstep_state {
    uint64_t x[31];
    uint64_t nzcv;
    uint64_t v[32][2];
    uint64_t fpcr;
    uint64_t fpsr;
};

#define SLOTS 4
#define DATA_BYTES 8192

_Static_assert(sizeof(struct lockstep_state) == 784, "布局要与 oracle.S 一致");

extern const uint8_t lockstep_tramp_start[], lockstep_tramp_slot[], lockstep_tramp_end[];

struct lockstep_oracle {
    uint8_t *page;
    size_t size;
    size_t slot_offset;
    uint8_t *data;
};

static const int caught_signals[] = {SIGILL, SIGSEGV, SIGBUS, SIGTRAP, SIGFPE};
#define NSIGNALS (sizeof(caught_signals) / sizeof(caught_signals[0]))

static struct sigaction previous_actions[NSIGNALS];
static pthread_once_t install_once = PTHREAD_ONCE_INIT;
static __thread sigjmp_buf *active_jmp;
static __thread volatile sig_atomic_t caught;

static void on_signal(int sig, siginfo_t *info, void *ucontext)
{
    (void)info;
    (void)ucontext;
    if (active_jmp) {
        caught = sig;
        siglongjmp(*active_jmp, 1);
    }
    /* 不是被测指令触发的：恢复原来的处理方式，返回后出错指令会再次触发它 */
    for (size_t i = 0; i < NSIGNALS; i++) {
        if (caught_signals[i] == sig)
            sigaction(sig, &previous_actions[i], NULL);
    }
}

static void install_handlers(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_signal;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    for (size_t i = 0; i < NSIGNALS; i++)
        sigaction(caught_signals[i], &sa, &previous_actions[i]);
}

struct lockstep_oracle *lockstep_oracle_new(void)
{
    pthread_once(&install_once, install_handlers);

    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    size_t code_size = (size_t)(lockstep_tramp_end - lockstep_tramp_start);
    size_t size = (code_size + page_size - 1) & ~(page_size - 1);
    void *page = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED)
        return NULL;
    memcpy(page, lockstep_tramp_start, code_size);

    void *data = mmap(NULL, DATA_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct lockstep_oracle *oracle = malloc(sizeof(*oracle));
    if (!oracle || data == MAP_FAILED) {
        munmap(page, size);
        if (data != MAP_FAILED)
            munmap(data, DATA_BYTES);
        free(oracle);
        return NULL;
    }
    oracle->page = page;
    oracle->size = size;
    oracle->slot_offset = (size_t)(lockstep_tramp_slot - lockstep_tramp_start);
    oracle->data = data;
    return oracle;
}

void lockstep_oracle_free(struct lockstep_oracle *oracle)
{
    if (!oracle)
        return;
    munmap(oracle->page, oracle->size);
    munmap(oracle->data, DATA_BYTES);
    free(oracle);
}

uint64_t lockstep_oracle_slot(const struct lockstep_oracle *oracle)
{
    return (uint64_t)(uintptr_t)(oracle->page + oracle->slot_offset);
}

/* 跳板所在页的内容（只读视图），供解释器一侧复制，以便 LDR (literal) 读到相同的字节 */
const uint8_t *lockstep_oracle_page(const struct lockstep_oracle *oracle)
{
    return oracle->page;
}

uint8_t *lockstep_oracle_data(const struct lockstep_oracle *oracle)
{
    return oracle->data;
}

/* 执行 n 条指令（不足 4 条的用 NOP 补齐）。返回 0 表示正常执行，正数是触发的
 * 信号编号，-1 表示参数错误或宿主调用失败 */
int lockstep_oracle_run(struct lockstep_oracle *oracle, const uint32_t *insns, unsigned n,
                        struct lockstep_state *st)
{
    if (n > SLOTS || mprotect(oracle->page, oracle->size, PROT_READ | PROT_WRITE))
        return -1;
    for (unsigned i = 0; i < SLOTS; i++) {
        uint32_t insn = i < n ? insns[i] : 0xd503201fu; /* nop */
        memcpy(oracle->page + oracle->slot_offset + 4 * i, &insn, sizeof(insn));
    }
    if (mprotect(oracle->page, oracle->size, PROT_READ | PROT_EXEC))
        return -1;
    __builtin___clear_cache((char *)oracle->page, (char *)oracle->page + oracle->size);

    void (*entry)(struct lockstep_state *) =
        (void (*)(struct lockstep_state *))(uintptr_t)oracle->page;
    sigjmp_buf jmp;
    caught = 0;
    if (sigsetjmp(jmp, 1) == 0) {
        active_jmp = &jmp;
        entry(st);
        active_jmp = NULL;
        return 0;
    }
    active_jmp = NULL;
    return caught;
}
