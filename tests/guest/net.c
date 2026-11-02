/*
 * socket 与 I/O 多路复用的 guest 测试程序（M4）：用 musl-gcc -static 编译，在
 * appine-run 里直接运行（不需要 rootfs，也不需要外网）。每项检查通过时打印一行
 * "ok <名字>"，失败时打印原因并以 1 退出，全部通过时最后打印 "all ok"。
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL %s:%d: ", __func__, __LINE__);                                            \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

static void ok(const char *name)
{
    printf("ok %s\n", name);
    fflush(stdout);
}

static volatile sig_atomic_t sigpipes, alarms;

static void on_sigpipe(int sig)
{
    (void)sig;
    sigpipes++;
}

static void on_alarm(int sig)
{
    (void)sig;
    alarms++;
}

static void handle(int sig, void (*fn)(int))
{
    struct sigaction sa = {0};
    sa.sa_handler = fn;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(sig, &sa, NULL) == 0, "sigaction %d", sig);
}

static int wait_child(pid_t pid)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

/* ---- AF_UNIX socketpair ---- */

static void test_stream_pair(void)
{
    int sv[2];
    char buf[16];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    CHECK(write(sv[0], "hello", 5) == 5, "write");
    CHECK(write(sv[0], "world", 5) == 5, "write");
    int avail = 0;
    CHECK(ioctl(sv[1], FIONREAD, &avail) == 0 && avail == 10, "FIONREAD %d", avail);
    CHECK(recv(sv[1], buf, 3, MSG_PEEK) == 3 && memcmp(buf, "hel", 3) == 0, "peek");
    /* 流式 socket 把两次写入合并读出 */
    CHECK(read(sv[1], buf, sizeof buf) == 10 && memcmp(buf, "helloworld", 10) == 0, "read");
    CHECK(shutdown(sv[0], SHUT_WR) == 0, "shutdown");
    CHECK(read(sv[1], buf, sizeof buf) == 0, "EOF after SHUT_WR");
    CHECK(write(sv[1], "back", 4) == 4, "other direction still open");
    CHECK(read(sv[0], buf, sizeof buf) == 4, "read back");

    /* 对方关闭之后：MSG_NOSIGNAL 只返回 EPIPE，否则还有 SIGPIPE */
    close(sv[1]);
    handle(SIGPIPE, on_sigpipe);
    CHECK(send(sv[0], "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE, "EPIPE");
    CHECK(sigpipes == 0, "no SIGPIPE with MSG_NOSIGNAL");
    CHECK(write(sv[0], "x", 1) == -1 && errno == EPIPE, "EPIPE");
    CHECK(sigpipes == 1, "SIGPIPE %d", (int)sigpipes);
    struct pollfd p = {sv[0], POLLIN | POLLOUT, 0};
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP), "POLLHUP 0x%x", p.revents);
    close(sv[0]);
    ok("unix stream socketpair");
}

static void test_message_pairs(void)
{
    int types[] = {SOCK_DGRAM, SOCK_SEQPACKET};
    for (int i = 0; i < 2; i++) {
        int sv[2];
        char buf[16];
        CHECK(socketpair(AF_UNIX, types[i] | SOCK_CLOEXEC, 0, sv) == 0, "socketpair %d", types[i]);
        CHECK(fcntl(sv[0], F_GETFD) == FD_CLOEXEC, "SOCK_CLOEXEC");
        CHECK(send(sv[0], "first", 5, 0) == 5 && send(sv[0], "second", 6, 0) == 6, "send");
        /* 消息边界保持不变，读不完的部分被丢弃 */
        struct iovec iov = {buf, 3};
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
        CHECK(recvmsg(sv[1], &msg, 0) == 3 && (msg.msg_flags & MSG_TRUNC), "truncated");
        CHECK(recv(sv[1], buf, sizeof buf, 0) == 6 && memcmp(buf, "second", 6) == 0, "boundary");
        int type = 0;
        socklen_t len = sizeof type;
        CHECK(getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &len) == 0 && type == types[i], "SO_TYPE");
        CHECK(recv(sv[1], buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN, "EAGAIN");
        close(sv[0]);
        close(sv[1]);
    }
    ok("unix datagram and seqpacket socketpairs");
}

