# seccomp：严格模式、过滤器的检查和各种动作、优先级、快速路径上的系统调用、继承和 TSYNC、
# PTRACE_EVENT_SECCOMP、通知 fd（USER_NOTIF、ADDFD）。docker 里的 root 没有 CAP_SYS_ADMIN，开头先去掉它，两边一样
rm -rf /tmp/seccomp-test
mkdir -p /tmp/seccomp-test
cd /tmp/seccomp-test
cat > sc.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

/* 没有 linux-headers：自己定义 */
struct sock_filter {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};
struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};
struct seccomp_data {
    int nr;
    uint32_t arch;
    uint64_t instruction_pointer;
    uint64_t args[6];
};
struct seccomp_notif {
    uint64_t id;
    uint32_t pid, flags;
    struct seccomp_data data;
};
struct seccomp_notif_resp {
    uint64_t id;
    int64_t val;
    int32_t error;
    uint32_t flags;
};
struct seccomp_notif_addfd {
    uint64_t id;
    uint32_t flags, srcfd, newfd, newfd_flags;
};
#define BPF_LD 0x00
#define BPF_LDX 0x01
#define BPF_ST 0x02
#define BPF_ALU 0x04
#define BPF_JMP 0x05
#define BPF_RET 0x06
#define BPF_W 0x00
#define BPF_H 0x08
#define BPF_ABS 0x20
#define BPF_MEM 0x60
#define BPF_LEN 0x80
#define BPF_IMM 0x00
#define BPF_MOD 0x90
#define BPF_DIV 0x30
#define BPF_X 0x08
#define BPF_JSET 0x40
#define BPF_JEQ 0x10
#define BPF_K 0x00
#define BPF_A 0x10
#define STMT(c, k) {(uint16_t)(c), 0, 0, (uint32_t)(k)}
#define JUMP(c, k, t, f) {(uint16_t)(c), (t), (f), (uint32_t)(k)}
#define RET_KILL_PROCESS 0x80000000U
#define RET_KILL_THREAD 0x00000000U
#define RET_TRAP 0x00030000U
#define RET_ERRNO 0x00050000U
#define RET_USER_NOTIF 0x7fc00000U
#define RET_TRACE 0x7ff00000U
#define RET_LOG 0x7ffc0000U
#define RET_ALLOW 0x7fff0000U
#define FLAG_TSYNC 1
#define FLAG_LOG 2
#define FLAG_NEW_LISTENER 8
#define FLAG_TSYNC_ESRCH 16
#define FLAG_WAIT_KILLABLE_RECV 32
#define AUDIT_ARCH_AARCH64 0xc00000b7
#define NOTIF_RECV 0xc0502100
#define NOTIF_SEND 0xc0182101
#define NOTIF_ID_VALID 0x40082102
#define NOTIF_ADDFD 0x40182103
/* ADDFD 的结构体可以加长：ioctl 编号里的长度是调用者给的 */
#define NOTIF_ADDFD_SIZE(n) (0x40002103 | (n) << 16)
#define ADDFD_FLAG_SETFD 1
#define ADDFD_FLAG_SEND 2
#ifndef PTRACE_O_TRACESECCOMP
#define PTRACE_O_TRACESECCOMP 0x80
#endif
#ifndef PTRACE_EVENT_SECCOMP
#define PTRACE_EVENT_SECCOMP 7
#endif
#ifndef PTRACE_O_SUSPEND_SECCOMP
#define PTRACE_O_SUSPEND_SECCOMP (1 << 21)
#endif
#define PTRACE_SECCOMP_GET_FILTER 0x420c
#define NT_ARM_SYSTEM_CALL 0x404

static long sys_seccomp(unsigned op, unsigned flags, void *args)
{
    return syscall(SYS_seccomp, op, flags, args);
}

/* 过滤编号为 nr 的系统调用，返回 action，别的放行 */
static long install(int nr, uint32_t action, unsigned flags)
{
    struct sock_filter f[] = {
        STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        JUMP(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1),
        STMT(BPF_RET | BPF_K, action),
        STMT(BPF_RET | BPF_K, RET_ALLOW),
    };
    struct sock_fprog p = {4, f};
    return sys_seccomp(1, flags, &p);
}

