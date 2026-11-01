# docker-run-options: --privileged
# cgroup.freeze（docs/rounds/165-cgroup-freeze.md）：冻结的进程不运行、不消耗 CPU，状态是 S，
# wait 看不到；阻塞中的 read、nanosleep 解冻后接着做（epoll_wait 与被信号打断一样是 EINTR），
# 冻结期间来的信号解冻后才处理，SIGKILL 照样结束；cgroup.events 的 frozen（空的 cgroup、
# 子 cgroup 跟着祖先），迁移进来就停、迁移出去就接着运行，clone3 放进冻结的 cgroup 的子进程
# 不运行，cgroup.kill 结束冻结的进程。原生一侧是特权容器，只在建的 t 里比
cd /sys/fs/cgroup
mkdir t t/f t/f/c t/o
cat > /tmp/freeze.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CG "/sys/fs/cgroup/t/"

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void put(const char *file, const char *v)
{
    char path[128];
    snprintf(path, sizeof path, CG "%s", file);
    int fd = open(path, O_WRONLY);
    if (write(fd, v, strlen(v)) < 0)
        printf("write %s %s: %s\n", file, v, strerror(errno));
    close(fd);
}

static void show(const char *file)
{
    char path[128], b[128] = "";
    snprintf(path, sizeof path, CG "%s", file);
    int fd = open(path, O_RDONLY);
    int n = read(fd, b, sizeof b - 1);
    close(fd);
    for (int i = 0; i < n; i++)
        if (b[i] == '\n')
            b[i] = ' ';
    printf("%s: %s\n", file, b);
}

static void stat_of(pid_t p, char *state, long *ticks)
{
    char path[64], b[512];
    snprintf(path, sizeof path, "/proc/%d/stat", p);
    int fd = open(path, O_RDONLY);
    int n = read(fd, b, sizeof b - 1);
    close(fd);
    b[n > 0 ? n : 0] = 0;
    char *s = strrchr(b, ')') + 2;
    *state = *s;
    unsigned long ut = 0, st = 0;
    sscanf(s, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %lu %lu", &ut, &st);
    *ticks = ut + st;
}

static void into(pid_t p, const char *cg)
{
    char b[16], file[64];
    snprintf(b, sizeof b, "%d", p);
    snprintf(file, sizeof file, "%s/cgroup.procs", cg);
    put(file, b);
}

static volatile sig_atomic_t got;
static void on(int s) { got = s; }

// 子进程：放进 cg，做 job，结果写进自己的管道
static pid_t child(const char *cg, int *out, void (*job)(int))
{
    int p[2];
    pipe(p);
    pid_t c = fork();
    if (c == 0) {
        close(p[0]);
        into(getpid(), cg);
        job(p[1]);
        _exit(0);
    }
    close(p[1]);
    *out = p[0];
    return c;
}

static int data[2];

static void reader(int out)
{
    char b[8];
    int n = read(data[0], b, sizeof b);
    dprintf(out, "read %d %s", n, n < 0 ? strerror(errno) : "ok");
}

static void sleeper(int out)
{
    double s = now();
    struct timespec ts = {1, 0};
    int r = nanosleep(&ts, NULL);
    dprintf(out, "nanosleep %d, slept past the thaw %d", r, now() - s >= 1.5);
}

static void poller(int out)
{
    int ep = epoll_create1(0), q[2];
    pipe(q);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, q[0], &ev);
    int n = epoll_wait(ep, &ev, 1, 3000);
    dprintf(out, "epoll_wait %d %s", n, n < 0 ? strerror(errno) : "");
}

static void spinner(int out)
{
    signal(SIGUSR1, on);
    volatile unsigned long i = 0;
    while (!got)
        i++;
    dprintf(out, "spinner got signal %d", got);
}

static void pauser(int out)
{
    signal(SIGUSR2, on);
    pause();
    dprintf(out, "pauser got signal %d", got);
}

// 最多等 3 秒：内核不对时用例失败而不是卡住
static void result(const char *what, int fd)
{
    char b[128] = "";
    struct pollfd p = {fd, POLLIN, 0};
    int n = poll(&p, 1, 3000) > 0 ? read(fd, b, sizeof b - 1) : 0;
    printf("%s: %s\n", what, n > 0 ? b : "(nothing)");
}

