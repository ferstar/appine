/*
 * 内核层和引擎的 bug 只结束撞上它的 guest 进程（设计文档 §7.4、§12 第 5 条）。bug 用
 * debug 构建的测试钩子模拟：appine-run 要设置 APPINE_TEST_HOOKS=1 和 APPINE_ON_BUG=kill。
 * 每种 bug 都在子进程里触发，父进程检查子进程被 SIGSEGV 结束，自己照常运行。
 *
 * 带参数时只为宿主调用 Machine 的方法布下 bug（Machine 的测试用）：
 * host-call-then-exit 布下之后退出，host-call-then-read 布下之后读 stdin。
 */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_PANIC 1           /* 内核层 panic */
#define HOOK_BREAK_PAGE 2      /* 让一页在页表里指向不能访问的宿主内存 */
#define HOOK_PANIC_ON_EXIT 3   /* 调用的线程退出时 panic */
#define HOOK_PANIC_ON_START 4  /* 这个进程下一个开始运行的线程 panic */
#define HOOK_PANIC_IN_TIMERS 5 /* 定时器线程处理这个进程时 panic */
#define HOOK_PANIC_IN_HOST 6   /* 宿主下一次调用 Machine 的方法时 panic */

static void fail(const char *what)
{
    printf("FAIL: %s\n", what);
    exit(1);
}

static void expect_segv(pid_t pid, const char *what)
{
    int status;
    if (waitpid(pid, &status, 0) != pid)
        fail(what);
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
        printf("%s: status %#x\n", what, status);
        fail(what);
    }
    printf("%s: killed by SIGSEGV\n", what);
}

/* 在 p 所在的页上模拟引擎的 bug，再访问它 */
static void touch_broken_page(void)
{
    volatile long *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        _exit(2);
    *p = 1;
    if (syscall(TEST_SYSCALL, HOOK_BREAK_PAGE, p) != 0)
        _exit(3);
    *p = 2;
    _exit(4);
}

static int pipefd[2];

static void *blocked_reader(void *arg)
{
    char c;
    (void)arg;
    if (read(pipefd[0], &c, 1) != 0)
        _exit(5);
    _exit(6);
}

static void *worker(void *arg)
{
    return arg;
}

static void *exit_with_bug(void *arg)
{
    if (syscall(TEST_SYSCALL, HOOK_PANIC_ON_EXIT) != 0)
        _exit(10);
    return arg;
}

static volatile sig_atomic_t alarmed;

static void on_alarm(int sig)
{
    (void)sig;
    alarmed = 1;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);

    if (argc > 1) {
        if (syscall(TEST_SYSCALL, HOOK_PANIC_IN_HOST) != 0)
            return 2;
        if (strcmp(argv[1], "host-call-then-read") == 0) {
            char c;
            read(0, &c, 1);
        }
        return 0;
    }

    pid_t pid = fork();
    if (pid == 0) {
        syscall(TEST_SYSCALL, HOOK_PANIC);
        _exit(7);
    }
    expect_segv(pid, "kernel panic");

    pid = fork();
    if (pid == 0)
        touch_broken_page();
    expect_segv(pid, "engine fault");

    /* 多线程：另一个线程正阻塞在内核里，整个进程照样被结束 */
    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (pipe(pipefd) != 0 || pthread_create(&t, NULL, blocked_reader, NULL) != 0)
            _exit(8);
        usleep(20000);
        touch_broken_page();
    }
    expect_segv(pid, "engine fault with another thread");

    /*
     * 线程退出时：进程的最后一个线程（用 SYS_exit：exit_group 已经定下了进程的退出码，
     * 与 Linux 一样之后的 oops 不再改它），以及另一个线程还在 pthread_join 里等它
     */
    pid = fork();
    if (pid == 0) {
        exit_with_bug(NULL);
        syscall(SYS_exit, 11);
    }
    expect_segv(pid, "panic while exiting");

    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (pthread_create(&t, NULL, exit_with_bug, NULL) != 0)
            _exit(12);
        pthread_join(t, NULL);
        _exit(13);
    }
    expect_segv(pid, "panic while a thread exits");

    /* 新线程开始运行时 */
    pid = fork();
    if (pid == 0) {
        pthread_t t;
        if (syscall(TEST_SYSCALL, HOOK_PANIC_ON_START) != 0 ||
            pthread_create(&t, NULL, worker, NULL) != 0)
            _exit(14);
        pthread_join(t, NULL);
        _exit(15);
    }
    expect_segv(pid, "panic while a thread starts");

    /* 定时器线程处理这个进程时 */
    pid = fork();
    if (pid == 0) {
        if (syscall(TEST_SYSCALL, HOOK_PANIC_IN_TIMERS) != 0)
            _exit(16);
        for (;;)
            pause();
    }
    expect_segv(pid, "panic in the timer thread");

    /* 撞上 bug 之后，线程、fork 照常可用 */
    pthread_t t;
    void *r;
    if (pthread_create(&t, NULL, worker, (void *)42) != 0 || pthread_join(t, &r) != 0 || r != (void *)42)
        fail("threads after the bugs");
    pid = fork();
    if (pid == 0)
        _exit(9);
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 9)
        fail("fork after the bugs");
    /* 定时器线程还在 */
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGALRM);
    sigprocmask(SIG_BLOCK, &block, &old);
    signal(SIGALRM, on_alarm);
    ualarm(20000, 0);
    while (!alarmed)
        sigsuspend(&old);
    printf("timers after the bugs: ok\n");
    printf("all ok\n");
    return 0;
}
