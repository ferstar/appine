# ptrace 和 process_vm_readv/writev：一个小跟踪器逐项检查停住、继续、事件和寄存器，再用
# strace 跟踪几个程序
set -e
cd /tmp
cat > tracer.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef NT_ARM_SYSTEM_CALL
#define NT_ARM_SYSTEM_CALL 0x404
#endif
#ifndef NT_ARM_HW_BREAK
#define NT_ARM_HW_BREAK 0x402
#endif

/* aarch64 的 user_pt_regs */
struct regs {
    unsigned long long x[31], sp, pc, pstate;
};

static const char *describe(int st)
{
    static char b[64];
    if (WIFEXITED(st))
        snprintf(b, sizeof b, "exited %d", WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        snprintf(b, sizeof b, "killed by %d", WTERMSIG(st));
    else if (WIFSTOPPED(st))
        snprintf(b, sizeof b, "stopped %#x event %d", WSTOPSIG(st), st >> 16);
    else
        snprintf(b, sizeof b, "status %#x", st);
    return b;
}

static int wait_for(pid_t pid)
{
    int st;
    if (waitpid(pid, &st, __WALL) != pid) {
        perror("waitpid");
        exit(1);
    }
    return st;
}

static void regs(pid_t pid, struct regs *r, int set)
{
    struct iovec iov = {r, sizeof *r};
    if (ptrace(set ? PTRACE_SETREGSET : PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) || iov.iov_len != sizeof *r)
        perror("regset");
}

/* /proc/<pid>/status 的 State 和 TracerPid 是不是自己；在运行的进程的状态取决于调度，
   不打印 */
static void status(pid_t pid, int show_state)
{
    char path[64], line[128], state[64] = "";
    int tracer = -1;
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f)) {
        sscanf(line, "State: %63[^\n]", state);
        sscanf(line, "TracerPid: %d", &tracer);
    }
    fclose(f);
    printf("  state %s, traced by %s\n", show_state ? state : "-",
           tracer == getpid() ? "me" : tracer == 0 ? "nobody" : "other");
}

/* 系统调用的入口和出口：GET_SYSCALL_INFO、x7、改返回值、跳过系统调用，改了返回值就
   PTRACE_DETACH */
static void syscalls(void)
{
    printf("== syscall stops\n");
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        long a = syscall(SYS_getppid);
        long b = syscall(SYS_kill, 0, 0);
        long c = syscall(SYS_getppid);
        long d = syscall(SYS_getppid);
        printf("  child sees getppid %ld, kill %ld, then getppid %s, after detach %ld\n", a, b,
               c == getppid() ? "real" : "changed", d);
        _exit(5);
    }
    int st = wait_for(pid);
    printf("first stop: %s\n", describe(st));
    siginfo_t si;
    long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("  siginfo %ld signo %d code %d from child %d\n", r, si.si_signo, si.si_code, si.si_pid == pid);
    status(pid, 1);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);
    struct __ptrace_syscall_info info;
    r = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
    printf("  info in a signal stop: size %ld op %d arch %#x\n", r, info.op, info.arch);
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    long last = -2;
    int getppids = 0;
    for (;;) {
        st = wait_for(pid);
        if (!WIFSTOPPED(st)) {
            printf("end: %s\n", describe(st));
            break;
        }
        if (WSTOPSIG(st) != (SIGTRAP | 0x80)) {
            printf("unexpected: %s\n", describe(st));
            ptrace(PTRACE_SYSCALL, pid, 0, 0);
            continue;
        }
        r = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
        unsigned long msg;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        struct regs g;
        regs(pid, &g, 0);
        if (info.op == PTRACE_SYSCALL_INFO_ENTRY) {
            last = info.entry.nr;
            if (last == SYS_kill) {
                printf("enter kill(%lld, %lld): size %ld message %lu x7 %llu sp %s\n",
                       (long long)info.entry.args[0], (long long)info.entry.args[1], r, msg, g.x[7],
                       info.stack_pointer == g.sp ? "ok" : "differs");
                /* 跳过它：编号改成 -1，返回值 77 */
                int no = -1;
                struct iovec iov = {&no, sizeof no};
                ptrace(PTRACE_SETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov);
                g.x[0] = 77;
                regs(pid, &g, 1);
            }
        } else if (info.op == PTRACE_SYSCALL_INFO_EXIT) {
            if (last == SYS_getppid && getppids++ == 0) {
                printf("exit getppid: size %ld message %lu x7 %llu rval is parent %d error %d\n", r, msg, g.x[7],
                       info.exit.rval == getpid(), info.exit.is_error);
                g.x[0] = 4242;
                g.x[7] = 99; /* 跟踪者改不了 x7 */
                regs(pid, &g, 1);
            } else if (last == SYS_getppid && getppids == 3) {
                g.x[0] = 1234;
                regs(pid, &g, 1);
                printf("detach %ld\n", ptrace(PTRACE_DETACH, pid, 0, 0));
                printf("end: %s\n", describe(wait_for(pid)));
                break;
            } else if (last == SYS_kill) {
                printf("exit of the skipped kill: rval %lld error %d\n", (long long)info.exit.rval, info.exit.is_error);
            }
        }
        ptrace(PTRACE_SYSCALL, pid, 0, 0);
    }
}