static const char *describe(int st)
{
    static char b[64];
    if (WIFEXITED(st))
        snprintf(b, sizeof b, "exited %d", WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        snprintf(b, sizeof b, "killed by %d%s", WTERMSIG(st), WCOREDUMP(st) ? " (core)" : "");
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

static void result(const char *what, long r)
{
    if (r < 0)
        printf("%s: -1 %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

/* 去掉 CAP_SYS_ADMIN（docker 里的 root 本来就没有） */
static void drop_sys_admin(void)
{
    struct {
        uint32_t version;
        int pid;
    } hdr = {0x20080522, 0};
    struct {
        uint32_t effective, permitted, inheritable;
    } data[2];
    syscall(SYS_capget, &hdr, data);
    data[0].effective &= ~(1u << 21);
    data[0].permitted &= ~(1u << 21);
    if (syscall(SYS_capset, &hdr, data))
        perror("capset");
}

static void status_lines(const char *who, pid_t tid)
{
    char path[64], line[128];
    snprintf(path, sizeof path, "/proc/self/task/%d/status", tid);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, "Seccomp", 7) || !strncmp(line, "NoNewPrivs", 10))
            printf("  %s %s", who, line);
    fclose(f);
}

static void basics(void)
{
    printf("== basics\n");
    result("PR_GET_SECCOMP", prctl(PR_GET_SECCOMP, 0, 0, 0, 0));
    uint32_t actions[] = {RET_KILL_PROCESS, RET_KILL_THREAD, RET_TRAP, RET_ERRNO, RET_USER_NOTIF,
                          RET_TRACE, RET_LOG, RET_ALLOW, 0x00010000, 0x7fff0001};
    for (unsigned i = 0; i < sizeof actions / sizeof *actions; i++) {
        char what[64];
        snprintf(what, sizeof what, "action %#x available", actions[i]);
        result(what, sys_seccomp(2, 0, &actions[i]));
    }
    uint16_t sizes[3];
    result("notif sizes", sys_seccomp(3, 0, sizes));
    printf("  %u %u %u\n", sizes[0], sizes[1], sizes[2]);
    result("notif sizes with flags", sys_seccomp(3, 1, sizes));
    result("unknown op", sys_seccomp(4, 0, NULL));
    char line[256];
    FILE *f = fopen("/proc/sys/kernel/seccomp/actions_avail", "r");
    fgets(line, sizeof line, f);
    printf("actions_avail: %s", line);
    fclose(f);
    f = fopen("/proc/sys/kernel/seccomp/actions_logged", "r");
    fgets(line, sizeof line, f);
    printf("actions_logged: %s", line);
    fclose(f);
    status_lines("self", gettid());
}

static void strict(void)
{
    printf("== strict mode\n");
    pid_t pid = fork();
    if (pid == 0) {
        prctl(PR_SET_SECCOMP, 1, 0, 0, 0);
        write(1, "  read, write, exit allowed\n", sizeof "  read, write, exit allowed\n" - 1);
        syscall(SYS_exit, 3);
    }
    printf("exit: %s\n", describe(wait_for(pid)));
    pid = fork();
    if (pid == 0) {
        sys_seccomp(0, 0, NULL);
        syscall(SYS_getpid);
        _exit(0);
    }
    printf("getpid: %s\n", describe(wait_for(pid)));
    /* exit_group 不在允许的里面：从 main 返回也是 SIGKILL */
    pid = fork();
    if (pid == 0) {
        sys_seccomp(0, 0, NULL);
        syscall(SYS_exit_group, 0);
    }
    printf("exit_group: %s\n", describe(wait_for(pid)));
    pid = fork();
    if (pid == 0) {
        sys_seccomp(0, 0, NULL);
        prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
        _exit(0);
    }
    printf("prctl: %s\n", describe(wait_for(pid)));
    result("strict with flags", sys_seccomp(0, 1, NULL));
    result("strict with args", sys_seccomp(0, 0, (void *)8));
}

static void checks(void)
{
    printf("== filter checks\n");
    struct sock_filter ok[] = {STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog p = {1, ok};
    result("without no_new_privs", sys_seccomp(1, 0, &p));
    result("prctl without no_new_privs", prctl(PR_SET_SECCOMP, 2, &p, 0, 0));
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    struct sock_fprog empty = {0, ok};
    result("empty", sys_seccomp(1, 0, &empty));
    struct sock_fprog big = {4097, ok};
    result("4097 instructions", sys_seccomp(1, 0, &big));
    struct sock_fprog null = {1, NULL};
    result("null program", sys_seccomp(1, 0, &null));
    struct sock_fprog bad = {1, (void *)16};
    result("bad program address", sys_seccomp(1, 0, &bad));
    result("bad fprog address", sys_seccomp(1, 0, (void *)16));
    struct sock_filter half[] = {STMT(BPF_LD | BPF_H | BPF_ABS, 0), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog ph = {2, half};
    result("halfword load", sys_seccomp(1, 0, &ph));
    struct sock_filter odd[] = {STMT(BPF_LD | BPF_W | BPF_ABS, 2), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog po = {2, odd};
    result("misaligned load", sys_seccomp(1, 0, &po));
    struct sock_filter past[] = {STMT(BPF_LD | BPF_W | BPF_ABS, 64), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog pp = {2, past};
    result("load past the data", sys_seccomp(1, 0, &pp));
    struct sock_filter mod[] = {STMT(BPF_ALU | BPF_MOD | BPF_K, 3), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog pm = {2, mod};
    result("modulo", sys_seccomp(1, 0, &pm));
    struct sock_filter noret[] = {STMT(BPF_LD | BPF_IMM, 1)};
    struct sock_fprog pn = {1, noret};
    result("no return", sys_seccomp(1, 0, &pn));
    struct sock_filter jump[] = {JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog pj = {2, jump};
    result("jump out", sys_seccomp(1, 0, &pj));
    struct sock_filter mem[] = {STMT(BPF_LD | BPF_MEM, 0), STMT(BPF_RET | BPF_K, RET_ALLOW)};
    struct sock_fprog pmem = {2, mem};
    result("load before store", sys_seccomp(1, 0, &pmem));
    result("unknown flag", sys_seccomp(1, 64, &p));
    result("killable without listener", sys_seccomp(1, FLAG_WAIT_KILLABLE_RECV, &p));
    result("tsync and listener", sys_seccomp(1, FLAG_TSYNC | FLAG_NEW_LISTENER, &p));
    /* 按转换后的 eBPF 长度计数：RET K、RET A、除以 X、双分支和大常数的
       展开长度不同。每条链各自 fork，避免前一个用例消耗后一个的容量。 */
    const char *kinds[] = {"ret k", "ret a", "div x", "jset", "jeq large", "jeq small", "jeq both"};
    for (int kind = 0; kind < 7; kind++) {
        pid_t pid = fork();
        if (pid == 0) {
            static struct sock_filter many[4096];
            for (int i = 0; i < 4096; i++)
                many[i] = (struct sock_filter)STMT(BPF_RET | BPF_K, RET_ALLOW);
            if (kind == 1) {
                for (int i = 0; i < 4095; i++)
                    many[i] = (struct sock_filter)STMT(BPF_LD | BPF_IMM, RET_ALLOW);
                many[4095] = (struct sock_filter)STMT(BPF_RET | BPF_A, 0);
            } else if (kind == 2) {
                many[0] = (struct sock_filter)STMT(BPF_LDX | BPF_IMM, 1);
                for (int i = 1; i < 4095; i++)
                    many[i] = (struct sock_filter)STMT(BPF_ALU | BPF_DIV | BPF_X, 0);
            } else if (kind >= 3) {
                for (int i = 0; i < 4094; i++)
                    many[i] = (struct sock_filter)JUMP(
                        BPF_JMP | (kind == 3 ? BPF_JSET : BPF_JEQ) | BPF_K,
                        kind <= 4 ? 0x80000000U : 0, kind == 6 ? 1 : 0, 1);
            }
            struct sock_fprog pmany = {4096, many};
            int n = 0;
            while (sys_seccomp(1, 0, &pmany) == 0)
                n++;
            printf("  %s: %d filters of 4096 fit, then %s\n", kinds[kind], n, strerror(errno));
            status_lines("child", gettid());
            result("  then strict", sys_seccomp(0, 0, NULL));
            result("  PR_GET_SECCOMP", prctl(PR_GET_SECCOMP, 0, 0, 0, 0));
            _exit(0);
        }
        printf("chain: %s\n", describe(wait_for(pid)));
    }
}

static void sys_handler(int sig, siginfo_t *si, void *uc_)
{
    ucontext_t *uc = uc_;
    printf("  SIGSYS %d code %d errno %#x syscall %d arch %#x addr %s x0 %llu\n", sig, si->si_code,
           si->si_errno, si->si_syscall, si->si_arch,
           (uint64_t)si->si_call_addr == uc->uc_mcontext.pc ? "is the pc" : "differs",
           (unsigned long long)uc->uc_mcontext.regs[0]);
    uc->uc_mcontext.regs[0] = 4242;
}

static volatile pid_t getppid_tid;

static void *getppid_thread(void *arg)
{
    (void)arg;
    getppid_tid = gettid();
    syscall(SYS_getppid);
    return (void *)7;
}

static void *exit_thread(void *arg)
{
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat", getpid());
    for (;;) {
        FILE *f = fopen(path, "r");
        if (!f) {
            if (errno == ENOENT)
                break;
            _exit(90);
        }
        char state = 0;
        int fields = fscanf(f, "%*d %*s %c", &state);
        fclose(f);
        if (fields != 1)
            _exit(91);
        if (state == 'Z')
            break;
        usleep(1000);
    }
    syscall(SYS_exit, *(int *)arg);
    return NULL;
}

static void actions(void)
{
    printf("== actions\n");
    /* 不写 core：core_pattern 是管道（CI 的 Ubuntu 宿主上是 apport）时 Linux 不管限制是不是 0
       都交给它，只有 1 不交；写文件时不到一页也不写 */
    struct rlimit no_core = {1, 1};
    setrlimit(RLIMIT_CORE, &no_core);
    pid_t pid = fork();
    if (pid == 0) {
        install(SYS_getppid, RET_ERRNO | EPERM, 0);
        result("errno", syscall(SYS_getppid));
        install(SYS_getppid, RET_ERRNO | 9999, 0);
        result("errno capped", syscall(SYS_getppid));
        install(SYS_getpgid, RET_ERRNO | 0, 0);
        result("errno 0", syscall(SYS_getpgid, 0));
        install(SYS_getsid, RET_LOG, FLAG_LOG);
        result("log", syscall(SYS_getsid, 0) >= 0);
        install(SYS_getuid, RET_USER_NOTIF, 0);
        result("user_notif without a listener", syscall(SYS_getuid));
        install(SYS_getgid, RET_TRACE, 0);
        result("trace without a tracer", syscall(SYS_getgid));
        /* 参数：第一个参数的低 32 位是 12345 时 ERRNO 7，高 32 位是 1 时 ERRNO 9 */
        struct sock_filter args[] = {
            STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getpriority, 0, 6),
            STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, 12345, 0, 1),
            STMT(BPF_RET | BPF_K, RET_ERRNO | 7),
            STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0]) + 4),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 1),
            STMT(BPF_RET | BPF_K, RET_ERRNO | 9),
            STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_AARCH64, 1, 0),
            STMT(BPF_RET | BPF_K, RET_KILL_PROCESS),
            STMT(BPF_LD | BPF_W | BPF_LEN, 0),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, 64, 1, 0),
            STMT(BPF_RET | BPF_K, RET_KILL_PROCESS),
            STMT(BPF_RET | BPF_K, RET_ALLOW),
        };
        struct sock_fprog pa = {sizeof args / sizeof *args, args};
        result("install args filter", sys_seccomp(1, 0, &pa));
        result("arg 12345", syscall(SYS_getpriority, 12345, 0));
        result("arg 1 << 32", syscall(SYS_getpriority, 1UL << 32, 0));
        result("arg 0", syscall(SYS_getpriority, 0, 0) >= 0);
        /* 同样严格时最新的过滤器的数据，更严格的动作优先 */
        install(SYS_getppid, RET_ERRNO | EACCES, 0);
        result("newest errno wins", syscall(SYS_getppid));
        result("PR_GET_SECCOMP", prctl(PR_GET_SECCOMP, 0, 0, 0, 0));
        status_lines("child", gettid());
        _exit(0);
    }
    printf("child: %s\n", describe(wait_for(pid)));
    /* TRAP：处理函数看到原来的 x0，改掉它就是返回值 */
    pid = fork();
    if (pid == 0) {
        struct sigaction sa = {.sa_sigaction = sys_handler, .sa_flags = SA_SIGINFO};
        sigaction(SIGSYS, &sa, NULL);
        install(SYS_getppid, RET_TRAP | 0x1234, 0);
        install(SYS_getpgid, RET_ERRNO | 1, 0);
        result("trap", syscall(SYS_getppid, 5, 6));
        _exit(0);
    }
    printf("trap: %s\n", describe(wait_for(pid)));
    /* TRAP 时 SIGSYS 被屏蔽或者忽略：恢复默认动作 */
    for (int how = 0; how < 2; how++) {
        pid = fork();
        if (pid == 0) {
            if (how == 0) {
                sigset_t s;
                sigemptyset(&s);
                sigaddset(&s, SIGSYS);
                sigprocmask(SIG_BLOCK, &s, NULL);
            } else {
                signal(SIGSYS, SIG_IGN);
            }
            install(SYS_getppid, RET_TRAP, 0);
            syscall(SYS_getppid);
            _exit(0);
        }
        printf("trap with SIGSYS %s: %s\n", how ? "ignored" : "blocked", describe(wait_for(pid)));
    }
    /* KILL_PROCESS：有处理函数、在别的线程里也结束整个进程；不认得的动作也是 */
    uint32_t kills[] = {RET_KILL_PROCESS, 0x00010000};
    for (int i = 0; i < 2; i++) {
        pid = fork();
        if (pid == 0) {
            struct sigaction sa = {.sa_sigaction = sys_handler, .sa_flags = SA_SIGINFO};
            sigaction(SIGSYS, &sa, NULL);
            install(SYS_getppid, kills[i], 0);
            pthread_t t;
            pthread_create(&t, NULL, getppid_thread, NULL);
            pthread_join(t, NULL);
            _exit(0);
        }
        printf("kill %#x from a thread: %s\n", kills[i], describe(wait_for(pid)));
    }
    /* KILL_THREAD：只结束那个线程；只有一个线程时与结束进程一样 */
    pid = fork();
    if (pid == 0) {
        install(SYS_getppid, RET_KILL_THREAD, 0);
        pthread_t t;
        pthread_create(&t, NULL, getppid_thread, NULL);
        /* 被内核结束的线程没有走 pthread_exit，musl 的 pthread_join 等不到它：看它在
           /proc/self/task 里消失 */
        while (!getppid_tid)
            usleep(1000);
        char path[64];
        snprintf(path, sizeof path, "/proc/self/task/%d", getppid_tid);
        int waited = 0;
        while (access(path, F_OK) == 0 && waited++ < 5000)
            usleep(1000);
        printf("  thread %s, the process goes on\n", access(path, F_OK) ? "gone" : "still there");
        _exit(4);
    }
    printf("kill thread: %s\n", describe(wait_for(pid)));
    pid = fork();
    if (pid == 0) {
        install(SYS_getppid, RET_KILL_THREAD, 0);
        syscall(SYS_getppid);
        _exit(0);
    }
    printf("kill the only thread: %s\n", describe(wait_for(pid)));
    /* 主线程被 KILL_THREAD 结束，另一个线程用 exit 结束：进程的结束方式是最后一个退出的线程的 */
    pid = fork();
    if (pid == 0) {
        static int code = 5;
        pthread_t t;
        pthread_create(&t, NULL, exit_thread, &code);
        install(SYS_getppid, RET_KILL_THREAD, FLAG_TSYNC);
        syscall(SYS_getppid);
        _exit(0);
    }
    printf("kill the main thread: %s\n", describe(wait_for(pid)));
}

