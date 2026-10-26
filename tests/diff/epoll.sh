# epoll 的边沿触发和 EPOLLEXCLUSIVE：已经就绪时又来了数据也要再触发一次（mio 的唤醒器
# 就靠这一点，tokio 程序原来会卡住），独占的关注一个边沿只叫醒一个进程
set -e
cd /tmp
rm -rf /tmp/etdir
cat > et.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <netinet/in.h>
#include <pthread.h>
#include <pty.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int ep;

static int wait1(int ms)
{
    struct epoll_event e;
    return epoll_wait(ep, &e, 1, ms);
}

/* 关注 fd 的 events（边沿触发），poke 两次、中间不读：每次 poke 之后报告几个事件 */
static void twice(const char *name, int fd, uint32_t events, void (*poke)(void *), void *arg)
{
    ep = epoll_create1(0);
    struct epoll_event e = {.events = events | EPOLLET};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e) < 0) {
        printf("%s: epoll_ctl: %s\n", name, strerror(errno));
        return;
    }
    poke(arg);
    int a = wait1(100);
    poke(arg);
    int b = wait1(100);
    int c = wait1(50);
    printf("%-14s first %d, again %d, quiet %d\n", name, a, b, c);
    close(ep);
}

static void write_byte(void *arg) { write(*(int *)arg, "x", 1); }
static void write_one(void *arg) { uint64_t one = 1; write(*(int *)arg, &one, 8); }
static void read_one(void *arg) { uint64_t v; read(*(int *)arg, &v, 8); }

static struct sockaddr_in tcp_addr;
static void connect_tcp(void *arg)
{
    (void)arg;
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&tcp_addr, sizeof tcp_addr);
}

static struct sockaddr_un unix_addr;
static void connect_unix(void *arg)
{
    (void)arg;
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(c, (void *)&unix_addr, sizeof unix_addr);
}

static void touch(void *arg)
{
    static int n;
    char path[64];
    snprintf(path, sizeof path, "%s/f%d", (char *)arg, n++);
    close(open(path, O_CREAT | O_WRONLY, 0600));
}

static void raise_rt(void *arg)
{
    (void)arg;
    union sigval v = {0};
    sigqueue(getpid(), SIGRTMIN, v);
}

static void mq_put(void *arg) { mq_send(*(mqd_t *)arg, "m", 1, 0); }

static void arm(void *arg)
{
    struct itimerspec it = {.it_value = {0, 1000000}};
    timerfd_settime(*(int *)arg, 0, &it, NULL);
    usleep(20000);
}

static void ready_kinds(void)
{
    int efd = eventfd(0, EFD_NONBLOCK);
    twice("eventfd", efd, EPOLLIN, write_one, &efd);
    /* 写的一方：每次读都让它再触发 */
    int wfd = eventfd(5, EFD_NONBLOCK);
    twice("eventfd write", wfd, EPOLLOUT, read_one, &wfd);

    int p[2];
    pipe(p);
    twice("pipe", p[0], EPOLLIN, write_byte, &p[1]);

    int s[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    twice("unix stream", s[0], EPOLLIN, write_byte, &s[1]);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, s);
    twice("unix dgram", s[0], EPOLLIN, write_byte, &s[1]);

    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    unix_addr.sun_family = AF_UNIX;
    strcpy(unix_addr.sun_path, "/tmp/et.sock");
    unlink(unix_addr.sun_path);
    bind(ul, (void *)&unix_addr, sizeof unix_addr);
    listen(ul, 8);
    twice("unix listen", ul, EPOLLIN, connect_unix, NULL);

    int l = socket(AF_INET, SOCK_STREAM, 0);
    tcp_addr.sin_family = AF_INET;
    tcp_addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t al = sizeof tcp_addr;
    bind(l, (void *)&tcp_addr, sizeof tcp_addr);
    listen(l, 8);
    getsockname(l, (void *)&tcp_addr, &al);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (void *)&tcp_addr, sizeof tcp_addr);
    int sv = accept(l, 0, 0);
    twice("tcp", sv, EPOLLIN, write_byte, &c);
    twice("tcp listen", l, EPOLLIN, connect_tcp, NULL);

    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ua = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    al = sizeof ua;
    bind(u, (void *)&ua, sizeof ua);
    getsockname(u, (void *)&ua, &al);
    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    connect(uc, (void *)&ua, sizeof ua);
    twice("udp", u, EPOLLIN, write_byte, &uc);

    int master, slave;
    struct termios raw;
    openpty(&master, &slave, NULL, NULL, NULL);
    tcgetattr(slave, &raw);
    cfmakeraw(&raw);
    tcsetattr(slave, TCSANOW, &raw);
    twice("pty slave", slave, EPOLLIN, write_byte, &master);
    twice("pty master", master, EPOLLIN, write_byte, &slave);

    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    twice("timerfd", tfd, EPOLLIN, arm, &tfd);

    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &set, NULL);
    int sfd = signalfd(-1, &set, SFD_NONBLOCK);
    twice("signalfd", sfd, EPOLLIN, raise_rt, NULL);

    mkdir("/tmp/etdir", 0700);
    int ifd = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(ifd, "/tmp/etdir", IN_CREATE);
    twice("inotify", ifd, EPOLLIN, touch, "/tmp/etdir");

    struct mq_attr ma = {.mq_maxmsg = 8, .mq_msgsize = 16};
    mq_unlink("/et");
    mqd_t q = mq_open("/et", O_RDWR | O_CREAT | O_NONBLOCK, 0600, &ma);
    twice("mqueue", q, EPOLLIN, mq_put, &q);
    mq_unlink("/et");
}