/* PTRACE_O_TRACEEXEC，以及不用它时 execve 之后的 SIGTRAP */
static void exec_events(void)
{
    printf("== exec\n");
    for (int opt = 0; opt < 2; opt++) {
        pid_t pid = fork();
        if (pid == 0) {
            ptrace(PTRACE_TRACEME, 0, 0, 0);
            raise(SIGSTOP);
            execl("/bin/true", "true", (char *)0);
            _exit(1);
        }
        wait_for(pid);
        ptrace(PTRACE_SETOPTIONS, pid, 0, opt ? PTRACE_O_TRACEEXEC : 0);
        ptrace(PTRACE_CONT, pid, 0, 0);
        int st = wait_for(pid);
        unsigned long msg = 1;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        siginfo_t si;
        long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
        printf("%s: %s, message %s, siginfo %ld code %#x\n", opt ? "TRACEEXEC" : "plain", describe(st),
               msg == (unsigned long)pid ? "pid" : msg ? "other" : "0", r, si.si_code);
        ptrace(PTRACE_CONT, pid, 0, 0);
        printf("  then %s\n", describe(wait_for(pid)));
    }
}

/* PTRACE_SEIZE、PTRACE_INTERRUPT、fork 之后跟进新进程、PTRACE_EVENT_EXIT */
static void seize_fork(void)
{
    printf("== seize and fork\n");
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        pid_t g = fork();
        if (g == 0)
            _exit(7);
        int st;
        waitpid(g, &st, 0);
        printf("  child: grandchild %s\n", describe(st));
        exit(9);
    }
    printf("seize %ld\n", ptrace(PTRACE_SEIZE, pid, 0, PTRACE_O_TRACEFORK | PTRACE_O_TRACEEXIT));
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    int st = wait_for(pid);
    printf("interrupt: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, 0);
    write(go[1], "x", 1);
    st = wait_for(pid);
    unsigned long g = 0;
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &g);
    printf("fork: %s\n", describe(st));
    st = wait_for(g);
    printf("grandchild starts: %s\n", describe(st));
    status(g, 1);
    ptrace(PTRACE_CONT, g, 0, 0);
    st = wait_for(g);
    unsigned long msg = 0;
    ptrace(PTRACE_GETEVENTMSG, g, 0, &msg);
    printf("grandchild exiting: %s", describe(st));
    printf(", status %s\n", describe(msg));
    ptrace(PTRACE_CONT, g, 0, 0);
    printf("grandchild: %s\n", describe(wait_for(g)));
    /* 孙进程退出了，子进程收到 SIGCHLD：被跟踪的线程先停下来让跟踪者看 */
    ptrace(PTRACE_CONT, pid, 0, 0);
    while (st = wait_for(pid), WIFSTOPPED(st) && st >> 16 == 0) {
        printf("child gets a signal: %s\n", describe(st));
        ptrace(PTRACE_CONT, pid, 0, WSTOPSIG(st));
    }
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
    printf("child exiting: %s", describe(st));
    printf(", status %s\n", describe(msg));
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("child: %s\n", describe(wait_for(pid)));
}

/* group-stop：SEIZE 时报告成 PTRACE_EVENT_STOP，PTRACE_LISTEN 等 SIGCONT；ATTACH 时
   group-stop 没有 siginfo */
