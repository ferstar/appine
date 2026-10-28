# 不常用但程序会依赖的接口：实时信号排队、pidfd、execveat、终端的属主和权限等
set -e
cd /tmp
cat > rtsig.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

static int got[16], n;

static void handler(int sig, siginfo_t *si, void *ctx)
{
    (void)ctx;
    if (n < 16)
        got[n++] = (sig - SIGRTMIN) * 100 + si->si_value.sival_int;
}

int main(void)
{
    sigset_t set, old;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigaddset(&set, SIGRTMIN + 1);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, &old);
    /* 同一个实时信号排三次队，各带各的值；标准信号只挂起一个 */
    for (int i = 1; i <= 3; i++)
        sigqueue(getpid(), SIGRTMIN + 1, (union sigval){.sival_int = i});
    for (int i = 4; i <= 5; i++)
        sigqueue(getpid(), SIGRTMIN, (union sigval){.sival_int = i});
    kill(getpid(), SIGUSR1);
    kill(getpid(), SIGUSR1);
    siginfo_t si;
    int sig = sigwaitinfo(&set, &si);
    printf("first %s\n", sig == SIGUSR1 ? "SIGUSR1" : "other");
    sigdelset(&set, SIGUSR1);
    struct timespec zero = {0, 0};
    printf("usr1 again %d\n", sigtimedwait(&(sigset_t){0}, NULL, &zero) == -1 && errno == EAGAIN);
    sig = sigwaitinfo(&set, &si);
    printf("wait %d value %d\n", sig - SIGRTMIN, si.si_value.sival_int);
    /* 剩下的交给处理函数：编号小的先，同一种按排队的先后 */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGRTMIN, &sa, NULL);
    sigaction(SIGRTMIN + 1, &sa, NULL);
    sigprocmask(SIG_SETMASK, &old, NULL);
    printf("delivered");
    for (int i = 0; i < n; i++)
        printf(" %d", got[i]);
    printf("\n");
    /* 队列满了：sigqueue 返回 EAGAIN，kill 照样挂起。Linux 按用户计数，同一用户的
       其他进程也算在内，所以上限设成 0 */
    struct rlimit rl = {0, 0};
    setrlimit(RLIMIT_SIGPENDING, &rl);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int ok = 0;
    while (sigqueue(getpid(), SIGRTMIN, (union sigval){.sival_int = ok}) == 0 && ok < 100)
        ok++;
    printf("queued %d errno %s\n", ok, errno == EAGAIN ? "EAGAIN" : strerror(errno));
    printf("kill %d\n", kill(getpid(), SIGRTMIN + 1));
    int left = 0;
    while (sigtimedwait(&set, &si, &zero) > 0)
        left++;
    printf("left %d\n", left);
    return 0;
}
C
gcc -O1 -o rtsig rtsig.c
./rtsig

# pidfd：进程结束时可读，waitid(P_PIDFD) 等它，pidfd_send_signal 发信号，
# pidfd_getfd 复制它的 fd；clone 和 clone3 的 CLONE_PIDFD。只测 6.1 和更新的内核
# 行为相同的部分（6.9 起回收之后有 POLLHUP，CLONE_PIDFD 可以和 CLONE_THREAD 一起用）
cat > pidfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* 镜像里没有 linux-headers */
#define P_PIDFD 3
#define PIDFD_NONBLOCK O_NONBLOCK
#define CLONE_CLEAR_SIGHAND 0x100000000ULL
struct clone_args {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};

static int pidfd_open(pid_t pid, unsigned flags) { return syscall(SYS_pidfd_open, pid, flags); }
static int pidfd_send_signal(int fd, int sig, siginfo_t *info, unsigned flags)
{
    return syscall(SYS_pidfd_send_signal, fd, sig, info, flags);
}
static int pidfd_getfd(int fd, int target, unsigned flags) { return syscall(SYS_pidfd_getfd, fd, target, flags); }

static int readable(int fd, int ms)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, ms) == 1 ? p.revents : 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int pipefd[2];
    pipe(pipefd);
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        int fd = open("/tmp/pidfd-target", O_CREAT | O_RDWR | O_TRUNC, 0644);
        dprintf(fd, "hello from child\n");
        dup2(fd, 42);
        read(pipefd[0], &c, 1);
        _exit(7);
    }
    int fd = pidfd_open(pid, 0);
    printf("pidfd cloexec %d\n", (fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0);
    char link[64] = {0}, path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    readlink(path, link, sizeof link - 1);
    printf("link %s\n", link);
    printf("readable before %d\n", readable(fd, 50));
    printf("read %zd %s\n", read(fd, path, 8), strerror(errno));
    /* 子进程打开文件后才能取它的 fd：轮询到 42 号有效为止 */
    int copy = -1;
    for (int i = 0; i < 200 && copy < 0; i++) {
        copy = pidfd_getfd(fd, 42, 0);
        if (copy < 0)
            usleep(10000);
    }
    char buf[64] = {0};
    pread(copy, buf, sizeof buf - 1, 0);
    printf("getfd %s", buf);
    printf("getfd bad %d %s\n", pidfd_getfd(fd, 1000, 0), strerror(errno));
    printf("getfd flags %d %s\n", pidfd_getfd(fd, 42, 1), strerror(errno));
    printf("signal 0 %d\n", pidfd_send_signal(fd, 0, NULL, 0));
    int nb = pidfd_open(pid, PIDFD_NONBLOCK);
    siginfo_t si;
    printf("waitid nonblock %d %s\n", waitid(P_PIDFD, nb, &si, WEXITED), strerror(errno));
    write(pipefd[1], "x", 1);
    printf("readable after %d\n", readable(fd, 5000) & POLLIN);
    memset(&si, 0, sizeof si);
    printf("waitid %d pid ok %d code %d status %d\n", waitid(P_PIDFD, fd, &si, WEXITED),
           si.si_pid == pid, si.si_code, si.si_status);
    printf("after reap: send %d %s\n", pidfd_send_signal(fd, SIGTERM, NULL, 0), strerror(errno));
    printf("after reap: waitid %d %s\n", waitid(P_PIDFD, fd, &si, WEXITED), strerror(errno));
    printf("after reap: getfd %d %s\n", pidfd_getfd(fd, 42, 0), strerror(errno));
    printf("open gone %d %s\n", pidfd_open(pid, 0), strerror(errno));
    printf("open bad flags %d %s\n", pidfd_open(getpid(), 1), strerror(errno));
    close(fd);
    close(nb);

    /* 给自己发：pidfd 和 /proc/self 目录都行，带 siginfo 时 si_signo 要一致 */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int self = open("/proc/self", O_DIRECTORY | O_CLOEXEC);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    info.si_signo = SIGUSR1;
    info.si_code = SI_QUEUE;
    info.si_value.sival_int = 99;
    printf("send self %d\n", pidfd_send_signal(self, SIGUSR1, &info, 0));
    sigwaitinfo(&set, &si);
    printf("got code %d value %d\n", si.si_code, si.si_value.sival_int);
    printf("mismatch %d %s\n", pidfd_send_signal(self, SIGUSR2, &info, 0), strerror(errno));
    printf("bad flags %d %s\n", pidfd_send_signal(self, SIGUSR1, NULL, 1 << 8), strerror(errno));
    printf("not pidfd %d %s\n", pidfd_send_signal(1, SIGUSR1, NULL, 0), strerror(errno));

    /* clone(CLONE_PIDFD) 写到 parent_tid，不能和 CLONE_PARENT_SETTID 一起用 */
    int cfd = -1;
    pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &cfd, 0, 0);
    if (pid == 0)
        _exit(3);
    printf("clone pidfd %d\n", cfd >= 0);
    waitid(P_PIDFD, cfd, &si, WEXITED);
    printf("clone child %d\n", si.si_status);
    printf("clone settid %ld %s\n", syscall(SYS_clone, CLONE_PIDFD | CLONE_PARENT_SETTID | SIGCHLD, 0, &cfd, 0, 0),
           strerror(errno));

    /* clone3：pidfd 单独一个字段，exit_signal 也是 */
    struct clone_args args;
    memset(&args, 0, sizeof args);
    int c3 = -1;
    args.flags = CLONE_PIDFD;
    args.pidfd = (uintptr_t)&c3;
    args.exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0)
        _exit(5);
    waitid(P_PIDFD, c3, &si, WEXITED);
    printf("clone3 pidfd %d child %d\n", c3 >= 0, si.si_status);
    args.flags = 0;
    args.exit_signal = 0x100;
    printf("clone3 bad signal %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    args.exit_signal = SIGCHLD;
    printf("clone3 short %ld %s\n", syscall(SYS_clone3, &args, 63), strerror(errno));
    char big[sizeof args + 8];
    memset(big, 0, sizeof big);
    memcpy(big, &args, sizeof args);
    big[sizeof args] = 1;
    printf("clone3 nonzero tail %ld %s\n", syscall(SYS_clone3, big, sizeof big), strerror(errno));
    args.flags = CLONE_FS | CLONE_NEWNS;
    printf("clone3 fs newns %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    args.flags = 0;
    args.stack = (uintptr_t)big;
    printf("clone3 stack no size %ld %s\n", syscall(SYS_clone3, &args, sizeof args), strerror(errno));
    /* CLONE_CLEAR_SIGHAND：处理函数恢复默认，忽略的仍然忽略 */
    signal(SIGUSR2, SIG_IGN);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = (void (*)(int))exit;
    sigaction(SIGHUP, &sa, NULL);
    memset(&args, 0, sizeof args);
    args.flags = CLONE_CLEAR_SIGHAND;
    args.exit_signal = SIGCHLD;
    pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0) {
        struct sigaction a, b;
        sigaction(SIGHUP, NULL, &a);
        sigaction(SIGUSR2, NULL, &b);
        _exit((a.sa_handler == SIG_DFL) * 2 + (b.sa_handler == SIG_IGN));
    }
    int status;
    waitpid(pid, &status, 0);
    printf("clear sighand %d\n", WEXITSTATUS(status));
    return 0;
}
C
gcc -O1 -o pidfd pidfd.c
./pidfd

