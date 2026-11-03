/*
 * 宿主侧的故障兜底（engine 的 map、设计文档 §12）：引擎自己的 bug（翻译错的代码、坏掉的页表项）
 * 让宿主收到 SIGSEGV、SIGBUS 或 SIGILL 时，只结束当时在运行的 guest 进程，不让整个 App
 * 崩溃。
 *
 * appine_cpu_run()、appine_dbt_run() 经由 appine_guard_run() 运行，这段时间是"上了保险"
 * 的：故障发生在这期间时，处理函数 siglongjmp 回 appine_guard_run()，由它返回
 * APPINE_EXIT_HOST_FAULT。所以这段时间里引擎不回调内核层，不调用 malloc（APPINE_PERF_MAP
 * 剖析时写 perf map 文件除外），持有的锁都用 appine_guard_hold() 登记，跳回之后释放。
 *
 * 其余的故障（内核层自己的、别的宿主线程的）先往 stderr 写一行报告，再交给原来的处理
 * 函数（Android 上是 debuggerd，生成 tombstone）或者默认动作。
 */
#define _GNU_SOURCE /* -std=c17 下 sigjmp_buf、siginfo_t、ucontext_t 需要它 */
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdlib.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#include <ucontext.h>
#endif

#include "internal.h"

#define MAX_LOCKS 4

/* 每个线程一个，第一次运行 guest 代码时分配，线程退出时释放 */
struct guard {
    sigjmp_buf env;
    volatile sig_atomic_t armed; /* 处理函数在同一个线程上读它 */
    uint64_t fpcr;               /* 上保险时宿主的 FPCR：翻译后的代码里装的是 guest 的 */
    unsigned nlocks;
    struct {
        void *lock;
        bool rw;
    } locks[MAX_LOCKS];
    struct appine_host_fault fault;
};

static const int signals[] = {SIGSEGV, SIGBUS, SIGILL};
#define NSIGNALS (sizeof(signals) / sizeof(signals[0]))

static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_key_t key;
static bool installed;
static struct sigaction previous[NSIGNALS];

static uint64_t read_fpcr(void)
{
    uint64_t v = 0;
#ifdef __aarch64__
    __asm__ volatile("mrs %0, fpcr" : "=r"(v));
#endif
    return v;
}

static void write_fpcr(uint64_t v)
{
#ifdef __aarch64__
    __asm__ volatile("msr fpcr, %0" ::"r"(v));
#else
    (void)v;
#endif
}

static uint64_t context_pc(void *uc)
{
#if defined(__aarch64__) && defined(__linux__)
    return ((ucontext_t *)uc)->uc_mcontext.pc;
#else
    (void)uc;
    return 0;
#endif
}

/* ---- 信号处理函数里只能用异步信号安全的函数：报告自己拼 ---- */

struct line {
    char buf[192];
    size_t n;
};

static void put_str(struct line *l, const char *s)
{
    while (*s && l->n < sizeof(l->buf))
        l->buf[l->n++] = *s++;
}

static void put_num(struct line *l, uint64_t v, unsigned base)
{
    char digits[20];
    unsigned n = 0;
    if (base == 16)
        put_str(l, "0x");
    do {
        digits[n++] = "0123456789abcdef"[v % base];
        v /= base;
    } while (v);
    while (n && l->n < sizeof(l->buf))
        l->buf[l->n++] = digits[--n];
}

static void report(int sig, const siginfo_t *info, void *uc)
{
    struct line l = {.n = 0};
    char name[17] = "?";
#ifdef __linux__
    prctl(PR_GET_NAME, name, 0, 0, 0);
    name[16] = 0;
#endif
    put_str(&l, "appine: host signal ");
    put_num(&l, (uint64_t)sig, 10);
    put_str(&l, " (code ");
    if (info->si_code < 0)
        put_str(&l, "-");
    put_num(&l, (uint64_t)(info->si_code < 0 ? -(int64_t)info->si_code : info->si_code), 10);
    put_str(&l, ") in thread ");
    put_str(&l, name);
    put_str(&l, " at pc ");
    put_num(&l, context_pc(uc), 16);
    put_str(&l, ", addr ");
    put_num(&l, (uint64_t)(uintptr_t)info->si_addr, 16);
    put_str(&l, "\n");
    ssize_t r = write(2, l.buf, l.n);
    (void)r;
}

