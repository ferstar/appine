# 等待和唤醒：别的进程在别的管道、socket、eventfd、PTY 上来回读写，起进程（含 vfork、
# posix_spawn）、结束进程，改别的 epoll、timerfd、inotify、signalfd，设定时器，建立、关闭
# TCP 连接，查 netlink，加锁解锁、设租约、改管道大小时，空闲着等自己的对象的进程不被叫醒
# （自愿上下文切换的次数不变）；每一种对象上真正的事件都叫醒另一个进程里等着的线程，
# vfork 的父进程、改短了的定时器、等锁和租约的进程照常
set -e
rm -rf /tmp/wakeups.d /tmp/wakeups.idle /tmp/wakeups.lock /dev/shm/wakeups.lease
mkdir -p /tmp/wakeups.d /tmp/wakeups.idle
cd /tmp
cat > wakeups.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/prctl.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

/* /proc/<pid>/status 的 voluntary_ctxt_switches：进程每睡下一次加一 */
static long switches(pid_t pid)
{
    char path[64], line[128];
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    FILE *f = fopen(path, "r");
    long n = -1;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "voluntary_ctxt_switches: %ld", &n) == 1)
            break;
    if (f)
        fclose(f);
    return n;
}

/* 打开一个 PTY，从设备设成 raw（不回显） */
static void open_pty(int *master, int *slave)
{
    *master = posix_openpt(O_RDWR | O_NOCTTY);
    grantpt(*master);
    unlockpt(*master);
    *slave = open(ptsname(*master), O_RDWR | O_NOCTTY);
    struct termios t;
    tcgetattr(*slave, &t);
    cfmakeraw(&t);
    tcsetattr(*slave, TCSANOW, &t);
}

static void arm(int tfd, long ns)
{
    struct itimerspec its = {.it_value = {ns / 1000000000, ns % 1000000000}};
    timerfd_settime(tfd, 0, &its, NULL);
}