static pid_t sibling_tid;
static int sibling_go[2], sibling_done[2];

static void *sibling(void *arg)
{
    sibling_tid = gettid();
    write(sibling_done[1], "r", 1);
    char c;
    while (read(sibling_go[0], &c, 1) == 1 && c != 'q') {
        if (c == 'f')
            install(SYS_getsid, RET_ERRNO | 2, 0);
        if (c == 'c') {
            errno = 0;
            long r = syscall(SYS_getppid);
            printf("  sibling getppid: %ld %s\n", r, r < 0 ? strerror(errno) : "");
            status_lines("sibling", gettid());
        }
        write(sibling_done[1], "d", 1);
    }
    return arg;
}

static void sibling_do(char c)
{
    write(sibling_go[1], &c, 1);
    if (c != 'q')
        read(sibling_done[0], &c, 1);
}

/* 不持 BKL 的快速路径（appine 的 syscall::fast）上的系统调用也要经过过滤器；vDSO 里读时钟
   不进内核，过滤器管不到 */
static void fast_path(void)
{
    printf("== fast path\n");
    static const struct {
        int nr;
        const char *name;
    } calls[] = {
        {SYS_futex, "futex"},
        {SYS_nanosleep, "nanosleep"},
        {SYS_clock_nanosleep, "clock_nanosleep"},
        {SYS_clock_gettime, "clock_gettime"},
        {SYS_clock_getres, "clock_getres"},
        {SYS_gettimeofday, "gettimeofday"},
        {SYS_newfstatat, "newfstatat"},
        {SYS_statx, "statx"},
    };
    for (unsigned i = 0; i < sizeof calls / sizeof *calls; i++) {
        pid_t pid = fork();
        if (pid == 0) {
            prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            install(calls[i].nr, RET_ERRNO | 77, 0);
            static int word;
            struct timespec ts = {0, 1000};
            char buf[256];
            long r;
            switch (calls[i].nr) {
            case SYS_futex:
                r = syscall(SYS_futex, &word, 1 /* FUTEX_WAKE */, 1, 0, 0, 0);
                break;
            case SYS_nanosleep:
                r = syscall(SYS_nanosleep, &ts, NULL);
                break;
            case SYS_clock_nanosleep:
                r = syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &ts, NULL);
                break;
            case SYS_clock_gettime:
                r = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts);
                break;
            case SYS_clock_getres:
                r = syscall(SYS_clock_getres, CLOCK_MONOTONIC, &ts);
                break;
            case SYS_gettimeofday:
                r = syscall(SYS_gettimeofday, buf, NULL);
                break;
            case SYS_newfstatat:
                r = syscall(SYS_newfstatat, AT_FDCWD, "/", buf, 0);
                break;
            default:
                r = syscall(SYS_statx, AT_FDCWD, "/", 0, 0x7ff, buf);
                break;
            }
            result(calls[i].name, r);
            if (calls[i].nr == SYS_clock_gettime)
                result("  through the vDSO", clock_gettime(CLOCK_MONOTONIC, &ts));
            _exit(0);
        }
        wait_for(pid);
    }
}