# execveat：相对目录 fd 的路径、AT_EMPTY_PATH（fexecve）、AT_SYMLINK_NOFOLLOW；新程序
# 看到的 AT_EXECFN 和 comm 是 /dev/fd/<n>[/<路径>]
mkdir -p /tmp/xat
cat > /tmp/xat/show.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    char exe[256] = {0}, comm[32] = {0};
    readlink("/proc/self/exe", exe, sizeof exe - 1);
    FILE *f = fopen("/proc/self/comm", "r");
    fgets(comm, sizeof comm, f);
    comm[strcspn(comm, "\n")] = 0;
    printf("argv0 %s execfn %s comm %s exe %s\n", argv[0], (char *)getauxval(AT_EXECFN), comm, exe);
    return 0;
}
C
cat > xat.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void run(const char *what, int dirfd, const char *path, int flags)
{
    char *argv[] = {"show", NULL};
    pid_t pid = fork();
    if (pid == 0) {
        syscall(SYS_execveat, dirfd, path, argv, environ, flags);
        printf("%s: %s\n", what, strerror(errno));
        _exit(1);
    }
    waitpid(pid, NULL, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int dir = open("/tmp/xat", O_DIRECTORY | O_RDONLY);
    int opath = open("/tmp/xat/show", O_PATH);
    int file = open("/tmp/xat/show", O_RDONLY | O_CLOEXEC);
    symlink("show", "/tmp/xat/link");
    run("relative", dir, "show", 0);
    run("absolute", dir, "/tmp/xat/show", 0);
    run("cwd", AT_FDCWD, "xat/show", 0);
    run("empty path O_PATH", opath, "", AT_EMPTY_PATH);
    run("empty path cloexec", file, "", AT_EMPTY_PATH);
    run("symlink", dir, "link", 0);
    run("symlink nofollow", dir, "link", AT_SYMLINK_NOFOLLOW);
    run("bad fd", -1, "", AT_EMPTY_PATH);
    run("bad flags", dir, "show", -1);
    run("not a dir", opath, "show", 0);
    run("empty without flag", dir, "", 0);
    /* 脚本：close-on-exec 的 fd 在解释器打开它之前就关了 */
    int s = open("/tmp/xat/script", O_CREAT | O_WRONLY | O_TRUNC, 0755);
    dprintf(s, "#!/bin/sh\necho script $0\n");
    close(s);
    int sc = open("/tmp/xat/script", O_RDONLY | O_CLOEXEC);
    run("script cloexec", sc, "", AT_EMPTY_PATH);
    run("script relative cloexec", dir, "script", 0);
    int sk = open("/tmp/xat/script", O_RDONLY);
    run("script", sk, "", AT_EMPTY_PATH);
    return 0;
}
C
gcc -O1 -o /tmp/xat/show /tmp/xat/show.c
gcc -O1 -o xatest xat.c
./xatest

# /proc/net/{tcp,tcp6,udp,udp6,icmp,icmp6,raw,unix}：按 fstat 的 inode 找到自己的
# socket，只看与环境无关的列（端口是临时分配的，只比较是否与 getsockname 一致）
cat > procnet.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static unsigned long ino(int fd)
{
    struct stat st;
    fstat(fd, &st);
    return st.st_ino;
}

static unsigned port(int fd, int peer)
{
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    (peer ? getpeername : getsockname)(fd, (struct sockaddr *)&ss, &len);
    return ntohs(((struct sockaddr_in *)&ss)->sin_port);
}

/* 在 /proc/net/<file> 里找 inode 为 fd 的那一行，打印地址、端口和状态 */
static void inet(const char *file, const char *what, int fd)
{
    char path[64], line[512];
    snprintf(path, sizeof path, "/proc/net/%s", file);
    FILE *f = fopen(path, "r");
    fgets(line, sizeof line, f);
    int width = strlen(line);
    while (fgets(line, sizeof line, f)) {
        char local[64], remote[64];
        unsigned st, lport, rport;
        unsigned long inode;
        if (sscanf(line, "%*d: %63[0-9A-F]:%X %63[0-9A-F]:%X %X %*s %*s %*s %*s %*s %lu", local, &lport,
                   remote, &rport, &st, &inode) != 6 || inode != ino(fd))
            continue;
        printf("%s %s: local %s port %s remote %s port %s st %02X width %s\n", file, what, local,
               lport == port(fd, 0) ? "ok" : "bad", remote,
               rport == 0 ? "0" : rport == port(fd, 1) ? "ok" : "bad", st,
               (int)strlen(line) == width ? "same" : "differs");
        fclose(f);
        return;
    }
    printf("%s %s: missing\n", file, what);
    fclose(f);
}

/* raw socket 的一行：本端的"端口"是协议号，对端端口总是 0，行尾不补空格 */
static void raw_line(const char *file, const char *what, int fd)
{
    char path[64], line[512];
    snprintf(path, sizeof path, "/proc/net/%s", file);
    FILE *f = fopen(path, "r");
    fgets(line, sizeof line, f);
    printf("%s header %s", file, line);
    while (fgets(line, sizeof line, f)) {
        char local[64], remote[64];
        unsigned st, lport, rport;
        unsigned long inode;
        if (sscanf(line, "%*d: %63[0-9A-F]:%X %63[0-9A-F]:%X %X %*s %*s %*s %*s %*s %lu", local, &lport,
                   remote, &rport, &st, &inode) != 6 || inode != ino(fd))
            continue;
        printf("%s %s: local %s port %u remote %s port %u st %02X tail '%c'\n", file, what, local, lport,
               remote, rport, st, line[strlen(line) - 2]);
        fclose(f);
        return;
    }
    printf("%s %s: missing\n", file, what);
    fclose(f);
}

static void unix_line(const char *what, int fd)
{
    char line[512];
    FILE *f = fopen("/proc/net/unix", "r");
    fgets(line, sizeof line, f);
    printf("unix header %s", line);
    while (fgets(line, sizeof line, f)) {
        unsigned ref, proto, flags, type, st;
        unsigned long inode;
        int n = 0;
        if (sscanf(line, "%*x: %X %X %X %X %X %lu%n", &ref, &proto, &flags, &type, &st, &inode, &n) != 6 ||
            inode != ino(fd))
            continue;
        printf("unix %s: flags %08X type %04X st %02X path%s", what, flags, type, st, line + n);
        fclose(f);
        return;
    }
    printf("unix %s: missing\n", what);
    fclose(f);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int lis = socket(AF_INET, SOCK_STREAM, 0);
    bind(lis, (struct sockaddr *)&a, sizeof a);
    listen(lis, 4);
    socklen_t len = sizeof a;
    getsockname(lis, (struct sockaddr *)&a, &len);
    int cli = socket(AF_INET, SOCK_STREAM, 0);
    connect(cli, (struct sockaddr *)&a, sizeof a);
    int srv = accept(lis, NULL, NULL);
    inet("tcp", "listen", lis);
    inet("tcp", "client", cli);
    inet("tcp", "server", srv);
    int udp = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in u = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    bind(udp, (struct sockaddr *)&u, sizeof u);
    inet("udp", "bound", udp);
    connect(udp, (struct sockaddr *)&a, sizeof a);
    inet("udp", "connected", udp);
    struct sockaddr_in6 a6 = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    int lis6 = socket(AF_INET6, SOCK_STREAM, 0);
    bind(lis6, (struct sockaddr *)&a6, sizeof a6);
    listen(lis6, 4);
    inet("tcp6", "listen", lis6);
    int udp6 = socket(AF_INET6, SOCK_DGRAM, 0);
    bind(udp6, (struct sockaddr *)&a6, sizeof a6);
    inet("udp6", "bound", udp6);
    inet("tcp", "tcp6 socket", lis6);
    /* ping socket 列在 icmp 里，raw ICMP socket 列在 raw 里 */
    struct sockaddr_in p = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int ping = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    bind(ping, (struct sockaddr *)&p, sizeof p);
    inet("icmp", "ping", ping);
    connect(ping, (struct sockaddr *)&p, sizeof p);
    inet("icmp", "ping connected", ping);
    int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    int type;
    socklen_t tl = sizeof type;
    getsockopt(raw, SOL_SOCKET, SO_TYPE, &type, &tl);
    printf("raw SO_TYPE %d\n", type);
    raw_line("raw", "icmp", raw);
    inet("icmp", "raw socket", raw);
    int ping6 = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6);
    bind(ping6, (struct sockaddr *)&a6, sizeof a6);
    inet("icmp6", "ping", ping6);

    struct sockaddr_un un = {.sun_family = AF_UNIX};
    strcpy(un.sun_path, "/tmp/procnet.sock");
    unlink(un.sun_path);
    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(ul, (struct sockaddr *)&un, sizeof un);
    listen(ul, 4);
    unix_line("listen", ul);
    int uc = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(uc, (struct sockaddr *)&un, sizeof un);
    unix_line("client", uc);
    struct sockaddr_un ab = {.sun_family = AF_UNIX};
    memcpy(ab.sun_path, "\0appine\0test", 12);
    int ua = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(ua, (struct sockaddr *)&ab, offsetof(struct sockaddr_un, sun_path) + 12);
    unix_line("abstract", ua);
    int sp[2];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sp);
    unix_line("pair", sp[0]);
    return 0;
}
C
gcc -O1 -o procnet procnet.c
./procnet