/* 回环地址上监听一个系统分配的端口 */
static int tcp_listener(struct sockaddr_in *a)
{
    *a = (struct sockaddr_in){.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof *a;
    int l = socket(AF_INET, SOCK_STREAM, 0);
    bind(l, (void *)a, sizeof *a);
    listen(l, 16);
    getsockname(l, (void *)a, &len);
    return l;
}

/* 一个 TCP 连接的两端 */
static void tcp_pair(int *s, int *c)
{
    struct sockaddr_in a;
    int l = tcp_listener(&a);
    *s = socket(AF_INET, SOCK_STREAM, 0);
    connect(*s, (void *)&a, sizeof a);
    *c = accept(l, NULL, NULL);
    close(l);
}

/* netlink 查询所有网络接口（getifaddrs 的第一步）。镜像里没有 linux-headers：
 * linux/netlink.h、linux/rtnetlink.h 的定义 */
#define NETLINK_ROUTE 0
#define RTM_GETLINK 18
#define NLM_F_REQUEST 1
#define NLM_F_DUMP 0x300
static void ask_links(int nl)
{
    struct {
        unsigned int len;
        unsigned short type, flags;
        unsigned int seq, pid;
        unsigned char family, pad[3];
    } req = {.len = sizeof req, .type = RTM_GETLINK, .flags = NLM_F_REQUEST | NLM_F_DUMP, .seq = 1};
    send(nl, &req, sizeof req, 0);
}

/* ---- 空闲的等待者 ---- */

static const char *idle_kinds[] = {
    "epoll eventfd", "poll pipe",    "read unix stream", "poll unix dgram", "read pty",
    "read eventfd",  "epoll timerfd", "read inotify",    "read tcp",        "epoll tcp",
};
#define NIDLE (int)(sizeof idle_kinds / sizeof idle_kinds[0])

/* 子进程：建好自己的对象，告诉父进程，然后一直等它 */
static void idle(int kind, int ready)
{
    char buf[512];
    int a[2], e, m, s;
    struct epoll_event ev = {.events = EPOLLIN};
    switch (kind) {
    case 0:
        e = epoll_create1(0);
        a[0] = eventfd(0, 0);
        epoll_ctl(e, EPOLL_CTL_ADD, a[0], &ev);
        write(ready, "r", 1);
        epoll_wait(e, &ev, 1, -1);
        break;
    case 1: {
        pipe(a);
        struct pollfd p = {a[0], POLLIN, 0};
        write(ready, "r", 1);
        poll(&p, 1, -1);
        break;
    }
    case 2:
        socketpair(AF_UNIX, SOCK_STREAM, 0, a);
        write(ready, "r", 1);
        read(a[0], buf, sizeof buf);
        break;
    case 3: {
        socketpair(AF_UNIX, SOCK_DGRAM, 0, a);
        struct pollfd p = {a[0], POLLIN, 0};
        write(ready, "r", 1);
        poll(&p, 1, -1);
        break;
    }
    case 4:
        open_pty(&m, &s);
        write(ready, "r", 1);
        read(s, buf, sizeof buf);
        break;
    case 5:
        a[0] = eventfd(0, 0);
        write(ready, "r", 1);
        read(a[0], buf, 8);
        break;
    case 6:
        e = epoll_create1(0);
        a[0] = timerfd_create(CLOCK_MONOTONIC, 0);
        epoll_ctl(e, EPOLL_CTL_ADD, a[0], &ev);
        write(ready, "r", 1);
        epoll_wait(e, &ev, 1, -1);
        break;
    case 7:
        a[0] = inotify_init1(0);
        inotify_add_watch(a[0], "/tmp/wakeups.idle", IN_CREATE);
        write(ready, "r", 1);
        read(a[0], buf, sizeof buf);
        break;
    case 8:
        tcp_pair(&a[0], &a[1]);
        write(ready, "r", 1);
        read(a[0], buf, sizeof buf);
        break;
    case 9:
        tcp_pair(&a[0], &a[1]);
        e = epoll_create1(0);
        epoll_ctl(e, EPOLL_CTL_ADD, a[0], &ev);
        write(ready, "r", 1);
        epoll_wait(e, &ev, 1, -1);
        break;
    }
    _exit(0);
}

/* 两个进程来回传 `size` 字节：本进程写 pw 读 pr，子进程读 cr 写 cw */
static void pingpong(int pw, int pr, int cr, int cw, size_t size, int rounds)
{
    long v = 1;
    pid_t p = fork();
    if (p == 0) {
        for (int i = 0; i < rounds; i++) {
            read(cr, &v, size);
            write(cw, &v, size);
        }
        _exit(0);
    }
    for (int i = 0; i < rounds; i++) {
        write(pw, &v, size);
        read(pr, &v, size);
    }
    waitpid(p, NULL, 0);
}

/* 别的对象上的事件：都与空闲的进程无关 */
static void traffic(void)
{
    int a[2], b[2];
    pipe(a);
    pipe(b);
    pingpong(a[1], b[0], a[0], b[1], 1, 1000);
    close(a[0]), close(a[1]), close(b[0]), close(b[1]);

    socketpair(AF_UNIX, SOCK_STREAM, 0, a);
    pingpong(a[0], a[0], a[1], a[1], 1, 1000);
    close(a[0]), close(a[1]);

    int x = eventfd(0, 0), y = eventfd(0, 0);
    pingpong(x, y, x, y, 8, 1000);
    close(x), close(y);

    int m, s;
    char c = 'x';
    open_pty(&m, &s);
    for (int i = 0; i < 200; i++) {
        write(m, &c, 1);
        read(s, &c, 1);
        write(s, &c, 1);
        read(m, &c, 1);
    }
    close(m), close(s);

    for (int i = 0; i < 30; i++) {
        pid_t p = fork();
        if (p == 0)
            _exit(0);
        waitpid(p, NULL, 0);
    }

    int e = epoll_create1(0), ev = eventfd(1, 0);
    struct epoll_event w = {.events = EPOLLIN};
    for (int i = 0; i < 200; i++) {
        epoll_ctl(e, EPOLL_CTL_ADD, ev, &w);
        epoll_ctl(e, EPOLL_CTL_DEL, ev, NULL);
    }
    close(e), close(ev);

    int t = timerfd_create(CLOCK_MONOTONIC, 0);
    for (int i = 0; i < 200; i++)
        arm(t, 3600000000000L);
    close(t);

    int in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(in, "/tmp/wakeups.d", IN_CREATE | IN_DELETE);
    for (int i = 0; i < 50; i++) {
        char name[64];
        snprintf(name, sizeof name, "/tmp/wakeups.d/t%d", i);
        close(open(name, O_CREAT | O_WRONLY, 0644));
        unlink(name);
    }
    close(in);

    /* posix_spawn（musl、glibc 都用 CLONE_VFORK）和 vfork 之后直接退出 */
    char *args[] = {"/bin/true", NULL};
    for (int i = 0; i < 30; i++) {
        pid_t p;
        posix_spawn(&p, "/bin/true", NULL, NULL, args, environ);
        waitpid(p, NULL, 0);
        if ((p = vfork()) == 0)
            _exit(0);
        waitpid(p, NULL, 0);
    }

    /* 设定时器（都远在之后，最后取消） */
    struct itimerval far = {.it_value = {3600, 0}}, off = {{0, 0}, {0, 0}};
    struct sigevent sev = {.sigev_notify = SIGEV_NONE};
    timer_t timer;
    timer_create(CLOCK_MONOTONIC, &sev, &timer);
    struct itimerspec later = {.it_value = {3600, 0}};
    for (int i = 0; i < 200; i++) {
        setitimer(ITIMER_REAL, &far, NULL);
        timer_settime(timer, 0, &later, NULL);
    }
    setitimer(ITIMER_REAL, &off, NULL);
    timer_delete(timer);

    /* 改自己的 signalfd 的掩码 */
    sigset_t set;
    sigemptyset(&set);
    int sfd = signalfd(-1, &set, 0);
    for (int i = 0; i < 50; i++) {
        sigaddset(&set, SIGUSR2);
        signalfd(sfd, &set, 0);
        sigemptyset(&set);
        signalfd(sfd, &set, 0);
    }
    close(sfd);

    /* 建立、关闭 TCP 连接（宿主 socket），查 netlink */
    struct sockaddr_in sa;
    int l = tcp_listener(&sa);
    for (int i = 0; i < 50; i++) {
        int c = socket(AF_INET, SOCK_STREAM, 0);
        connect(c, (void *)&sa, sizeof sa);
        close(accept(l, NULL, NULL));
        close(c);
    }
    close(l);
    for (int i = 0; i < 20; i++) {
        struct ifaddrs *ifa;
        if (getifaddrs(&ifa) == 0)
            freeifaddrs(ifa);
    }

    /* flock、记录锁、OFD 锁的加锁和解锁，持着记录锁的进程退出 */
    int f = open("/tmp/wakeups.lock", O_RDWR | O_CREAT, 0644);
    struct flock fl = {.l_whence = SEEK_SET};
    for (int i = 0; i < 100; i++) {
        flock(f, LOCK_EX);
        flock(f, LOCK_SH);
        flock(f, LOCK_UN);
        fl.l_type = F_WRLCK;
        fcntl(f, F_SETLK, &fl);
        fl.l_type = F_UNLCK;
        fcntl(f, F_SETLK, &fl);
        fl.l_type = F_WRLCK;
        fcntl(f, F_OFD_SETLK, &fl);
        fl.l_type = F_UNLCK;
        fcntl(f, F_OFD_SETLK, &fl);
    }
    for (int i = 0; i < 20; i++) {
        pid_t p = fork();
        if (p == 0) {
            fl.l_type = F_WRLCK;
            fcntl(f, F_SETLK, &fl);
            _exit(0);
        }
        waitpid(p, NULL, 0);
    }
    close(f);

    /* 设、放弃租约，关闭设过租约的文件 */
    for (int i = 0; i < 20; i++) {
        int lf = open("/dev/shm/wakeups.lease", O_RDONLY | O_CREAT, 0644);
        fcntl(lf, F_SETLEASE, F_RDLCK);
        fcntl(lf, F_SETLEASE, F_UNLCK);
        fcntl(lf, F_SETLEASE, F_RDLCK);
        close(lf);
    }

    /* 改管道的大小 */
    pipe(a);
    for (int i = 0; i < 50; i++)
        fcntl(a[1], F_SETPIPE_SZ, i % 2 ? 65536 : 131072);
    close(a[0]), close(a[1]);
}

static void idle_waiters(void)
{
    int ready[2];
    pipe(ready);
    pid_t pids[NIDLE];
    for (int i = 0; i < NIDLE; i++)
        if ((pids[i] = fork()) == 0) {
            prctl(PR_SET_PDEATHSIG, SIGKILL); /* 卡住被结束时不留下它们 */
            idle(i, ready[1]);
        }
    char c;
    for (int i = 0; i < NIDLE; i++)
        read(ready[0], &c, 1);
    usleep(300000);
    long before[NIDLE];
    for (int i = 0; i < NIDLE; i++)
        before[i] = switches(pids[i]);
    traffic();
    usleep(100000);
    for (int i = 0; i < NIDLE; i++)
        printf("idle %-18s woken %s\n", idle_kinds[i], switches(pids[i]) != before[i] ? "yes" : "no");
    for (int i = 0; i < NIDLE; i++)
        kill(pids[i], SIGKILL);
    while (wait(NULL) > 0)
        ;
    close(ready[0]), close(ready[1]);
}

/* ---- 真正的事件叫醒等着的进程 ---- */

static int fds[2], lsock, ep, efd, tfd, ifd, master, slave, pidfd;
static pid_t sleeper;
static struct sockaddr_un addr = {.sun_family = AF_UNIX};
static char big[4096];

static void on_alarm(int sig) { (void)sig; }

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* 子进程执行 waiter（返回非 0 表示等到了；最多 3 秒，到时 SIGALRM 打断阻塞的调用），
 * 100 毫秒之后本进程执行 poke。2 秒之内等到才算被叫醒：没被叫醒的线程被 SIGALRM 叫醒
 * 之后也会看到条件早已成立 */
static void check(const char *name, int (*waiter)(void), void (*poke)(void))
{
    double start = now();
    pid_t p = fork();
    if (p == 0) {
        struct sigaction sa = {.sa_handler = on_alarm};
        sigaction(SIGALRM, &sa, NULL);
        alarm(3);
        _exit(waiter() ? 0 : 1);
    }
    usleep(100000);
    poke();
    int st;
    waitpid(p, &st, 0);
    int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0 && now() - start < 2;
    printf("%-27s %s\n", name, ok ? "woken" : "timed out");
}

static int pollin(int fd)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, 3000) == 1;
}

