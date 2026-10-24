/*
 * System V IPC、POSIX 定时器、mremap、凭据和调度设置的 guest 测试程序（M6）：用 musl-gcc -static
 * 编译，在 appine-run 里直接运行（不需要 rootfs）。每项检查通过时打印一行
 * "ok <名字>"，失败时打印原因并以 1 退出，全部通过时最后打印 "all ok"。
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdatomic.h>
#include <grp.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/resource.h>
#include <sys/sem.h>
#include <poll.h>
#include <sys/shm.h>
#include <fcntl.h>
#include <sys/signalfd.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

/* 子进程的退出码 */
static int wait_child(pid_t pid)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    CHECK(WIFEXITED(status), "child status %#x", status);
    return WEXITSTATUS(status);
}

/* ---- 消息队列：按类型收取、阻塞的 msgrcv、删除后 EIDRM ---- */

struct message {
    long mtype;
    char text[16];
};

static void test_msg(void)
{
    int q = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
    CHECK(q >= 0, "msgget");
    struct message m = {2, "two"};
    CHECK(msgsnd(q, &m, 4, 0) == 0, "msgsnd 2");
    m.mtype = 1;
    strcpy(m.text, "one");
    CHECK(msgsnd(q, &m, 4, 0) == 0, "msgsnd 1");
    CHECK(msgrcv(q, &m, sizeof m.text, 1, 0) == 4 && strcmp(m.text, "one") == 0, "type 1");
    CHECK(msgrcv(q, &m, 2, 0, IPC_NOWAIT) < 0 && errno == E2BIG, "E2BIG");
    CHECK(msgrcv(q, &m, sizeof m.text, 0, 0) == 4 && m.mtype == 2, "first message");
    CHECK(msgrcv(q, &m, sizeof m.text, 0, IPC_NOWAIT) < 0 && errno == ENOMSG, "ENOMSG");

    /* 子进程等着收，父进程再发 */
    pid_t pid = fork();
    if (pid == 0) {
        struct message r;
        ssize_t n = msgrcv(q, &r, sizeof r.text, 7, 0);
        _exit(n == 6 && strcmp(r.text, "hello") == 0 ? 0 : 1);
    }
    usleep(50000);
    m.mtype = 7;
    strcpy(m.text, "hello");
    CHECK(msgsnd(q, &m, 6, 0) == 0, "msgsnd to waiter");
    CHECK(wait_child(pid) == 0, "child msgrcv");

    struct msqid_ds ds;
    CHECK(msgctl(q, IPC_STAT, &ds) == 0 && ds.msg_qnum == 0 && ds.msg_lrpid == pid, "IPC_STAT");
    CHECK(msgctl(q, IPC_RMID, NULL) == 0, "IPC_RMID");
    CHECK(msgsnd(q, &m, 1, 0) < 0 && errno == EINVAL, "removed queue");
    ok("msg");
}

/* ---- 信号量：原子的多个操作、阻塞、SEM_UNDO ---- */

static void test_sem(void)
{
    int s = semget(IPC_PRIVATE, 2, IPC_CREAT | 0600);
    CHECK(s >= 0, "semget");
    unsigned short vals[2] = {1, 0};
    CHECK(semctl(s, 0, SETALL, vals) == 0, "SETALL");
    /* 第二个操作做不到：整个 semop 都不生效 */
    struct sembuf both[2] = {{0, -1, IPC_NOWAIT}, {1, -1, IPC_NOWAIT}};
    CHECK(semop(s, both, 2) < 0 && errno == EAGAIN, "atomic semop");
    CHECK(semctl(s, 0, GETVAL) == 1, "value unchanged");

    /* 子进程等信号量 1，父进程加一 */
    pid_t pid = fork();
    if (pid == 0) {
        struct sembuf down = {1, -1, 0};
        _exit(semop(s, &down, 1) == 0 ? 0 : 1);
    }
    while (semctl(s, 1, GETNCNT) != 1) {
        usleep(1000);
    }
    struct sembuf up = {1, 1, 0};
    CHECK(semop(s, &up, 1) == 0, "semop up");
    CHECK(wait_child(pid) == 0, "child semop");

    /* SEM_UNDO：子进程退出时撤销它的操作 */
    pid = fork();
    if (pid == 0) {
        struct sembuf take = {0, -1, SEM_UNDO};
        _exit(semop(s, &take, 1) == 0 && semctl(s, 0, GETVAL) == 0 ? 0 : 1);
    }
    CHECK(wait_child(pid) == 0, "child SEM_UNDO");
    CHECK(semctl(s, 0, GETVAL) == 1, "undone at exit");
    CHECK(semctl(s, 0, GETPID) == pid, "GETPID");
    CHECK(semctl(s, 0, IPC_RMID) == 0, "IPC_RMID");
    ok("sem");
}

