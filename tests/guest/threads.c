/*
 * 多线程与信号的 guest 测试程序（M3a）：用 musl-gcc -static 编译，在 appine-run 里
 * 直接运行（不需要 rootfs）。每项检查通过时打印一行 "ok <名字>"，失败时打印原因并
 * 以 1 退出，全部通过时最后打印 "all ok"。
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
#include <sys/wait.h>
#include <time.h>
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

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

static void sleep_ms(long ms)
{
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

static void handle(int sig, void (*fn)(int), int flags)
{
    struct sigaction sa = {0};
    sa.sa_handler = fn;
    sa.sa_flags = flags;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(sig, &sa, NULL) == 0, "sigaction %d", sig);
}

/* ---- 互斥锁、原子操作、线程局部存储 ---- */

#define THREADS 8
#define ROUNDS 4000

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static atomic_long atomic_counter;
static __thread int tls_value;

static void *count_worker(void *arg)
{
    tls_value = (int)(intptr_t)arg;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_mutex_lock(&mutex);
        counter++;
        pthread_mutex_unlock(&mutex);
        atomic_fetch_add(&atomic_counter, 1);
        if (i % 500 == 0)
            sched_yield();
    }
    return (void *)(intptr_t)(tls_value * 10);
}

static void test_counters(void)
{
    pthread_t t[THREADS];
    for (int i = 0; i < THREADS; i++)
        CHECK(pthread_create(&t[i], NULL, count_worker, (void *)(intptr_t)(i + 1)) == 0, "create");
    for (int i = 0; i < THREADS; i++) {
        void *ret;
        CHECK(pthread_join(t[i], &ret) == 0, "join");
        CHECK((intptr_t)ret == (i + 1) * 10, "thread %d returned %ld", i, (long)(intptr_t)ret);
    }
    CHECK(counter == THREADS * ROUNDS, "mutex counter %ld", counter);
    CHECK(atomic_counter == THREADS * ROUNDS, "atomic counter %ld", (long)atomic_counter);
    CHECK(tls_value == 0, "main thread tls %d", tls_value);
    ok("mutex-atomic-tls");
}

/* ---- 条件变量：生产者和消费者 ---- */

#define ITEMS 2000
#define QUEUE 8

static pthread_mutex_t qmutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER;
static int queue[QUEUE];
static int q_head, q_len;

static void *producer(void *arg)
{
    (void)arg;
    for (int i = 1; i <= ITEMS; i++) {
        pthread_mutex_lock(&qmutex);
        while (q_len == QUEUE)
            pthread_cond_wait(&not_full, &qmutex);
        queue[(q_head + q_len) % QUEUE] = i;
        q_len++;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&qmutex);
    }
    return NULL;
}

static void *consumer(void *arg)
{
    (void)arg;
    long sum = 0;
    for (int i = 0; i < ITEMS; i++) {
        pthread_mutex_lock(&qmutex);
        while (q_len == 0)
            pthread_cond_wait(&not_empty, &qmutex);
        sum += queue[q_head];
        q_head = (q_head + 1) % QUEUE;
        q_len--;
        pthread_cond_signal(&not_full);
        pthread_mutex_unlock(&qmutex);
    }
    return (void *)sum;
}

static void test_condvar(void)
{
    pthread_t p, c;
    void *sum;
    CHECK(pthread_create(&c, NULL, consumer, NULL) == 0, "create consumer");
    CHECK(pthread_create(&p, NULL, producer, NULL) == 0, "create producer");
    pthread_join(p, NULL);
    pthread_join(c, &sum);
    CHECK((long)sum == (long)ITEMS * (ITEMS + 1) / 2, "sum %ld", (long)sum);

    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_nsec += 50 * 1000000;
    if (ts.tv_nsec >= 1000000000) {
        ts.tv_sec++;
        ts.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&qmutex);
    int rc = pthread_cond_timedwait(&not_empty, &qmutex, &ts);
    pthread_mutex_unlock(&qmutex);
    CHECK(rc == ETIMEDOUT, "timedwait returned %d", rc);
    ok("condvar");
}

/* ---- 发给指定线程的信号 ---- */

static pthread_t handled_by;
static atomic_int usr1_count;

static void on_usr1(int sig)
{
    (void)sig;
    handled_by = pthread_self();
    atomic_fetch_add(&usr1_count, 1);
}

static void *wait_for_usr1(void *arg)
{
    (void)arg;
    while (atomic_load(&usr1_count) == 0)
        sleep_ms(1);
    return NULL;
}

static void test_thread_signal(void)
{
    handle(SIGUSR1, on_usr1, 0);
    pthread_t t;
    CHECK(pthread_create(&t, NULL, wait_for_usr1, NULL) == 0, "create");
    sleep_ms(10);
    CHECK(pthread_kill(t, SIGUSR1) == 0, "pthread_kill");
    pthread_join(t, NULL);
    CHECK(pthread_equal(handled_by, t), "SIGUSR1 handled by the wrong thread");
    ok("thread-signal");
}

/* ---- 备用信号栈 ---- */

static char *alt_base;
static size_t alt_size;
static volatile int on_alt, reported_onstack;

static void on_usr2(int sig)
{
    (void)sig;
    char local;
    on_alt = &local >= alt_base && &local < alt_base + alt_size;
    stack_t cur;
    sigaltstack(NULL, &cur);
    reported_onstack = (cur.ss_flags & SS_ONSTACK) != 0;
}

