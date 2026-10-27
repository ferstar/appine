# fork 之后父子进程的内存互不影响（写时复制）：几个子进程和父进程同时写；多线程的父进程
# 在别的线程不停写的时候 fork，子进程看到的是某一瞬间的快照；借出的内存解除映射、搬走、
# 丢弃；子进程结束之后借出的页恢复成各自的权限；子进程里用锁和条件变量；一层套一层的
# fork；python 的 os.fork
set -e
cd /tmp
cat > cow.c <<'C'
#define _GNU_SOURCE
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096
#define PAGES 2048 /* 8 MB */

static unsigned char *mem;

static int check(unsigned char *p, size_t pages, unsigned char want)
{
    for (size_t i = 0; i < pages * PAGE; i += 61)
        if (p[i] != want)
            return 1;
    return 0;
}

static int wait_all(int n)
{
    int bad = 0, st;
    for (int i = 0; i < n; i++) {
        wait(&st);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    return bad;
}

/* 几个子进程和父进程同时写：各自只看到自己的修改 */
static int concurrent_writers(void)
{
    memset(mem, 'p', PAGES * PAGE);
    int fds[2];
    pipe(fds);
    for (int c = 0; c < 4; c++) {
        if (fork() == 0) {
            char go;
            read(fds[0], &go, 1);
            /* 写一半的页，另一半必须还是 fork 时的内容 */
            for (int i = c % 2; i < PAGES; i += 2)
                mem[(size_t)i * PAGE + c] = 'a' + c;
            int bad = 0;
            for (int i = 0; i < PAGES; i++) {
                unsigned char want = (i % 2 == c % 2) ? 'a' + c : 'p';
                bad += mem[(size_t)i * PAGE + c] != want;
                bad += mem[(size_t)i * PAGE + 100] != 'p';
            }
            _exit(bad != 0);
        }
    }
    for (int c = 0; c < 4; c++)
        write(fds[1], "g", 1);
    /* 父进程一边写，子进程一边检查 */
    for (int i = 0; i < PAGES; i += 3)
        mem[(size_t)i * PAGE + 100] = 'q';
    int bad = wait_all(4);
    for (int i = 0; i < PAGES; i++)
        bad += mem[(size_t)i * PAGE + 100] != (i % 3 ? 'p' : 'q') || mem[(size_t)i * PAGE] != 'p';
    close(fds[0]);
    close(fds[1]);
    return bad;
}

/* 线程按顺序把计数写进每一页，再加一：任何一瞬间前面若干页是 v、后面的是 v - 1 */
static atomic_int stop;
static unsigned *bufs[3];
#define TPAGES 64

static void *scribbler(void *arg)
{
    unsigned *b = arg;
    for (unsigned v = 1; !atomic_load(&stop); v++)
        for (int i = 0; i < TPAGES; i++)
            b[i * (PAGE / 4)] = v;
    return NULL;
}

static int consistent(unsigned *b)
{
    unsigned first = b[0];
    int i = 1;
    while (i < TPAGES && b[i * (PAGE / 4)] == first)
        i++;
    while (i < TPAGES && b[i * (PAGE / 4)] == first - 1)
        i++;
    return i == TPAGES;
}

static int threads_and_fork(void)
{
    pthread_t t[3];
    for (int i = 0; i < 3; i++) {
        bufs[i] = calloc(TPAGES, PAGE);
        pthread_create(&t[i], NULL, scribbler, bufs[i]);
    }
    int bad = 0;
    for (int round = 0; round < 40; round++) {
        pid_t pid = fork();
        if (pid == 0) {
            int ok = 1;
            for (int i = 0; i < 3; i++) {
                ok &= consistent(bufs[i]);
                /* 子进程自己写：快照不受父进程的线程影响 */
                unsigned snap = bufs[i][0];
                usleep(2000);
                ok &= bufs[i][0] == snap;
                bufs[i][0] = 0;
            }
            _exit(!ok);
        }
        int st;
        waitpid(pid, &st, 0);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < 3; i++) {
        pthread_join(t[i], NULL);
        bad += !consistent(bufs[i]);
    }
    return bad;
}

/* 借出的内存被父进程解除映射、mremap 搬走、MADV_DONTNEED：子进程的内容不变 */
static int unmap_move_discard(void)
{
    size_t len = 64 * PAGE;
    unsigned char *a = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char *b = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char *c = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(a, 'a', len);
    memset(b, 'b', len);
    memset(c, 'c', len);
    int fds[2], back[2];
    pipe(fds);
    pipe(back);
    pid_t pid = fork();
    if (pid == 0) {
        char x;
        read(fds[0], &x, 1);
        int bad = check(a, 64, 'a') + check(b, 64, 'b') + check(c, 64, 'c');
        /* 子进程也改一改：父进程那边不受影响 */
        mprotect(c, len, PROT_READ);
        mprotect(c, len, PROT_READ | PROT_WRITE);
        memset(c, 'C', len / 2);
        madvise(b, len / 2, MADV_DONTNEED);
        bad += b[0] != 0 || b[len - 1] != 'b';
        write(back[1], "d", 1);
        _exit(bad != 0);
    }
    munmap(a, len / 2);
    unsigned char *moved = mremap(b, len, 2 * len, MREMAP_MAYMOVE);
    madvise(c + len / 2, len / 2, MADV_DONTNEED);
    /* 解除映射的地方重新映射、写上别的内容 */
    unsigned char *again =
        mmap(a, len / 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    memset(again, 'z', len / 2);
    memset(moved, 'm', len);
    write(fds[1], "g", 1);
    char x;
    read(back[0], &x, 1);
    int bad = wait_all(1);
    bad += check(c, 32, 'c') + (c[len - 1] != 0) + check(moved, 64, 'm');
    return bad;
}

/* 子进程结束、还回借来的页之后，父进程的页恢复成各自所在映射的权限。一次 mmap 用
 * mprotect 切成的几段宿主内存是连着的，不能按第一段的权限一起恢复：保护页的 PROT_NONE
 * 会让后面的页都访问不了，可写的一段会让后面只读的一段也能写 */
static sigjmp_buf probe_jmp;

static void probe_fault(int sig)
{
    (void)sig;
    siglongjmp(probe_jmp, 1);
}

static int probe(volatile unsigned char *p, int write)
{
    if (sigsetjmp(probe_jmp, 1))
        return 0;
    if (write)
        *p = 'w';
    else
        (void)*p;
    return 1;
}

static void returned_loans(void)
{
    /* 保护页、两页可写、一页只读；一页可写、一页只读；最后像线程的信号栈那样，一段开头
     * 是没写过的保护页的映射，接在上一段的下面 */
    int flags = MAP_PRIVATE | MAP_ANONYMOUS;
    unsigned char *m = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, flags, -1, 0);
    unsigned char *n = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, flags, -1, 0);
    unsigned char *s = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE, flags | MAP_STACK, -1, 0);
    memset(m, 'r', 4 * PAGE);
    memset(n, 'r', 2 * PAGE);
    mprotect(m, PAGE, PROT_NONE);
    mprotect(m + 3 * PAGE, PAGE, PROT_READ);
    mprotect(n + PAGE, PAGE, PROT_READ);
    mprotect(s, PAGE, PROT_NONE);
    memset(s + PAGE, 's', 3 * PAGE);
    pid_t pid = fork();
    if (pid == 0)
        _exit(0);
    wait_all(1);
    struct sigaction sa = {.sa_handler = probe_fault}, old;
    sigaction(SIGSEGV, &sa, &old);
    printf("returned loans: guard read %d, rw write %d %d, ro read %d write %d; rw write %d, ro write %d; "
           "stack guard read %d, stack write %d\n",
           probe(m, 0), probe(m + PAGE, 1), probe(m + 2 * PAGE, 1), probe(m + 3 * PAGE, 0),
           probe(m + 3 * PAGE, 1), probe(n, 1), probe(n + PAGE, 1), probe(s, 0), probe(s + PAGE, 1));
    sigaction(SIGSEGV, &old, NULL);
}