static void inherit(void)
{
    printf("== inheritance and tsync\n");
    pid_t pid = fork();
    if (pid == 0) {
        install(SYS_getppid, RET_ERRNO | ENOTTY, 0);
        pid_t g = fork();
        if (g == 0) {
            result("  forked child", syscall(SYS_getppid));
            execl("/bin/sh", "sh", "-c", "grep -E '^(Seccomp|NoNewPrivs)' /proc/self/status", NULL);
            _exit(1);
        }
        int st;
        waitpid(g, &st, 0);
        _exit(0);
    }
    printf("fork and exec: %s\n", describe(wait_for(pid)));
    pid = fork();
    if (pid == 0) {
        pipe(sibling_go);
        pipe(sibling_done);
        pthread_t t;
        pthread_create(&t, NULL, sibling, NULL);
        char c;
        read(sibling_done[0], &c, 1);
        /* 没有 seccomp 的兄弟线程可以同步 */
        result("tsync", install(SYS_getppid, RET_ERRNO | EPERM, FLAG_TSYNC));
        sibling_do('c');
        /* 兄弟线程自己又装了一个：它的链不再是我的链的一部分 */
        sibling_do('f');
        long r = install(SYS_getpgid, RET_ERRNO | 3, FLAG_TSYNC);
        printf("  tsync after the sibling diverged: %s\n",
               r == sibling_tid ? "the sibling's tid" : r < 0 ? strerror(errno) : "something else");
        result("  tsync_esrch", install(SYS_getpgid, RET_ERRNO | 3, FLAG_TSYNC | FLAG_TSYNC_ESRCH));
        sibling_do('q');
        pthread_join(t, NULL);
        status_lines("main", gettid());
        _exit(0);
    }
    printf("threads: %s\n", describe(wait_for(pid)));
}