/* 经由 SCM_RIGHTS 把管道的写端交给子进程，子进程收到后写入 */
static void test_scm_rights(void)
{
    int sv[2], pfd[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    CHECK(pipe(pfd) == 0, "pipe");
    pid_t pid = fork();
    if (pid == 0) {
        char data[4], control[CMSG_SPACE(sizeof(int))];
        struct iovec iov = {data, sizeof data};
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                             .msg_controllen = sizeof control};
        if (recvmsg(sv[1], &msg, MSG_CMSG_CLOEXEC) != 2)
            _exit(2);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        if (!c || c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS)
            _exit(3);
        int fd;
        memcpy(&fd, CMSG_DATA(c), sizeof fd);
        if (fcntl(fd, F_GETFD) != FD_CLOEXEC || write(fd, "passed", 6) != 6)
            _exit(4);
        _exit(0);
    }
    char control[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec iov = {"fd", 2};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                         .msg_controllen = sizeof control};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &pfd[1], sizeof(int));
    CHECK(sendmsg(sv[0], &msg, 0) == 2, "sendmsg");
    close(pfd[1]);
    CHECK(wait_child(pid) == 0, "child failed");
    char buf[8];
    CHECK(read(pfd[0], buf, sizeof buf) == 6 && memcmp(buf, "passed", 6) == 0, "data via passed fd");
    CHECK(read(pfd[0], buf, sizeof buf) == 0, "pipe closed everywhere");
    close(pfd[0]);
    close(sv[0]);
    close(sv[1]);
    ok("SCM_RIGHTS");
}

/* 抽象地址上的 listen/connect/accept，以及双方的 SO_PEERCRED */
static void test_unix_listen(void)
{
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    memcpy(addr.sun_path, "\0appine-test", 12);
    socklen_t alen = offsetof(struct sockaddr_un, sun_path) + 12;
    int srv = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(srv >= 0, "socket");
    CHECK(bind(srv, (struct sockaddr *)&addr, alen) == 0, "bind");
    int dup = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(dup, (struct sockaddr *)&addr, alen) == -1 && errno == EADDRINUSE, "EADDRINUSE");
    close(dup);
    CHECK(listen(srv, 4) == 0, "listen");
    CHECK(accept(srv, NULL, NULL) == -1 && errno == EAGAIN, "nonblocking accept");

    pid_t pid = fork();
    if (pid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&addr, alen) != 0)
            _exit(2);
        struct ucred cred;
        socklen_t len = sizeof cred;
        if (getsockopt(c, SOL_SOCKET, SO_PEERCRED, &cred, &len) != 0 || cred.pid != getppid())
            _exit(3);
        char buf[8];
        if (write(c, "ping", 4) != 4 || read(c, buf, sizeof buf) != 4 || memcmp(buf, "pong", 4))
            _exit(4);
        _exit(0);
    }
    struct pollfd p = {srv, POLLIN, 0};
    CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLIN), "listener readable");
    int conn = accept4(srv, NULL, NULL, SOCK_CLOEXEC);
    CHECK(conn >= 0, "accept");
    struct ucred cred;
    socklen_t len = sizeof cred;
    CHECK(getsockopt(conn, SOL_SOCKET, SO_PEERCRED, &cred, &len) == 0 && cred.pid == pid,
          "peer pid %d", cred.pid);
    struct sockaddr_un name;
    len = sizeof name;
    CHECK(getsockname(conn, (struct sockaddr *)&name, &len) == 0 && len == alen &&
              memcmp(name.sun_path, addr.sun_path, 12) == 0,
          "accepted socket has the listener's name");
    char buf[8];
    CHECK(read(conn, buf, sizeof buf) == 4 && memcmp(buf, "ping", 4) == 0, "ping");
    CHECK(write(conn, "pong", 4) == 4, "pong");
    CHECK(wait_child(pid) == 0, "child failed");
    close(conn);
    close(srv);
    /* 地址随 socket 关闭释放 */
    int again = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(again, (struct sockaddr *)&addr, alen) == 0, "rebind");
    close(again);
    ok("unix listen, connect and SO_PEERCRED");
}

/* ---- TCP、UDP 回环（宿主 socket） ---- */

static void test_tcp_loopback(void)
{
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof addr;
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(srv >= 0, "socket");
    int one = 1;
    CHECK(setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) == 0, "SO_REUSEADDR");
    CHECK(bind(srv, (struct sockaddr *)&addr, sizeof addr) == 0, "bind");
    CHECK(getsockname(srv, (struct sockaddr *)&addr, &len) == 0 && addr.sin_port != 0, "port");
    CHECK(listen(srv, 4) == 0, "listen");

    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int rc = connect(c, (struct sockaddr *)&addr, sizeof addr);
    CHECK(rc == 0 || errno == EINPROGRESS, "connect");
    struct pollfd p = {c, POLLOUT, 0};
    CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLOUT), "connected");
    int err = -1;
    len = sizeof err;
    CHECK(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0, "SO_ERROR %d", err);
    int conn = accept(srv, NULL, NULL);
    CHECK(conn >= 0, "accept");
    fcntl(c, F_SETFL, 0);

    /* 阻塞的 recv 等对方的数据；大块数据分多次到达 */
    static char big[256 * 1024], got[256 * 1024];
    memset(big, 'z', sizeof big);
    pid_t pid = fork();
    if (pid == 0) {
        _exit(send(conn, big, sizeof big, 0) == (ssize_t)sizeof big ? 0 : 1);
    }
    size_t total = 0;
    while (total < sizeof got) {
        ssize_t n = recv(c, got + total, sizeof got - total, 0);
        CHECK(n > 0, "recv %zd", n);
        total += n;
    }
    CHECK(memcmp(big, got, sizeof big) == 0, "data");
    CHECK(wait_child(pid) == 0, "sender failed");
    CHECK(shutdown(conn, SHUT_WR) == 0, "shutdown");
    CHECK(recv(c, got, 1, 0) == 0, "EOF");
    close(conn);
    close(c);
    close(srv);
    ok("tcp loopback");
}

