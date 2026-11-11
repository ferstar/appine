# docker-run-options: --privileged
# The eleventh map-bug triage's socket items: MSG_TRUNC on a TCP receive discards the data without
# touching the buffer (a NULL one included) and returns its length, with MSG_PEEK and MSG_WAITALL
# too; a blocking TCP send or write that meets a shutdown after part of its data returns that count
# without SIGPIPE, and the next call gets EPIPE and SIGPIPE
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