static int ready(int fd)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(60);
    put("f/cgroup.freeze", "2");
    put("f/cgroup.freeze", "abc");
    // 空的 cgroup 一要求冻结就是冻结的，子 cgroup 跟着，但它自己的 cgroup.freeze 是 0
    put("f/cgroup.freeze", "1");
    show("f/cgroup.freeze");
    show("f/cgroup.events");
    show("f/c/cgroup.events");
    show("f/c/cgroup.freeze");
    show("cgroup.events");
    put("f/cgroup.freeze", "0");
    show("f/cgroup.events");
    // runc 的读法：读写方式打开，写进去，seek 回 0 读回来确认（seq_file 从 0 读时重新生成）
    int ff = open(CG "f/cgroup.freeze", O_RDWR);
    char fb[4] = "";
    write(ff, "1", 1);
    lseek(ff, 0, SEEK_SET);
    read(ff, fb, 2);
    printf("read back after writing 1: %c\n", fb[0]);
    write(ff, "0", 1);
    pread(ff, fb, 2, 0);
    printf("pread back after writing 0: %c\n", fb[0]);
    close(ff);

    pipe(data);
    int r1, r2, r3, r4, r5;
    pid_t c1 = child("f", &r1, reader);
    pid_t c2 = child("f", &r2, sleeper);
    pid_t c3 = child("f/c", &r3, poller);
    pid_t c4 = child("f", &r4, spinner);
    pid_t c5 = child("f", &r5, pauser);
    usleep(300000);
    put("f/cgroup.freeze", "1");
    usleep(300000);
    show("f/cgroup.events");
    show("f/c/cgroup.events");
    show("cgroup.events");
    char s1, s2, s3, s4, s5;
    long t1, t2, t3, t4, t5, t4b;
    stat_of(c1, &s1, &t1);
    stat_of(c2, &s2, &t2);
    stat_of(c3, &s3, &t3);
    stat_of(c4, &s4, &t4);
    stat_of(c5, &s5, &t5);
    printf("states %c %c %c %c %c\n", s1, s2, s3, s4, s5);
    // 冻结期间：写进管道、发信号都不处理，不消耗 CPU，wait 看不到
    write(data[1], "x", 1);
    kill(c4, SIGUSR1);
    kill(c5, SIGUSR2);
    usleep(300000);
    stat_of(c4, &s4, &t4b);
    printf("spinner used no CPU while frozen: %d\n", t4b - t4 <= 1);
    printf("nothing from the frozen: %d %d %d\n", ready(r1), ready(r4), ready(r5));
    int st;
    printf("waitpid WUNTRACED: %d\n", waitpid(-1, &st, WNOHANG | WUNTRACED));
    // 迁移进来的停下；clone3 放进来的不运行
    int r6, r7;
    pid_t c6 = child("f", &r6, pauser);
    int p7[2];
    pipe(p7);
    int dir = open(CG "f", O_RDONLY | O_DIRECTORY);
    struct { unsigned long long flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; } a = {
        .flags = 0x200000000ULL, .exit_signal = SIGCHLD, .cgroup = dir};
    pid_t c7 = syscall(435, &a, sizeof a);
    if (c7 == 0) {
        dprintf(p7[1], "clone3 child ran");
        _exit(0);
    }
    close(p7[1]);
    r7 = p7[0];
    usleep(300000);
    printf("new members did not run: %d %d\n", !ready(r6), !ready(r7));
    show("f/cgroup.events");
    // 迁移出去的接着运行
    into(c4, "o");
    usleep(300000);
    result("moved out", r4);
    // 子 cgroup 自己也要求冻结：解冻父 cgroup 之后它仍是冻结的
    put("f/c/cgroup.freeze", "1");
    put("f/cgroup.freeze", "0");
    usleep(300000);
    show("f/cgroup.events");
    show("f/c/cgroup.events");
    result("reader", r1);
    result("sleeper", r2);
    result("pauser", r5);
    kill(c6, SIGUSR2);
    result("moved in", r6);
    result("clone3", r7);
    printf("poller still frozen: %d\n", !ready(r3));
    put("f/c/cgroup.freeze", "0");
    result("poller", r3);
    while (wait(NULL) > 0)
        ;
    show("f/cgroup.events");
    // SIGKILL、cgroup.kill 结束冻结的进程
    int r8, r9;
    pid_t c8 = child("f", &r8, pauser);
    pid_t c9 = child("f", &r9, pauser);
    usleep(200000);
    put("f/cgroup.freeze", "1");
    usleep(200000);
    kill(c8, SIGKILL);
    waitpid(c8, &st, 0);
    printf("SIGKILL: signaled %d sig %d\n", WIFSIGNALED(st), WTERMSIG(st));
    put("f/cgroup.kill", "1");
    waitpid(c9, &st, 0);
    printf("cgroup.kill: signaled %d sig %d\n", WIFSIGNALED(st), WTERMSIG(st));
    show("f/cgroup.events");
    put("f/cgroup.freeze", "0");
    return 0;
}
C
gcc -o /tmp/freeze /tmp/freeze.c && /tmp/freeze