# fdinfo 里 epoll、timerfd、signalfd 特有的行；mountinfo 的设备号与 stat 一致，
# fdinfo 的 mnt_id 能在 mountinfo 里找到
cat > fdinfo.c <<'C'
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <unistd.h>

static void show(int fd)
{
    unsigned tfds[8] = {0};
    unsigned long long datas[8];
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f)) {
        unsigned tfd, events;
        unsigned long long data, sec, nsec;
        /* Linux 按内部的树排列关注，顺序不固定：按 fd 排 */
        if (sscanf(line, "tfd: %u events: %x data: %llx", &tfd, &events, &data) == 3)
            tfds[tfd < 8 ? tfd : 7] = events, datas[tfd < 8 ? tfd : 7] = data;
        else if (sscanf(line, "it_value: (%llu, %llu)", &sec, &nsec) == 2)
            printf("  it_value %llu\n", sec);
        else if (!strncmp(line, "clockid", 7) || !strncmp(line, "ticks", 5) || !strncmp(line, "settime", 7) ||
                 !strncmp(line, "it_interval", 11) || !strncmp(line, "sigmask", 7))
            printf("  %s", line);
    }
    fclose(f);
    for (int i = 0; i < 8; i++)
        if (tfds[i])
            printf("  tfd %d events %x data %llx\n", i, tfds[i], datas[i]);
}

static int mnt_id(int fd)
{
    char path[64], line[256];
    int id = -1;
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    while (fgets(line, sizeof line, f))
        sscanf(line, "mnt_id: %d", &id);
    fclose(f);
    return id;
}

/* mountinfo 里挂载点是 `where` 的那一行：设备号是否与 stat 一致，编号是否与 fd 的 mnt_id 一致 */
static void mount(const char *where, int fd)
{
    char line[512], point[256], type[64];
    int id, parent;
    unsigned maj, min;
    struct stat st;
    stat(where, &st);
    FILE *f = fopen("/proc/self/mountinfo", "r");
    while (fgets(line, sizeof line, f)) {
        char *dash = strstr(line, " - ");
        if (sscanf(line, "%d %d %u:%u %*s %255s", &id, &parent, &maj, &min, point) != 5 || strcmp(point, where) ||
            !dash || sscanf(dash, " - %63s", type) != 1)
            continue;
        /* 容器的根是 overlay，这里是 ext4：只看 /proc 的类型 */
        printf("%s: type %s dev %s mnt_id %s\n", where, strcmp(where, "/") ? type : "-",
               maj == major(st.st_dev) && min == minor(st.st_dev) ? "same" : "differs",
               id == mnt_id(fd) ? "same" : "differs");
    }
    fclose(f);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int ep = epoll_create1(0);
    int p[2];
    pipe(p);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.u64 = 0x1234};
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
    ev.events = EPOLLOUT;
    ev.data.u64 = 7;
    epoll_ctl(ep, EPOLL_CTL_ADD, p[1], &ev);
    printf("epoll\n");
    show(ep);
    int t = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec its = {.it_interval = {3, 0}, .it_value = {100, 0}};
    timerfd_settime(t, 0, &its, NULL);
    printf("timerfd\n");
    show(t);
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    its.it_value.tv_sec = now.tv_sec + 50;
    timerfd_settime(t, TFD_TIMER_ABSTIME, &its, NULL);
    printf("timerfd abstime\n");
    show(t);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGRTMIN);
    printf("signalfd\n");
    show(signalfd(-1, &set, 0));
    int root = open("/", O_RDONLY);
    int proc = open("/proc/self/stat", O_RDONLY);
    int sys = open("/sys/devices/system/cpu/online", O_RDONLY);
    mount("/", root);
    mount("/proc", proc);
    mount("/sys", sys);
    return 0;
}
C
gcc -O1 -o fdinfo fdinfo.c
./fdinfo

# 零碎的：老式 termio ioctl、RNDGETENTCNT、带偏移写管道的读端、MADV_REMOVE、
# MREMAP_DONTUNMAP
cat > misc.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pty.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <termios.h>
#include <unistd.h>

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif
#define RNDGETENTCNT _IOR('R', 0x00, int)