static void trace(void)
{
    printf("== ptrace\n");
    pid_t pid = fork();
    if (pid == 0) {
        pid_t parent = getppid();
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        install(SYS_getppid, RET_TRACE | 0x77, 0);
        long a = syscall(SYS_getppid);
        long b = syscall(SYS_getppid);
        long c = syscall(SYS_getppid, 1);
        printf("  child sees %s, %ld, %s\n", a == getpid() ? "its own pid" : "something else", b,
               c == parent ? "its parent" : "something else");
        result("  trace after options cleared", syscall(SYS_getppid));
        _exit(0);
    }
    int st = wait_for(pid);
    errno = 0;
    long r = ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESECCOMP | PTRACE_O_SUSPEND_SECCOMP);
    printf("suspend seccomp without CAP_SYS_ADMIN: %ld %s\n", r, strerror(errno));
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESECCOMP);
    ptrace(PTRACE_CONT, pid, 0, 0);
    for (int i = 0; i < 3; i++) {
        st = wait_for(pid);
        unsigned long msg = 0;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        struct __ptrace_syscall_info info;
        long size = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
        printf("stop: %s, message %#lx, info size %ld op %d nr %lld ret_data %#x\n", describe(st),
               msg, size, info.op, (long long)info.seccomp.nr, info.seccomp.ret_data);
        if (i == 0) {
            /* 改成 getpid */
            int nr = SYS_getpid;
            struct iovec iov = {&nr, sizeof nr};
            ptrace(PTRACE_SETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov);
        } else if (i == 1) {
            /* 跳过，返回 55 */
            int nr = -1;
            struct iovec iov = {&nr, sizeof nr};
            ptrace(PTRACE_SETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov);
            unsigned long long regs[34];
            struct iovec riov = {regs, sizeof regs};
            ptrace(PTRACE_GETREGSET, pid, 1, &riov);
            regs[0] = 55;
            ptrace(PTRACE_SETREGSET, pid, 1, &riov);
        } else {
            errno = 0;
            r = ptrace(PTRACE_SECCOMP_GET_FILTER, pid, 0, NULL);
            printf("get filter without CAP_SYS_ADMIN: %ld %s\n", r, strerror(errno));
            /* 不再跟踪 seccomp：之后的 TRACE 是 ENOSYS */
            ptrace(PTRACE_SETOPTIONS, pid, 0, 0);
        }
        ptrace(PTRACE_CONT, pid, 0, 0);
    }
    printf("end: %s\n", describe(wait_for(pid)));

    /* PTRACE_SYSCALL：先是系统调用入口的停止，再是 seccomp 的，最后是出口 */
    pid = fork();
    if (pid == 0) {
        pid_t parent = getppid();
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        install(SYS_getppid, RET_TRACE | 1, 0);
        raise(SIGSTOP);
        long r = syscall(SYS_getppid);
        printf("  child: getppid %s\n", r == parent ? "real" : "changed");
        _exit(0);
    }
    wait_for(pid);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESECCOMP | PTRACE_O_TRACESYSGOOD);
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    /* musl 的 raise 返回前还会恢复信号掩码；先找到目标调用的入口。 */
    int seen = 0;
    while (seen < 3) {
        st = wait_for(pid);
        if (!WIFSTOPPED(st)) {
            fprintf(stderr, "unexpected trace status: %s\n", describe(st));
            exit(1);
        }
        struct __ptrace_syscall_info info = {0};
        if (ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info) < 0) {
            perror("PTRACE_GET_SYSCALL_INFO");
            exit(1);
        }
        if (seen || (info.op == 1 && info.entry.nr == SYS_getppid)) {
            printf("stop: %s, op %d\n", describe(st), info.op);
            seen++;
        }
        if (seen < 3)
            ptrace(PTRACE_SYSCALL, pid, 0, 0);
    }
    ptrace(PTRACE_CONT, pid, 0, 0);
    printf("end: %s\n", describe(wait_for(pid)));

    /* 用 PTRACE_CONT 继续到 seccomp 的停止，再改用 PTRACE_SYSCALL：与 arm64 Linux 一样出口也停
       （strace --seccomp-bpf 这样拿返回值）；跳过的系统调用在出口给返回值 */
    pid = fork();
    if (pid == 0) {
        pid_t parent = getppid();
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        install(SYS_getppid, RET_TRACE | 2, 0);
        raise(SIGSTOP);
        long a = syscall(SYS_getppid);
        long b = syscall(SYS_getppid);
        printf("  child: getppid %s, then %ld\n", a == parent ? "real" : "changed", b);
        _exit(0);
    }
    wait_for(pid);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESECCOMP | PTRACE_O_TRACESYSGOOD);
    ptrace(PTRACE_CONT, pid, 0, 0);
    for (int i = 0; i < 2; i++) {
        printf("stop: %s\n", describe(wait_for(pid)));
        if (i == 1) {
            int nr = -1;
            struct iovec iov = {&nr, sizeof nr};
            ptrace(PTRACE_SETREGSET, pid, NT_ARM_SYSTEM_CALL, &iov);
        }
        ptrace(PTRACE_SYSCALL, pid, 0, 0);
        st = wait_for(pid);
        struct __ptrace_syscall_info info = {0};
        ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
        if (i == 0) {
            printf("stop: %s, op %d, rval is the tracer %d\n", describe(st), info.op,
                   info.exit.rval == getpid());
        } else {
            printf("stop: %s, op %d\n", describe(st), info.op);
            unsigned long long regs[34];
            struct iovec riov = {regs, sizeof regs};
            ptrace(PTRACE_GETREGSET, pid, 1, &riov);
            regs[0] = 66;
            ptrace(PTRACE_SETREGSET, pid, 1, &riov);
        }
        ptrace(PTRACE_CONT, pid, 0, 0);
    }
    printf("end: %s\n", describe(wait_for(pid)));
}

