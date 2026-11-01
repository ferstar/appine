# 内核的边角：别的进程截短已映射的文件（SIGBUS，不能带走整个 App）、MADV_DONTNEED、
# PROT_NONE 的大片预留、停止又继续之后 nanosleep 只睡剩下的时间、持有 robust 锁的进程
# 退出之后等待者拿到 EOWNERDEAD、EPOLLEXCLUSIVE 的参数检查、跳到未对齐的地址（SIGBUS）
set -e
mkdir -p /tmp/diff-edges
cd /tmp/diff-edges
cat > edges.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static sigjmp_buf env;
static volatile int last_sig, last_code;
static void *volatile last_addr;

static void on_fault(int sig, siginfo_t *si, void *ctx)
{
    (void)ctx;
    last_sig = sig;
    last_code = si->si_code;
    last_addr = si->si_addr;
    siglongjmp(env, 1);
}

/* 读一个字节并打印结果；出错时打印信号和 si_code */
static void probe(const char *label, volatile char *p)
{
    if (sigsetjmp(env, 1)) {
        printf(" %s:%s/%d", label, last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code);
        return;
    }
    int v = *p;
    printf(" %s:%d", label, v);
}

static void run_child(void (*fn)(void))
{
    pid_t pid = fork();
    if (pid == 0) {
        fn();
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

static void child_ftruncate(void)
{
    int fd = open("data", O_RDWR);
    ftruncate(fd, 4096 + 10);
}

static void child_truncate(void) { truncate("data", 0); }

static void child_otrunc(void) { close(open("data", O_WRONLY | O_TRUNC)); }

static long ms_since(struct timespec *t0)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0->tv_sec) * 1000 + (t.tv_nsec - t0->tv_nsec) / 1000000;
}

static void on_usr1(int sig) { (void)sig; }