struct termio_ {
    unsigned short iflag, oflag, cflag, lflag;
    unsigned char line, cc[8];
};

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int m, s;
    openpty(&m, &s, NULL, NULL, NULL);
    struct termio_ t;
    struct termios ts;
    tcgetattr(s, &ts);
    printf("TCGETA %d lflag same %d cc same %d\n", ioctl(s, TCGETA, &t), t.lflag == (ts.c_lflag & 0xffff),
           !memcmp(t.cc, ts.c_cc, 8));
    t.lflag &= ~ECHO;
    t.cc[VMIN] = 3;
    printf("TCSETA %d\n", ioctl(s, TCSETA, &t));
    tcgetattr(s, &ts);
    printf("echo off %d vmin %d\n", !(ts.c_lflag & ECHO), ts.c_cc[VMIN]);
    printf("TCGETA bad %d %s\n", ioctl(s, TCGETA, (void *)8), strerror(errno));

    int r = open("/dev/urandom", O_RDONLY), cnt = -1;
    printf("RNDGETENTCNT %d %d\n", ioctl(r, RNDGETENTCNT, &cnt), cnt);

    int p[2];
    pipe(p);
    struct iovec iov = {"x", 1};
    printf("pwritev2 read end %zd %s\n", pwritev2(p[0], &iov, 1, 0, 0), strerror(errno));
    printf("pwritev2 read end at -1 %zd %s\n", pwritev2(p[0], &iov, 1, -1, 0), strerror(errno));
    printf("pread write end %zd %s\n", pread(p[1], &cnt, 1, 0), strerror(errno));

    long pg = sysconf(_SC_PAGESIZE);
    char *priv = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *shared = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    char *ro = mmap(NULL, pg, PROT_READ, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    strcpy(shared, "data");
    printf("remove private %d %s\n", madvise(priv, pg, MADV_REMOVE), strerror(errno));
    printf("remove read-only %d %s\n", madvise(ro, pg, MADV_REMOVE), strerror(errno));
    printf("remove shared %d left %d\n", madvise(shared, pg, MADV_REMOVE), shared[0]);
    mlock(shared, pg);
    printf("remove locked %d %s\n", madvise(shared, pg, MADV_REMOVE), strerror(errno));

    strcpy(priv, "moved");
    char *moved = mremap(priv, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    printf("dontunmap moved %d content %s old %d\n", moved != priv, moved, priv[0]);
    priv[0] = 1;
    printf("old still writable %d\n", priv[0]);
    printf("dontunmap no maymove %p %s\n", mremap(moved, pg, pg, MREMAP_DONTUNMAP), strerror(errno));
    printf("dontunmap grow %p %s\n", mremap(moved, pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP),
           strerror(errno));
    return 0;
}
C
gcc -O1 -o misc misc.c
./misc

# smaps、smaps_rollup、pagemap；mlock 和 MAP_POPULATE 把页调进内存
cat > smaps.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* smaps 里起点为 addr 的映射：打印几项计数和 VmFlags */
static void smaps(const char *what, void *addr)
{
    char line[256];
    int in = 0;
    FILE *f = fopen("/proc/self/smaps", "r");
    printf("%s:", what);
    while (fgets(line, sizeof line, f)) {
        unsigned long start, end, kb;
        char name[32];
        if (sscanf(line, "%lx-%lx", &start, &end) == 2 && strchr(line, ' ') - line == (long)strcspn(line, " ")) {
            if (in)
                break;
            in = start == (unsigned long)addr;
            continue;
        }
        if (!in)
            continue;
        if (sscanf(line, "%31[^:]: %lu kB", name, &kb) == 2 &&
            (!strcmp(name, "Size") || !strcmp(name, "Rss") || !strcmp(name, "Locked") || !strcmp(name, "Anonymous")))
            printf(" %s %lu", name, kb);
        if (!strncmp(line, "VmFlags:", 8))
            printf(" |%s", line + 8);
    }
    fclose(f);
}

static int present(void *addr, int pages)
{
    int fd = open("/proc/self/pagemap", O_RDONLY), n = 0;
    for (int i = 0; i < pages; i++) {
        uint64_t e;
        pread(fd, &e, 8, ((uintptr_t)addr / 4096 + i) * 8);
        n += e >> 63;
    }
    close(fd);
    return n;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    long pg = 4096;
    char *a = mmap(NULL, 16 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    unsigned char vec[16];
    mincore(a, 16 * pg, vec);
    int n = 0;
    for (int i = 0; i < 16; i++)
        n += vec[i] & 1;
    printf("before mlock: resident %d present %d\n", n, present(a, 16));
    mlock(a, 4 * pg);
    mincore(a, 16 * pg, vec);
    n = 0;
    for (int i = 0; i < 16; i++)
        n += vec[i] & 1;
    printf("after mlock: resident %d present %d\n", n, present(a, 16));
    smaps("locked part", a);
    smaps("rest", a + 4 * pg);
    char *b = mmap(NULL, 4 * pg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    smaps("read-only", b);
    int fd = open("/tmp/smaps-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fd, 8 * pg);
    char *c = mmap(NULL, 8 * pg, PROT_READ, MAP_PRIVATE | MAP_POPULATE, fd, 0);
    printf("populated file: present %d\n", present(c, 8));
    int pm = open("/proc/self/pagemap", O_RDONLY);
    uint64_t e;
    printf("unaligned read %zd\n", pread(pm, &e, 4, 0));
    FILE *r = fopen("/proc/self/smaps_rollup", "r");
    char line[256];
    fgets(line, sizeof line, r);
    printf("rollup header ends with [rollup]: %d at column %ld\n", strstr(line, "[rollup]") != NULL,
           strstr(line, "[rollup]") - line);
    /* 6.6 起多一行 KSM：只看几个字段在不在 */
    int fields = 0;
    while (fgets(line, sizeof line, r))
        fields += !strncmp(line, "Rss:", 4) || !strncmp(line, "Pss_Anon:", 9) || !strncmp(line, "Locked:", 7);
    printf("rollup fields %d\n", fields);
    /* maps 里没有名字的行末尾有一个空格 */
    FILE *m = fopen("/proc/self/maps", "r");
    int unnamed = 0, trailing = 0;
    while (fgets(line, sizeof line, m)) {
        char *nl = strchr(line, '\n');
        if (nl && nl[-1] == ' ' && nl[-2] == '0')
            trailing++;
        int cols = 0;
        for (char *t = strtok(line, " \n"); t; t = strtok(NULL, " \n"))
            cols++;
        unnamed += cols == 5;
    }
    printf("unnamed lines all end with a space: %d\n", unnamed == trailing && unnamed > 0);
    return 0;
}
C
gcc -O1 -o smaps smaps.c
./smaps

# 经由 /proc/self/fd 重新打开 memfd 和删除之后还开着的文件；O_NOFOLLOW 停在链接上是 ELOOP
cat > mfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static void try(const char *what, const char *path, int flags)
{
    int fd = open(path, flags, 0600);
    char buf[16] = {0};
    if (fd < 0) { printf("%s: %s\n", what, strerror(errno)); return; }
    pread(fd, buf, 4, 0);
    printf("%s: ok [%s] flags %o\n", what, buf, fcntl(fd, F_GETFL) & O_ACCMODE);
    close(fd);
}
int main(void)
{
    int m = memfd_create("t", 0);
    write(m, "data", 4);
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", m);
    try("rdonly", p, O_RDONLY);
    try("rdwr", p, O_RDWR);
    try("creat", p, O_RDWR | O_CREAT);
    try("creat excl", p, O_RDWR | O_CREAT | O_EXCL);
    try("creat trunc", p, O_RDWR | O_CREAT | O_TRUNC);
    try("nofollow", p, O_RDONLY | O_NOFOLLOW);
    int t = open("/tmp/unlinked", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(t, "gone", 4);
    unlink("/tmp/unlinked");
    snprintf(p, sizeof p, "/proc/self/fd/%d", t);
    try("unlinked", p, O_RDONLY);
    int pp[2];
    pipe(pp);
    snprintf(p, sizeof p, "/proc/self/fd/%d", pp[1]);
    try("pipe creat", p, O_WRONLY | O_CREAT);
    return 0;
}
C
gcc -O1 -o mfd mfd.c
./mfd

# unshare、ru_maxrss、tkill 排不上队、futex 的负数、已经连接的 socket、接口 ioctl、
# 有可写映射时封印写入。定时器超限次数封顶由 LTP 的 timer_settime03 测：间隔 1 纳秒的
# 定时器在原生一侧会造成信号风暴，结果取决于时机
cat > misc2.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_CMP_REQUEUE 4

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    /* unshare(CLONE_FILES)：之后关的 fd 不影响原来共用 fd 表的进程 */
    int fd = open("/dev/null", O_RDONLY);
    pid_t pid = syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
    if (pid == 0) {
        printf("unshare files %d\n", unshare(CLONE_FILES));
        close(fd);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    printf("fd still open in parent %d\n", fcntl(fd, F_GETFD) != -1);
    pid = syscall(SYS_clone, CLONE_FS | SIGCHLD, 0, 0, 0, 0);
    if (pid == 0) {
        unshare(CLONE_FS);
        chdir("/tmp");
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    char cwd[64];
    printf("cwd after child chdir %s\n", getcwd(cwd, sizeof cwd));
    printf("unshare bad %d %s\n", unshare(-1), strerror(errno));
    printf("unshare vm single-threaded %d\n", unshare(CLONE_VM));

    /* 子进程用了 64 MB：ru_maxrss 至少有一半（内存紧张时有的页可能已经换出） */
    pid = fork();
    if (pid == 0) {
        char *p = malloc(64 << 20);
        memset(p, 1, 64 << 20);
        _exit(p[100] - 1);
    }
    waitpid(pid, NULL, 0);
    struct rusage ru;
    getrusage(RUSAGE_CHILDREN, &ru);
    printf("children maxrss >= 32MB %d\n", ru.ru_maxrss >= 32 * 1024);

    /* tkill 的实时信号排不上队是 EAGAIN */
    struct rlimit zero = {0, 0}, old;
    getrlimit(RLIMIT_SIGPENDING, &old);
    setrlimit(RLIMIT_SIGPENDING, &zero);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &set, NULL);
    printf("tkill %ld %s\n", syscall(SYS_tkill, gettid(), SIGRTMIN), strerror(errno));
    setrlimit(RLIMIT_SIGPENDING, &old);

    int f1 = 0, f2 = 0;
    printf("requeue negative %ld %s\n",
           syscall(SYS_futex, &f1, FUTEX_CMP_REQUEUE, 1, (long)-1, &f2, 0), strerror(errno));

    /* 阻塞的 connect 返回之后再 connect 是 EISCONN */
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int l = socket(AF_INET, SOCK_STREAM, 0);
    bind(l, (void *)&a, sizeof a);
    listen(l, 1);
    socklen_t len = sizeof a;
    getsockname(l, (void *)&a, &len);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&a, sizeof a);
    printf("connect again %d %s\n", connect(c, (void *)&a, sizeof a), strerror(errno));
    printf("SIOCATMARK null %d %s\n", ioctl(c, SIOCATMARK, NULL), strerror(errno));
    int u = socket(AF_INET, SOCK_DGRAM, 0), mark;
    printf("SIOCATMARK udp %d %s\n", ioctl(u, SIOCATMARK, &mark), strerror(errno));
    printf("SIOCSIFFLAGS bad %d %s\n", ioctl(u, SIOCSIFFLAGS, (void *)8), strerror(errno));

    /* memfd 有可写的共享映射时不能封印写入 */
    int m = memfd_create("seal", MFD_ALLOW_SEALING);
    ftruncate(m, 4096);
    void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, m, 0);
    printf("seal write mapped %d %s\n", fcntl(m, F_ADD_SEALS, F_SEAL_WRITE), strerror(errno));
    munmap(p, 4096);
    printf("seal write unmapped %d\n", fcntl(m, F_ADD_SEALS, F_SEAL_WRITE));
    return 0;
}
C
gcc -O1 -o misc2 misc2.c
./misc2

# 信号驱动的 I/O（O_ASYNC、F_SETSIG、FIOSETOWN）和文件租约。租约用 /dev/shm 里的文件：
# 原生一侧的根是 overlayfs，Linux 在它上面不让设写租约
cat > async.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void wait_sig(const char *what, sigset_t *set)
{
    siginfo_t si;
    struct timespec t = {5, 0};
    int sig = sigtimedwait(set, &si, &t);
    if (sig < 0) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    printf("%s: sig %s code %d", what, sig == SIGIO ? "SIGIO" : sig == SIGUSR1 ? "SIGUSR1" : "?", si.si_code);
    if (sig == SIGUSR1)
        printf(" fd %d band %lx", si.si_fd, (long)si.si_band);
    printf("\n");
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGIO);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int p[2];
    pipe(p);
    fcntl(p[0], F_SETOWN, getpid());
    fcntl(p[0], F_SETSIG, SIGUSR1);
    printf("setfl %d async %d\n", fcntl(p[0], F_SETFL, O_ASYNC | O_NONBLOCK),
           (fcntl(p[0], F_GETFL) & O_ASYNC) != 0);
    if (fork() == 0) {
        usleep(100000);
        write(p[1], "x", 1);
        _exit(0);
    }
    wait_sig("pipe", &set);
    char c;
    read(p[0], &c, 1);
    wait(NULL);
    /* 默认的 SIGIO */
    fcntl(p[0], F_SETSIG, 0);
    write(p[1], "y", 1);
    wait_sig("pipe default", &set);
    read(p[0], &c, 1);
    /* 没有属主：不发 */
    fcntl(p[0], F_SETOWN, 0);
    write(p[1], "z", 1);
    struct timespec zero = {0, 200000000};
    printf("no owner: %d\n", sigtimedwait(&set, NULL, &zero));
    read(p[0], &c, 1);
    /* TCP：数据从别的进程来 */
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int l = socket(AF_INET, SOCK_STREAM, 0);
    bind(l, (void *)&a, sizeof a);
    listen(l, 1);
    socklen_t len = sizeof a;
    getsockname(l, (void *)&a, &len);
    if (fork() == 0) {
        int c2 = socket(AF_INET, SOCK_STREAM, 0);
        connect(c2, (void *)&a, sizeof a);
        usleep(200000);
        write(c2, "hello", 5);
        usleep(200000);
        _exit(0);
    }
    int s = accept(l, NULL, NULL);
    int pid = getpid(), on = 1, owner = 0;
    ioctl(s, FIOSETOWN, &pid);
    ioctl(s, FIOGETOWN, &owner);
    printf("socket owner ok %d\n", owner == pid);
    fcntl(s, F_SETSIG, SIGUSR1);
    ioctl(s, FIOASYNC, &on);
    while (sigtimedwait(&set, NULL, &(struct timespec){0, 0}) > 0)
        ;
    wait_sig("tcp data", &set);
    char buf[16];
    printf("read %zd\n", read(s, buf, sizeof buf));
    wait(NULL);
    return 0;
}
C
gcc -O1 -o /tmp/async async.c && /tmp/async
cat > lease.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FILE "/dev/shm/appine-lease"

static const char *name(int t) { return t == F_RDLCK ? "RDLCK" : t == F_WRLCK ? "WRLCK" : "UNLCK"; }

/* 子进程以 flags 打开文件（打破租约），退出码是 errno */
static pid_t opener(int flags)
{
    pid_t pid = fork();
    if (pid == 0)
        _exit(open(FILE, flags) >= 0 ? 0 : errno);
    return pid;
}

/* 等子进程，打印它打开的结果 */
static void reap(const char *what, pid_t pid)
{
    int status;
    waitpid(pid, &status, 0);
    printf("%s: %s\n", what, WEXITSTATUS(status) ? strerror(WEXITSTATUS(status)) : "opened");
}

static void wait_break(sigset_t *set, int fd)
{
    siginfo_t si;
    struct timespec t = {5, 0};
    int sig = sigtimedwait(set, &si, &t);
    printf("  signal %s code %d fd ok %d, lease now %s\n", sig == SIGUSR1 ? "SIGUSR1" : strerror(errno),
           si.si_code, si.si_fd == fd, name(fcntl(fd, F_GETLEASE)));
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    close(open(FILE, O_CREAT | O_RDWR | O_TRUNC, 0644));
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGIO);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int w = open(FILE, O_RDWR);
    printf("read lease on a writable fd %d %s\n", fcntl(w, F_SETLEASE, F_RDLCK), strerror(errno));
    int r = open(FILE, O_RDONLY);
    printf("write lease with another open %d %s\n", fcntl(r, F_SETLEASE, F_WRLCK), strerror(errno));
    close(w);
    printf("unlock without a lease %d %s\n", fcntl(r, F_SETLEASE, F_UNLCK), strerror(errno));
    fcntl(r, F_SETSIG, SIGUSR1);
    printf("write lease %d, lease %s\n", fcntl(r, F_SETLEASE, F_WRLCK), name(fcntl(r, F_GETLEASE)));
    /* 非阻塞的打开：通知持有者，立即返回 EWOULDBLOCK */
    reap("nonblocking read open", opener(O_RDONLY | O_NONBLOCK));
    wait_break(&set, r);
    /* 读方式打开：降成读租约之后打开完成 */
    pid_t pid = opener(O_RDONLY);
    usleep(200000);
    printf("  downgrade %d, lease %s\n", fcntl(r, F_SETLEASE, F_RDLCK), name(fcntl(r, F_GETLEASE)));
    reap("read open", pid);
    /* 写方式打开：要放弃租约，降成读租约不行 */
    pid = opener(O_WRONLY);
    wait_break(&set, r);
    printf("  downgrade %d, unlock %d\n", fcntl(r, F_SETLEASE, F_RDLCK), fcntl(r, F_SETLEASE, F_UNLCK));
    reap("write open", pid);
    unlink(FILE);
    return 0;
}
C
gcc -O1 -o lease lease.c
./lease

# POSIX 消息队列：优先级、非阻塞和超时、状态行、poll、mq_notify（信号和 musl 的线程）
cat > mq.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile int notified;
static void on_notify(union sigval v) { notified = v.sival_int; }

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    mq_unlink("/appine-mq");
    struct mq_attr attr = {.mq_maxmsg = 4, .mq_msgsize = 64};
    mqd_t q = mq_open("/appine-mq", O_CREAT | O_RDWR | O_EXCL, 0600, &attr);
    printf("open %d excl again %d %s\n", q >= 0, mq_open("/appine-mq", O_CREAT | O_EXCL | O_RDWR, 0600, &attr),
           strerror(errno));
    printf("bad attr %d %s\n", mq_open("/appine-mq2", O_CREAT | O_RDWR, 0600, &(struct mq_attr){.mq_maxmsg = 0}),
           strerror(errno));
    mq_send(q, "low", 3, 1);
    mq_send(q, "high", 4, 9);
    mq_send(q, "low2", 4, 1);
    struct mq_attr got;
    mq_getattr(q, &got);
    printf("attr maxmsg %ld msgsize %ld curmsgs %ld\n", got.mq_maxmsg, got.mq_msgsize, got.mq_curmsgs);
    char status[128] = {0};
    pread(q, status, 16, 0);
    printf("status %.16s\n", status);
    struct pollfd p = {q, POLLIN | POLLOUT, 0};
    poll(&p, 1, 0);
    printf("poll %x\n", p.revents);
    printf("too long %d %s\n", mq_send(q, status, 65, 0), strerror(errno));
    printf("bad prio %d %s\n", mq_send(q, "x", 1, 40000), strerror(errno));
    char buf[64];
    unsigned prio;
    printf("small buffer %zd %s\n", mq_receive(q, buf, 10, &prio), strerror(errno));
    for (int i = 0; i < 3; i++) {
        ssize_t n = mq_receive(q, buf, sizeof buf, &prio);
        printf("got %.*s prio %u\n", (int)n, buf, prio);
    }
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    t.tv_nsec += 50000000;
    if (t.tv_nsec >= 1000000000) t.tv_sec++, t.tv_nsec -= 1000000000;
    printf("timed out %zd %s\n", mq_timedreceive(q, buf, sizeof buf, &prio, &t), strerror(errno));
    struct mq_attr nb = {.mq_flags = O_NONBLOCK};
    mq_setattr(q, &nb, NULL);
    printf("nonblocking %zd %s\n", mq_receive(q, buf, sizeof buf, &prio), strerror(errno));

    /* 信号通知：空队列来了消息 */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1, .sigev_value.sival_int = 42};
    printf("notify %d\n", mq_notify(q, &sev));
    pid_t pid = fork();
    if (pid == 0) {
        printf("child notify %d %s\n", mq_notify(q, &sev), strerror(errno));
        mq_send(q, "wake", 4, 0);
        _exit(0);
    }
    siginfo_t si;
    sigwaitinfo(&set, &si);
    printf("signal code %d value %d from child %d\n", si.si_code, si.si_value.sival_int, si.si_pid == pid);
    waitpid(pid, NULL, 0);
    mq_receive(q, buf, sizeof buf, &prio);
    /* musl 的 SIGEV_THREAD：经由 netlink socket 通知一个线程 */
    struct sigevent th = {.sigev_notify = SIGEV_THREAD, .sigev_notify_function = on_notify,
                          .sigev_value.sival_int = 7};
    printf("thread notify %d\n", mq_notify(q, &th));
    mq_send(q, "go", 2, 0);
    for (int i = 0; i < 200 && !notified; i++)
        usleep(10000);
    printf("thread notified %d\n", notified);
    mq_close(q);
    printf("unlink %d reopen %d %s\n", mq_unlink("/appine-mq"), mq_open("/appine-mq", O_RDWR), strerror(errno));
    return 0;
}
C
gcc -O1 -o mq mq.c
./mq

