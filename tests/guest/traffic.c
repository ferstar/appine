/*
 * 网络接口的流量计数（core/kernel/src/traffic.rs）：在回环上收发之后，/proc/net/dev、
 * /sys/class/net/lo/statistics 和 getifaddrs 的 ifa_data（netlink 的 IFLA_STATS）三处 lo 的
 * 计数相同，增加的字节数正好是收发的载荷，偷看（MSG_PEEK）的不算。只在 appine 里成立：Linux
 * 还算协议头和 ACK。通过时打印 "all ok"，失败时打印原因并以 1 退出。
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <ifaddrs.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            printf("FAIL: ");                                                                      \
            printf(__VA_ARGS__);                                                                   \
            printf(" (errno %d)\n", errno);                                                        \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

struct counts {
    unsigned long long rx_bytes, rx_packets, tx_bytes, tx_packets;
};

static struct counts from_proc(void)
{
    FILE *f = fopen("/proc/net/dev", "r");
    CHECK(f, "open /proc/net/dev");
    char line[512];
    struct counts c;
    int found = 0;
    while (fgets(line, sizeof line, f)) {
        char *colon = strchr(line, ':');
        if (!colon)
            continue;
        *colon = 0;
        char *name = line + strspn(line, " ");
        if (strcmp(name, "lo") != 0)
            continue;
        unsigned long long v[16];
        CHECK(sscanf(colon + 1, "%llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu "
                                "%llu %llu",
                     &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9], &v[10],
                     &v[11], &v[12], &v[13], &v[14], &v[15]) == 16,
              "the lo line has 16 counters");
        c = (struct counts){v[0], v[1], v[8], v[9]};
        found = 1;
    }
    fclose(f);
    CHECK(found, "no lo in /proc/net/dev");
    return c;
}

static unsigned long long sys_value(const char *file)
{
    char path[128];
    snprintf(path, sizeof path, "/sys/class/net/lo/statistics/%s", file);
    FILE *f = fopen(path, "r");
    CHECK(f, "open %s", path);
    unsigned long long v;
    CHECK(fscanf(f, "%llu", &v) == 1, "read %s", path);
    fclose(f);
    return v;
}

static struct counts from_sys(void)
{
    return (struct counts){sys_value("rx_bytes"), sys_value("rx_packets"), sys_value("tx_bytes"),
                           sys_value("tx_packets")};
}

/* struct rtnl_link_stats 的前四项 */
struct link_stats {
    uint32_t rx_packets, tx_packets, rx_bytes, tx_bytes;
};

static struct counts from_ifaddrs(void)
{
    struct ifaddrs *list;
    CHECK(getifaddrs(&list) == 0, "getifaddrs");
    struct counts c;
    int found = 0;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (strcmp(i->ifa_name, "lo") != 0 || !i->ifa_addr || i->ifa_addr->sa_family != AF_PACKET)
            continue;
        CHECK(i->ifa_data, "lo has no ifa_data");
        struct link_stats *s = i->ifa_data;
        c = (struct counts){s->rx_bytes, s->rx_packets, s->tx_bytes, s->tx_packets};
        found = 1;
    }
    freeifaddrs(list);
    CHECK(found, "no AF_PACKET entry for lo");
    return c;
}

/* 三处的计数相同 */
static struct counts counters(void)
{
    struct counts p = from_proc(), s = from_sys(), a = from_ifaddrs();
    CHECK(memcmp(&p, &s, sizeof p) == 0 && memcmp(&p, &a, sizeof p) == 0,
          "/proc/net/dev %llu/%llu %llu/%llu, sysfs %llu/%llu %llu/%llu, ifaddrs %llu/%llu %llu/%llu",
          p.rx_bytes, p.rx_packets, p.tx_bytes, p.tx_packets, s.rx_bytes, s.rx_packets, s.tx_bytes,
          s.tx_packets, a.rx_bytes, a.rx_packets, a.tx_bytes, a.tx_packets);
    return p;
}