static int poll0(void) { return pollin(fds[0]); }
static int read0(void) { return read(fds[0], big, sizeof big) >= 0; }
static void write1(void) { write(fds[1], "x", 1); }

static int send_full(void)
{
    fcntl(fds[1], F_SETFL, 0);
    return send(fds[1], "x", 1, 0) == 1;
}
static void drain0(void)
{
    while (recv(fds[0], big, sizeof big, MSG_DONTWAIT) > 0)
        ;
}

static int connect_new(void)
{
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    return connect(s, (void *)&addr, sizeof addr) == 0;
}
static void accept_one(void) { close(accept(lsock, NULL, NULL)); }
static int accept_one_w(void) { return accept(lsock, NULL, NULL) >= 0; }
static void connect_one(void) { connect_new(); }

static int peer_closed(void)
{
    close(fds[1]);
    return pollin(fds[0]);
}
static void close1(void) { close(fds[1]); }
static void shut1(void) { shutdown(fds[1], SHUT_WR); }

static int read_slave(void) { return read(slave, big, 1) == 1; }
static void write_master(void) { write(master, "x", 1); }
static int poll_master(void) { return pollin(master); }
static void write_slave(void) { write(slave, "y", 1); }

static int read_efd(void) { return read(efd, big, 8) == 8; }
static void write_efd(void)
{
    long one = 1;
    write(efd, &one, 8);
}
static int epoll_any(void)
{
    struct epoll_event ev;
    return epoll_wait(ep, &ev, 1, 3000) == 1;
}