# 原生 AIO（直接发 syscall，不用 libaio）：读写、fsync、poll、eventfd 通知、取消
cat > aio.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* 镜像里没有 linux-headers：linux/aio_abi.h 的定义 */
typedef unsigned long aio_context_t;
enum { IOCB_CMD_PREAD = 0, IOCB_CMD_PWRITE = 1, IOCB_CMD_FSYNC = 2, IOCB_CMD_POLL = 5 };
#define IOCB_FLAG_RESFD 1
struct iocb {
    uint64_t aio_data;
    uint32_t aio_key, aio_rw_flags;
    uint16_t aio_lio_opcode;
    int16_t aio_reqprio;
    uint32_t aio_fildes;
    uint64_t aio_buf, aio_nbytes;
    int64_t aio_offset;
    uint64_t aio_reserved2;
    uint32_t aio_flags, aio_resfd;
};
struct io_event {
    uint64_t data, obj;
    int64_t res, res2;
};

static struct iocb cb(int fd, int op, void *buf, size_t n, long long off, uint64_t data)
{
    struct iocb c;
    memset(&c, 0, sizeof c);
    c.aio_fildes = fd;
    c.aio_lio_opcode = op;
    c.aio_buf = (uintptr_t)buf;
    c.aio_nbytes = n;
    c.aio_offset = off;
    c.aio_data = data;
    return c;
}