/* 子进程里的锁和条件变量（futex 在 fork 时借来的页上） */
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int turn;

static void *pingpong(void *arg)
{
    int me = (int)(long)arg;
    for (int i = 0; i < 2000; i++) {
        pthread_mutex_lock(&lock);
        while (turn % 2 != me)
            pthread_cond_wait(&cond, &lock);
        turn++;
        pthread_cond_broadcast(&cond);
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static int locks_in_child(void)
{
    pthread_mutex_lock(&lock);
    turn = 0;
    pthread_mutex_unlock(&lock);
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t t[2];
        for (long i = 0; i < 2; i++)
            pthread_create(&t[i], NULL, pingpong, (void *)i);
        for (int i = 0; i < 2; i++)
            pthread_join(t[i], NULL);
        _exit(turn != 4000);
    }
    return wait_all(1);
}

/* 一层套一层：每一层都改一页，再看上面几层的内容 */
static int nested(int depth)
{
    mem[(size_t)depth * PAGE] = 'n';
    if (depth == 6) {
        for (int i = 0; i <= 6; i++)
            if (mem[(size_t)i * PAGE] != 'n')
                return 1;
        return mem[7 * PAGE] != 'p';
    }
    pid_t pid = fork();
    if (pid == 0)
        _exit(nested(depth + 1));
    mem[(size_t)(depth + 1) * PAGE] = 'x';
    int st;
    waitpid(pid, &st, 0);
    return !WIFEXITED(st) || WEXITSTATUS(st) != 0 || mem[(size_t)depth * PAGE] != 'n';
}

int main(void)
{
    mem = malloc(PAGES * PAGE);
    printf("concurrent writers: %d bad\n", concurrent_writers());
    printf("threads and fork: %d bad\n", threads_and_fork());
    printf("unmap, move, discard: %d bad\n", unmap_move_discard());
    returned_loans();
    printf("locks in child: %d bad\n", locks_in_child());
    memset(mem, 'p', PAGES * PAGE);
    printf("nested: %d bad\n", nested(0));
    return 0;
}
C
gcc -O2 -o cow cow.c -pthread
./cow
python3 -c '
import os
data = bytearray(b"p" * (1 << 22))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.read(r, 1)
    ok = data[::4096] == b"p" * 1024
    data[0:10] = b"c" * 10
    os._exit(0 if ok else 1)
data[:] = b"q" * (1 << 22)
os.write(w, b"g")
_, st = os.waitpid(pid, 0)
print("python fork:", os.WEXITSTATUS(st), bytes(data[:4]))
'