static void test_udp_loopback(void)
{
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    struct sockaddr_in from;
    socklen_t len = sizeof a;
    int s1 = socket(AF_INET, SOCK_DGRAM, 0), s2 = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(bind(s1, (struct sockaddr *)&a, sizeof a) == 0, "bind");
    CHECK(getsockname(s1, (struct sockaddr *)&a, &len) == 0, "getsockname");
    CHECK(sendto(s2, "datagram", 8, 0, (struct sockaddr *)&a, sizeof a) == 8, "sendto");
    char buf[16];
    len = sizeof from;
    CHECK(recvfrom(s1, buf, sizeof buf, 0, (struct sockaddr *)&from, &len) == 8, "recvfrom");
    CHECK(from.sin_addr.s_addr == htonl(INADDR_LOOPBACK) && from.sin_port != 0, "source address");
    close(s1);
    close(s2);
    ok("udp loopback");
}

/*
 * DNS 存根（127.0.0.53:53）的回答在 appine 自己的队列里：FIONREAD 要报告下一个回答的长度。
 * glibc 的解析器用 sendmmsg 一次发出 A、AAAA 两个查询，按 FIONREAD 分配第二个回答的缓冲区，
 * 报告 0 时它收不下 AAAA 的回答，解析失败（Debian 的 apt）。没有外网时存根回答 SERVFAIL，
 * 也有回答可读，所以这里不看回答的内容，只看长度对得上。
 */
static void test_dns_stub_fionread(void)
{
    static const unsigned char query[] = {
        0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0,  /* 头：RD，一个问题 */
        9, 'l', 'o', 'c', 'a', 'l', 'h', 'o', 's', 't', 0, /* localhost */
        0, 1, 0, 1,                                         /* A，IN */
    };
    unsigned char q2[sizeof query];
    memcpy(q2, query, sizeof query);
    q2[0] = 0x56;
    q2[sizeof q2 - 3] = 28; /* AAAA */
    struct sockaddr_in ns = {.sin_family = AF_INET, .sin_port = htons(53)};
    inet_pton(AF_INET, "127.0.0.53", &ns.sin_addr);
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(connect(fd, (struct sockaddr *)&ns, sizeof ns) == 0, "connect to the stub");
    struct iovec iov[2] = {{(void *)query, sizeof query}, {q2, sizeof q2}};
    struct mmsghdr msgs[2] = {{.msg_hdr = {.msg_iov = &iov[0], .msg_iovlen = 1}},
                              {.msg_hdr = {.msg_iov = &iov[1], .msg_iovlen = 1}}};
    CHECK(sendmmsg(fd, msgs, 2, 0) == 2, "sendmmsg");
    for (int i = 0; i < 2; i++) {
        struct pollfd p = {fd, POLLIN, 0};
        CHECK(poll(&p, 1, 30000) == 1, "answer %d", i);
        int avail = -1;
        CHECK(ioctl(fd, FIONREAD, &avail) == 0, "FIONREAD");
        unsigned char buf[4096];
        ssize_t n = recv(fd, buf, sizeof buf, 0);
        CHECK(n >= 12 && avail == n, "answer %d: FIONREAD %d, recv %zd", i, avail, n);
        CHECK(buf[0] == (i ? 0x56 : 0x12) && buf[1] == 0x34, "answer %d id", i);
    }
    int avail = -1;
    CHECK(ioctl(fd, FIONREAD, &avail) == 0 && avail == 0, "FIONREAD when empty: %d", avail);
    close(fd);
    ok("dns stub fionread");
}