/* mio 的唤醒器：eventfd 以边沿触发关注，从来不读走计数，另一个线程每写一次都要把
 * epoll_wait 叫醒一次 */
static int waker, acks[2];

static void *wake_thread(void *arg)
{
    (void)arg;
    for (int i = 0; i < 5; i++) {
        uint64_t one = 1;
        write(waker, &one, 8);
        char c;
        read(acks[0], &c, 1);
    }
    return NULL;
}

static void mio_waker(void)
{
    waker = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    pipe(acks);
    ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN | EPOLLRDHUP | EPOLLET};
    epoll_ctl(ep, EPOLL_CTL_ADD, waker, &e);
    pthread_t t;
    pthread_create(&t, NULL, wake_thread, NULL);
    int woken = 0;
    while (woken < 5 && wait1(2000) == 1) {
        woken++;
        write(acks[1], "a", 1);
    }
    printf("mio waker: woken %d of 5\n", woken);
    if (woken < 5)
        exit(1);
    pthread_join(t, NULL);
}

/* 几个进程各用一个 epoll，以 EPOLLEXCLUSIVE（水平触发）等同一个监听 socket：来一个连接
 * 只有一个进程从 epoll_wait 返回；那个进程不 accept 也一样（没轮到的继续睡到超时） */
static void exclusive(const char *name, int l, void (*poke)(void *))
{
    int out[2];
    pipe(out);
    for (int i = 0; i < 3; i++) {
        if (fork() == 0) {
            int e = epoll_create1(0);
            struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE};
            epoll_ctl(e, EPOLL_CTL_ADD, l, &ev);
            write(out[1], "r", 1);
            struct epoll_event got;
            int n = epoll_wait(e, &got, 1, 1000);
            write(out[1], n == 1 ? "w" : "t", 1);
            _exit(0);
        }
    }
    char c;
    for (int i = 0; i < 3; i++)
        read(out[0], &c, 1);
    usleep(200000);
    poke(NULL);
    int woken = 0;
    for (int i = 0; i < 3; i++) {
        read(out[0], &c, 1);
        woken += c == 'w';
    }
    while (wait(NULL) > 0)
        ;
    printf("exclusive %-11s %d of 3 woken\n", name, woken);
    close(out[0]);
    close(out[1]);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    ready_kinds();
    mio_waker();

    int ul = socket(AF_UNIX, SOCK_STREAM, 0);
    strcpy(unix_addr.sun_path, "/tmp/ex.sock");
    unlink(unix_addr.sun_path);
    bind(ul, (void *)&unix_addr, sizeof unix_addr);
    listen(ul, 8);
    exclusive("unix listen", ul, connect_unix);

    int l = socket(AF_INET, SOCK_STREAM, 0);
    tcp_addr.sin_port = 0;
    socklen_t al = sizeof tcp_addr;
    bind(l, (void *)&tcp_addr, sizeof tcp_addr);
    listen(l, 8);
    getsockname(l, (void *)&tcp_addr, &al);
    exclusive("tcp listen", l, connect_tcp);
    return 0;
}
C
gcc -O1 -o et et.c
./et

# 真实的 tokio 程序：uv 离线安装一个包（缓存里没有，失败）。原来 mio 的第二次唤醒丢了，
# uv 一直卡住
rm -rf /tmp/uvenv
uv venv -q /tmp/uvenv
uv pip install -q --offline --python /tmp/uvenv/bin/python pip || echo "uv exit $?"