/* 交给装上兜底之前的处理方式 */
static void chain(int sig, siginfo_t *info, void *uc)
{
    const struct sigaction *old = &previous[0];
    for (size_t i = 0; i < NSIGNALS; i++) {
        if (signals[i] == sig)
            old = &previous[i];
    }
    if (old->sa_handler != SIG_DFL && old->sa_handler != SIG_IGN) {
        if (old->sa_flags & SA_SIGINFO)
            old->sa_sigaction(sig, info, uc);
        else
            old->sa_handler(sig);
        return;
    }
    /* 默认动作（同步故障被忽略也按默认处理）：恢复默认的处理方式后返回，出错的指令
     * 重新执行时由宿主内核结束进程。别人用 kill 发来的不会重新产生，自己再发一次 */
    struct sigaction dfl = {.sa_flags = 0};
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    sigaction(sig, &dfl, NULL);
    if (info->si_code <= 0)
        raise(sig);
}

static void handler(int sig, siginfo_t *info, void *uc)
{
    struct guard *g = pthread_getspecific(key);
    /* si_code > 0：宿主内核因为这个线程的这条指令产生的，不是别人发来的 */
    if (g && g->armed && info->si_code > 0) {
        g->armed = 0;
        g->fault = (struct appine_host_fault){
            .signo = sig,
            .code = info->si_code,
            .addr = (uint64_t)(uintptr_t)info->si_addr,
            .pc = context_pc(uc),
        };
        siglongjmp(g->env, 1);
    }
    report(sig, info, uc);
    chain(sig, info, uc);
}

static void install(void)
{
    if (pthread_key_create(&key, free) != 0)
        return;
    struct sigaction sa = {.sa_flags = SA_SIGINFO | SA_ONSTACK};
    sa.sa_sigaction = handler;
    sigemptyset(&sa.sa_mask);
    for (size_t i = 0; i < NSIGNALS; i++)
        sigaction(signals[i], &sa, &previous[i]);
    __atomic_store_n(&installed, true, __ATOMIC_RELEASE);
}

void appine_fault_guard_install(void)
{
    pthread_once(&once, install);
}

/* 当前线程的 guard；没有装上兜底时返回 NULL */
static struct guard *current(void)
{
    if (!__atomic_load_n(&installed, __ATOMIC_ACQUIRE))
        return NULL;
    return pthread_getspecific(key);
}

void appine_guard_hold(void *lock, bool rw)
{
    struct guard *g = current();
    if (g && g->armed && g->nlocks < MAX_LOCKS) {
        g->locks[g->nlocks].lock = lock;
        g->locks[g->nlocks].rw = rw;
        g->nlocks++;
    }
}

void appine_guard_drop(void)
{
    struct guard *g = current();
    if (g && g->armed && g->nlocks)
        g->nlocks--;
}

/* 从处理函数跳回来之后：恢复宿主的 FPCR，释放还持有的锁，解除对这个信号的屏蔽（处理
 * 函数运行期间它被屏蔽，siglongjmp 不恢复屏蔽字） */
static enum appine_exit recover(struct guard *g)
{
    write_fpcr(g->fpcr);
    while (g->nlocks) {
        g->nlocks--;
        if (g->locks[g->nlocks].rw)
            pthread_rwlock_unlock(g->locks[g->nlocks].lock);
        else
            pthread_mutex_unlock(g->locks[g->nlocks].lock);
    }
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, g->fault.signo);
    pthread_sigmask(SIG_UNBLOCK, &set, NULL);
    return APPINE_EXIT_HOST_FAULT;
}

/* 当前线程的 guard，第一次用时分配；没有装上兜底时返回 NULL */
static struct guard *current_or_new(void)
{
    struct guard *g = current();
    if (!g && __atomic_load_n(&installed, __ATOMIC_ACQUIRE)) {
        g = calloc(1, sizeof(*g));
        if (g && pthread_setspecific(key, g) != 0) {
            free(g);
            g = NULL;
        }
    }
    return g;
}

enum appine_exit appine_guard_run(enum appine_exit (*fn)(void *, struct appine_cpu *), void *ctx,
                                  struct appine_cpu *cpu)
{
    /* siglongjmp 回来之后还要用它：放在内存里，不让它只活在会被破坏的寄存器里 */
    struct guard *volatile g = current_or_new();
    if (!g)
        return fn(ctx, cpu);
    if (sigsetjmp(g->env, 0))
        return recover(g);
    g->fpcr = read_fpcr();
    g->nlocks = 0;
    g->armed = 1;
    enum appine_exit r = fn(ctx, cpu);
    g->armed = 0;
    return r;
}

void appine_host_fault_last(struct appine_host_fault *out)
{
    struct guard *g = current();
    *out = g ? g->fault : (struct appine_host_fault){0};
}
