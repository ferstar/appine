/*
 * 进程结束（finish_group）的每一步分别兜住内核层的 bug（设计文档 §12 第 5 条）：第 n 步
 * 出错时只丢掉这一步做的事，后面的步骤照样做。bug 用 debug 构建的测试钩子模拟，appine-run
 * 要设置 APPINE_TEST_HOOKS=1 和 APPINE_ON_BUG=kill；要有控制终端，所以拷进仓库里运行。
 *
 * 结束的进程 v 是会话组长、有控制终端，退出之前留下：
 *   - AIO 上下文（/proc/sys/fs/aio-nr）、用 SEM_UNDO 加过的 SysV 信号量；
 *   - 前台进程组里的子进程 a：登记了 PDEATHSIG（SIGUSR1），被 v 跟踪（SEIZE）；
 *   - 自己一个进程组、停住的子进程 s：v 退出之后它的进程组成了孤儿。
 * 都做了的话，v 结束之后 aio-nr 和信号量恢复，a 收到控制终端的 SIGHUP 和 PDEATHSIG、
 * 父进程是 1、不再被跟踪，s 收到 SIGHUP 之后继续运行，父进程照常回收 v。
 * 每一步出错时各检查一遍，最后打印 "all ok"。
 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/sem.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_PANIC_IN_FINISH 7 /* 这个进程结束时第 n 步 panic */

enum {
    E_AIO = 1,      /* 第 1 步 */
    E_REPARENT = 2, /* 第 2 步：子进程交给 init，PDEATHSIG、孤儿进程组也要用它的结果 */
    E_DEATHSIG = 4, /* 第 3 步 */
    E_SEM = 8,      /* 第 4 步 */
    E_CTTY = 16,    /* 第 5 步；第 6 步报告给跟踪者，出错时直接交给父进程，什么也不丢 */
    E_PARENT = 32,  /* 第 7 步 */
    E_UNTRACE = 64, /* 第 8 步 */
    E_ORPHAN = 128, /* 第 9 步 */
};
#define STEPS 9
/* 第 n 步出错时丢掉的，第 0 项是不出错 */
static const int lost[STEPS + 1] = {
    0, E_AIO, E_REPARENT | E_DEATHSIG | E_ORPHAN, E_DEATHSIG, E_SEM, E_CTTY, 0, E_PARENT, E_UNTRACE, E_ORPHAN,
};

static int step;

static void fail(const char *what, const char *detail)
{
    printf("FAIL: step %d: %s%s%s\n", step, what, detail ? ": " : "", detail ? detail : "");
    exit(1);
}

static volatile sig_atomic_t got_hup, got_usr1;

static void on_signal(int sig)
{
    if (sig == SIGHUP)
        got_hup = 1;
    else
        got_usr1 = 1;
}

static long aio_nr(void)
{
    long n;
    FILE *f = fopen("/proc/sys/fs/aio-nr", "r");
    if (!f || fscanf(f, "%ld", &n) != 1)
        fail("read aio-nr", NULL);
    fclose(f);
    return n;
}

static int tracer_pid(void)
{
    char line[256];
    int pid = -1;
    FILE *f = fopen("/proc/self/status", "r");
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "TracerPid: %d", &pid) == 1)
            break;
    if (f)
        fclose(f);
    return pid;
}

static void read_line(int fd, char *buf, size_t size)
{
    size_t n = 0;
    while (n + 1 < size && read(fd, &buf[n], 1) == 1 && buf[n] != '\n')
        n++;
    buf[n] = 0;
}

/* v 的子进程：等测试程序在 go 上写一个字节（期间收到的信号由处理函数记下），再报告 */
static void child_report(int report, int go, char who)
{
    char c, buf[64];
    while (read(go, &c, 1) < 0 && errno == EINTR)
        ;
    int n = who == 'a' ? snprintf(buf, sizeof buf, "a %d %d %d %d\n", got_hup, got_usr1, getppid() == 1, tracer_pid())
                       : snprintf(buf, sizeof buf, "s %d\n", got_hup);
    write(report, buf, n);
    _exit(0);
}