/* getifaddrs 经由 NETLINK_ROUTE 列出接口和地址 */
static void test_netlink(void)
{
    struct ifaddrs *list;
    CHECK(getifaddrs(&list) == 0, "getifaddrs");
    int found = 0;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (strcmp(i->ifa_name, "lo") == 0 && i->ifa_addr && i->ifa_addr->sa_family == AF_INET &&
            ((struct sockaddr_in *)i->ifa_addr)->sin_addr.s_addr == htonl(INADDR_LOOPBACK) &&
            (i->ifa_flags & IFF_LOOPBACK))
            found = 1;
    }
    freeifaddrs(list);
    CHECK(found, "lo with 127.0.0.1 not found");
    CHECK(if_nametoindex("lo") > 0, "if_nametoindex");
    ok("netlink interfaces");

    /* 没有实现的协议与没有编进内核的子系统一样建不了：libaudit 据此当作没有审计，
     * 不去等不会来的 ACK，ss 退回读 /proc/net。uevent 只是听，建得了；generic 有 nlctrl
     * 一个族（回答的内容由差分测试 compat 比对） */
    CHECK(socket(AF_NETLINK, SOCK_RAW, 9 /* NETLINK_AUDIT */) < 0 && errno == EPROTONOSUPPORT,
          "audit netlink socket");
    CHECK(socket(AF_NETLINK, SOCK_RAW, 4 /* NETLINK_SOCK_DIAG */) < 0 && errno == EPROTONOSUPPORT,
          "sock_diag netlink socket");
    int ue = socket(AF_NETLINK, SOCK_DGRAM, 15 /* NETLINK_KOBJECT_UEVENT */);
    CHECK(ue >= 0, "uevent netlink socket");
    close(ue);
    int ge = socket(AF_NETLINK, SOCK_RAW, 16 /* NETLINK_GENERIC */);
    CHECK(ge >= 0, "generic netlink socket");
    close(ge);
    ok("netlink protocols");
}

/* ---- epoll、eventfd、select ---- */

static void test_epoll(void)
{
    int ep = epoll_create1(EPOLL_CLOEXEC);
    int ev = eventfd(0, EFD_NONBLOCK);
    int sv[2];
    CHECK(ep >= 0 && ev >= 0 && socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "create");
    struct epoll_event e = {.events = EPOLLIN, .data.u32 = 1};
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e) == 0, "add eventfd");
    e.data.u32 = 2;
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &e) == 0, "add socket");
    struct epoll_event out[4];
    CHECK(epoll_wait(ep, out, 4, 0) == 0, "nothing ready");

    /* 另一个进程写 socket：epoll_wait 被唤醒 */
    pid_t pid = fork();
    if (pid == 0) {
        usleep(20000);
        _exit(write(sv[0], "x", 1) == 1 ? 0 : 1);
    }
    CHECK(epoll_wait(ep, out, 4, 5000) == 1 && out[0].data.u32 == 2, "socket readable");
    CHECK(wait_child(pid) == 0, "writer failed");
    uint64_t one = 1;
    CHECK(write(ev, &one, 8) == 8, "eventfd write");
    CHECK(epoll_wait(ep, out, 4, 0) == 2, "both ready");
    uint64_t v;
    CHECK(read(ev, &v, 8) == 8 && v == 1, "eventfd read");
    CHECK(read(ev, &v, 8) == -1 && errno == EAGAIN, "eventfd empty");

    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(sv[1], &rd);
    struct timeval tv = {0, 0};
    CHECK(select(sv[1] + 1, &rd, NULL, NULL, &tv) == 1 && FD_ISSET(sv[1], &rd), "select");
    close(ep);
    close(ev);
    close(sv[0]);
    close(sv[1]);
    ok("epoll, eventfd and select");
}

/* ---- 间隔定时器 ---- */

static void test_itimer(void)
{
    handle(SIGALRM, on_alarm);
    struct itimerval t = {{0, 20000}, {0, 20000}}, cur;
    CHECK(setitimer(ITIMER_REAL, &t, NULL) == 0, "setitimer");
    CHECK(getitimer(ITIMER_REAL, &cur) == 0 && cur.it_interval.tv_usec == 20000, "getitimer");
    sigset_t none;
    sigemptyset(&none);
    while (alarms < 3)
        sigsuspend(&none);
    struct itimerval off = {0};
    CHECK(setitimer(ITIMER_REAL, &off, &cur) == 0 && cur.it_interval.tv_usec == 20000, "disarm");
    CHECK(getitimer(ITIMER_REAL, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_usec == 0,
          "disarmed");
    /* alarm(2) 经由 setitimer 实现 */
    alarms = 0;
    CHECK(alarm(1) == 0, "alarm");
    CHECK(alarm(0) >= 1, "alarm remaining");
    ok("interval timers");
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    test_stream_pair();
    test_message_pairs();
    test_scm_rights();
    test_unix_listen();
    test_tcp_loopback();
    test_udp_loopback();
    test_dns_stub_fionread();
    test_netlink();
    test_epoll();
    test_itimer();
    printf("all ok\n");
    return 0;
}