static int poll_ifd(void) { return pollin(ifd); }
static int read_ifd(void) { return read(ifd, big, sizeof big) > 0; }
static void create_file(void)
{
    static int n;
    char name[64];
    snprintf(name, sizeof name, "/tmp/wakeups.d/c%d", n++);
    close(open(name, O_CREAT | O_WRONLY, 0644));
}

static int read_tfd(void) { return read(tfd, big, 8) == 8; }
static void arm_tfd(void) { arm(tfd, 50000000); }

static int poll_ep(void) { return pollin(ep); }
static void add_ready(void)
{
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, eventfd(1, 0), &ev);
}

static int poll_pidfd(void) { return pollin(pidfd); }
static void kill_sleeper(void)
{
    kill(sleeper, SIGKILL);
    waitpid(sleeper, NULL, 0);
}

static void closefds(void) { close(fds[0]), close(fds[1]); }
static void nothing(void) {}

/* vfork：子进程 exec 或者过一会儿退出之后父进程继续。卡住时 SIGALRM 结束这个进程（vfork
 * 的等待只有致命信号能打断） */
static int vfork_exec(void)
{
    signal(SIGALRM, SIG_DFL);
    pid_t p = vfork();
    if (p == 0) {
        execl("/bin/true", "true", (char *)NULL);
        _exit(1);
    }
    int st;
    return waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 0;
}
static int vfork_exit_later(void)
{
    signal(SIGALRM, SIG_DFL);
    pid_t p = vfork();
    if (p == 0) {
        usleep(100000);
        _exit(7);
    }
    int st;
    return waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 7;
}

