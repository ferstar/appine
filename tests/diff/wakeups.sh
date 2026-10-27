# 等待和唤醒：别的进程在别的管道、socket、eventfd、PTY 上来回读写，起进程、结束进程，
# 改别的 epoll、timerfd、inotify 时，空闲着等自己的对象的进程不被叫醒（自愿上下文切换
# 的次数不变）；每一种对象上真正的事件都叫醒另一个进程里等着的线程
set -e
rm -rf /tmp/wakeups.d /tmp/wakeups.idle
mkdir -p /tmp/wakeups.d /tmp/wakeups.idle
cd /tmp
cat > wakeups.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/syscall.h>
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

/* ---- 空闲的等待者 ---- */

static const char *idle_kinds[] = {
    "epoll eventfd", "poll pipe",    "read unix stream", "poll unix dgram",
    "read pty",      "read eventfd", "epoll timerfd",    "read inotify",
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
}

static void idle_waiters(void)
{
    int ready[2];
    pipe(ready);
    pid_t pids[NIDLE];
    for (int i = 0; i < NIDLE; i++)
        if ((pids[i] = fork()) == 0)
            idle(i, ready[1]);
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
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    alarm(300); /* 漏了唤醒、卡住时结束，不让差分测试一直等 */
    idle_waiters();
    wake_cases();
    return 0;
}
C
gcc -O1 -o wakeups wakeups.c
./wakeups