static void group_stop(void)
{
    printf("== group stop\n");
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(3);
    }
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    kill(pid, SIGSTOP);
    int st = wait_for(pid);
    printf("signal: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    st = wait_for(pid);
    siginfo_t si;
    long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("group stop: %s, siginfo %ld\n", describe(st), r);
    printf("listen %ld\n", ptrace(PTRACE_LISTEN, pid, 0, 0));
    status(pid, 1);
    errno = 0;
    r = ptrace(PTRACE_CONT, pid, 0, 0);
    printf("cont while listening %ld %s\n", r, strerror(errno));
    kill(pid, SIGCONT);
    st = wait_for(pid);
    printf("after SIGCONT: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, 0);
    st = wait_for(pid);
    printf("then: %s\n", describe(st));
    ptrace(PTRACE_CONT, pid, 0, SIGCONT);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));

    pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(4);
    }
    printf("attach %ld\n", ptrace(PTRACE_ATTACH, pid, 0, 0));
    st = wait_for(pid);
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("attach stop: %s, siginfo %ld signo %d\n", describe(st), r, si.si_signo);
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    st = wait_for(pid);
    errno = 0;
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("group stop: %s, siginfo %ld %s\n", describe(st), r, strerror(errno));
    ptrace(PTRACE_CONT, pid, 0, 0);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));
}

/* 被跟踪的进程忽略的信号也挂起，跟踪者看得到它 */
static void ignored(void)
{
    printf("== ignored signal\n");
    int go[2], ready[2];
    pipe(go);
    pipe(ready);
    pid_t pid = fork();
    if (pid == 0) {
        signal(SIGUSR1, SIG_IGN);
        write(ready[1], "r", 1);
        char c;
        read(go[0], &c, 1);
        _exit(6);
    }
    char c;
    read(ready[0], &c, 1);
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    kill(pid, SIGUSR1);
    printf("ignored SIGUSR1: %s\n", describe(wait_for(pid)));
    ptrace(PTRACE_CONT, pid, 0, SIGUSR1);
    write(go[1], "x", 1);
    printf("end: %s\n", describe(wait_for(pid)));
}

static void *thread_main(void *arg)
{
    (void)arg;
    return 0;
}

/* PTRACE_O_TRACECLONE：新线程挂着 SIGSTOP 开始，退出时单独报告 */
static void threads(void)
{
    printf("== threads\n");
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        pthread_t t;
        pthread_create(&t, 0, thread_main, 0);
        pthread_join(t, 0);
        _exit(0);
    }
    wait_for(pid);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACECLONE);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st = wait_for(pid);
    unsigned long tid = 0;
    ptrace(PTRACE_GETEVENTMSG, pid, 0, &tid);
    printf("clone: %s, new thread is %s\n", describe(st), tid > (unsigned long)pid ? "after the process" : "?");
    st = wait_for(tid);
    printf("thread starts: %s\n", describe(st));
    ptrace(PTRACE_CONT, tid, 0, 0);
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("thread: %s\n", describe(wait_for(tid)));
    printf("process: %s\n", describe(wait_for(pid)));
}

/* PTRACE_O_EXITKILL：跟踪者退出时被跟踪的进程被结束 */
static void exitkill(void)
{
    printf("== exitkill\n");
    int alive[2];
    pipe(alive);
    pid_t tracer = fork();
    if (tracer == 0) {
        close(alive[0]);
        pid_t victim = fork();
        if (victim == 0) {
            pause();
            _exit(0);
        }
        close(alive[1]);
        ptrace(PTRACE_SEIZE, victim, 0, PTRACE_O_EXITKILL);
        _exit(0);
    }
    close(alive[1]);
    waitpid(tracer, 0, 0);
    struct pollfd p = {alive[0], POLLIN, 0};
    char c;
    int n = poll(&p, 1, 5000);
    printf("victim %s\n", n == 1 && read(alive[0], &c, 1) == 0 ? "killed" : "still running");
}

static char message[32] = "hello from the child";

