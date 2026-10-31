# compat.sh 的续篇（difftest 把整个脚本作为 sh -c 的一个参数，不能超过 128 KB）：raw ICMP
# socket、PR_SET_CHILD_SUBREAPER、O_APPEND 文件上带偏移的写、readlink 的空路径、F_GETFL 里的
# O_LARGEFILE、/proc/<pid>/task/<tid>/children、PR_SET_PDEATHSIG 和 __WNOTHREAD
set -e
cd /tmp
# raw ICMP socket（appine 在宿主上用 ping socket 模拟）：IPv4 收到的数据带 IP 头，echo 回答
# 里是自己发出的 id，校验和对得上；ICMP_FILTER、ICMPV6_FILTER、IPV6_CHECKSUM、IP_RECVTTL 和
# IP_PKTINFO 的控制消息（放不下时截短）、MSG_TRUNC。回环上 Linux 的 raw socket 也收到发出
# 的 echo 请求，用例都先用过滤器只留回答。IP 头的 id、TOS 和标志不比
cat > rawicmp.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/ip_icmp.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#define ICMP_FILTER 1

static unsigned fold(unsigned long acc)
{
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    return acc;
}

static unsigned long add(unsigned long acc, const void *p, int n)
{
    const unsigned char *b = p;
    for (int i = 0; i + 1 < n; i += 2)
        acc += b[i] << 8 | b[i + 1];
    if (n & 1)
        acc += b[n - 1] << 8;
    return acc;
}

static unsigned short cksum(const void *p, int n)
{
    return htons(~fold(add(0, p, n)) & 0xffff);
}

static void echo(int s, int seq, int len)
{
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    unsigned char pkt[256] = {0};
    struct icmphdr *h = (void *)pkt;
    h->type = ICMP_ECHO;
    h->un.echo.id = htons(0x1234);
    h->un.echo.sequence = htons(seq);
    for (int i = 8; i < len; i++)
        pkt[i] = i;
    h->checksum = cksum(pkt, len);
    if (sendto(s, pkt, len, 0, (void *)&to, sizeof to) != len)
        printf("sendto: %s\n", strerror(errno));
}

static void show(const unsigned char *buf, int n)
{
    const struct iphdr *ip = (const void *)buf;
    struct in_addr a;
    printf("  %d bytes: version %d ihl %d tot_len %d ttl %d protocol %d ip checksum %s", n,
           ip->version, ip->ihl, ntohs(ip->tot_len), ip->ttl, ip->protocol,
           fold(add(0, ip, 20)) == 0xffff ? "ok" : "bad");
    a.s_addr = ip->saddr;
    printf(" src %s", inet_ntoa(a));
    a.s_addr = ip->daddr;
    printf(" dst %s\n", inet_ntoa(a));
    const struct icmphdr *h = (const void *)(buf + ip->ihl * 4);
    int len = n - ip->ihl * 4, same = 1;
    for (int i = 8; i < len; i++)
        same &= buf[ip->ihl * 4 + i] == (unsigned char)i;
    printf("  icmp type %d code %d id %#x seq %d checksum %s payload %s\n", h->type, h->code,
           ntohs(h->un.echo.id), ntohs(h->un.echo.sequence),
           fold(add(0, h, len)) == 0xffff ? "ok" : "bad", same ? "same" : "differs");
}

static void control(struct msghdr *m)
{
    printf("  msg_flags%s controllen %zu\n", m->msg_flags & MSG_CTRUNC ? " MSG_CTRUNC" : "",
           (size_t)m->msg_controllen);
    for (struct cmsghdr *c = CMSG_FIRSTHDR(m); c; c = CMSG_NXTHDR(m, c)) {
        printf("  cmsg level %d type %d len %zu", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len);
        if (c->cmsg_type == IP_TTL && c->cmsg_len == CMSG_LEN(sizeof(int)))
            printf(" ttl %d", *(int *)CMSG_DATA(c));
        if (c->cmsg_type == IP_PKTINFO && c->cmsg_len == CMSG_LEN(sizeof(struct in_pktinfo))) {
            struct in_pktinfo *p = (void *)CMSG_DATA(c);
            printf(" ifindex %d spec_dst %s", p->ipi_ifindex, inet_ntoa(p->ipi_spec_dst));
            printf(" addr %s", inet_ntoa(p->ipi_addr));
        }
        printf("\n");
    }
}