/* 定时器先设 10 秒、再改成 100 毫秒：定时器线程要按新的时刻醒来 */
static volatile int fired;
static void on_fire(int sig) { (void)sig, fired = 1; }
static int itimer_shortened(void)
{
    struct sigaction sa = {.sa_handler = on_fire};
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {.it_value = {10, 0}};
    setitimer(ITIMER_REAL, &it, NULL);
    it.it_value = (struct timeval){0, 100000};
    setitimer(ITIMER_REAL, &it, NULL);
    while (!fired)
        pause();
    return 1;
}
static int posix_timer_shortened(void)
{
    struct sigaction sa = {.sa_handler = on_fire};
    sigaction(SIGUSR1, &sa, NULL);
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    timer_t t;
    timer_create(CLOCK_MONOTONIC, &sev, &t);
    struct itimerspec it = {.it_value = {10, 0}};
    timer_settime(t, 0, &it, NULL);
    it.it_value = (struct timespec){0, 100000000};
    timer_settime(t, 0, &it, NULL);
    while (!fired)
        pause();
    return 1;
}

/* 一个线程 poll 掩码是空的 signalfd（进程挂着 SIGUSR2），另一个线程改掩码 */
static int sfd;
static void *poll_sfd(void *arg)
{
    (void)arg;
    return (void *)(long)pollin(sfd);
}
static int signalfd_mask(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    kill(getpid(), SIGUSR2); /* 发给进程：两个线程都看得到 */
    sigset_t none;
    sigemptyset(&none);
    sfd = signalfd(-1, &none, 0);
    pthread_t t;
    pthread_create(&t, NULL, poll_sfd, NULL);
    usleep(100000);
    signalfd(sfd, &set, 0);
    void *r;
    pthread_join(t, &r);
    return r != NULL;
}

/* ---- 锁和租约：放开的一方叫醒等着的进程（等的进程先关掉继承来的 fd，它与本进程共用
 * 打开文件，否则 flock、OFD 锁、租约不会随本进程关闭而放开） ---- */
static int lockfd, leasefd;
static pid_t holder;
static struct flock wrlck = {.l_type = F_WRLCK, .l_whence = SEEK_SET};

static int open_lock(void)
{
    close(lockfd);
    return open("/tmp/wakeups.lock", O_RDWR);
}
static int flock_ex(void) { return flock(open_lock(), LOCK_EX) == 0; }
static int flock_sh(void) { return flock(open_lock(), LOCK_SH) == 0; }
static int setlkw(void) { return fcntl(open_lock(), F_SETLKW, &wrlck) == 0; }
static int ofd_setlkw(void) { return fcntl(open_lock(), F_OFD_SETLKW, &wrlck) == 0; }
static void unflock(void) { flock(lockfd, LOCK_UN); }
static void downgrade(void) { flock(lockfd, LOCK_SH); }
static void unlock_posix(void)
{
    struct flock fl = {.l_type = F_UNLCK, .l_whence = SEEK_SET};
    fcntl(lockfd, F_SETLK, &fl);
}
static void close_lock(void) { close(lockfd); }
static void kill_holder(void)
{
    kill(holder, SIGKILL);
    waitpid(holder, NULL, 0);
}