int main(void)
{
    struct sigaction sa = {.sa_sigaction = on_fault, .sa_flags = SA_SIGINFO};
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    long page = 4096;

    /* 别的进程截短映射着的文件：超出新末尾的页得到 SIGBUS */
    void (*truncators[])(void) = {child_ftruncate, child_truncate, child_otrunc};
    const char *names[] = {"ftruncate", "truncate", "O_TRUNC"};
    for (int i = 0; i < 3; i++) {
        int fd = open("data", O_RDWR | O_CREAT | O_TRUNC, 0644);
        char buf[3 * 4096];
        memset(buf, 'x', sizeof buf);
        write(fd, buf, sizeof buf);
        char *shared = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        char *private = mmap(NULL, 3 * page, PROT_READ, MAP_PRIVATE, fd, 0);
        printf("%s before:", names[i]);
        probe("shared2", shared + 2 * page);
        probe("private2", private + 2 * page);
        run_child(truncators[i]);
        printf("\n%s after:", names[i]);
        for (int p = 0; p < 3; p++) {
            char label[16];
            snprintf(label, sizeof label, "shared%d", p);
            probe(label, shared + p * page);
            snprintf(label, sizeof label, "private%d", p);
            probe(label, private + p * page);
        }
        printf("\n");
        munmap(shared, 3 * page);
        munmap(private, 3 * page);
        close(fd);
    }

    /* MADV_DONTNEED：私有匿名内存读回零，共享内存内容不变 */
    char *anon = mmap(NULL, 8 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *shm = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(anon, 7, 8 * page);
    memset(shm, 9, 2 * page);
    madvise(anon + page, 5 * page, MADV_DONTNEED);
    madvise(shm, 2 * page, MADV_DONTNEED);
    printf("dontneed %d %d %d %d shared %d\n", anon[0], anon[page], anon[6 * page - 1], anon[6 * page],
           shm[page]);

    /* PROT_NONE 的 8 GB 预留，中间一页改成可写 */
    char *reserve = mmap(NULL, 1L << 33, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    char *mid = reserve + (1L << 32);
    mprotect(mid, page, PROT_READ | PROT_WRITE);
    mid[0] = 5;
    printf("reserve");
    probe("mid", mid);
    probe("next", mid + page);
    mprotect(mid, page, PROT_NONE);
    probe("protected", mid);
    mprotect(mid, page, PROT_READ);
    probe("readable", mid);
    munmap(reserve, 1L << 33);
    probe("unmapped", mid);
    printf("\n");

    /* 停止又继续：nanosleep 接着睡剩下的时间，而不是重新睡满 */
    int pipefd[2];
    pipe(pipefd);
    pid_t sleeper = fork();
    if (sleeper == 0) {
        struct timespec t0, req = {0, 600 * 1000000L};
        clock_gettime(CLOCK_MONOTONIC, &t0);
        nanosleep(&req, NULL);
        long ms = ms_since(&t0);
        write(pipefd[1], &ms, sizeof ms);
        _exit(0);
    }
    usleep(150000);
    kill(sleeper, SIGSTOP);
    usleep(150000);
    kill(sleeper, SIGCONT);
    long slept;
    read(pipefd[0], &slept, sizeof slept);
    waitpid(sleeper, NULL, 0);
    printf("stopped sleep %s\n", slept >= 600 && slept < 850 ? "kept its deadline" : "restarted");

    /* 停止又继续：poll（剩下的时间写回 timespec）、futex（restart_block）按原来的到期时刻
       接着等，epoll_wait 是 EINTR（signal(7)，cgroup 冻结之后解冻也一样） */
    for (int which = 0; which < 3; which++) {
        pid_t waiter = fork();
        if (waiter == 0) {
            int q[2];
            pipe(q);
            struct timespec t0;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            long r;
            if (which == 0) {
                struct pollfd p = {q[0], POLLIN, 0};
                r = poll(&p, 1, 600);
            } else if (which == 2) {
                static int word;
                struct timespec rel = {0, 600 * 1000000L};
                r = syscall(SYS_futex, &word, 128 /* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */, 0, &rel, NULL, 0);
            } else {
                int ep = epoll_create1(0);
                struct epoll_event ev = {.events = EPOLLIN};
                epoll_ctl(ep, EPOLL_CTL_ADD, q[0], &ev);
                r = epoll_wait(ep, &ev, 1, 600);
            }
            long out[3] = {r, r < 0 ? errno : 0, ms_since(&t0)};
            write(pipefd[1], out, sizeof out);
            _exit(0);
        }
        usleep(150000);
        kill(waiter, SIGSTOP);
        usleep(150000);
        kill(waiter, SIGCONT);
        long out[3];
        read(pipefd[0], out, sizeof out);
        waitpid(waiter, NULL, 0);
        printf("stopped %s: %ld %s %s\n", which == 2 ? "futex" : which ? "epoll_wait" : "poll", out[0],
               out[1] ? strerror(out[1]) : "", out[2] >= 600 && out[2] < 850 ? "kept its deadline"
               : out[2] < 600 ? "returned early" : "restarted");
    }

    /* 有处理函数时返回 EINTR，rem 是剩下的时间 */
    struct sigaction su = {.sa_handler = on_usr1};
    sigaction(SIGUSR1, &su, NULL);
    pid_t parent = getpid();
    pid_t poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    struct timespec req = {2, 0}, rem = {0, 0};
    int r = nanosleep(&req, &rem);
    printf("interrupted sleep %d %s rem<2s %d\n", r, strerror(errno), rem.tv_sec < 2 && (rem.tv_sec || rem.tv_nsec));
    waitpid(poker, NULL, 0);

    /* 有超时的 futex 等待被处理函数打断是 EINTR，即使有 SA_RESTART（restart_block） */
    su.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &su, NULL);
    poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    static int fword;
    struct timespec ft = {2, 0};
    r = syscall(SYS_futex, &fword, 128, 0, &ft, NULL, 0);
    printf("interrupted futex with SA_RESTART %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);

    /* 进程持有进程间共享的 robust 锁时退出：等待者拿到 EOWNERDEAD */
    pthread_mutex_t *m = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(m, &ma);
    pid_t owner = fork();
    if (owner == 0) {
        pthread_mutex_lock(m);
        _exit(0);
    }
    waitpid(owner, NULL, 0);
    alarm(10);
    r = pthread_mutex_lock(m);
    printf("robust lock %s\n", r == EOWNERDEAD ? "EOWNERDEAD" : strerror(r));
    if (r == EOWNERDEAD)
        pthread_mutex_consistent(m);
    printf("unlock %d\n", pthread_mutex_unlock(m));
    alarm(0);

    /* EPOLLEXCLUSIVE 只能在加入时用，不能用于 epoll，不能配别的标志 */
    int ep = epoll_create1(0), ep2 = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE};
    printf("exclusive add %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &ev));
    printf("exclusive mod %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN;
    printf("mod exclusive interest %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT;
    printf("exclusive oneshot %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[1], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE;
    printf("exclusive epoll %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, ep2, &ev), errno);

    /* 跳到未对齐的地址：取指时 SIGBUS（BUS_ADRALN），si_addr 是目标。经由函数指针调用和 ret */
    uintptr_t targets[] = {1, 2, 0x1001};
    for (int i = 0; i < 3; i++) {
        for (int how = 0; how < 2; how++) {
            if (sigsetjmp(env, 1)) {
                printf("unaligned %s %#lx: %s/%d addr=%p\n", how ? "ret" : "call", (unsigned long)targets[i],
                       last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code, last_addr);
                continue;
            }
            if (how)
                __asm__ volatile("mov x30, %0\n ret" ::"r"(targets[i]) : "x30");
            else
                ((void (*)(void))targets[i])();
        }
    }
    return 0;
}
C
gcc -O2 -o edges edges.c
./edges