static void v4(void)
{
    int s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP), v;
    unsigned filt;
    socklen_t l = sizeof v;
    getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l);
    printf("SO_TYPE %d\n", v);
    l = sizeof filt;
    getsockopt(s, SOL_RAW, ICMP_FILTER, &filt, &l);
    printf("ICMP_FILTER %#x len %d\n", filt, (int)l);
    filt = ~(1u << ICMP_ECHOREPLY);
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    filt = 0;
    getsockopt(s, SOL_RAW, ICMP_FILTER, &filt, &l);
    printf("ICMP_FILTER %#x\n", filt);
    l = sizeof v;
    getsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, &l);
    printf("IP_RECVTTL %d\n", v);
    printf("SOL_RAW 99: %s\n", setsockopt(s, SOL_RAW, 99, &v, sizeof v) ? strerror(errno) : "ok");

    unsigned char buf[512];
    struct sockaddr_in from;
    l = sizeof from;
    echo(s, 1, 64);
    int n = recvfrom(s, buf, sizeof buf, 0, (void *)&from, &l);
    printf("recvfrom %d from %s\n", n, inet_ntoa(from.sin_addr));
    show(buf, n);

    /* 控制消息：PKTINFO 在 TTL 前面；放得下、只放得下第一条、第二条只放得下一部分 */
    v = 1;
    setsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, sizeof v);
    setsockopt(s, IPPROTO_IP, IP_PKTINFO, &v, sizeof v);
    size_t rooms[] = {256, CMSG_SPACE(sizeof(struct in_pktinfo)),
                      CMSG_SPACE(sizeof(struct in_pktinfo)) + 20, 8};
    for (int i = 0; i < 4; i++) {
        union {
            struct cmsghdr h;
            char b[256];
        } cbuf;
        memset(&cbuf, 0, sizeof cbuf);
        struct iovec iov = {buf, sizeof buf};
        struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = &cbuf,
                           .msg_controllen = rooms[i]};
        echo(s, 2 + i, 64);
        n = recvmsg(s, &m, 0);
        printf("recvmsg %d with %zu bytes of control\n", n, rooms[i]);
        control(&m);
    }
    v = 0;
    setsockopt(s, IPPROTO_IP, IP_RECVTTL, &v, sizeof v);
    setsockopt(s, IPPROTO_IP, IP_PKTINFO, &v, sizeof v);

    /* 截短：MSG_TRUNC 要完整的长度，收到的是 IP 头的开头 */
    echo(s, 6, 200);
    struct iovec iov = {buf, 10};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
    n = recvmsg(s, &m, MSG_TRUNC);
    printf("truncated: %d, MSG_TRUNC %d, first byte %#x\n", n, !!(m.msg_flags & MSG_TRUNC), buf[0]);

    /* 过滤器挡掉的回答不会收到，偷看的时候也一样 */
    filt = ~0u;
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    for (int peek = 0; peek < 2; peek++) {
        echo(s, 7 + peek, 64);
        usleep(200000);
        n = recv(s, buf, sizeof buf, MSG_DONTWAIT | (peek ? MSG_PEEK : 0));
        printf("all blocked%s: %d %s\n", peek ? " (peek)" : "", n, n < 0 ? strerror(errno) : "");
    }
    filt = ~(1u << ICMP_ECHOREPLY);
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
    printf("after unblocking: %d %s\n", n, n < 0 ? strerror(errno) : "");
    echo(s, 9, 64);
    n = recv(s, buf, sizeof buf, 0);
    printf("next reply:\n");
    show(buf, n);
    close(s);
}

static void v6(void)
{
    int s = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6), v;
    socklen_t l = sizeof v;
    getsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, &l);
    printf("IPV6_CHECKSUM %d\n", v);
    v = 2;
    printf("IPV6_CHECKSUM at IPPROTO_IPV6: %s\n",
           setsockopt(s, IPPROTO_IPV6, IPV6_CHECKSUM, &v, sizeof v) ? strerror(errno) : "ok");
    v = 3;
    printf("IPV6_CHECKSUM 3: %s\n",
           setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v) ? strerror(errno) : "ok");
    v = -5;
    setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v);
    getsockopt(s, IPPROTO_IPV6, IPV6_CHECKSUM, &v, &l);
    printf("IPV6_CHECKSUM after -5: %d\n", v);
    v = 2;
    setsockopt(s, SOL_RAW, IPV6_CHECKSUM, &v, sizeof v);

    struct icmp6_filter f;
    l = sizeof f;
    getsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, &l);
    printf("ICMP6_FILTER %#x %#x len %d\n", f.icmp6_filt[0], f.icmp6_filt[4], (int)l);
    ICMP6_FILTER_SETBLOCKALL(&f);
    ICMP6_FILTER_SETPASS(ICMP6_ECHO_REPLY, &f);
    setsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, sizeof f);
    memset(&f, 0, sizeof f);
    getsockopt(s, IPPROTO_ICMPV6, ICMP6_FILTER, &f, &l);
    printf("ICMP6_FILTER %#x %#x\n", f.icmp6_filt[0], f.icmp6_filt[4]);

    struct sockaddr_in6 to = {.sin6_family = AF_INET6, .sin6_addr = IN6ADDR_LOOPBACK_INIT};
    unsigned char pkt[64] = {0}, buf[256];
    struct icmp6_hdr *h = (void *)pkt;
    h->icmp6_type = ICMP6_ECHO_REQUEST;
    h->icmp6_id = htons(0x4321);
    h->icmp6_seq = htons(1);
    for (int i = 8; i < 64; i++)
        pkt[i] = i;
    if (sendto(s, pkt, 64, 0, (void *)&to, sizeof to) != 64)
        printf("sendto: %s\n", strerror(errno));
    struct sockaddr_in6 from;
    l = sizeof from;
    int n = recvfrom(s, buf, sizeof buf, 0, (void *)&from, &l);
    char a[64];
    printf("recvfrom %d from %s\n", n, inet_ntop(AF_INET6, &from.sin6_addr, a, sizeof a));
    /* 伪头部：源、目的地址，长度，下一个头 */
    unsigned char pseudo[40] = {0};
    memcpy(pseudo, &from.sin6_addr, 16);
    memcpy(pseudo + 16, &to.sin6_addr, 16);
    pseudo[35] = n;
    pseudo[39] = IPPROTO_ICMPV6;
    h = (void *)buf;
    printf("  icmp6 type %d id %#x seq %d checksum %s\n", h->icmp6_type, ntohs(h->icmp6_id),
           ntohs(h->icmp6_seq), fold(add(add(0, pseudo, 40), buf, n)) == 0xffff ? "ok" : "bad");
    close(s);
}

