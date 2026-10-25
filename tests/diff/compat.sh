# 不常用但程序会依赖的接口：实时信号排队、pidfd、execveat 等
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

# /proc/net/{tcp,tcp6,udp,udp6,unix}：按 fstat 的 inode 找到自己的 socket，只看与环境
# 无关的列（端口是临时分配的，只比较是否与 getsockname 一致）
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
    mount("/", root);
    mount("/proc", proc);
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