/* process_vm_readv/writev 和 PTRACE_PEEKDATA：fork 之后子进程里同一个地址 */
static void memory(void)
{
    printf("== memory\n");
    int go[2], back[2];
    pipe(go);
    pipe(back);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        write(back[1], message, sizeof message);
        _exit(0);
    }
    char buf[32] = {0};
    struct iovec local = {buf, 5}, remote = {message + 6, 5};
    printf("readv %zd '%.5s'\n", process_vm_readv(pid, &local, 1, &remote, 1, 0), buf);
    struct iovec from = {"HELLO", 5}, to = {message, 5};
    printf("writev %zd\n", process_vm_writev(pid, &from, 1, &to, 1, 0));
    struct iovec two[2] = {{message, 4}, {(void *)16, 4}}, eight = {buf, 8};
    printf("second piece unmapped %zd\n", process_vm_readv(pid, &eight, 1, two, 2, 0));
    errno = 0;
    printf("unmapped %zd %s\n", process_vm_readv(pid, &eight, 1, &two[1], 1, 0), strerror(errno));
    errno = 0;
    printf("flags %zd %s\n", process_vm_readv(pid, &eight, 1, two, 1, 1), strerror(errno));
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid);
    errno = 0;
    long word = ptrace(PTRACE_PEEKDATA, pid, message, 0);
    printf("peek '%.8s' %s\n", (char *)&word, strerror(errno));
    errno = 0;
    ptrace(PTRACE_PEEKDATA, pid, (void *)16, 0);
    printf("peek unmapped %s\n", strerror(errno));
    status(pid, 1);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    status(pid, 0);
    write(go[1], "x", 1);
    read(back[0], buf, sizeof buf);
    printf("child now says '%s'\n", buf);
    waitpid(pid, 0, 0);
}

static void __attribute__((noinline)) steps(void)
{
    asm volatile("nop\n nop\n mov x9, #5\n add x9, x9, #1\n" ::: "x9");
}

static long __attribute__((noinline)) do_svc(void)
{
    register long x0 asm("x0");
    asm volatile("mov x8, #172\n svc #0\n nop\n" : "=r"(x0)::"x8", "memory");
    return x0;
}