/* 没有 CAP_NET_RAW 建不了 raw socket；协议号不对的先报告 */
static void unprivileged(void)
{
    fflush(stdout);
    if (fork() == 0) {
        if (setgid(65534) || setuid(65534))
            perror("setuid");
        int cases[][3] = {{AF_INET, SOCK_RAW, IPPROTO_ICMP}, {AF_INET6, SOCK_RAW, IPPROTO_ICMPV6},
                          {AF_INET, SOCK_RAW, IPPROTO_UDP}, {AF_INET, SOCK_RAW, 0},
                          {AF_INET, SOCK_RAW, 300}, {AF_INET, SOCK_DGRAM, IPPROTO_ICMP}};
        for (int i = 0; i < 6; i++) {
            int s = socket(cases[i][0], cases[i][1], cases[i][2]);
            printf("nobody socket(%d, %d, %d): %s\n", cases[i][0], cases[i][1], cases[i][2],
                   s < 0 ? strerror(errno) : "ok");
            close(s);
        }
        fflush(stdout);
        _exit(0);
    }
    wait(NULL);
}

int main(void)
{
    v4();
    v6();
    unprivileged();
    return 0;
}
C
gcc -O1 -o rawicmp rawicmp.c
./rawicmp

# busybox 的 ping、ping6 以 root 运行时用 raw socket，普通用户建 raw socket 得到 EPERM 时
# 改用 ping socket
ping -c 2 -s 100 127.0.0.1 | sed 's/ time=.*//; /round-trip/d'
ping6 -c 1 ::1 | sed 's/ time=.*//; /round-trip/d'
su -s /bin/sh nobody -c 'ping -c 1 127.0.0.1' | sed 's/ time=.*//; /round-trip/d'

# PR_SET_CHILD_SUBREAPER：孤儿过继给祖先里最近的 subreaper，已经是僵尸的也由它回收；它退出
# 之后它的孤儿交给再往上的；fork 不继承，execve 保留；没有 subreaper 时给 1 号进程
cat > subreaper.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static int get(void)
{
    int v = -1;
    prctl(PR_GET_CHILD_SUBREAPER, &v);
    return v;
}

static void wait_byte(int fd)
{
    char c;
    if (read(fd, &c, 1) != 1)
        perror("read");
}

int main(int argc, char **argv)
{
    if (argc > 1) {
        printf("after execve: %d\n", get());
        return 0;
    }
    pid_t me = getpid(), r;
    int st;
    /* 行缓冲：fork 出来的进程不会再输出一遍父进程缓冲着的内容 */
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("default: %d\n", get());
    printf("set 5: %d, now %d\n", prctl(PR_SET_CHILD_SUBREAPER, 5), get());
    printf("set 0: %d, now %d\n", prctl(PR_SET_CHILD_SUBREAPER, 0), get());
    errno = 0;
    printf("get into a bad pointer: %d %s\n", prctl(PR_GET_CHILD_SUBREAPER, (int *)8),
           strerror(errno));
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    fflush(stdout);

    if (fork() == 0) {
        printf("child: %d\n", get());
        prctl(PR_SET_CHILD_SUBREAPER, 1);
        fflush(stdout);
        execl("/proc/self/exe", argv[0], "again", (char *)NULL);
        _exit(1);
    }
    wait(NULL);

    /* 孙子进程在父进程退出之后过继给这里，由这里回收 */
    int p[2];
    pipe(p);
    pid_t a = fork();
    if (a == 0) {
        if (fork() == 0) {
            wait_byte(p[0]);
            printf("orphan's parent is the subreaper: %s\n", getppid() == me ? "yes" : "no");
            fflush(stdout);
            _exit(3);
        }
        _exit(0);
    }
    waitpid(a, NULL, 0);
    write(p[1], "x", 1);
    r = waitpid(-1, &st, 0);
    printf("reaped the orphan: %s, status %d\n", r > 0 && r != a ? "yes" : "no", WEXITSTATUS(st));

    /* 父进程退出时已经是僵尸的孙子进程 */
    a = fork();
    if (a == 0) {
        pid_t g = fork();
        if (g == 0)
            _exit(5);
        siginfo_t si;
        waitid(P_PID, g, &si, WEXITED | WNOWAIT);
        _exit(0);
    }
    waitpid(a, NULL, 0);
    r = waitpid(-1, &st, 0);
    printf("reaped the zombie: %s, status %d\n", r > 0 ? "yes" : "no", WEXITSTATUS(st));

    /* 最近的 subreaper；它退出之后交给再往上的 */
    int q[2], back[2];
    pipe(q);
    pipe(back);
    pid_t b = fork();
    if (b == 0) {
        prctl(PR_SET_CHILD_SUBREAPER, 1);
        pid_t bpid = getpid();
        pid_t c = fork();
        if (c == 0) {
            if (fork() == 0) {
                wait_byte(q[0]);
                printf("adopted by the nearest subreaper: %s\n", getppid() == bpid ? "yes" : "no");
                fflush(stdout);
                write(back[1], "x", 1);
                wait_byte(q[0]);
                printf("then by the next one up: %s\n", getppid() == me ? "yes" : "no");
                fflush(stdout);
                _exit(0);
            }
            _exit(0);
        }
        waitpid(c, NULL, 0);
        write(q[1], "x", 1);
        wait_byte(back[0]);
        _exit(0);
    }
    waitpid(b, NULL, 0);
    write(q[1], "x", 1);
    r = waitpid(-1, &st, 0);
    printf("reaped it: %s\n", r > 0 && r != b ? "yes" : "no");

    /* 清掉之后孤儿给 1 号进程 */
    prctl(PR_SET_CHILD_SUBREAPER, 0);
    a = fork();
    if (a == 0) {
        if (fork() == 0) {
            wait_byte(p[0]);
            printf("without a subreaper: adopted by pid %d\n", getppid());
            fflush(stdout);
            write(back[1], "x", 1);
            _exit(0);
        }
        _exit(0);
    }
    waitpid(a, NULL, 0);
    write(p[1], "x", 1);
    wait_byte(back[0]);
    return 0;
}
C
gcc -O1 -o subreaper subreaper.c && ./subreaper

