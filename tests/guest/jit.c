/*
 * 自修改代码的 guest 测试程序（M7）：像 JIT 一样往内存里写指令再执行，检查改写并执行
 * ic ivau 之后，Tier 1 不会继续执行旧的翻译。用 musl-gcc -static 编译，在 appine-run
 * 里直接运行。每项检查通过时打印一行 "ok <名字>"，失败时打印原因并以 1 退出，全部
 * 通过时最后打印 "all ok"。
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf("\n");                                                                          \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

#define PAGE 4096

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

typedef int (*fn_t)(void);

/* w0 = n * k：一个带回边的循环，翻译成几个互相链接的块 */
static void emit_loop(uint32_t *code, unsigned n, unsigned k)
{
    code[0] = 0x52800000u;                       /* mov w0, #0 */
    code[1] = 0x52800001u | n << 5;              /* mov w1, #n */
    code[2] = 0x11000000u | k << 10;             /* add w0, w0, #k */
    code[3] = 0x71000421u;                       /* subs w1, w1, #1 */
    code[4] = 0x54000001u | (0x7fffeu << 5);     /* b.ne 2 */
    code[5] = 0xd65f03c0u;                       /* ret */
}

static void publish(uint32_t *code, unsigned words)
{
    __builtin___clear_cache((char *)code, (char *)(code + words));
}

/* ---- CTR_EL0：修改代码之后必须执行 ic ivau ---- */

static void test_ctr(void)
{
    uint64_t ctr;
    __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
    CHECK(!(ctr >> 29 & 1), "CTR_EL0.DIC = 1: %#lx", (unsigned long)ctr);
    CHECK((ctr & 15) >= 2 && (ctr >> 16 & 15) >= 2, "cache lines too small: %#lx", (unsigned long)ctr);
    ok("ctr_el0");
}

/* ---- 同一页反复改写：每次都执行新代码 ---- */

static void test_rewrite(void)
{
    uint32_t *code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(code != MAP_FAILED, "mmap rwx");
    fn_t f = (fn_t)(uintptr_t)code;
    for (unsigned k = 1; k <= 200; k++) {
        emit_loop(code, 50, k);
        publish(code, 6);
        for (int i = 0; i < 20; i++)
            CHECK(f() == (int)(50 * k), "k = %u: got %d", k, f());
    }
    /* 只改写循环体里的一条指令 */
    emit_loop(code, 10, 3);
    publish(code, 6);
    CHECK(f() == 30, "before patch: %d", f());
    code[2] = 0x11000000u | 7 << 10;
    publish(code + 2, 1);
    CHECK(f() == 70, "after patch: %d", f());
    munmap(code, PAGE);
    ok("rewrite");
}

/* ---- W^X：用 mprotect 在可写和可执行之间切换 ---- */

static void test_wx_mprotect(void)
{
    uint32_t *code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(code != MAP_FAILED, "mmap rw");
    fn_t f = (fn_t)(uintptr_t)code;
    for (unsigned k = 1; k <= 50; k++) {
        CHECK(mprotect(code, PAGE, PROT_READ | PROT_WRITE) == 0, "mprotect rw");
        emit_loop(code, 7, k);
        CHECK(mprotect(code, PAGE, PROT_READ | PROT_EXEC) == 0, "mprotect rx");
        publish(code, 6);
        CHECK(f() == (int)(7 * k), "k = %u: got %d", k, f());
    }
    munmap(code, PAGE);
    ok("wx_mprotect");
}

/* ---- 双映射：同一个 memfd 映射成可写和可执行两份，在可写的一份里改写 ---- */