static void test_altstack(void)
{
    alt_size = 64 * 1024;
    alt_base = malloc(alt_size);
    stack_t ss = {.ss_sp = alt_base, .ss_size = alt_size, .ss_flags = 0};
    CHECK(sigaltstack(&ss, NULL) == 0, "sigaltstack");
    stack_t small = {.ss_sp = alt_base, .ss_size = 1024, .ss_flags = 0};
    CHECK(sigaltstack(&small, NULL) == -1 && errno == ENOMEM, "small altstack accepted");
    handle(SIGUSR2, on_usr2, SA_ONSTACK);
    raise(SIGUSR2);
    CHECK(on_alt, "handler did not run on the alternate stack");
    CHECK(reported_onstack, "SS_ONSTACK not reported");
    stack_t cur;
    sigaltstack(NULL, &cur);
    CHECK(!(cur.ss_flags & SS_ONSTACK) && cur.ss_sp == alt_base, "altstack after return");
    ok("altstack");
}

/* ---- 访存出错：处理函数修复后返回，或者 siglongjmp 跳出 ---- */

static char *page;
static volatile int faults, last_code;
static sigjmp_buf jump;

static void on_segv_fix(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    faults++;
    last_code = si->si_code;
    if (si->si_addr == page)
        mprotect(page, 4096, PROT_READ | PROT_WRITE);
}

static void on_segv_jump(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    last_code = si->si_code;
    siglongjmp(jump, 1);
}

static void test_faults(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv_fix;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    page = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(page != MAP_FAILED, "mmap");
    *(volatile char *)page = 42;
    CHECK(faults == 1 && page[0] == 42, "faults %d value %d", faults, page[0]);
    CHECK(last_code == SEGV_ACCERR, "si_code %d", last_code);

    sa.sa_sigaction = on_segv_jump;
    sigaction(SIGSEGV, &sa, NULL);
    /* 地址经由 volatile 变量传过去，编译器看不出它是常量 */
    static volatile uintptr_t unmapped = 16;
    if (sigsetjmp(jump, 1) == 0) {
        *(volatile int *)unmapped = 1;
        CHECK(0, "no fault at address 16");
    }
    CHECK(last_code == SEGV_MAPERR, "si_code %d", last_code);
    /* siglongjmp 恢复了屏蔽字，SIGSEGV 不能还被屏蔽着 */
    sigset_t cur;
    sigprocmask(SIG_BLOCK, NULL, &cur);
    CHECK(!sigismember(&cur, SIGSEGV), "SIGSEGV still blocked");
    signal(SIGSEGV, SIG_DFL);
    ok("faults");
}

/* ---- SA_RESTART：被信号打断的 read 是否重新执行 ---- */

static int restart_pipe[2];
static pthread_t main_thread;

static void *interrupt_read(void *arg)
{
    (void)arg;
    sleep_ms(30);
    pthread_kill(main_thread, SIGUSR1);
    sleep_ms(30);
    write(restart_pipe[1], "x", 1);
    return NULL;
}

static void test_restart(void)
{
    main_thread = pthread_self();
    for (int restart = 0; restart <= 1; restart++) {
        CHECK(pipe(restart_pipe) == 0, "pipe");
        handle(SIGUSR1, on_usr1, restart ? SA_RESTART : 0);
        atomic_store(&usr1_count, 0);
        pthread_t t;
        pthread_create(&t, NULL, interrupt_read, NULL);
        char c;
        ssize_t n = read(restart_pipe[0], &c, 1);
        if (restart) {
            CHECK(n == 1 && c == 'x', "restarted read returned %zd", n);
        } else {
            CHECK(n == -1 && errno == EINTR, "interrupted read returned %zd", n);
            CHECK(read(restart_pipe[0], &c, 1) == 1, "second read");
        }
        CHECK(atomic_load(&usr1_count) == 1, "handler ran %d times", atomic_load(&usr1_count));
        pthread_join(t, NULL);
        close(restart_pipe[0]);
        close(restart_pipe[1]);
    }
    ok("restart");
}

/* ---- 多线程进程里 fork：子进程只有调用 fork 的线程，内存是一份拷贝 ---- */

static atomic_int stop_spinning;

static void *spin(void *arg)
{
    (void)arg;
    while (!atomic_load(&stop_spinning))
        sleep_ms(1);
    return NULL;
}

static void test_fork(void)
{
    pthread_t t;
    pthread_create(&t, NULL, spin, NULL);
    long before = counter;
    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        counter = -1;
        pthread_mutex_lock(&mutex);
        pthread_mutex_unlock(&mutex);
        _exit(getppid() > 1 ? 7 : 8);
    }
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 7, "child status %#x", status);
    CHECK(counter == before, "child changed the parent's memory");
    atomic_store(&stop_spinning, 1);
    pthread_join(t, NULL);
    ok("fork");
}

/* ---- 分离的线程退出：musl 用 __unmapself 释放自己的栈 ---- */

static atomic_int detached_done;

static void *detached(void *arg)
{
    (void)arg;
    atomic_fetch_add(&detached_done, 1);
    return NULL;
}

static void test_detached(void)
{
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    for (int i = 0; i < 16; i++) {
        pthread_t t;
        CHECK(pthread_create(&t, &attr, detached, NULL) == 0, "create detached");
    }
    for (int i = 0; i < 2000 && atomic_load(&detached_done) < 16; i++)
        sleep_ms(1);
    CHECK(atomic_load(&detached_done) == 16, "%d detached threads ran", atomic_load(&detached_done));
    ok("detached");
}

int main(void)
{
    test_counters();
    test_condvar();
    test_thread_signal();
    test_altstack();
    test_faults();
    test_restart();
    test_fork();
    test_detached();
    printf("all ok\n");
    return 0;
}