# 带偏移的写在 O_APPEND 的文件上照样写到末尾、文件位置不变（pwrite(2) 的 BUGS；musl 的 pwrite
# 改用 pwritev2 的 RWF_NOAPPEND，这里直接调用系统调用）；readlink 的路径为空时读 dirfd 本身，
# 它不是符号链接时是 ENOENT
cat > positional.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

static void show(const char *what, int fd, long n)
{
    struct stat st;
    fstat(fd, &st);
    printf("%s: %ld, size %ld, position %ld\n", what, n, (long)st.st_size,
           (long)lseek(fd, 0, SEEK_CUR));
}

static void readlink_empty(const char *what, int dirfd)
{
    char buf[64] = {0};
    errno = 0;
    long n = syscall(SYS_readlinkat, dirfd, "", buf, sizeof buf);
    printf("readlinkat(%s, \"\"): %ld %s\n", what, n, n < 0 ? strerror(errno) : buf);
}

int main(void)
{
    int fd = open("/tmp/append", O_CREAT | O_TRUNC | O_RDWR, 0644);
    write(fd, "0123456789", 10);
    close(fd);
    fd = open("/tmp/append", O_RDWR | O_APPEND);
    lseek(fd, 3, SEEK_SET);
    show("pwrite64 on O_APPEND", fd, syscall(SYS_pwrite64, fd, "ab", 2, 0));
    struct iovec v = {"cd", 2};
    show("pwritev on O_APPEND", fd, syscall(SYS_pwritev, fd, &v, 1, 0, 0));
    int plain = open("/tmp/append", O_RDWR);
    show("pwrite64 without O_APPEND", plain, syscall(SYS_pwrite64, plain, "XY", 2, 0));
    char all[32] = {0};
    pread(plain, all, sizeof all - 1, 0);
    printf("contents %s\n", all);

    unlink("/tmp/rl-link");
    symlink("target", "/tmp/rl-link");
    int link = open("/tmp/rl-link", O_PATH | O_NOFOLLOW);
    int dir = open("/tmp", O_PATH | O_DIRECTORY);
    int p[2];
    pipe(p);
    readlink_empty("AT_FDCWD", AT_FDCWD);
    readlink_empty("symlink", link);
    readlink_empty("directory", dir);
    readlink_empty("regular file", fd);
    readlink_empty("pipe", p[0]);
    readlink_empty("bad fd", 999);
    return 0;
}
C
gcc -O1 -o positional positional.c && ./positional

# raw ICMP socket 收得到到达的所有 ICMP（appine 里经由汇集点）：别的 UDP socket 发的报文引起
# 的端口不可达（引用原来的 IP 头、UDP 头和载荷），别的 raw socket 发的 echo 的回答；没连接的
# UDP socket 看不到错误，连接了的只看到与连接对得上的硬错误（ECONNREFUSED）；raw socket 打开
# IP_RECVERR 之后，网关报告的超时（TTL 1 发往外面的地址）进它的错误队列。引用的 IP 头的
# TTL、id 不比
cat > icmperr.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define ICMP_FILTER 1

/* linux/errqueue.h（仓库里没有内核头文件） */
struct sock_extended_err {
    unsigned ee_errno;
    unsigned char ee_origin, ee_type, ee_code, ee_pad;
    unsigned ee_info, ee_data;
};
#define SO_EE_OFFENDER(ee) ((struct sockaddr *)((ee) + 1))

static unsigned long add(unsigned long acc, const unsigned char *b, int n)
{
    for (int i = 0; i + 1 < n; i += 2)
        acc += b[i] << 8 | b[i + 1];
    if (n & 1)
        acc += b[n - 1] << 8;
    return acc;
}

static int sum_ok(const void *p, int n)
{
    unsigned long acc = add(0, p, n);
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    return acc == 0xffff;
}

static int raw(unsigned pass)
{
    int s = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    unsigned filt = ~pass;
    setsockopt(s, SOL_RAW, ICMP_FILTER, &filt, sizeof filt);
    return s;
}

static int udp(void)
{
    int u = socket(AF_INET, SOCK_DGRAM, 0), ttl = 64;
    setsockopt(u, IPPROTO_IP, IP_TTL, &ttl, sizeof ttl);
    return u;
}

static struct sockaddr_in lo(int port)
{
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port),
                            .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    return a;
}

static const char *err(long n)
{
    return n < 0 ? strerror(errno) : "ok";
}