/* ---- 共享内存：父子进程看到同一段内存，IPC_RMID 等最后一次 shmdt ---- */

static void test_shm(void)
{
    int id = shmget(IPC_PRIVATE, 10000, IPC_CREAT | 0600);
    CHECK(id >= 0, "shmget");
    char *p = shmat(id, NULL, 0);
    CHECK(p != (void *)-1, "shmat");
    CHECK(p[9999] == 0, "zeroed");
    pid_t pid = fork();
    if (pid == 0) {
        strcpy(p, "from child");
        _exit(0);
    }
    CHECK(wait_child(pid) == 0, "child");
    CHECK(strcmp(p, "from child") == 0, "shared contents: %s", p);

    struct shmid_ds ds;
    CHECK(shmctl(id, IPC_STAT, &ds) == 0, "IPC_STAT");
    CHECK(ds.shm_segsz == 10000 && ds.shm_nattch == 1, "segsz %zu nattch %lu", ds.shm_segsz,
          (unsigned long)ds.shm_nattch);
    /* 第二次映射到另一个地址，只读 */
    char *q = shmat(id, NULL, SHM_RDONLY);
    CHECK(q != (void *)-1 && q != p && strcmp(q, "from child") == 0, "second shmat");
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && ds.shm_nattch == 2, "nattch 2");
    CHECK(shmdt(q) == 0, "shmdt");
    CHECK(shmdt(q) < 0 && errno == EINVAL, "shmdt twice");

    /* 删除之后仍然可以用，直到最后一次 shmdt */
    CHECK(shmctl(id, IPC_RMID, NULL) == 0, "IPC_RMID");
    strcpy(p, "still here");
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && (ds.shm_perm.mode & SHM_DEST), "SHM_DEST");
    CHECK(shmdt(p) == 0, "last shmdt");
    CHECK(shmctl(id, IPC_STAT, &ds) < 0 && errno == EINVAL, "freed");
    ok("shm");
}

/* ---- POSIX 定时器：周期信号、超限次数、SIGEV_THREAD、CPU 时钟 ---- */

static volatile sig_atomic_t ticks;
static volatile int last_overrun;

static void on_tick(int sig, siginfo_t *si, void *uc)
{
    (void)sig;
    (void)uc;
    ticks++;
    last_overrun = si->si_overrun;
}

static atomic_int thread_ticks;

static void on_thread_tick(union sigval v)
{
    atomic_fetch_add(&thread_ticks, v.sival_int);
}