static int open_leased(void)
{
    close(leasefd);
    return open("/dev/shm/wakeups.lease", O_WRONLY) >= 0;
}
static void unlease(void) { fcntl(leasefd, F_SETLEASE, F_UNLCK); }

/* ---- 宿主 socket ---- */
static int tcp_s, tcp_c, tcp_ep, tcp_ev, nl;

/* 一个线程 epoll 等着 tcp_s 时另一个线程把它关掉：对端读到 EOF（epoll 不持有文件，关闭
 * 之后连接就断开了）。等着的线程要被叫醒，放开它在宿主上等着的 socket */
static void *epoll_tcp(void *arg)
{
    (void)arg;
    struct epoll_event ev;
    epoll_wait(tcp_ep, &ev, 1, 5000);
    return NULL;
}
static int eof_c(void)
{
    close(tcp_s);
    return read(tcp_c, big, sizeof big) == 0;
}
static void close_s(void) { close(tcp_s); }
static int read_c(void) { return read(tcp_c, big, sizeof big) == 1; }
static void write_s(void) { write(tcp_s, "x", 1); }
static int accept_tcp(void) { return accept(lsock, NULL, NULL) >= 0; }
static struct sockaddr_in tcp_addr;
static void connect_tcp(void)
{
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&tcp_addr, sizeof tcp_addr);
}

static int poll_nl(void) { return pollin(nl); }
static int read_nl(void) { return recv(nl, big, sizeof big, 0) > 0; }
static void ask_nl(void) { ask_links(nl); }

/* 写满的管道：改大之后写得进去 */
static int write_full(void)
{
    fcntl(fds[1], F_SETFL, 0);
    return write(fds[1], big, sizeof big) == sizeof big;
}
static void grow_pipe(void) { fcntl(fds[1], F_SETPIPE_SZ, 1 << 20); }