/* 等 raw socket 收到一个报文，打印外层和引用的 UDP */
static void unreachable(int r, int port)
{
    unsigned char b[512];
    struct pollfd p = {r, POLLIN, 0};
    if (poll(&p, 1, 3000) != 1) {
        printf("  nothing\n");
        return;
    }
    int n = recv(r, b, sizeof b, 0);
    struct ip *o = (void *)b;
    struct icmp *ic = (void *)(b + o->ip_hl * 4);
    struct ip *in = &ic->icmp_ip;
    unsigned char *u = (unsigned char *)in + in->ip_hl * 4;
    printf("  %d bytes from %s:", n, inet_ntoa(o->ip_src));
    printf(" type %d code %d checksum %s; quoted proto %d src %s", ic->icmp_type, ic->icmp_code,
           sum_ok(ic, n - o->ip_hl * 4) ? "ok" : "bad", in->ip_p, inet_ntoa(in->ip_src));
    printf(" dst %s len %d header %s dport %s udp len %d payload %.4s\n", inet_ntoa(in->ip_dst),
           ntohs(in->ip_len), sum_ok(in, 20) ? "ok" : "bad",
           u[2] << 8 == (port & 0xff00) && u[3] == (port & 0xff) ? "matches" : "differs",
           u[4] << 8 | u[5], (char *)u + 8);
}

int main(void)
{
    int r = raw(1u << ICMP_DEST_UNREACH), closed = 33999;
    struct sockaddr_in to = lo(closed);
    char buf[64];

    printf("unconnected UDP to a closed port:\n");
    int u = udp();
    sendto(u, "abcd", 4, 0, (void *)&to, sizeof to);
    unreachable(r, closed);
    printf("  UDP sendto again: %s\n", err(sendto(u, "abcd", 4, 0, (void *)&to, sizeof to)));
    unreachable(r, closed);
    usleep(100000);
    printf("  recv: %s\n", err(recv(u, buf, sizeof buf, MSG_DONTWAIT)));
    int e = -1;
    socklen_t l = sizeof e;
    getsockopt(u, SOL_SOCKET, SO_ERROR, &e, &l);
    printf("  SO_ERROR %d, error queue: %s\n", e, err(recv(u, buf, sizeof buf, MSG_ERRQUEUE)));

    printf("connected UDP to a closed port:\n");
    int c = udp();
    connect(c, (void *)&to, sizeof to);
    send(c, "abcd", 4, 0);
    unreachable(r, closed);
    printf("  recv: %s\n", err(recv(c, buf, sizeof buf, MSG_DONTWAIT)));
    printf("  recv again: %s\n", err(recv(c, buf, sizeof buf, MSG_DONTWAIT)));

    printf("connected UDP, sendto another closed port:\n");
    struct sockaddr_in other = lo(closed + 1);
    int c2 = udp();
    connect(c2, (void *)&to, sizeof to);
    sendto(c2, "abcd", 4, 0, (void *)&other, sizeof other);
    unreachable(r, closed + 1);
    printf("  recv: %s\n", err(recv(c2, buf, sizeof buf, MSG_DONTWAIT)));
    struct sockaddr_in peer;
    l = sizeof peer;
    getpeername(c2, (void *)&peer, &l);
    printf("  peer port %d\n", ntohs(peer.sin_port));

    printf("echo from another raw socket:\n");
    int s = raw(0), r2 = raw(1u << ICMP_ECHOREPLY);
    unsigned char e1[16] = {ICMP_ECHO, 0, 0, 0, 0x12, 0x34, 0, 1};
    unsigned long acc = add(0, e1, sizeof e1);
    while (acc >> 16)
        acc = (acc & 0xffff) + (acc >> 16);
    e1[2] = ~acc >> 8;
    e1[3] = ~acc;
    struct sockaddr_in any = lo(0);
    sendto(s, e1, sizeof e1, 0, (void *)&any, sizeof any);
    struct pollfd p = {r2, POLLIN, 0};
    if (poll(&p, 1, 3000) == 1) {
        unsigned char b[128];
        int n = recv(r2, b, sizeof b, 0);
        struct icmp *ic = (void *)(b + 20);
        printf("  %d bytes: type %d id %#x seq %d\n", n, ic->icmp_type, ntohs(ic->icmp_id),
               ntohs(ic->icmp_seq));
    }

    printf("IP_RECVERR on a raw socket, TTL 1 to an outside address:\n");
    int q = raw(~0u), on = 1, one = 1;
    setsockopt(q, IPPROTO_IP, IP_RECVERR, &on, sizeof on);
    struct sockaddr_in out = {.sin_family = AF_INET, .sin_addr.s_addr = inet_addr("1.1.1.1")};
    connect(q, (void *)&out, sizeof out);
    setsockopt(q, IPPROTO_IP, IP_TTL, &one, sizeof one);
    unsigned char e2[16] = {ICMP_ECHO, 0, 0, 0, 0x43, 0x21, 0, 1};
    send(q, e2, sizeof e2, 0);
    p.fd = q;
    p.events = 0;
    printf("  poll %d revents%s\n", poll(&p, 1, 3000), p.revents & POLLERR ? " POLLERR" : "");
    unsigned char data[64], ctl[256];
    struct sockaddr_in from;
    struct iovec iov = {data, sizeof data};
    struct msghdr m = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov,
                       .msg_iovlen = 1, .msg_control = ctl, .msg_controllen = sizeof ctl};
    int n = recvmsg(q, &m, MSG_ERRQUEUE);
    printf("  error queue: %d, flags %s, original type %d id %#x seq %d, to %s\n", n,
           m.msg_flags & MSG_ERRQUEUE ? "MSG_ERRQUEUE" : "-", data[0], data[4] << 8 | data[5],
           data[6] << 8 | data[7], inet_ntoa(from.sin_addr));
    for (struct cmsghdr *h = CMSG_FIRSTHDR(&m); h; h = CMSG_NXTHDR(&m, h)) {
        if (h->cmsg_level != IPPROTO_IP || h->cmsg_type != IP_RECVERR)
            continue;
        struct sock_extended_err *ee = (void *)CMSG_DATA(h);
        struct sockaddr_in *off = (void *)SO_EE_OFFENDER(ee);
        printf("  errno %s origin %d type %d code %d offender %s\n", strerror(ee->ee_errno),
               ee->ee_origin, ee->ee_type, ee->ee_code, inet_ntoa(off->sin_addr));
    }
    l = sizeof e;
    getsockopt(q, SOL_SOCKET, SO_ERROR, &e, &l);
    printf("  then SO_ERROR %d, error queue: %s\n", e, err(recv(q, buf, sizeof buf, MSG_ERRQUEUE)));
    return 0;
}
C
gcc -O1 -o icmperr icmperr.c && ./icmperr