static void test_timers(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_tick;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction");

    struct sigevent sev = {0};
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGUSR1;
    timer_t t;
    CHECK(timer_create(CLOCK_MONOTONIC, &sev, &t) == 0, "timer_create");
    struct itimerspec its = {{0, 10000000}, {0, 10000000}};
    CHECK(timer_settime(t, 0, &its, NULL) == 0, "timer_settime");
    while (ticks < 3) {
        pause();
    }
    struct itimerspec cur;
    CHECK(timer_gettime(t, &cur) == 0 && cur.it_interval.tv_nsec == 10000000, "timer_gettime");

    /* 信号挂着期间的到期次数记进 si_overrun */
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigprocmask(SIG_BLOCK, &block, NULL);
    int before = ticks;
    usleep(100000);
    sigprocmask(SIG_UNBLOCK, &block, NULL);
    CHECK(ticks == before + 1 && last_overrun > 0, "overrun %d", last_overrun);
    CHECK(timer_delete(t) == 0, "timer_delete");
    /* musl 的 timer_delete 直接返回 -EINVAL，不设置 errno */
    CHECK(timer_delete(t) != 0, "deleted");

    /* SIGEV_THREAD：musl 在用户态起一个线程调用回调 */
    sev.sigev_notify = SIGEV_THREAD;
    sev.sigev_notify_function = on_thread_tick;
    sev.sigev_value.sival_int = 5;
    CHECK(timer_create(CLOCK_REALTIME, &sev, &t) == 0, "SIGEV_THREAD");
    struct itimerspec once = {{0, 0}, {0, 20000000}};
    CHECK(timer_settime(t, 0, &once, NULL) == 0, "arm once");
    for (int i = 0; i < 200 && atomic_load(&thread_ticks) == 0; i++) {
        usleep(10000);
    }
    CHECK(atomic_load(&thread_ticks) == 5, "callback %d", atomic_load(&thread_ticks));
    timer_delete(t);

    /* 进程 CPU 时钟：忙等 30 毫秒的 CPU 时间之后到期 */
    sev.sigev_notify = SIGEV_SIGNAL;
    CHECK(timer_create(CLOCK_PROCESS_CPUTIME_ID, &sev, &t) == 0, "cpu timer");
    struct itimerspec cpu = {{0, 0}, {0, 30000000}};
    before = ticks;
    CHECK(timer_settime(t, 0, &cpu, NULL) == 0, "arm cpu timer");
    while (ticks == before) {
    }
    timer_delete(t);
    ok("timers");
}

/* ---- signalfd：取走被阻塞的信号，poll 能看到 ---- */

static void test_signalfd(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &set, NULL) == 0, "block");
    int fd = signalfd(-1, &set, SFD_NONBLOCK | SFD_CLOEXEC);
    CHECK(fd >= 0, "signalfd");
    struct signalfd_siginfo si;
    CHECK(read(fd, &si, sizeof si) < 0 && errno == EAGAIN, "empty");
    CHECK(kill(getpid(), SIGUSR2) == 0, "kill");
    struct pollfd p = {fd, POLLIN, 0};
    CHECK(poll(&p, 1, 1000) == 1 && (p.revents & POLLIN), "poll");
    CHECK(read(fd, &si, sizeof si) == sizeof si, "read");
    CHECK(si.ssi_signo == SIGUSR2 && si.ssi_pid == (uint32_t)getpid() && si.ssi_code == SI_USER,
          "signo %u pid %u code %d", si.ssi_signo, si.ssi_pid, si.ssi_code);
    sigset_t pending;
    CHECK(sigpending(&pending) == 0 && !sigismember(&pending, SIGUSR2), "consumed");
    close(fd);
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    ok("signalfd");
}

/* ---- splice、tee、vmsplice：在管道之间搬数据 ---- */

static void test_splice(void)
{
    int a[2], b[2];
    CHECK(pipe(a) == 0 && pipe(b) == 0, "pipe");
    struct iovec iov[2] = {{"hel", 3}, {"lo", 2}};
    CHECK(vmsplice(a[1], iov, 2, 0) == 5, "vmsplice");
    /* tee 复制但不取走，splice 搬走 */
    CHECK(tee(a[0], b[1], 100, 0) == 5, "tee");
    CHECK(splice(a[0], NULL, b[1], NULL, 100, 0) == 5, "splice");
    char buf[16] = {0};
    CHECK(read(b[0], buf, sizeof buf) == 10 && strcmp(buf, "hellohello") == 0, "read %s", buf);
    CHECK(splice(a[0], NULL, b[1], NULL, 1, SPLICE_F_NONBLOCK) < 0 && errno == EAGAIN, "empty");
    loff_t off = 0;
    CHECK(splice(a[0], &off, b[1], NULL, 1, 0) < 0 && errno == ESPIPE, "offset on a pipe");
    close(a[0]);
    close(a[1]);
    close(b[0]);
    close(b[1]);
    ok("splice");
}