/* 单步到下一个 SIGTRAP，打印 pc 相对 `base` 的位置和 siginfo */
static struct regs step(pid_t pid, void *base)
{
    struct regs r;
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    int st = wait_for(pid);
    regs(pid, &r, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("  step: %s, pc %+lld, code %d, addr is pc %d\n", describe(st), (long long)(r.pc - (unsigned long)base),
           si.si_code, si.si_addr == (void *)r.pc);
    return r;
}

/* 断点（PTRACE_POKETEXT 和 /proc/<pid>/mem 写进 brk）、PTRACE_SINGLESTEP（普通指令和 svc）、
   /proc/<pid>/auxv，以及不能强制写入的只读共享映射 */
static void breakpoints(void)
{
    printf("== breakpoints and single step\n");
    int fd = open("shared.tmp", O_RDWR | O_CREAT | O_TRUNC, 0600);
    ftruncate(fd, 4096);
    char *shared = mmap(0, 4096, PROT_READ, MAP_SHARED, fd, 0);
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        /* 先执行过：Tier 1 已经翻译了它，写进断点之后要作废 */
        for (int i = 0; i < 100; i++)
            steps();
        raise(SIGSTOP);
        steps();
        long r = do_svc();
        _exit(r == getpid() ? 0 : 1);
    }
    wait_for(pid);
    const unsigned brk = 0xd4200000;
    errno = 0;
    long word = ptrace(PTRACE_PEEKTEXT, pid, steps, 0);
    long patched = (word & ~0xffffffffL) | brk;
    printf("poke code %ld\n", ptrace(PTRACE_POKETEXT, pid, steps, patched));
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st = wait_for(pid);
    struct regs r;
    regs(pid, &r, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("breakpoint: %s, at steps %d, code %d, addr %d\n", describe(st), r.pc == (unsigned long)steps,
           si.si_code, si.si_addr == (void *)steps);
    ptrace(PTRACE_POKETEXT, pid, steps, word);
    for (int i = 0; i < 4; i++)
        r = step(pid, steps);
    printf("  x9 %llu\n", r.x[9]);

    /* 经由 /proc/<pid>/mem 在 do_svc 下断点 */
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/mem", pid);
    int mem = open(path, O_RDWR);
    unsigned insn = 0, back = 0;
    printf("mem read %zd\n", pread(mem, &insn, 4, (off_t)(unsigned long)do_svc));
    printf("mem write %zd\n", pwrite(mem, &brk, 4, (off_t)(unsigned long)do_svc));
    pread(mem, &back, 4, (off_t)(unsigned long)do_svc);
    printf("  reads back brk %d\n", back == brk);
    errno = 0;
    printf("mem unmapped %zd %s\n", pread(mem, &back, 4, 16), strerror(errno));
    ptrace(PTRACE_CONT, pid, 0, 0);
    st = wait_for(pid);
    regs(pid, &r, 0);
    printf("breakpoint: %s, at do_svc %d\n", describe(st), r.pc == (unsigned long)do_svc);
    pwrite(mem, &insn, 4, (off_t)(unsigned long)do_svc);
    for (int i = 0; i < 3; i++)
        r = step(pid, do_svc);
    printf("  x0 is the pid %d\n", r.x[0] == (unsigned long long)pid);

    /* 只读的共享映射不能强制写入 */
    errno = 0;
    printf("poke read-only shared %ld %s\n", ptrace(PTRACE_POKEDATA, pid, shared, 0L), strerror(errno));
    errno = 0;
    printf("mem write read-only shared %zd %s\n", pwrite(mem, "x", 1, (off_t)(unsigned long)shared),
           strerror(errno));
    close(mem);

    snprintf(path, sizeof path, "/proc/%d/auxv", pid);
    int f = open(path, O_RDONLY);
    unsigned long aux[128] = {0};
    ssize_t n = read(f, aux, sizeof aux);
    close(f);
    int entry = 0, pagesz = 0, last = 0;
    for (ssize_t i = 0; i + 1 < n / 8; i += 2) {
        if (aux[i] == AT_ENTRY)
            entry = aux[i + 1] == getauxval(AT_ENTRY);
        if (aux[i] == AT_PAGESZ)
            pagesz = aux[i + 1] == 4096;
        last = aux[i] == AT_NULL;
    }
    printf("auxv: entry %d, page size %d, ends with AT_NULL %d\n", entry, pagesz, last);
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("end: %s\n", describe(wait_for(pid)));
}

static void errors(void)
{
    printf("== errors\n");
    errno = 0;
    ptrace(PTRACE_CONT, getpid(), 0, 0);
    printf("cont self: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_ATTACH, getpid(), 0, 0);
    printf("attach self: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_ATTACH, 999999, 0, 0);
    printf("attach missing: %s\n", strerror(errno));
    int go[2];
    pipe(go);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(0);
    }
    errno = 0;
    ptrace(PTRACE_SEIZE, pid, 0, 1 << 30);
    printf("seize with a bad option: %s\n", strerror(errno));
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    errno = 0;
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    printf("seize twice: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("cont while running: %s\n", strerror(errno));
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid);
    errno = 0;
    ptrace(PTRACE_SETOPTIONS, pid, 0, 1 << 30);
    printf("bad option: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_CONT, pid, 0, 99);
    printf("bad signal: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_PEEKUSER, pid, 0, 0);
    printf("peekuser: %s\n", strerror(errno));
    unsigned long long b[40];
    struct iovec iov = {b, 7};
    errno = 0;
    ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    printf("regset of 7 bytes: %s\n", strerror(errno));
    iov.iov_len = 16;
    printf("regset of 16 bytes: %ld, got %zu\n", ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov), iov.iov_len);
    iov.iov_len = sizeof b;
    printf("whole regset: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov), iov.iov_len));
    iov.iov_len = sizeof b;
    printf("fp regset: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_PRFPREG, &iov), iov.iov_len));
    iov.iov_len = sizeof b;
    printf("hw breakpoints: got %zu\n", (ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_BREAK, &iov), iov.iov_len));
    errno = 0;
    ptrace(PTRACE_GETREGSET, pid, 0x7777, &iov);
    printf("unknown regset: %s\n", strerror(errno));
    errno = 0;
    ptrace(PTRACE_GETSIGMASK, pid, 4, b);
    printf("sigmask of 4 bytes: %s\n", strerror(errno));
    ptrace(PTRACE_KILL, pid, 0, 0);
    printf("kill: %s\n", describe(wait_for(pid)));
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    /* 等不到该来的停止时不要一直卡着 */
    alarm(60);
    syscalls();
    exec_events();
    seize_fork();
    group_stop();
    ignored();
    threads();
    exitkill();
    memory();
    breakpoints();
    errors();
    return 0;
}
C
gcc -O1 -o tracer tracer.c -lpthread
./tracer

# strace：地址、pid、环境变量个数在两边不同，归一化之后比
norm() {
    sed -E 's/0x[0-9a-f]+/ADDR/g; s/\/\* [0-9]+ vars \*\//VARS/; s/(si_pid|si_uid)=[0-9]+/\1=N/g' |
        sed -E 's/^(getpid|getppid|gettid|set_tid_address|clone|wait4)\((.*)\) += [0-9]+$/\1(\2) = PID/' |
        sed -E 's/^tkill\([0-9]+/tkill(TID/'
}
cat > sig.c <<'C'
#include <signal.h>
#include <stdlib.h>
#include <unistd.h>

static void handler(int sig)
{
    (void)sig;
    write(1, "handled\n", 8);
}

int main(void)
{
    signal(SIGUSR1, handler);
    raise(SIGUSR1);
    raise(SIGUSR2);
    return 0;
}
C
gcc -O1 -o sig sig.c
echo "some text" > text
env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -o st1 cat text
norm < st1
echo "strace sig: $(env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -o st2 ./sig; echo $?)"
norm < st2
# -f：每个进程一个文件，按 pid 的顺序
rm -rf st3 && mkdir st3
env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -ff -e trace=process -e signal=none -o st3/t sh -c 'true; ls /dev/null >/dev/null; exit 2' || echo "strace -f: $?"
for f in $(ls st3 | sort -t. -k2 -n); do
    echo "-- process"
    norm < st3/$f
done

# 硬件观察点和断点（NT_ARM_HW_WATCH、NT_ARM_HW_BREAK）：槽数、非法的控制字、读回规范化的
# 控制字；写、读、不对齐的一个字节的观察点停在访问之前（数据还没写），si_code 是
# TRAP_HWBKPT、si_addr 是访问的地址；停用之后单步完成访问；断点停在指令上
cat > hw.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct dbg { uint32_t info, pad; struct { uint64_t addr; uint32_t ctrl, pad; } regs[16]; };

static volatile long data[4];
static volatile char bytes[16];

__attribute__((noinline)) static void store(volatile long *p, long v) { *p = v; }
__attribute__((noinline)) static long load(volatile long *p) { return *p; }
__attribute__((noinline)) static void store_byte(volatile char *p) { *p = 'x'; }
__attribute__((noinline)) static int target(int x) { return x + 1; }

static int setdbg(pid_t pid, int nt, int n, const uint64_t *addr, const uint32_t *ctrl)
{
    struct dbg d;
    memset(&d, 0, sizeof d);
    for (int i = 0; i < n; i++) { d.regs[i].addr = addr[i]; d.regs[i].ctrl = ctrl[i]; }
    struct iovec iov = {&d, 8 + 16 * (n ? n : 1)};
    return ptrace(PTRACE_SETREGSET, pid, nt, &iov) < 0 ? -errno : 0;
}

static uint32_t ctrl(uint32_t bas, uint32_t type) { return bas << 5 | type << 3 | 2 << 1 | 1; }

static void report(pid_t pid, const char *what)
{
    int st;
    waitpid(pid, &st, 0);
    if (!WIFSTOPPED(st)) { printf("%s: not stopped %#x\n", what, st); return; }
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    struct { uint64_t x[31], sp, pc, pstate; } r;
    struct iovec iov = {&r, sizeof r};
    ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    long cur = ptrace(PTRACE_PEEKDATA, pid, &data[1], 0);
    printf("%s: sig %d code %d addr %s pc %s data[1] %ld\n", what, WSTOPSIG(st), si.si_code,
           si.si_addr == (void *)&data[1] ? "data[1]" : si.si_addr == (void *)&bytes[3] ? "bytes[3]" :
           si.si_addr == (void *)(uintptr_t)target ? "target" : "other",
           r.pc == (uintptr_t)target ? "target" : "other", cur);
}

int main(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        store(&data[1], 5);
        long v = load(&data[1]);
        store_byte(&bytes[3]);
        v += target(1);
        _exit(v == 7 ? 0 : 1);
    }
    int st;
    waitpid(pid, &st, 0);
    struct dbg d;
    struct iovec iov = {&d, sizeof d};
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_WATCH, &iov);
    printf("watch slots >= 2: %d, len %zu\n", (d.info & 0xff) >= 2, iov.iov_len);
    iov.iov_len = sizeof d;
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_BREAK, &iov);
    printf("break slots >= 2: %d\n", (d.info & 0xff) >= 2);
    uint64_t a = (uint64_t)&data[1];
    uint32_t bad[] = {ctrl(0xff, 0), ctrl(0x5, 2), 0x1};
    for (int i = 0; i < 3; i++)
        printf("bad ctrl %#x: %d\n", bad[i], setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &bad[i]));
    uint32_t c = ctrl(0xff, 2); /* 写 */
    printf("set store watch: %d\n", setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c));
    iov.iov_len = sizeof d;
    ptrace(PTRACE_GETREGSET, pid, NT_ARM_HW_WATCH, &iov);
    printf("read back: addr ok %d ctrl %#x\n", d.regs[0].addr == a, d.regs[0].ctrl);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "store watch");
    c = 0;
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    report(pid, "stepped");
    c = ctrl(0xff, 1); /* 读 */
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "load watch");
    /* 不对齐的一个字节：地址按 8 对齐，BAS 选第 3 个字节 */
    uint64_t b8 = (uint64_t)&bytes[3] & ~7ul;
    uint32_t cb = ctrl(1u << ((uint64_t)&bytes[3] & 7), 2);
    printf("set byte watch: %d\n", setdbg(pid, NT_ARM_HW_WATCH, 1, &b8, &cb));
    c = 0;
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "byte watch");
    setdbg(pid, NT_ARM_HW_WATCH, 1, &a, &c);
    uint64_t t = (uint64_t)target;
    uint32_t cbk = ctrl(0xf, 0);
    printf("set breakpoint: %d\n", setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk));
    uint64_t tu = t + 2;
    printf("unaligned breakpoint: %d\n", setdbg(pid, NT_ARM_HW_BREAK, 1, &tu, &cbk));
    setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "breakpoint");
    cbk = 0;
    setdbg(pid, NT_ARM_HW_BREAK, 1, &t, &cbk);
    ptrace(PTRACE_CONT, pid, 0, 0);
    waitpid(pid, &st, 0);
    printf("exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
C
gcc -O1 -o hw hw.c && ./hw
# 原子的读改写（LSE）也算读；16 字节的 SIMD 存储碰到后一半时报告访问的起点
cat > hw3.c <<'C'
#define _GNU_SOURCE
#include <elf.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

struct dbg { uint32_t info, pad; struct { uint64_t addr; uint32_t ctrl, pad; } regs[16]; };
static volatile int atom;
static volatile uint64_t vec[4] __attribute__((aligned(16)));

static void watch(pid_t pid, uint64_t addr, uint32_t bas, uint32_t type)
{
    struct dbg d;
    memset(&d, 0, sizeof d);
    d.regs[0].addr = addr;
    d.regs[0].ctrl = type ? bas << 5 | type << 3 | 2 << 1 | 1 : 0;
    struct iovec iov = {&d, 8 + 16};
    if (ptrace(PTRACE_SETREGSET, pid, NT_ARM_HW_WATCH, &iov) < 0)
        perror("SETREGSET");
}

static void report(pid_t pid, const char *what, volatile void *expect)
{
    int st;
    waitpid(pid, &st, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    printf("%s: sig %d code %d addr %s\n", what, WSTOPSIG(st), si.si_code,
           si.si_addr == (void *)expect ? "ok" : "other");
}

int main(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        __atomic_fetch_add(&atom, 1, __ATOMIC_SEQ_CST);
        __asm__ volatile("movi v0.16b, #7\n\tstr q0, [%0]" : : "r"(&vec[1]) : "v0", "memory");
        _exit(atom == 1 && vec[1] == 0x0707070707070707ull ? 0 : 1);
    }
    int st;
    waitpid(pid, &st, 0);
    /* 只读的观察点：原子的读改写也算读 */
    watch(pid, (uint64_t)&atom & ~7ull, 0xf << ((uint64_t)&atom & 7), 1);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "atomic add, load watch", &atom);
    watch(pid, 0, 0, 0);
    /* 16 字节的 SIMD 存储碰到后一半 */
    watch(pid, (uint64_t)&vec[2], 0xff, 2);
    ptrace(PTRACE_CONT, pid, 0, 0);
    report(pid, "simd store", &vec[1]);
    watch(pid, 0, 0, 0);
    ptrace(PTRACE_CONT, pid, 0, 0);
    waitpid(pid, &st, 0);
    printf("exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
C
gcc -O1 -o hw3 hw3.c && ./hw3