# busybox 的 traceroute：UDP 探测包（每个换一个端口）和 -I 的 echo，都用另一个 raw socket 收。
# 只到本机的地址：网关对 ICMP 限速（每秒一个），几个探测包接着发时原生上也时有时无
traceroute -n -q 2 -w 2 127.0.0.1 2>&1 | sed 's/ [0-9.]* ms//g'
traceroute -I -n -q 2 -w 2 127.0.0.1 2>&1 | sed 's/ [0-9.]* ms//g'
traceroute6 -n -q 2 -w 2 ::1 2>&1 | sed 's/ [0-9.]* ms//g'

# F_GETFL、fdinfo 的标志：open 打开的带 O_LARGEFILE（O_PATH 的不带），管道、socket、eventfd、
# epoll、memfd 这些按各自的系统调用
python3 - <<'PY'
import os, socket, select, fcntl
def show(what, fd):
    with open("/proc/self/fdinfo/%d" % fd) as f:
        info = [l.split()[1] for l in f if l.startswith("flags:")][0]
    print(what, oct(fcntl.fcntl(fd, fcntl.F_GETFL)), info)
show("open", os.open("/tmp", os.O_RDONLY))
show("open wronly", os.open("/tmp/largefile", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600))
show("o_path", os.open("/tmp", os.O_PATH))
r, w = os.pipe()
show("pipe r", r)
show("pipe w", w)
keep = [socket.socket(), socket.socketpair(), select.epoll()]
show("socket", keep[0].fileno())
show("unix pair", keep[1][0].fileno())
show("eventfd", os.eventfd(0, os.EFD_NONBLOCK))
show("epoll", keep[2].fileno())
show("memfd", os.memfd_create("x"))
show("reopened pipe", os.open("/proc/self/fd/%d" % r, os.O_RDONLY))
PY

# /proc/<pid>/task/<tid>/children：线程建的子进程（含僵尸进程），线程不算；建它们的线程退出
# 之后排到同一个进程里还活着的线程的最后；CLONE_PARENT 建的算调用者的父线程的；subreaper
# 收养的孤儿；非主线程 exec 之后都归它（它接过主线程的编号）。pid 换成名字比
cat > children.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t pids[32];
static const char *names[32];
static int n;
static int fds[2];
static pthread_barrier_t bar;
static pid_t thread_tid;

static void add(pid_t p, const char *name)
{
    pids[n] = p;
    names[n++] = name;
}

static const char *name_of(pid_t p)
{
    for (int i = 0; i < n; i++)
        if (pids[i] == p)
            return names[i];
    return "?";
}

static void show(const char *what, pid_t tid)
{
    char path[64], buf[1024];
    snprintf(path, sizeof path, "/proc/self/task/%d/children", tid);
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("%s: %s\n", what, strerror(errno));
        return;
    }
    size_t len = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[len] = 0;
    int tail = len == 0 || buf[len - 1] == ' ';
    printf("%s:", what);
    char *save;
    for (char *t = strtok_r(buf, " ", &save); t; t = strtok_r(NULL, " ", &save))
        printf(" %s", name_of(atoi(t)));
    printf("%s\n", tail ? "" : " (no trailing space)");
}

static pid_t spawn(const char *name)
{
    pid_t p = fork();
    if (p == 0) {
        pause();
        _exit(0);
    }
    add(p, name);
    return p;
}

/* pthread_join 返回时线程还可能没退完（Linux 在把子进程交出去之前就叫醒 join 的线程），等到
 * /proc/self/task 里没有它 */
static void wait_gone(pid_t tid)
{
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d", tid);
    while (access(path, F_OK) == 0)
        usleep(1000);
}

static void *worker(void *arg)
{
    thread_tid = gettid();
    spawn("t1");
    pthread_barrier_wait(&bar);
    pthread_barrier_wait(&bar);
    return NULL;
}