/* 通知 fd：子进程装过滤器，把通知 fd 交给父进程，父进程回答它的系统调用。子进程把每次的
   结果写进管道，由父进程打印，输出的先后是确定的 */
static int report[2];

static void reportf(const char *fmt, long v)
{
    char b[128];
    int n = snprintf(b, sizeof b, fmt, v);
    write(report[1], b, n);
}

static void report_line(void)
{
    char b[128];
    int n = 0;
    while (n < (int)sizeof b - 1 && read(report[0], &b[n], 1) == 1 && b[n] != '\n')
        n++;
    b[n] = 0;
    printf("  child: %s\n", b);
}

static void send_fd(int sock, int fd)
{
    char c = 'x';
    struct iovec iov = {&c, 1};
    union {
        struct cmsghdr h;
        char b[CMSG_SPACE(sizeof(int))];
    } u;
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &fd, sizeof fd);
    sendmsg(sock, &m, 0);
}

static int recv_fd(int sock)
{
    char c;
    struct iovec iov = {&c, 1};
    union {
        struct cmsghdr h;
        char b[CMSG_SPACE(sizeof(int))];
    } u;
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.b, .msg_controllen = sizeof u.b};
    recvmsg(sock, &m, 0);
    int fd;
    memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&m)), sizeof fd);
    return fd;
}

static int restarted;

static void usr1(int sig)
{
    (void)sig;
    restarted++;
}

static long errno_or(long r)
{
    return r < 0 ? -errno : r;
}

/* 子进程那一边 */
static void notified(int sock, unsigned flags)
{
    pid_t expected_ppid = getppid();
    struct sigaction sa = {.sa_handler = usr1, .sa_flags = SA_RESTART};
    sigaction(SIGUSR1, &sa, NULL);
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    struct sock_filter f[] = {
        STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 2, 0),
        JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getpgid, 1, 0),
        STMT(BPF_RET | BPF_K, RET_ALLOW),
        STMT(BPF_RET | BPF_K, RET_USER_NOTIF),
    };
    struct sock_fprog p = {5, f};
    int listener = sys_seccomp(1, FLAG_NEW_LISTENER | flags, &p);
    reportf("listener is close-on-exec: %ld\n", fcntl(listener, F_GETFD) & FD_CLOEXEC);
    reportf("second listener: %ld\n", errno_or(sys_seccomp(1, FLAG_NEW_LISTENER, &p)));
    send_fd(sock, listener);
    close(listener);
    close(sock);
    reportf("getppid answered: %ld\n", errno_or(syscall(SYS_getppid, 31)));
    reportf("getppid with an error: %ld\n", errno_or(syscall(SYS_getppid)));
    reportf("getppid continued: %ld\n", errno_or(syscall(SYS_getppid)) == expected_ppid);
    long r = errno_or(syscall(SYS_getppid));
    reportf("interrupted and restarted: %ld\n", r * 100 + restarted);
    reportf("final errno response: %ld\n", errno_or(syscall(SYS_getppid)));
}