static void wake_cases(void)
{
    socketpair(AF_UNIX, SOCK_STREAM, 0, fds);
    check("poll unix stream", poll0, write1);
    drain0();
    check("read unix stream", read0, write1);
    fcntl(fds[1], F_SETFL, O_NONBLOCK);
    while (send(fds[1], big, sizeof big, MSG_DONTWAIT) > 0)
        ;
    check("send unix stream (full)", send_full, drain0);
    drain0();
    check("read unix stream (shut)", read0, shut1);
    closefds();
    socketpair(AF_UNIX, SOCK_STREAM, 0, fds);
    check("poll unix stream (close)", peer_closed, close1);
    close(fds[0]);

    socketpair(AF_UNIX, SOCK_DGRAM, 0, fds);
    check("poll unix dgram", poll0, write1);
    drain0();
    check("read unix dgram", read0, write1);
    closefds();

    strcpy(addr.sun_path, "/tmp/wakeups.sock");
    unlink(addr.sun_path);
    lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(lsock, (void *)&addr, sizeof addr);
    listen(lsock, 0);
    check("accept unix", accept_one_w, connect_one);
    connect_new(); /* backlog 0：再来一个连接就要等 accept */
    check("connect unix (backlog)", connect_new, accept_one);
    close(lsock);

    open_pty(&master, &slave);
    check("read pty slave", read_slave, write_master);
    check("poll pty master", poll_master, write_slave);
    close(master), close(slave);

    efd = eventfd(0, 0);
    check("read eventfd", read_efd, write_efd);
    ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev);
    check("epoll eventfd", epoll_any, write_efd);
    close(ep), close(efd);

    ifd = inotify_init1(0);
    inotify_add_watch(ifd, "/tmp/wakeups.d", IN_CREATE);
    check("poll inotify", poll_ifd, create_file);
    if (pollin(ifd))
        read_ifd();
    check("read inotify", read_ifd, create_file);
    close(ifd);

    tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    check("read timerfd (armed later)", read_tfd, arm_tfd);
    ep = epoll_create1(0);
    epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev);
    check("epoll timerfd (armed later)", epoll_any, arm_tfd);
    close(ep), close(tfd);

    ep = epoll_create1(0);
    check("poll epoll (epoll_ctl)", poll_ep, add_ready);
    close(ep);

    pipe(fds);
    check("poll pipe", poll0, write1);
    closefds();

    if ((sleeper = fork()) == 0) {
        pause();
        _exit(0);
    }
    pidfd = syscall(SYS_pidfd_open, sleeper, 0);
    check("poll pidfd", poll_pidfd, kill_sleeper);
    close(pidfd);

    check("vfork (exec)", vfork_exec, nothing);
    check("vfork (exit later)", vfork_exit_later, nothing);
    check("setitimer (shortened)", itimer_shortened, nothing);
    check("timer_settime (shortened)", posix_timer_shortened, nothing);
    check("poll signalfd (mask)", signalfd_mask, nothing);

    lockfd = open("/tmp/wakeups.lock", O_RDWR | O_CREAT, 0644);
    flock(lockfd, LOCK_EX);
    check("flock (unlock)", flock_ex, unflock);
    flock(lockfd, LOCK_EX);
    check("flock (downgrade)", flock_sh, downgrade);
    flock(lockfd, LOCK_EX);
    check("flock (close)", flock_ex, close_lock);
    lockfd = open("/tmp/wakeups.lock", O_RDWR);
    fcntl(lockfd, F_SETLK, &wrlck);
    check("fcntl lock (unlock)", setlkw, unlock_posix);
    int ready[2];
    pipe(ready);
    if ((holder = fork()) == 0) {
        fcntl(open("/tmp/wakeups.lock", O_RDWR), F_SETLK, &wrlck);
        write(ready[1], "r", 1);
        pause();
        _exit(0);
    }
    char c;
    read(ready[0], &c, 1);
    close(ready[0]), close(ready[1]);
    check("fcntl lock (holder exits)", setlkw, kill_holder);
    fcntl(lockfd, F_OFD_SETLK, &wrlck);
    check("ofd lock (close)", ofd_setlkw, close_lock);

    signal(SIGIO, SIG_IGN); /* 租约被打破时发给持有者 */
    leasefd = open("/dev/shm/wakeups.lease", O_RDONLY | O_CREAT, 0644);
    fcntl(leasefd, F_SETLEASE, F_RDLCK);
    check("open (lease released)", open_leased, unlease);
    close(leasefd);

    tcp_pair(&tcp_s, &tcp_c);
    check("read tcp", read_c, write_s);
    tcp_ep = epoll_create1(0);
    tcp_ev = eventfd(0, 0);
    epoll_ctl(tcp_ep, EPOLL_CTL_ADD, tcp_s, &ev);
    epoll_ctl(tcp_ep, EPOLL_CTL_ADD, tcp_ev, &ev);
    pthread_t t;
    pthread_create(&t, NULL, epoll_tcp, NULL);
    usleep(100000);
    check("epoll tcp (closed: peer eof)", eof_c, close_s);
    long one = 1;
    write(tcp_ev, &one, 8);
    pthread_join(t, NULL);
    close(tcp_c), close(tcp_ep), close(tcp_ev);
    lsock = tcp_listener(&tcp_addr);
    check("accept tcp", accept_tcp, connect_tcp);
    close(lsock);

    nl = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("poll netlink (answer)", poll_nl, ask_nl);
    close(nl);
    nl = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("read netlink (answer)", read_nl, ask_nl);
    close(nl);

    pipe(fds);
    fcntl(fds[1], F_SETFL, O_NONBLOCK);
    while (write(fds[1], big, sizeof big) > 0)
        ;
    check("write pipe (F_SETPIPE_SZ)", write_full, grow_pipe);
    closefds();
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(120); /* 漏了唤醒、卡住时结束，不让差分测试一直等 */
    idle_waiters();
    wake_cases();
    return 0;
}
C
gcc -O1 -o wakeups wakeups.c -lpthread
./wakeups