static void *execer(void *arg)
{
    static char pairs[32][32];
    char *argv[40];
    int a = 0;
    spawn("f");
    argv[a++] = "children";
    argv[a++] = "after-exec";
    for (int i = 0; i < n; i++) {
        snprintf(pairs[i], sizeof pairs[i], "%s=%d", names[i], pids[i]);
        argv[a++] = pairs[i];
    }
    argv[a] = NULL;
    execv("/proc/self/exe", argv);
    perror("execv");
    _exit(1);
}

static void wait_zombie(pid_t p)
{
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", p);
    for (;;) {
        FILE *f = fopen(path, "r");
        size_t len = f ? fread(buf, 1, sizeof buf - 1, f) : 0;
        if (f)
            fclose(f);
        buf[len] = 0;
        char *s = strrchr(buf, ')');
        if (s && s[1] && s[2] == 'Z')
            return;
        usleep(1000);
    }
}

int main(int argc, char **argv)
{
    /* 线程 exec 时缓冲区里还没写出去的会丢掉 */
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && strcmp(argv[1], "after-exec") == 0) {
        for (int i = 2; i < argc; i++) {
            char *eq = strchr(argv[i], '=');
            *eq = 0;
            add(atoi(eq + 1), argv[i]);
        }
        printf("after exec, tid is the pid: %d\n", gettid() == getpid());
        show("after exec", getpid());
        for (int i = 0; i < n; i++)
            kill(pids[i], SIGKILL);
        while (wait(NULL) > 0)
            ;
        return 0;
    }
    pid_t me = getpid();
    spawn("a");
    pid_t b = spawn("b");
    pthread_t t;
    pthread_barrier_init(&bar, NULL, 2);
    pthread_create(&t, NULL, worker, NULL);
    pthread_barrier_wait(&bar);
    show("main", me);
    show("thread", thread_tid);
    printf("/proc/self/children: %s\n", access("/proc/self/children", F_OK) ? strerror(errno) : "exists");
    pthread_barrier_wait(&bar);
    pthread_join(t, NULL);
    wait_gone(thread_tid);
    show("after the thread exited", me);
    kill(b, SIGKILL);
    wait_zombie(b);
    show("b is a zombie", me);
    waitpid(b, NULL, 0);
    show("b reaped", me);

    pipe(fds);
    pid_t c = fork();
    if (c == 0) {
        long gc = syscall(SYS_clone, CLONE_PARENT, 0, 0, 0, 0);
        if (gc == 0) {
            pause();
            _exit(0);
        }
        write(fds[1], &gc, sizeof gc);
        pause();
        _exit(0);
    }
    add(c, "c");
    long gc;
    read(fds[0], &gc, sizeof gc);
    add(gc, "gc");
    show("CLONE_PARENT", me);

    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t d = fork();
    if (d == 0) {
        long e = fork();
        if (e == 0) {
            pause();
            _exit(0);
        }
        write(fds[1], &e, sizeof e);
        _exit(0);
    }
    add(d, "d");
    long e;
    read(fds[0], &e, sizeof e);
    add(e, "e");
    waitpid(d, NULL, 0);
    show("orphan taken by the subreaper", me);

    pthread_create(&t, NULL, execer, NULL);
    pthread_join(t, NULL);
    return 1;
}
C
gcc -O1 -pthread -o children children.c && ./children
echo "children in task/<tid>: $(ls /proc/$$/task/$$ | grep -c '^children$'), in /proc/<pid>: $(ls /proc/$$ | grep -c '^children$')"

# PR_SET_PDEATHSIG 按线程：建立子进程的线程一退出就发（进程还在也一样），别的线程退出不发；
# 设置跟着设它的线程，新线程、fork 出来的子进程都是 0，execve 保留，setuid 之后清掉；
# siginfo 是 SI_USER，发送者是父进程。wait 的 __WNOTHREAD 只等调用的线程建的子进程
cat > pdeath.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif

static int fds[2];
static pid_t parent;
static char *self;

/* 子进程等 PR_SET_PDEATHSIG 的信号：在 ms 毫秒里收到的都写进管道 */
static void await_signals(int ms)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    char buf[256] = "", one[96];
    siginfo_t si;
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (sigtimedwait(&set, &si, &ts) > 0) {
        snprintf(one, sizeof one, " %s code %d from parent %d uid %d",
                 si.si_signo == SIGUSR1 ? "USR1" : "USR2", si.si_code, si.si_pid == parent,
                 (int)si.si_uid);
        strcat(buf, one);
    }
    strcat(buf, buf[0] ? "\n" : " none\n");
    write(fds[1], buf, strlen(buf));
}

static void block(void)
{
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
}

static int stay;
static volatile int usr2_set;
static void *set_usr2(void *arg)
{
    prctl(PR_SET_PDEATHSIG, SIGUSR2);
    usr2_set = 1;
    if (stay)
        for (;;)
            pause();
    return NULL;
}

/* how：0 只在主线程设 SIGUSR1；1 另一个线程再设 SIGUSR2、一直活着；2 设了之后 exec；3 另一个
 * 线程设了 SIGUSR2 之后退出（跟着它没了） */
static pid_t spawn(int how, int ms)
{
    pid_t p = fork();
    if (p == 0) {
        block();
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        if (how == 1 || how == 3) {
            pthread_t t;
            stay = how == 1;
            pthread_create(&t, NULL, set_usr2, NULL);
            if (stay)
                while (!usr2_set)
                    usleep(1000);
            else
                pthread_join(t, NULL);
        }
        if (how == 2) {
            char a[16], b[16], c[16];
            snprintf(a, sizeof a, "%d", fds[1]);
            snprintf(b, sizeof b, "%d", parent);
            snprintf(c, sizeof c, "%d", ms);
            execl(self, self, "pdeath-exec", a, b, c, (char *)NULL);
            _exit(1);
        }
        char ok = 'r';
        write(fds[1], &ok, 1);
        await_signals(ms);
        _exit(0);
    }
    char c;
    read(fds[0], &c, 1);
    return p;
}