static void victim(int master, int ready[2], int report, int go, int done, int semid)
{
    struct sigaction sa = {.sa_handler = on_signal};
    unsigned long ctx = 0;
    struct sembuf up = {0, 1, SEM_UNDO};
    int slave;
    if (setsid() < 0 || (slave = open(ptsname(master), O_RDWR)) < 0 || ioctl(slave, TIOCSCTTY, 0) != 0)
        _exit(30);
    if (syscall(SYS_io_setup, 8, &ctx) != 0 || semop(semid, &up, 1) != 0)
        _exit(31);
    pid_t a = fork();
    if (a == 0) {
        close(done);
        sigaction(SIGHUP, &sa, NULL);
        sigaction(SIGUSR1, &sa, NULL);
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        write(ready[1], "a", 1);
        child_report(report, go, 'a');
    }
    pid_t s = fork();
    if (s == 0) {
        close(done);
        sigaction(SIGHUP, &sa, NULL);
        setpgid(0, 0);
        write(ready[1], "s", 1);
        raise(SIGSTOP);
        child_report(report, go, 's');
    }
    char c, buf[64];
    int status;
    for (int i = 0; i < 2; i++)
        if (read(ready[0], &c, 1) != 1)
            _exit(32);
    if (waitpid(s, &status, WUNTRACED) != s || !WIFSTOPPED(status) || ptrace(PTRACE_SEIZE, a, 0, 0) != 0)
        _exit(33);
    write(report, buf, snprintf(buf, sizeof buf, "v %d %d\n", a, s));
    if (step && syscall(TEST_SYSCALL, HOOK_PANIC_IN_FINISH, step) != 0)
        _exit(34);
    _exit(21);
}

static void finish_with_bug(void)
{
    int ready[2], report[2], go[2], done[2], master, semid;
    if (pipe(ready) || pipe(report) || pipe(go) || pipe(done))
        fail("pipe", NULL);
    if ((master = posix_openpt(O_RDWR | O_NOCTTY)) < 0 || grantpt(master) || unlockpt(master))
        fail("posix_openpt", NULL);
    if ((semid = semget(IPC_PRIVATE, 1, 0600)) < 0)
        fail("semget", NULL);
    long aio_before = aio_nr();
    int l = lost[step];
    alarm(10); /* 该做的没做、卡住时由 SIGALRM 结束测试 */
    pid_t v = fork();
    if (v == 0)
        victim(master, ready, report[1], go[0], done[1], semid);
    close(done[1]);
    char line[64], c;
    pid_t a, s;
    read_line(report[0], line, sizeof line);
    if (sscanf(line, "v %d %d", &a, &s) != 2)
        fail("setting up the process", line);
    /* v 的 fd 表在进程结束的各步之后才释放 */
    while (read(done[0], &c, 1) > 0)
        ;
    int status;
    if (!(l & E_PARENT) && (waitpid(v, &status, 0) != v || !WIFEXITED(status) || WEXITSTATUS(status) != 21))
        fail("the parent reaps it", NULL);
    if ((aio_nr() == aio_before) != !(l & E_AIO))
        fail("AIO contexts", NULL);
    if (semctl(semid, 0, GETVAL) != !!(l & E_SEM))
        fail("SEM_UNDO", NULL);
    write(go[1], "gg", 2);
    for (int want = !(l & E_UNTRACE) + !(l & E_ORPHAN); want > 0; want--) {
        int hup, usr1, init, tracer;
        read_line(report[0], line, sizeof line);
        if (sscanf(line, "a %d %d %d %d", &hup, &usr1, &init, &tracer) == 4) {
            if (hup != !(l & E_CTTY) || usr1 != !(l & E_DEATHSIG) || init != !(l & E_REPARENT) || tracer != 0)
                fail("the child in the foreground process group", line);
        } else if (sscanf(line, "s %d", &hup) != 1 || hup != 1) {
            fail("the child in the orphaned process group", line);
        }
    }
    alarm(0);
    /* 没有收拾好的留在那里：停住的 s、停在给已经退出的跟踪者的信号上的 a */
    kill(a, SIGKILL);
    kill(s, SIGKILL);
    semctl(semid, 0, IPC_RMID);
    int fds[] = {ready[0], ready[1], report[0], report[1], go[0], go[1], done[0], master};
    for (size_t i = 0; i < sizeof fds / sizeof fds[0]; i++)
        close(fds[i]);
    printf("panic in step %d: ok\n", step);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    for (step = 0; step <= STEPS; step++)
        finish_with_bug();
    printf("all ok\n");
    return 0;
}