static void notif(void)
{
    printf("== user notification\n");
    pipe(report);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        close(report[0]);
        close(sv[0]);
        notified(sv[1], 0);
        _exit(0);
    }
    close(report[1]);
    close(sv[1]);
    report_line();
    report_line();
    int l = recv_fd(sv[0]);
    close(sv[0]);
    struct pollfd pf = {l, POLLIN | POLLOUT};
    struct seccomp_notif n;
    memset(&n, 0, sizeof n);
    n.flags = 1;
    result("recv with a dirty buffer", ioctl(l, NOTIF_RECV, &n));
    memset(&n, 0, sizeof n);
    poll(&pf, 1, -1);
    printf("poll before recv: %#x\n", pf.revents);
    result("recv", ioctl(l, NOTIF_RECV, &n));
    printf("  pid is the child: %d, nr %d arch %#x arg %llu\n", n.pid == (uint32_t)pid, n.data.nr,
           n.data.arch, (unsigned long long)n.data.args[0]);
    poll(&pf, 1, 0);
    printf("poll after recv: %#x\n", pf.revents);
    result("id valid", ioctl(l, NOTIF_ID_VALID, &n.id));
    struct seccomp_notif_resp resp = {n.id, 777, 0, 0};
    struct seccomp_notif_resp bad = {n.id, 1, 0, 1};
    result("continue with a value", ioctl(l, NOTIF_SEND, &bad));
    result("send", ioctl(l, NOTIF_SEND, &resp));
    report_line();
    result("send again", ioctl(l, NOTIF_SEND, &resp));
    result("id no longer valid", ioctl(l, NOTIF_ID_VALID, &n.id));
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    resp = (struct seccomp_notif_resp){n.id, 0, -EPERM, 0};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    resp = (struct seccomp_notif_resp){n.id, 0, 0, 1};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    /* 被信号打断：重新执行时是一个新的通知 */
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    uint64_t old = n.id;
    kill(pid, SIGUSR1);
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    printf("  restarted with a new id: %d\n", n.id != old);
    resp = (struct seccomp_notif_resp){old, 1, 0, 0};
    result("answer the old id", ioctl(l, NOTIF_SEND, &resp));
    resp = (struct seccomp_notif_resp){n.id, 9, 0, 0};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    /* 回答最后一个通知，并保留 listener，观察子进程回收后的 POLLHUP。 */
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    struct pollfd hup = {l, 0};
    poll(&hup, 1, 0);
    printf("  poll before the child exits: %#x\n", hup.revents);
    resp = (struct seccomp_notif_resp){n.id, 0, -ENOSYS, 0};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    /* 子进程结束、被回收之前不算没有用户（僵尸进程仍用着过滤器） */
    char c;
    while (read(report[0], &c, 1) == 1)
        ;
    printf("child: %s\n", describe(wait_for(pid)));
    poll(&hup, 1, 0);
    printf("  poll after the child is reaped: %#x\n", hup.revents);
    close(l);
    close(report[0]);
}

/* 等回答时只被致命信号打断（WAIT_KILLABLE_RECV）：取走之后来的信号等回答之后再处理 */
static void killable(void)
{
    printf("== wait killable\n");
    pipe(report);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        close(report[0]);
        close(sv[0]);
        struct sigaction sa = {.sa_handler = usr1, .sa_flags = SA_RESTART};
        sigaction(SIGUSR1, &sa, NULL);
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        struct sock_filter f[] = {
            STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
            STMT(BPF_RET | BPF_K, RET_USER_NOTIF),
            STMT(BPF_RET | BPF_K, RET_ALLOW),
        };
        struct sock_fprog p = {4, f};
        int l = sys_seccomp(1, FLAG_NEW_LISTENER | FLAG_WAIT_KILLABLE_RECV, &p);
        send_fd(sv[1], l);
        close(l);
        close(sv[1]);
        long r = syscall(SYS_getppid);
        reportf("getppid %ld", r);
        reportf(", handler ran %ld times\n", restarted);
        _exit(0);
    }
    close(report[1]);
    close(sv[1]);
    int l = recv_fd(sv[0]);
    close(sv[0]);
    struct seccomp_notif n;
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    kill(pid, SIGUSR1);
    usleep(100000);
    struct seccomp_notif_resp resp = {n.id, 11, 0, 0};
    result("send after the signal", ioctl(l, NOTIF_SEND, &resp));
    report_line();
    printf("child: %s\n", describe(wait_for(pid)));
    close(l);
    close(report[0]);
}