static void report(const char *what, pid_t p)
{
    char buf[300];
    waitpid(p, NULL, 0);
    ssize_t n = read(fds[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("%s:%s", what, buf);
}

static pid_t from_thread;
static int how_in_thread;
static void *thread_spawn(void *arg)
{
    from_thread = spawn(how_in_thread, 1000);
    return NULL;
}

static void *nothing(void *arg) { return NULL; }

static void *get_pdeath(void *arg)
{
    int s = -1;
    prctl(PR_GET_PDEATHSIG, &s);
    printf("a new thread sees %d\n", s);
    return NULL;
}

static const char *err(long r) { return r < 0 ? strerror(errno) : "ok"; }

static pid_t a_pid, b_pid, c_pid, waiter_tid;
static void *waiter_thread(void *arg)
{
    int st;
    waiter_tid = gettid();
    b_pid = fork();
    if (b_pid == 0)
        _exit(7);
    pid_t r = waitpid(-1, &st, __WNOTHREAD);
    printf("thread waits any: %s, status %d\n", r == b_pid ? "its own child" : "other", WEXITSTATUS(st));
    r = waitpid(-1, &st, WNOHANG | __WNOTHREAD);
    printf("thread waits any again: %s\n", err(r));
    r = waitpid(a_pid, &st, WNOHANG | __WNOTHREAD);
    printf("thread waits the main thread's child: %s\n", err(r));
    r = waitpid(a_pid, &st, WNOHANG);
    printf("without __WNOTHREAD: %ld\n", (long)r);
    siginfo_t si;
    printf("waitid: %s\n", err(waitid(P_ALL, 0, &si, WEXITED | WNOHANG | __WNOTHREAD)));
    c_pid = fork();
    if (c_pid == 0) {
        pause();
        _exit(0);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    self = argv[0];
    if (argc > 1 && strcmp(argv[1], "pdeath-exec") == 0) {
        fds[1] = atoi(argv[2]);
        parent = atoi(argv[3]);
        int s = -1;
        prctl(PR_GET_PDEATHSIG, &s);
        char buf[64];
        snprintf(buf, sizeof buf, "after exec %d,", s);
        write(fds[1], "r", 1);
        write(fds[1], buf, strlen(buf));
        await_signals(atoi(argv[4]));
        _exit(0);
    }
    pipe(fds);
    parent = getpid();
    pthread_t t;

    how_in_thread = 0;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("creating thread exited", from_thread);

    pid_t p = spawn(0, 300);
    pthread_create(&t, NULL, nothing, NULL);
    pthread_join(t, NULL);
    report("another thread exited", p);

    how_in_thread = 1;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set in two threads", from_thread);

    how_in_thread = 3;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set in a thread that exited", from_thread);

    how_in_thread = 2;
    pthread_create(&t, NULL, thread_spawn, NULL);
    pthread_join(t, NULL);
    report("set before exec", from_thread);

    prctl(PR_SET_PDEATHSIG, SIGUSR2);
    int s = -1;
    prctl(PR_GET_PDEATHSIG, &s);
    printf("own %d\n", s);
    pthread_create(&t, NULL, get_pdeath, NULL);
    pthread_join(t, NULL);
    p = fork();
    if (p == 0) {
        prctl(PR_GET_PDEATHSIG, &s);
        printf("a forked child sees %d\n", s);
        prctl(PR_SET_PDEATHSIG, SIGUSR1);
        setuid(1000);
        prctl(PR_GET_PDEATHSIG, &s);
        printf("after setuid %d\n", s);
        _exit(0);
    }
    waitpid(p, NULL, 0);
    prctl(PR_SET_PDEATHSIG, 0);


    a_pid = fork();
    if (a_pid == 0) {
        pause();
        _exit(0);
    }
    pthread_create(&t, NULL, waiter_thread, NULL);
    pthread_join(t, NULL);
    /* pthread_join 返回时线程还可能没退完、它的子进程还没交给主线程：等到 /proc/self/task 里没有它 */
    char task[64];
    snprintf(task, sizeof task, "/proc/self/task/%d", waiter_tid);
    while (access(task, F_OK) == 0)
        usleep(1000);
    kill(c_pid, SIGKILL);
    printf("main waits the exited thread's child: %s\n", waitpid(c_pid, NULL, __WNOTHREAD) == c_pid ? "ok" : strerror(errno));
    kill(a_pid, SIGKILL);
    printf("main waits any: %s\n", waitpid(-1, NULL, __WNOTHREAD) == a_pid ? "its own child" : strerror(errno));

    /* 放在最后：孙进程成了孤儿，原生一侧这个程序是容器的 1 号进程时会收养它 */
    pid_t mid = fork();
    if (mid == 0) {
        parent = getpid();
        spawn(0, 1000);
        _exit(0);
    }
    waitpid(mid, NULL, 0);
    char buf[300];
    ssize_t n = read(fds[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("parent process exited:%s", buf);
    return 0;
}
C
gcc -O1 -pthread -o pdeath pdeath.c && ./pdeath
