/*
 * App 内存的上限和线程数的限制（设计文档 §12 第 4 条）。appine-run 要带 --memory-limit
 * 运行，上限远小于 1 GB。
 */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *what)
{
    printf("FAIL: %s (errno %d)\n", what, errno);
    exit(1);
}

/* fork 出一个停在 pause 里的子进程；失败时返回 -1 */
static pid_t spawn_paused(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        pause();
        _exit(0);
    }
    return pid;
}

static void reap(const pid_t *kids, int n)
{
    for (int i = 0; i < n; i++) {
        kill(kids[i], SIGKILL);
        waitpid(kids[i], NULL, 0);
    }
}

static int nproc_child(void)
{
    struct rlimit rl = {3, 3};
    pid_t kids[4];
    if (setrlimit(RLIMIT_NPROC, &rl) != 0)
        return 10;
    /* root 不受它限制 */
    for (int i = 0; i < 4; i++) {
        if ((kids[i] = spawn_paused()) < 0)
            return 11;
    }
    reap(kids, 4);
    /* 这个用户现在只有自己一个线程：再 fork 两个就到上限 */
    if (setuid(65534) != 0)
        return 12;
    int n = 0;
    while (n < 4 && (kids[n] = spawn_paused()) >= 0)
        n++;
    int err = errno;
    reap(kids, n);
    if (n != 2)
        return 13;
    return err == EAGAIN ? 0 : 14;
}

/* RLIMIT_NPROC：有默认值；普通用户的线程数到了上限时 fork 返回 EAGAIN */
static void nproc(void)
{
    struct rlimit rl;
    if (getrlimit(RLIMIT_NPROC, &rl) != 0 || rl.rlim_cur == RLIM_INFINITY || rl.rlim_cur == 0)
        fail("default RLIMIT_NPROC");
    pid_t pid = fork();
    if (pid == 0)
        _exit(nproc_child());
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("status %#x\n", status);
        fail("RLIMIT_NPROC");
    }
    printf("RLIMIT_NPROC ok\n");
}

#define CHUNK (16 << 20)

/* 分配并碰 bytes 字节的内存。每块之后歇一会儿：OOM killer 每隔几十毫秒才量一次 */
static void hog(size_t bytes, unsigned pause_us)
{
    for (size_t total = 0; total < bytes; total += CHUNK) {
        char *p = mmap(NULL, CHUNK, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED)
            _exit(2);
        for (size_t i = 0; i < CHUNK; i += 4096)
            p[i] = 1;
        usleep(pause_us);
    }
}

/* 超出内存上限：用得最多的进程被 SIGKILL 结束，其余的照常运行 */
static void oom(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        hog((size_t)1 << 30, 20000);
        _exit(1);
    }
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) {
        printf("status %#x\n", status);
        fail("out of memory");
    }
    printf("out of memory: killed by SIGKILL\n");
    /* 它的内存还回来了：这里再用一些也没事 */
    hog(32 << 20, 0);
    usleep(300000);
    printf("memory is back\n");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    nproc();
    oom();
    printf("all ok\n");
    return 0;
}