static void events(aio_context_t ctx, long min, const char *what)
{
    struct io_event ev[8];
    struct timespec t = {0, 200000000};
    long n = syscall(SYS_io_getevents, ctx, min, 8, ev, &t);
    printf("%s: %ld", what, n);
    for (long i = 0; i < n; i++)
        printf(" [data %llu res %lld]", (unsigned long long)ev[i].data, (long long)ev[i].res);
    printf("\n");
}

/* 在 io_getevents 里一直等（不限时） */
struct waiter {
    aio_context_t ctx;
    long min, n;
    int err;
};

static void *wait_events(void *arg)
{
    struct waiter *w = arg;
    struct io_event ev[8];
    w->n = syscall(SYS_io_getevents, w->ctx, w->min, 8, ev, NULL);
    w->err = errno;
    return NULL;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    aio_context_t ctx = 0;
    printf("setup %ld\n", syscall(SYS_io_setup, 8, &ctx));
    int fd = open("/tmp/aio-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
    char out[] = "hello aio", in[16] = {0};
    struct iocb w = cb(fd, IOCB_CMD_PWRITE, out, 9, 100, 1);
    struct iocb s = cb(fd, IOCB_CMD_FSYNC, NULL, 0, 0, 2);
    struct iocb *list[] = {&w, &s};
    printf("submit %ld\n", syscall(SYS_io_submit, ctx, 2, list));
    events(ctx, 2, "write+fsync");
    struct iocb r = cb(fd, IOCB_CMD_PREAD, in, 9, 100, 3);
    int efd = eventfd(0, 0);
    r.aio_flags = IOCB_FLAG_RESFD;
    r.aio_resfd = efd;
    struct iocb *one[] = {&r};
    syscall(SYS_io_submit, ctx, 1, one);
    uint64_t count;
    read(efd, &count, 8);
    events(ctx, 1, "read");
    printf("read back %s eventfd %llu\n", in, (unsigned long long)count);
    int p[2];
    pipe(p);
    struct iocb pl = cb(p[0], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 4);
    one[0] = &pl;
    syscall(SYS_io_submit, ctx, 1, one);
    events(ctx, 0, "poll before data");
    write(p[1], "x", 1);
    events(ctx, 1, "poll after data");
    struct iocb pl2 = cb(p[1], IOCB_CMD_POLL, (void *)(uintptr_t)POLLIN, 0, 0, 5);
    one[0] = &pl2;
    syscall(SYS_io_submit, ctx, 1, one);
    struct io_event res;
    printf("cancel %ld %s\n", syscall(SYS_io_cancel, ctx, &pl2, &res), strerror(errno));
    events(ctx, 1, "after cancel");
    struct iocb bad = cb(fd, 99, in, 1, 0, 6);
    one[0] = &bad;
    printf("bad opcode %ld %s\n", syscall(SYS_io_submit, ctx, 1, one), strerror(errno));
    printf("min > nr %ld %s\n", syscall(SYS_io_getevents, ctx, 2, 1, &res, NULL), strerror(errno));
    printf("destroy %ld again %ld %s\n", syscall(SYS_io_destroy, ctx), syscall(SYS_io_destroy, ctx),
           strerror(errno));
    /* 别的线程在 io_getevents 里等着时销毁上下文：它醒来返回，一个事件也没有是 EINVAL，
     * 已经完成的照样取走。mariadb 关闭时这样结束等事件的线程 */
    for (int done = 0; done <= 1; done++) {
        struct waiter wt = {0, done ? 2 : 1, 0, 0};
        syscall(SYS_io_setup, 8, &wt.ctx);
        struct iocb w2 = cb(fd, IOCB_CMD_PWRITE, out, 9, 0, 7);
        one[0] = &w2;
        if (done)
            syscall(SYS_io_submit, wt.ctx, 1, one);
        pthread_t t;
        pthread_create(&t, NULL, wait_events, &wt);
        usleep(100000);
        printf("destroy while waiting (%d done) %ld", done, syscall(SYS_io_destroy, wt.ctx));
        struct timespec limit;
        clock_gettime(CLOCK_REALTIME, &limit);
        limit.tv_sec += 5;
        if (pthread_timedjoin_np(t, NULL, &limit) != 0) {
            printf(", the waiter is still waiting\n");
            return 1;
        }
        printf(", the waiter got %ld %s\n", wt.n, wt.n < 0 ? strerror(wt.err) : "");
    }
    return 0;
}
C
gcc -O1 -o aio aio.c -pthread
./aio

# MREMAP_DONTUNMAP 用在各种映射上之后原来的地址上看到什么；remap_file_pages
cat > dun.c <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/mman.h>
#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif
static void show(const char *tag, char *old, char *new) {
    printf("%s: old=%.4s new=%.4s\n", tag, old[0] ? old : "(0)", new);
}
int main(void) {
    long pg = 4096;
    int fd = open("dun.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[8192];
    memset(buf, 'f', sizeof buf);
    write(fd, buf, sizeof buf);
    /* 共享文件映射 */
    char *a = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    memcpy(a, "AAAA", 4);
    char *b = mremap(a, 2 * pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    if (b == MAP_FAILED) { printf("shared file: %s\n", strerror(errno)); return 1; }
    show("shared file", a, b);
    memcpy(b, "BBBB", 4);
    show("shared file after write new", a, b);
    memcpy(a + pg, "CCCC", 4);
    show("shared file second page", a + pg, b + pg);
    pread(fd, buf, 4, 0);
    printf("file now %.4s\n", buf);
    /* 私有文件映射 */
    char *p = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    memcpy(p, "PPPP", 4);
    char *q = mremap(p, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    if (q == MAP_FAILED) { printf("private file: %s\n", strerror(errno)); return 1; }
    show("private file", p, q);
    memcpy(p, "OOOO", 4);
    pread(fd, buf, 4, 0);
    printf("private write old -> file %.4s\n", buf);
    /* 共享匿名映射 */
    char *s = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memcpy(s, "SSSS", 4);
    char *t = mremap(s, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("shared anon", s, t);
    memcpy(t, "TTTT", 4);
    show("shared anon after write new", s, t);
    /* 私有匿名映射 */
    char *x = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(x, "XXXX", 4);
    char *y = mremap(x, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("private anon", x, y);
    /* 只读的共享文件映射 */
    char *r = mmap(0, pg, PROT_READ, MAP_SHARED, fd, pg);
    char *r2 = mremap(r, pg, pg, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    show("readonly shared", r, r2);
    /* remap_file_pages */
    char *w = mmap(0, 2 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    printf("remap %d", remap_file_pages(w, pg, 0, 1, 0));
    printf(" -> %.4s %.4s\n", w, w + pg);
    printf("remap prot %d %s\n", remap_file_pages(w, pg, PROT_READ, 1, 0), strerror(errno));
    printf("remap private %d %s\n", remap_file_pages(p, pg, 0, 1, 0), strerror(errno));
    return 0;
}
C
gcc -O1 -o dun dun.c
./dun

# sysfs 里 CPU 的拓扑（busybox 的 nproc --all 数 cpu<n> 目录）；possible、present 在有的
# 宿主上比 online 多，不比
n=$(grep -c ^processor /proc/cpuinfo)
[ "$(nproc --all)" = "$n" ] && echo "nproc --all matches cpuinfo"
[ "$(ls -d /sys/devices/system/cpu/cpu[0-9]* | wc -l)" = "$n" ] && echo "cpu directories match cpuinfo"
for f in online cpu0/online cpu0/topology/thread_siblings_list; do
    echo "$f: $(cat /sys/devices/system/cpu/$f)"
done
test -r /sys/devices/system/cpu/possible && test -r /sys/devices/system/cpu/present && echo "possible present"
stat -c 'size %s blocks %b' /sys/devices/system/cpu/online
stat -f -c 'type %T' /sys
# /proc/loadavg 的格式（数值取决于宿主）
sed -E 's/[0-9]+\.[0-9][0-9]/L/g; s/[0-9]+\/[0-9]+ [0-9]+$/R\/T P/' /proc/loadavg

# /proc/<pid>/limits 与 getrlimit 一致，列宽与 Linux 相同；删除之后仍然打开着的文件可以
# futimens
cat > limits.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(char *out, rlim_t v)
{
    if (v == RLIM_INFINITY)
        strcpy(out, "unlimited");
    else
        sprintf(out, "%llu", (unsigned long long)v);
}

int main(void)
{
    struct rlimit rl = {777, 777};
    setrlimit(RLIMIT_NOFILE, &rl);
    FILE *f = fopen("/proc/self/limits", "r");
    char line[256];
    int n = 0, same = 0;
    fgets(line, sizeof line, f);
    printf("[%.*s]\n", (int)strcspn(line, "\n"), line);
    while (fgets(line, sizeof line, f)) {
        char soft[32], hard[32], want_soft[32], want_hard[32];
        struct rlimit r;
        getrlimit(n++, &r);
        show(want_soft, r.rlim_cur);
        show(want_hard, r.rlim_max);
        if (sscanf(line + 26, "%31s %31s", soft, hard) == 2 && !strcmp(soft, want_soft) &&
            !strcmp(hard, want_hard))
            same++;
        if (!strncmp(line, "Max open files", 14) || !strncmp(line, "Max nice", 8))
            printf("[%.*s]\n", (int)strcspn(line, "\n"), line);
    }
    printf("%d limits, %d same as getrlimit\n", n, same);

    int fd = open("futimens.tmp", O_CREAT | O_RDWR | O_TRUNC, 0644);
    unlink("futimens.tmp");
    usleep(100000);
    struct timespec ts[2] = {{1000000000, 0}, {1234567890, 5}};
    printf("futimens %d\n", futimens(fd, ts));
    struct stat st;
    fstat(fd, &st);
    printf("atime %ld mtime %ld.%09ld\n", (long)st.st_atime, (long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
    ts[1].tv_sec = 1500000000;
    printf("empty path %d\n", utimensat(fd, "", ts, AT_EMPTY_PATH));
    fstat(fd, &st);
    printf("mtime %ld nlink %ld\n", (long)st.st_mtime, (long)st.st_nlink);
    return 0;
}
C
gcc -O1 -o limits limits.c
./limits

# mremap 扩展私有文件映射：后面接着的是文件的内容，私有地写不影响文件，文件末尾之后的页
# 是 SIGBUS
cat > mrp.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static sigjmp_buf env;
static void on_bus(int sig) { siglongjmp(env, sig); }

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    long ps = 4096;
    int fd = open("/tmp/mrp.dat", O_RDWR | O_CREAT | O_TRUNC, 0600);
    char page[4096];
    for (int i = 0; i < 3; i++) {
        memset(page, 'a' + i, sizeof page);
        write(fd, page, sizeof page);
    }
    /* 只映射第一页，再扩到三页、五页：后面接着的是文件的内容，文件末尾之后 SIGBUS */
    int prots[] = {PROT_READ, PROT_READ | PROT_WRITE};
    for (int k = 0; k < 2; k++) {
        int prot = prots[k];
        char *p = mmap(NULL, ps, prot, MAP_PRIVATE, fd, 0);
        char *q = mremap(p, ps, 3 * ps, MREMAP_MAYMOVE);
        printf("prot %d: grew %s: %c %c %c\n", prot, q == MAP_FAILED ? "failed" : "ok",
               q == MAP_FAILED ? '-' : q[0], q == MAP_FAILED ? '-' : q[ps], q == MAP_FAILED ? '-' : q[2 * ps]);
        if (q == MAP_FAILED)
            continue;
        if (prot & PROT_WRITE) {
            q[2 * ps] = 'X';
            char c;
            pread(fd, &c, 1, 2 * ps);
            printf("private write: mapping %c, file %c\n", q[2 * ps], c);
        }
        char *r = mremap(q, 3 * ps, 5 * ps, MREMAP_MAYMOVE);
        signal(SIGBUS, on_bus);
        int sig = sigsetjmp(env, 1);
        if (!sig)
            printf("past the end: %c\n", r[3 * ps]);
        else
            printf("past the end: signal %d\n", sig);
        munmap(r, 5 * ps);
    }
    return 0;
}
C
gcc -O1 -o mrp mrp.c && ./mrp

# /sys/devices/system/cpu/cpu<n>/cache 照搬宿主的（CPU 编号换成 guest 的），只看已知的文件；
# 宿主没有 cpufreq 时也没有
cd /sys/devices/system/cpu
for c in cpu*/; do
    for d in "$c"cache/index*; do
        [ -d "$d" ] || continue
        for f in level type size coherency_line_size ways_of_associativity number_of_sets \
            shared_cpu_list shared_cpu_map id; do
            [ -e "$d/$f" ] && echo "$d/$f: $(cat "$d/$f")"
        done
    done
done
ls cpu0 | grep -c '^cache$' || true
[ -d cpu0/cpufreq ] && echo cpufreq || echo no cpufreq
cd /tmp

# signalfd 不属于哪个线程：poll、epoll 和读看的都是调用的线程和它的进程挂起的信号。发给
# 另一个线程的看不到，阻塞在 poll 里的线程收到发给自己的信号时醒来，fork 出来的子进程看
# 自己的，创建它的线程退出之后照样可用
cat > sfd.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/wait.h>
#include <unistd.h>

static int sfd, cmd[2], res[2], ep_b;
static sigset_t set;

static int pollnow(int fd)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, 0) == 1 && p.revents & POLLIN;
}

/* 读到的信号；没有时是 0（EAGAIN） */
static int readsig(int fd)
{
    struct signalfd_siginfo si;
    ssize_t n = read(fd, &si, sizeof si);
    return n == sizeof si ? (int)si.ssi_signo : errno == EAGAIN ? 0 : -errno;
}

/* 线程 B 按命令执行：p poll，e epoll（B 自己的 epoll），r 读，w 阻塞 poll 最多 2 秒 */
static void *thread_b(void *arg)
{
    (void)arg;
    ep_b = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep_b, EPOLL_CTL_ADD, sfd, &ev);
    char c;
    while (read(cmd[0], &c, 1) == 1 && c != 'q') {
        struct pollfd p = {sfd, POLLIN, 0};
        int r = 0;
        if (c == 'p')
            r = pollnow(sfd);
        else if (c == 'e')
            r = epoll_wait(ep_b, &ev, 1, 0);
        else if (c == 'r')
            r = readsig(sfd);
        else if (c == 'w')
            r = poll(&p, 1, 2000);
        write(res[1], &r, sizeof r);
    }
    return NULL;
}

static int in_b(char c)
{
    int r;
    write(cmd[1], &c, 1);
    read(res[0], &r, sizeof r);
    return r;
}

static void *create(void *arg)
{
    (void)arg;
    return (void *)(long)signalfd(-1, &set, SFD_NONBLOCK);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    sfd = signalfd(-1, &set, SFD_NONBLOCK);
    pipe(cmd);
    pipe(res);
    pthread_t b;
    pthread_create(&b, NULL, thread_b, NULL);
    int r[5];

    pthread_kill(pthread_self(), SIGUSR1);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('e'), r[3] = in_b('r'), r[4] = readsig(sfd);
    printf("to A: A poll %d, B poll %d, B epoll %d, B read %d, A read %d\n", r[0], r[1], r[2], r[3], r[4]);

    pthread_kill(b, SIGUSR2);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('e'), r[3] = readsig(sfd), r[4] = in_b('r');
    printf("to B: A poll %d, B poll %d, B epoll %d, A read %d, B read %d\n", r[0], r[1], r[2], r[3], r[4]);

    kill(getpid(), SIGUSR1);
    r[0] = pollnow(sfd), r[1] = in_b('p'), r[2] = in_b('r'), r[3] = pollnow(sfd), r[4] = in_b('p');
    printf("to the process: A poll %d, B poll %d, B read %d, then A poll %d, B poll %d\n", r[0], r[1],
           r[2], r[3], r[4]);

    char c = 'w';
    write(cmd[1], &c, 1);
    usleep(100000);
    pthread_kill(b, SIGUSR2);
    read(res[0], &r[0], sizeof r[0]);
    r[1] = in_b('r');
    printf("B blocked in poll, signal to B: poll %d, read %d\n", r[0], r[1]);

    pid_t child = fork();
    if (child == 0) {
        struct pollfd p = {sfd, POLLIN, 0};
        int polled = poll(&p, 1, 2000);
        printf("fork child: poll %d, read %d\n", polled, readsig(sfd));
        _exit(0);
    }
    usleep(100000);
    kill(child, SIGUSR2);
    waitpid(child, NULL, 0);
    printf("parent after the child's signal: poll %d\n", pollnow(sfd));

    pthread_t creator;
    void *fd2;
    pthread_create(&creator, NULL, create, NULL);
    pthread_join(creator, &fd2);
    pthread_kill(pthread_self(), SIGUSR1);
    r[0] = pollnow((int)(long)fd2), r[1] = readsig((int)(long)fd2);
    printf("creator thread gone: poll %d, read %d\n", r[0], r[1]);

    c = 'q';
    write(cmd[1], &c, 1);
    pthread_join(b, NULL);
    return 0;
}
C
gcc -O1 -o sfd sfd.c -lpthread && ./sfd

# /dev/pts/<n> 的属主、属组和权限（devpts 挂载时 gid=5、mode=620）：sshd 把终端 chown 给
# 登录的用户和 tty 组；普通用户只能改自己的终端
cat > ptyown.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, const char *path, int fd)
{
    struct stat a, b;
    stat(path, &a);
    fstat(fd, &b);
    printf("%s: path %u:%u %o, fd %u:%u %o\n", what, a.st_uid, a.st_gid, a.st_mode & 07777, b.st_uid,
           b.st_gid, b.st_mode & 07777);
}

static int open_pty(char *name, size_t size, int *slave)
{
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    grantpt(m);
    unlockpt(m);
    ptsname_r(m, name, size);
    *slave = open(name, O_RDWR | O_NOCTTY);
    return m;
}

int main(void)
{
    char name[64];
    int s;
    open_pty(name, sizeof name, &s);
    show("new", name, s);
    printf("chown to 1000:5 %d, chmod 0600 %d\n", chown(name, 1000, 5), chmod(name, 0600));
    show("changed", name, s);
    printf("fchmod 0620 %d, fchown 0:5 %d\n", fchmod(s, 0620), fchown(s, 0, 5));
    show("by fd", name, s);
    chown(name, 1000, 5);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (setgid(1000) || setuid(1000))
            _exit(1);
        int own = chmod(name, 0600);
        int give = chown(name, 0, -1);
        int e = errno;
        printf("uid 1000: chmod its own %d, chown to root %d errno %d\n", own, give, e);
        char n2[64];
        int s2;
        open_pty(n2, sizeof n2, &s2);
        show("opened by uid 1000", n2, s2);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("after uid 1000", name, s);
    return 0;
}
C
gcc -O1 -o ptyown ptyown.c && ./ptyown

# /proc/meminfo 和 sysinfo：照搬宿主的（两边是同一台宿主），可用量算上页缓存
awk '/^MemTotal:/ { t = $2 } /^MemFree:/ { f = $2 } /^MemAvailable:/ { a = $2 } /^Cached:/ { c = $2 }
    END { print "meminfo: total", t, "available >= free", (a >= f), "cached", (c > 0) }' /proc/meminfo
cat > si.c <<'C'
#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>

static unsigned long long field(const char *name)
{
    char line[128];
    unsigned long long kb = 0;
    FILE *f = fopen("/proc/meminfo", "r");
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, name, strlen(name)) && line[strlen(name)] == ':')
            sscanf(line + strlen(name) + 1, "%llu", &kb);
    fclose(f);
    return kb * 1024;
}

int main(void)
{
    struct sysinfo si;
    sysinfo(&si);
    unsigned long long u = si.mem_unit;
    printf("sysinfo: totalram %d, totalswap %d, bufferram %d, sharedram %d\n", si.totalram * u == field("MemTotal"),
           si.totalswap * u == field("SwapTotal"), si.bufferram > 0, si.sharedram > 0);
    return 0;
}
C
gcc -O1 -o si si.c && ./si

# PTY 的 TCFLSH、TCXONC：主设备上 TCOFLUSH 不清从设备写出的数据、TCIFLUSH 清；TCOOFF 让
# 这一端的写阻塞到 TCOON（^Q 恢复不了），停住之前写出的照样能读；TCIOFF 发 STOP 字符
python3 - <<'PY'
import os, select, termios, threading
m, s = os.openpty()
attrs = termios.tcgetattr(s)
attrs[3] &= ~termios.ECHO
termios.tcsetattr(s, termios.TCSANOW, attrs)
def readable(fd):
    return bool(select.select([fd], [], [], 0.2)[0])
def writable(fd):
    return bool(select.select([], [fd], [], 0)[1])
def blocked_write(fd, data):
    done = threading.Event()
    threading.Thread(target=lambda: (os.write(fd, data), done.set())).start()
    return done
os.write(s, b"abcdef")
print("pty read", os.read(m, 2))
termios.tcflush(m, termios.TCOFLUSH)
print("pty after master TCOFLUSH", os.read(m, 2))
termios.tcflush(m, termios.TCIFLUSH)
print("pty after master TCIFLUSH readable", readable(m))
os.write(s, b"ABC")
print("pty read", os.read(m, 1024))
os.write(m, b"xy\n")
termios.tcflush(s, termios.TCIFLUSH)
print("pty after slave TCIFLUSH readable", readable(s))
termios.tcflow(s, termios.TCOOFF)
print("pty slave writable while stopped", writable(s))
done = blocked_write(s, b"def")
print("pty slave write blocked", not done.wait(0.3))
os.write(m, b"\x11")
print("pty still blocked after ^Q", not done.wait(0.3))
termios.tcflow(s, termios.TCOON)
print("pty resumed", done.wait(2), os.read(m, 1024))
os.write(s, b"before")
os.write(m, b"\x13")
print("pty written before ^S", os.read(m, 16), "slave writable", writable(s))
os.write(m, b"\x11")
termios.tcflow(s, termios.TCIOFF)
termios.tcflow(s, termios.TCION)
print("pty xchars", os.read(m, 16))
termios.tcflow(m, termios.TCOOFF)
done = blocked_write(m, b"z\n")
print("pty master write blocked", not done.wait(0.3))
termios.tcflow(m, termios.TCOON)
print("pty master resumed", done.wait(2), os.read(s, 16))
PY

# FIFO 的读端以 O_NONBLOCK 打开时还没有写端：有写端打开过之前 poll 不报 POLLHUP
python3 - <<'PY'
import os, select, tempfile
fifo = os.path.join(tempfile.mkdtemp(), "fifo")
os.mkfifo(fifo)
def events(fd):
    p = select.poll()
    p.register(fd, select.POLLIN)
    return [e for _, e in p.poll(0)]
r = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
print("fifo before writer", events(r), os.read(r, 1))
w = os.open(fifo, os.O_WRONLY)
print("fifo with writer", events(r))
os.write(w, b"x")
print("fifo data", events(r))
os.close(w)
print("fifo writer gone", events(r), os.read(r, 2), events(r))
r2 = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
print("fifo new reader", events(r2))
PY

# 已经删除的当前目录里不能新建（ENOENT）；socket、管道的 fd 可以 fchmod（libuv 的 uv_pipe_chmod）
python3 - <<'PY'
import errno, os, socket
def err(f):
    try:
        f()
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]
os.mkdir("/tmp/deadcwd")
os.chdir("/tmp/deadcwd")
os.rmdir("/tmp/deadcwd")
print("dead cwd mkdir", err(lambda: os.mkdir("x")))
print("dead cwd create", err(lambda: open("x", "w")))
print("dead cwd symlink", err(lambda: os.symlink("a", "x")))
os.chdir("/tmp")
s = socket.socket(socket.AF_UNIX)
print("fchmod socket", err(lambda: os.fchmod(s.fileno(), 0o777)))
print("fchmod pipe", err(lambda: os.fchmod(os.pipe()[0], 0o600)))
PY

# 终端的时间：utimensat 可以改，读到数据改访问时间、写出数据改修改时间（只在相差 8 秒以上
# 时改，这里从很久以前改到现在），chmod、utimensat 改状态改变时间
python3 - <<'PY'
import os, time
m, s = os.openpty()
path = os.ttyname(s)
os.utime(path, (1000, 2000))
st = os.stat(path)
print("pty utime", st.st_atime, st.st_mtime, st.st_ctime > 2000)
ctime = os.stat(path).st_ctime_ns
time.sleep(0.05)
os.chmod(path, 0o600)
print("pty chmod ctime", os.stat(path).st_ctime_ns > ctime)
os.write(s, b"out\n")
os.write(m, b"in\n")
os.read(s, 16)
st = os.stat(path)
now = time.time()
print("pty after io", now - 10 < st.st_atime <= now + 1, now - 10 < st.st_mtime <= now + 1)
print("pty fstat", os.fstat(s).st_mtime == st.st_mtime)
PY