static void read_line(const char *path, const char *want)
{
    FILE *f = fopen(path, "r");
    CHECK(f, "open %s", path);
    char got[64] = "";
    CHECK(fgets(got, sizeof got, f), "read %s", path);
    fclose(f);
    CHECK(strcmp(got, want) == 0, "%s is %s", path, got);
}

#define STREAM_BYTES 100000
#define DATAGRAMS 10
#define DATAGRAM_BYTES 100
#define UNREAD_BYTES 50

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    DIR *d = opendir("/sys/class/net");
    CHECK(d, "opendir /sys/class/net");
    int listed = 0;
    for (struct dirent *e; (e = readdir(d));)
        listed |= strcmp(e->d_name, "lo") == 0;
    closedir(d);
    CHECK(listed, "lo is not listed in /sys/class/net");
    read_line("/sys/class/net/lo/type", "772\n");
    read_line("/sys/class/net/lo/operstate", "unknown\n");
    read_line("/sys/class/net/lo/statistics/rx_errors", "0\n");
    printf("ok sysfs\n");

    struct counts before = counters();

    /* 回环上的 TCP 连接 */
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof addr;
    int l = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(l >= 0 && bind(l, (void *)&addr, len) == 0 && listen(l, 1) == 0, "listen");
    CHECK(getsockname(l, (void *)&addr, &len) == 0, "getsockname");
    int c = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(c >= 0 && connect(c, (void *)&addr, len) == 0, "connect");
    int s = accept(l, NULL, NULL);
    CHECK(s >= 0, "accept");
    static char buf[STREAM_BYTES];
    memset(buf, 'x', sizeof buf);
    if (fork() == 0) {
        size_t sent = 0;
        while (sent < sizeof buf) {
            ssize_t n = write(c, buf + sent, sizeof buf - sent);
            if (n <= 0)
                _exit(1);
            sent += n;
        }
        _exit(0);
    }
    close(c);
    size_t got = 0;
    for (ssize_t n; (n = read(s, buf, sizeof buf)) > 0;)
        got += n;
    CHECK(got == STREAM_BYTES, "received %zu bytes", got);
    close(s);
    close(l);

    /* 没有 connect 的 UDP：按对端认接口；偷看一次，不算 */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    addr.sin_port = 0;
    len = sizeof addr;
    CHECK(u >= 0 && bind(u, (void *)&addr, len) == 0 && getsockname(u, (void *)&addr, &len) == 0,
          "bind udp");
    int v = socket(AF_INET, SOCK_DGRAM, 0);
    for (int i = 0; i < DATAGRAMS; i++)
        CHECK(sendto(v, buf, DATAGRAM_BYTES, 0, (void *)&addr, len) == DATAGRAM_BYTES, "sendto");
    CHECK(recv(u, buf, sizeof buf, MSG_PEEK) == DATAGRAM_BYTES, "peek");
    for (int i = 0; i < DATAGRAMS; i++)
        CHECK(recv(u, buf, sizeof buf, 0) == DATAGRAM_BYTES, "recv");
    /* 发出去没人读的：只算发出的，收发两个方向的数因此不同 */
    CHECK(sendto(v, buf, UNREAD_BYTES, 0, (void *)&addr, len) == UNREAD_BYTES, "sendto");
    close(u);
    close(v);

    struct counts after = counters();
    unsigned long long bytes = STREAM_BYTES + DATAGRAMS * DATAGRAM_BYTES;
    CHECK(after.tx_bytes - before.tx_bytes == bytes + UNREAD_BYTES &&
              after.rx_bytes - before.rx_bytes == bytes,
          "bytes: sent %llu, received %llu", after.tx_bytes - before.tx_bytes,
          after.rx_bytes - before.rx_bytes);
    /* TCP 至少 2 个（回环的 MSS 约 64 KB），每个数据报 1 个 */
    CHECK(after.tx_packets - before.tx_packets >= DATAGRAMS + 3 &&
              after.rx_packets - before.rx_packets >= DATAGRAMS + 2,
          "packets: sent %llu, received %llu", after.tx_packets - before.tx_packets,
          after.rx_packets - before.rx_packets);
    printf("ok counters\n");
    printf("all ok\n");
    return 0;
}
