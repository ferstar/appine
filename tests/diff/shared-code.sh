# 同一个程序的几个进程共用 Tier 1 的翻译（docs/rounds/108-shared-translation.md）。共用的
# 前提是它们在同样的地址映射着同样的代码，下面几种情形里两个进程在同一个地址上放了不同的
# 代码，各自执行到的必须是自己的：
#   files    父子进程在同一个地址映射两个不同的文件
#   jit      父子进程在同一个地址各写各的代码（可写可执行的匿名映射）
#   rewrite  父子进程先映射同一个文件，一起执行，之后子进程改写了自己那一份
#   exec     子进程 exec 同一个程序，在同一个地址映射另一个文件
#   dlopen   几个进程按不同的顺序 dlopen 两个库，同一个地址上是不同的库
#   alone    子进程在一个地址映射一个文件、执行之后退出，组里只剩父进程，它在同一个地址映射
#            另一个文件（各自一棵进程树：主进程在的时候组里不止它一个）
# 文件的 ctime 离现在超过两秒才算可信（才能共用翻译），所以编译、写好文件之后先等一会儿
set -e
cd /tmp
mkdir -p shared && cd shared
cat > shared.c <<'C'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

typedef int (*fn)(void);

/* movz w0, #n; ret */
static void code(uint32_t *p, int n)
{
    p[0] = 0x52800000u | (uint32_t)n << 5;
    p[1] = 0xd65f03c0u;
}

static void *at(int i)
{
    return (void *)(uintptr_t)(0x500000000000ull + (uint64_t)i * 0x100000);
}

static fn map_file(const char *path, int i)
{
    int fd = open(path, O_RDONLY);
    void *p = mmap(at(i), 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 0);
    close(fd);
    return p == MAP_FAILED ? NULL : (fn)p;
}

static fn map_jit(int i, int n)
{
    void *p = mmap(at(i), 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (p == MAP_FAILED)
        return NULL;
    code(p, n);
    __builtin___clear_cache(p, (char *)p + 8);
    return (fn)p;
}

/* 调用多次，返回结果（每次都一样时），不一样时返回 -1 */
static int run(fn f)
{
    int first = f();
    for (int i = 0; i < 200000; i++)
        if (f() != first)
            return -1;
    return first;
}

/* 父子进程各自准备好、互相等到对方也准备好，再一起执行 */
static int pa[2], pb[2];
static void meet(int child)
{
    char c = 0;
    if (child) {
        write(pa[1], &c, 1);
        read(pb[0], &c, 1);
    } else {
        read(pa[0], &c, 1);
        write(pb[1], &c, 1);
    }
}

static int both(const char *what, fn (*prepare)(int child))
{
    pipe(pa);
    pipe(pb);
    pid_t pid = fork();
    if (pid == 0) {
        fn f = prepare(1);
        meet(1);
        _exit(f ? run(f) : 100);
    }
    fn f = prepare(0);
    meet(0);
    int mine = f ? run(f) : 100, status;
    waitpid(pid, &status, 0);
    printf("%s: child %d parent %d\n", what, WEXITSTATUS(status), mine);
    return 0;
}

static fn files(int child)
{
    return map_file(child ? "f1" : "f2", 0);
}

static fn jit(int child)
{
    return map_jit(1, child ? 3 : 4);
}

/* 每种情形在主进程 fork 出来的一个进程里跑：主进程自己不映射别的代码，一直可以共用翻译，
 * fork 出来的进程都加入它的组 */
static void in_child(void (*body)(void))
{
    pid_t pid = fork();
    if (pid == 0) {
        body();
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

static void case_files(void)
{
    both("files", files);
}

static void case_jit(void)
{
    both("jit", jit);
}

static char *self;

/* rewrite：先一起执行同一个文件，之后子进程改写自己的那一份再执行 */
static void case_rewrite(void)
{
    fn f = map_file("f1", 2);
    printf("rewrite: before %d\n", run(f));
    pipe(pa);
    pipe(pb);
    pid_t pid = fork();
    if (pid == 0) {
        int before = run(f);
        mprotect((void *)f, 4096, PROT_READ | PROT_WRITE | PROT_EXEC);
        code((uint32_t *)(void *)f, 9);
        __builtin___clear_cache((char *)f, (char *)f + 8);
        meet(1);
        _exit(before * 10 + run(f));
    }
    meet(0);
    int status, mine = run(f);
    waitpid(pid, &status, 0);
    printf("rewrite: child %d parent %d\n", WEXITSTATUS(status), mine);
}

/* exec：同一个程序的新地址空间在父进程映射着 f1 的地址上映射 f2 */
static void case_exec(void)
{
    fn f = map_file("f1", 3);
    pid_t pid = fork();
    if (pid == 0) {
        execl(self, self, "f2", "3", (char *)NULL);
        _exit(101);
    }
    int status, mine = run(f);
    waitpid(pid, &status, 0);
    printf("exec: child %d parent %d\n", WEXITSTATUS(status), mine);
}

/* dlopen：两个进程按不同的顺序加载两个库，同一个地址上是不同的库 */
static void case_dlopen(void)
{
    int results[2], status;
    for (int order = 0; order < 2; order++) {
        pid_t pid = fork();
        if (pid == 0) {
            void *a = dlopen(order ? "./libb.so" : "./liba.so", RTLD_NOW);
            void *b = dlopen(order ? "./liba.so" : "./libb.so", RTLD_NOW);
            fn va = a ? (fn)dlsym(a, "v") : NULL, vb = b ? (fn)dlsym(b, "v") : NULL;
            _exit(va && vb ? run(va) * 10 + run(vb) : 100);
        }
        waitpid(pid, &status, 0);
        results[order] = WEXITSTATUS(status);
    }
    printf("dlopen: %d %d\n", results[0], results[1]);
}

/* alone：子进程映射 f1、执行完退出之后，组里只剩开组的父进程，它在同一格映射 f2，离开组时
 * 留着组里的翻译：那一格里还有子进程翻译的 f1 的块 */
static void case_alone(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        fn f = map_file("f1", 4);
        _exit(f ? run(f) : 100);
    }
    int status;
    waitpid(pid, &status, 0);
    fn f = map_file("f2", 4);
    printf("alone: child %d parent %d\n", WEXITSTATUS(status), f ? run(f) : 100);
}

int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "alone")) {
        setvbuf(stdout, NULL, _IONBF, 0);
        case_alone();
        return 0;
    }
    if (argc == 3) { /* exec 出来的：在 argv[2] 那一格映射 argv[1] */
        fn f = map_file(argv[1], atoi(argv[2]));
        return f ? run(f) : 100;
    }
    self = argv[0];
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int round = 0; round < 3; round++) {
        in_child(case_files);
        in_child(case_jit);
        in_child(case_rewrite);
        in_child(case_exec);
        in_child(case_dlopen);
    }
    return 0;
}
C
gcc -O2 -o shared shared.c -ldl
printf 'int v(void) { return 1; }\n' > a.c
printf 'int v(void) { return 2; }\n' > b.c
gcc -O2 -shared -fPIC -o liba.so a.c
gcc -O2 -shared -fPIC -o libb.so b.c
# 两个只有一页代码的文件：movz w0, #1; ret 和 movz w0, #2; ret
printf '\040\000\200\122\300\003\137\326' > f1
printf '\100\000\200\122\300\003\137\326' > f2
truncate -s 4096 f1 f2
sleep 3
for i in 1 2 3; do ./shared alone; done
./shared
