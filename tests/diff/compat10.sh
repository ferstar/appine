# docker-run-options: --privileged
# The eleventh map-bug triage's socket items: MSG_TRUNC on a TCP receive discards the data without
# touching the buffer (a NULL one included) and returns its length, with MSG_PEEK and MSG_WAITALL
# too; a blocking TCP send or write that meets a shutdown after part of its data returns that count
# without SIGPIPE, and the next call gets EPIPE and SIGPIPE; SIOCADDRT and SIOCDELRT for IPv4 and
# IPv6
set -e
cd /tmp
cat > trunc.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t pipes;

static void on_pipe(int sig)
{
    (void)sig;
    pipes++;
}

/* A connected TCP pair over loopback: *c the client, *s the accepted end */
static void pair(int *c, int *s)
{
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    if (bind(l, (struct sockaddr *)&a, sizeof a) || getsockname(l, (struct sockaddr *)&a, &al) ||
        listen(l, 1)) {
        perror("listener");
        exit(1);
    }
    *c = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(*c, (struct sockaddr *)&a, sizeof a)) {
        perror("connect");
        exit(1);
    }
    *s = accept(l, NULL, NULL);
    close(l);
}

static void untouched(const char *what, ssize_t n, const char *buf)
{
    int same = 1;
    for (int i = 0; i < 16; i++)
        same &= buf[i] == 'x';
    if (n < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %zd, buffer untouched %d\n", what, n, same);
}

static int victim;

static void *shut_later(void *a)
{
    struct timespec t = {0, 300 * 1000000};
    nanosleep(&t, NULL);
    shutdown(victim, SHUT_WR);
    return a;
}

/* A blocking send (or write) of `len` bytes to a peer that reads nothing, shut down for writing
 * by another thread once it blocks: the count is partial and no SIGPIPE comes; the next send is
 * EPIPE with SIGPIPE */
static void broken(const char *what, int use_write)
{
    int c, s;
    pair(&c, &s);
    size_t len = 64 << 20;
    char *big = calloc(1, len);
    victim = c;
    pthread_t t;
    pthread_create(&t, NULL, shut_later, NULL);
    pipes = 0;
    ssize_t n = use_write ? write(c, big, len) : send(c, big, len, 0);
    pthread_join(t, NULL);
    printf("%s: partial %d (%s), SIGPIPE %d\n", what, n > 0 && (size_t)n < len,
           n < 0 ? strerror(errno) : "a count", (int)pipes);
    n = use_write ? write(c, "x", 1) : send(c, "x", 1, 0);
    printf("%s again: %s, SIGPIPE %d\n", what, n < 0 ? strerror(errno) : "sent", (int)pipes);
    free(big);
    close(c);
    close(s);
}

int main(void)
{
    signal(SIGPIPE, on_pipe);
    int c, s;
    pair(&c, &s);
    char buf[16], all[16];
    if (send(c, "hello world", 11, 0) != 11)
        return 1;
    /* Wait until all of it is there */
    recv(s, all, 11, MSG_PEEK | MSG_WAITALL);
    memset(buf, 'x', sizeof buf);
    untouched("trunc peek 4", recv(s, buf, 4, MSG_PEEK | MSG_TRUNC), buf);
    untouched("trunc 5", recv(s, buf, 5, MSG_TRUNC), buf);
    untouched("trunc NULL 3", recv(s, NULL, 3, MSG_TRUNC), buf);
    struct iovec iov = {buf, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
    untouched("recvmsg trunc 1", recvmsg(s, &m, MSG_TRUNC), buf);
    ssize_t n = recv(s, buf, sizeof buf, 0);
    printf("rest: %.*s\n", (int)n, buf);
    char block[3000];
    memset(block, 'y', sizeof block);
    send(c, block, sizeof block, 0);
    memset(buf, 'x', sizeof buf);
    untouched("trunc waitall 3000", recv(s, buf, 3000, MSG_TRUNC | MSG_WAITALL), buf);
    close(c);
    close(s);
    broken("send", 0);
    broken("write", 1);
    return 0;
}
C
gcc -O1 -pthread -o trunc trunc.c
./trunc
rm -f trunc trunc.c

# SIOCADDRT and SIOCDELRT (busybox route, and the cases route itself never sends) in a new network
# namespace with a dummy device: added and deleted routes, metrics, an address label's source,
# and the errors of rtentry_to_fib_config and fib_table_delete
cat > rt.c <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <net/route.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static void in4(struct sockaddr *sa, const char *a)
{
    struct sockaddr_in *s = (struct sockaddr_in *)sa;
    memset(s, 0, sizeof *s);
    s->sin_family = AF_INET;
    inet_pton(AF_INET, a, &s->sin_addr);
}

static void try(int fd, const char *what, unsigned long req, struct rtentry *rt)
{
    int r = ioctl(fd, req, rt);
    printf("%s: %s\n", what, r ? strerror(errno) : "ok");
}

/* A route to dst/mask, through gw when given, on dev when given */
static struct rtentry route(const char *dst, const char *mask, const char *gw, char *dev, int flags)
{
    struct rtentry rt;
    memset(&rt, 0, sizeof rt);
    in4(&rt.rt_dst, dst);
    in4(&rt.rt_genmask, mask);
    if (gw)
        in4(&rt.rt_gateway, gw);
    rt.rt_dev = dev;
    rt.rt_flags = RTF_UP | flags;
    return rt;
}

int main(void)
{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    char d0[] = "d0", label[] = "d0:1", nolabel[] = "d0:9", nosuch[] = "nosuch";
    struct rtentry rt = route("10.5.0.0", "255.255.0.0", NULL, label, 0);
    try(fd, "label source", SIOCADDRT, &rt);
    rt = route("10.5.0.0", "255.255.0.0", NULL, label, 0);
    try(fd, "same again", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, nolabel, 0);
    try(fd, "unknown label", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, nosuch, 0);
    try(fd, "unknown device", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, (char *)8, 0);
    try(fd, "bad device pointer", SIOCADDRT, &rt);
    rt = route("10.4.0.1", "255.255.0.0", NULL, d0, 0);
    try(fd, "host bits set", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.0.255.0", NULL, d0, 0);
    try(fd, "mask with a hole", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    rt.rt_genmask.sa_family = AF_INET6;
    try(fd, "mask family", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    rt.rt_dst.sa_family = AF_UNIX;
    try(fd, "destination family", SIOCADDRT, &rt);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, RTF_GATEWAY);
    try(fd, "gateway flag without one", SIOCADDRT, &rt);
    rt = route("10.4.0.7", "0.0.0.0", "10.1.0.254", NULL, RTF_HOST | RTF_GATEWAY);
    rt.rt_metric = 8;
    try(fd, "host through a gateway, metric 8", SIOCADDRT, &rt);
    rt = route("10.3.0.0", "255.255.0.0", NULL, NULL, 0);
    try(fd, "delete a missing route", SIOCDELRT, &rt);
    rt = route("10.5.0.0", "255.255.0.0", NULL, d0, 0);
    try(fd, "delete", SIOCDELRT, &rt);
    try(fd, "bad structure", SIOCADDRT, (struct rtentry *)8);
    int u = socket(AF_UNIX, SOCK_DGRAM, 0);
    rt = route("10.4.0.0", "255.255.0.0", NULL, d0, 0);
    try(u, "AF_UNIX socket", SIOCADDRT, &rt);
    /* Without CAP_NET_ADMIN: EPERM before the structure is looked at */
    struct {
        uint32_t version;
        int pid;
    } h = {0x20080522, 0};
    struct {
        uint32_t eff, perm, inh;
    } d[2];
    syscall(SYS_capget, &h, d);
    d[0].eff &= ~(1u << 12);
    syscall(SYS_capset, &h, d);
    rt = route("10.4.0.1", "255.255.0.0", NULL, nosuch, 0);
    try(fd, "without CAP_NET_ADMIN", SIOCADDRT, &rt);
    try(fd, "without CAP_NET_ADMIN, bad structure", SIOCADDRT, (struct rtentry *)8);
    return 0;
}
C
gcc -O1 -o rt rt.c
unshare -n sh -c '
ip link add d0 type dummy
ip addr add 10.1.0.1/24 dev d0
ip addr add 10.1.0.2/24 dev d0 label d0:1
ip link set d0 up
route add -net 10.9.0.0 netmask 255.255.0.0 gw 10.1.0.254 && echo "route add -net gw"
route add -host 10.8.0.1 dev d0 && echo "route add -host dev"
route add -net 10.6.0.0 netmask 255.255.0.0 dev d0 metric 5 && echo "route add metric 5"
route add default gw 10.1.0.254 && echo "route add default"
route add -net 10.7.0.0 netmask 255.255.0.0 gw 10.2.0.1 || echo "unreachable gateway refused"
ip route show
route del -net 10.9.0.0 netmask 255.255.0.0 && echo "route del"
route del -net 10.9.0.0 netmask 255.255.0.0 || echo "deleted twice refused"
route del default && echo "route del default"
/tmp/rt
ip route show
echo 0 > /proc/sys/net/ipv6/conf/d0/accept_dad
ip -6 addr add 2001:db8:1::1/64 dev d0
route -A inet6 add 2001:db8:9::/48 gw 2001:db8:1::fe && echo "route -A inet6 add gw"
route -A inet6 add 2001:db8:8::/48 dev d0 && echo "route -A inet6 add dev"
ip -6 route show | grep 2001:db8:[89]
route -A inet6 del 2001:db8:9::/48 gw 2001:db8:1::fe && echo "route -A inet6 del"
route -A inet6 del 2001:db8:9::/48 gw 2001:db8:1::fe || echo "inet6 deleted twice refused"
ip -6 route show | grep 2001:db8:[89]
' 2>&1
rm -f rt rt.c