/* ---- mremap：原地扩展、搬家、缩小 ---- */

static void test_mremap(void)
{
    size_t page = 4096;
    char *a = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(a != MAP_FAILED, "mmap");
    strcpy(a, "data");
    char *b = mremap(a, 2 * page, 64 * page, MREMAP_MAYMOVE);
    CHECK(b != MAP_FAILED && strcmp(b, "data") == 0, "grow");
    b[63 * page] = 1;
    CHECK(mremap(b, 64 * page, page, 0) == b, "shrink");
    CHECK(mremap(b + 1, page, page, 0) == MAP_FAILED && errno == EINVAL, "unaligned");
    CHECK(munmap(b, page) == 0, "munmap");
    ok("mremap");
}

/* ---- 凭据：降低权限之后的规则、setgroups、能力 ---- */

static void test_creds(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        gid_t groups[] = {10, 20};
        CHECK(setgroups(2, groups) == 0, "setgroups");
        gid_t back[4];
        CHECK(getgroups(4, back) == 2 && back[1] == 20, "getgroups");
        CHECK(setresgid(100, 100, 100) == 0, "setresgid");
        /* 只改有效用户 ID：之后还能变回 root */
        CHECK(setresuid(-1, 1000, -1) == 0 && geteuid() == 1000 && getuid() == 0, "seteuid");
        CHECK(setgroups(0, NULL) < 0 && errno == EPERM, "no CAP_SETGID");
        CHECK(setresuid(-1, 0, -1) == 0 && geteuid() == 0, "back to root");
        /* 三个 ID 都改掉：再也回不去 */
        CHECK(setuid(1000) == 0, "setuid");
        uid_t r, e, s;
        CHECK(getresuid(&r, &e, &s) == 0 && r == 1000 && e == 1000 && s == 1000, "getresuid");
        CHECK(setuid(0) < 0 && errno == EPERM, "setuid 0");
        CHECK(kill(getppid(), 0) < 0 && errno == EPERM, "kill root's process");
        CHECK(setfsuid(0) == 1000 && setfsuid(-1) == 1000, "setfsuid");
        struct rlimit rl;
        CHECK(getrlimit(RLIMIT_NOFILE, &rl) == 0, "getrlimit");
        rl.rlim_max++;
        CHECK(setrlimit(RLIMIT_NOFILE, &rl) < 0 && errno == EPERM, "raise hard limit");
        _exit(0);
    }
    CHECK(wait_child(pid) == 0, "child");
    CHECK(getuid() == 0 && geteuid() == 0, "parent still root");
    ok("creds");
}

/* ---- 调度：亲和性、实时策略、RLIMIT_NOFILE 限制 fd ---- */

static void test_sched(void)
{
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(0, &set);
    CHECK(sched_setaffinity(0, sizeof set, &set) == 0, "setaffinity");
    CPU_ZERO(&set);
    CHECK(sched_getaffinity(0, sizeof set, &set) == 0 && CPU_COUNT(&set) == 1, "getaffinity");
    CHECK(sched_getcpu() == 0, "getcpu");
    /* musl 的 sched_setscheduler 等不调用内核，直接返回 ENOSYS */
    struct sched_param sp = {.sched_priority = 10};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == 0, "SCHED_RR");
    CHECK(syscall(SYS_sched_getscheduler, 0) == SCHED_RR, "getscheduler");
    struct timespec ts;
    CHECK(sched_rr_get_interval(0, &ts) == 0 && ts.tv_nsec > 0, "rr interval");
    sp.sched_priority = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &sp) == 0, "SCHED_OTHER");

    struct rlimit rl = {8, 8};
    CHECK(setrlimit(RLIMIT_NOFILE, &rl) == 0, "setrlimit");
    int fds[2];
    while (pipe(fds) == 0) {
    }
    CHECK(errno == EMFILE, "EMFILE");
    ok("sched");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    test_msg();
    test_sem();
    test_shm();
    test_timers();
    test_signalfd();
    test_splice();
    test_mremap();
    test_creds();
    test_sched();
    printf("all ok\n");
    return 0;
}