/* ADDFD：监管进程把自己的 fd 装进对方的 fd 表。子进程那一边 */
static void addfd_child(int sock)
{
    int null = open("/dev/null", O_RDONLY);
    dup2(null, 50);
    close(null);
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    int listener = install(SYS_getppid, RET_USER_NOTIF, FLAG_NEW_LISTENER);
    send_fd(sock, listener);
    close(listener);
    close(sock);
    long fd = syscall(SYS_getppid);
    reportf("got fd %ld", fd);
    reportf(", wrote %ld\n", write(fd, "hello", 5));
    close(fd);
    fd = syscall(SYS_getppid);
    char link[64] = {0};
    readlink("/proc/self/fd/50", link, sizeof link - 1);
    reportf("setfd and send: %ld", fd);
    reportf(", close-on-exec %ld", fcntl(50, F_GETFD) & FD_CLOEXEC);
    reportf(", a pipe %ld\n", !strncmp(link, "pipe:", 5));
    struct rlimit rl = {60, 60};
    setrlimit(RLIMIT_NOFILE, &rl);
    reportf("after the limit: %ld\n", errno_or(syscall(SYS_getppid)));
}

static void addfd(void)
{
    printf("== addfd\n");
    pipe(report);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        close(report[0]);
        close(sv[0]);
        addfd_child(sv[1]);
        _exit(0);
    }
    close(report[1]);
    close(sv[1]);
    int l = recv_fd(sv[0]);
    close(sv[0]);
    int p[2];
    pipe(p);
    struct seccomp_notif n = {0};
    result("recv", ioctl(l, NOTIF_RECV, &n));
    struct seccomp_notif_addfd a = {n.id + 1000, 0, p[1], 0, 0};
    result("unknown id", ioctl(l, NOTIF_ADDFD, &a));
    a.id = n.id;
    a.flags = 4;
    result("unknown flag", ioctl(l, NOTIF_ADDFD, &a));
    a.flags = 0;
    a.newfd = 7;
    result("newfd without SETFD", ioctl(l, NOTIF_ADDFD, &a));
    a.newfd = 0;
    a.newfd_flags = O_NONBLOCK;
    result("newfd_flags other than O_CLOEXEC", ioctl(l, NOTIF_ADDFD, &a));
    a.newfd_flags = 0;
    a.srcfd = 999;
    result("bad srcfd", ioctl(l, NOTIF_ADDFD, &a));
    a.srcfd = p[1];
    result("shorter struct", ioctl(l, NOTIF_ADDFD_SIZE(16), &a));
    struct {
        struct seccomp_notif_addfd a;
        uint64_t tail;
    } longer = {a, 1};
    result("longer struct, nonzero tail", ioctl(l, NOTIF_ADDFD_SIZE(32), &longer));
    longer.tail = 0;
    long fd = ioctl(l, NOTIF_ADDFD_SIZE(32), &longer);
    result("longer struct, zero tail", fd);
    struct seccomp_notif_resp resp = {n.id, fd, 0, 0};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    char buf[8] = {0};
    read(p[0], buf, 5);
    printf("  read from the pipe: %s\n", buf);
    /* SETFD 换掉 50 上的 /dev/null，SEND 同时回答 */
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    a = (struct seccomp_notif_addfd){n.id, ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND, p[1], 50, O_CLOEXEC};
    result("setfd and send", ioctl(l, NOTIF_ADDFD, &a));
    report_line();
    /* 受对方的 RLIMIT_NOFILE 限制：SETFD 超出是 EBADF，装满之后是 EMFILE */
    memset(&n, 0, sizeof n);
    ioctl(l, NOTIF_RECV, &n);
    a = (struct seccomp_notif_addfd){n.id, ADDFD_FLAG_SETFD, p[1], 70, 0};
    result("setfd past the limit", ioctl(l, NOTIF_ADDFD, &a));
    a.flags = 0;
    a.newfd = 0;
    long last = -1, r;
    while ((r = ioctl(l, NOTIF_ADDFD, &a)) >= 0)
        last = r;
    printf("addfd until full: last fd %ld, then %s\n", last, strerror(errno));
    resp = (struct seccomp_notif_resp){n.id, 0, -EPERM, 0};
    ioctl(l, NOTIF_SEND, &resp);
    report_line();
    printf("child: %s\n", describe(wait_for(pid)));
    close(p[0]);
    close(p[1]);
    close(l);
    close(report[0]);
}

/* 关闭最后一个 listener：已经挂起的调用和之后的调用都得到 ENOSYS。 */
static void close_listener(void)
{
    printf("== close listener\n");
    pipe(report);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        close(report[0]);
        close(sv[0]);
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        int l = install(SYS_getppid, RET_USER_NOTIF, FLAG_NEW_LISTENER);
        send_fd(sv[1], l);
        close(l);
        close(sv[1]);
        reportf("pending after close: %ld\n", errno_or(syscall(SYS_getppid)));
        reportf("next after close: %ld\n", errno_or(syscall(SYS_getppid)));
        _exit(0);
    }
    close(report[1]);
    close(sv[1]);
    int l = recv_fd(sv[0]);
    close(sv[0]);
    struct seccomp_notif n = {0};
    result("receive before close", ioctl(l, NOTIF_RECV, &n));
    close(l);
    report_line();
    report_line();
    printf("child: %s\n", describe(wait_for(pid)));
    close(report[0]);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(60);
    drop_sys_admin();
    basics();
    strict();
    checks();
    actions();
    fast_path();
    inherit();
    trace();
    notif();
    addfd();
    close_listener();
    killable();
    return 0;
}
C
gcc -O1 -o sc sc.c -lpthread && timeout 120 ./sc

# strace --seccomp-bpf：只在要看的系统调用上停（PTRACE_EVENT_SECCOMP），停住时改用
# PTRACE_SYSCALL 拿到出口的返回值。pid 在两边不同，去掉
echo "some text" > text
env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin strace -f --seccomp-bpf -e trace=openat,close -e signal=none \
    -o st sh -c 'cat text >/dev/null; head -c1 text >/dev/null'
echo "strace: $?"
sed -E 's/^[0-9]+ +//' st