static void test_dual_mapping(void)
{
    int fd = memfd_create("jit", 0);
    if (fd < 0 && errno == ENOENT) { /* 不带 rootfs 运行时没有文件系统 */
        printf("skip dual_mapping\n");
        return;
    }
    CHECK(fd >= 0, "memfd_create");
    CHECK(ftruncate(fd, PAGE) == 0, "ftruncate");
    uint32_t *rw = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    uint32_t *rx = mmap(NULL, PAGE, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
    CHECK(rw != MAP_FAILED && rx != MAP_FAILED && rw != rx, "mmap views");
    fn_t f = (fn_t)(uintptr_t)rx;
    for (unsigned k = 1; k <= 50; k++) {
        emit_loop(rw, 9, k);
        publish(rx, 6);
        CHECK(f() == (int)(9 * k), "k = %u: got %d", k, f());
    }
    munmap(rw, PAGE);
    munmap(rx, PAGE);
    close(fd);
    ok("dual_mapping");
}

/* ---- 另一个线程执行过的代码被改写 ---- */

/*
 * 两个槽轮流使用：第 k 轮改写第 k % 2 个槽，运行方这时只调用另一个槽，所以不会并发
 * 地执行正在被改写的指令；但它在第 k - 2 轮执行过这个槽，Tier 1 里留着旧的翻译。
 */
static uint32_t *shared_code;
static atomic_int phase, done;

static fn_t slot(int k)
{
    return (fn_t)(uintptr_t)(shared_code + (k % 2) * 64);
}

static void *runner(void *arg)
{
    (void)arg;
    for (int k = 1; k <= 20; k++) {
        while (atomic_load(&phase) < k)
            CHECK(slot(k - 1)() == 5 * (k - 1), "round %d: old slot", k);
        /* 改写方发布新代码之后才推进 phase：看到 k 就必须执行新代码 */
        int r = slot(k)();
        CHECK(r == 5 * k, "round %d: got %d", k, r);
        atomic_store(&done, k);
    }
    return NULL;
}

static void test_cross_thread(void)
{
    shared_code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(shared_code != MAP_FAILED, "mmap rwx");
    emit_loop((uint32_t *)(uintptr_t)slot(0), 5, 0);
    publish(shared_code, 128);
    pthread_t t;
    CHECK(pthread_create(&t, NULL, runner, NULL) == 0, "pthread_create");
    for (int k = 1; k <= 20; k++) {
        while (atomic_load(&done) < k - 1)
            usleep(100);
        usleep(1000);
        uint32_t *code = (uint32_t *)(uintptr_t)slot(k);
        emit_loop(code, 5, (unsigned)k);
        publish(code, 6);
        atomic_store(&phase, k);
    }
    pthread_join(t, NULL);
    munmap(shared_code, PAGE);
    ok("cross_thread");
}

/* ---- 对未映射的地址做缓存维护：SIGSEGV ---- */

static sigjmp_buf jump;
static void *fault_addr;

static void on_segv(int sig, siginfo_t *info, void *uc)
{
    (void)sig;
    (void)uc;
    fault_addr = info->si_addr;
    siglongjmp(jump, 1);
}

static void test_unmapped(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGSEGV, &sa, NULL) == 0, "sigaction");
    char *p = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED, "mmap");
    munmap(p, PAGE);
    if (sigsetjmp(jump, 1) == 0) {
        __asm__ volatile("ic ivau, %0" ::"r"(p + 64) : "memory");
        CHECK(0, "ic ivau on an unmapped page did not fault");
    }
    CHECK(fault_addr == p + 64, "ic ivau fault address %p, expected %p", fault_addr, p + 64);
    if (sigsetjmp(jump, 1) == 0) {
        __asm__ volatile("dc cvau, %0" ::"r"(p + 128) : "memory");
        CHECK(0, "dc cvau on an unmapped page did not fault");
    }
    CHECK(fault_addr == p + 128, "dc cvau fault address %p, expected %p", fault_addr, p + 128);
    signal(SIGSEGV, SIG_DFL);
    ok("unmapped");
}

int main(void)
{
    test_ctr();
    test_rewrite();
    test_wx_mprotect();
    test_dual_mapping();
    test_cross_thread();
    test_unmapped();
    printf("all ok\n");
    return 0;
}
