# The sequel to compat2.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MSG_WAITALL and MSG_PEEK on TCP
set -e
cd /tmp
# MSG_WAITALL on TCP waits for the whole length even when the data comes in pieces, and over 1 MiB;
# end of file, the receive timeout, a signal (even with SA_RESTART) and MSG_DONTWAIT end it with
# what has come. MSG_PEEK|MSG_WAITALL waits the same way and shows the head of the queue once,
# leaving it to be read
cat > waitall.c <<'C'
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static int lfd;
static struct sockaddr_in addr;
static volatile int handled;

static void on_alarm(int sig)
{
    (void)sig;
    handled++;
}

static void fill(unsigned char *p, size_t n, size_t from)
{
    for (size_t i = 0; i < n; i++)
        p[i] = (unsigned char)((from + i) * 7 + 1);
}

static int check(const unsigned char *p, size_t n, size_t from)
{
    for (size_t i = 0; i < n; i++)
        if (p[i] != (unsigned char)((from + i) * 7 + 1))
            return 0;
    return 1;
}

static void send_all(int fd, size_t from, size_t n)
{
    static unsigned char b[65536];
    while (n > 0) {
        size_t k = n < sizeof b ? n : sizeof b;
        fill(b, k, from);
        ssize_t w = send(fd, b, k, 0);
        if (w <= 0)
            exit(3);
        from += w;
        n -= w;
    }
}

/* The child connects and sends `first` bytes, waits 100 ms, sends `second` more, then either
 * closes (close_after) or waits for a byte on the pipe before closing. Returns the accepted fd. */
static int start(size_t first, size_t second, int close_after, int *done, pid_t *child)
{
    int p[2];
    if (pipe(p) < 0)
        exit(4);
    pid_t pid = fork();
    if (pid == 0) {
        close(p[1]);
        int c = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&addr, sizeof addr) < 0)
            _exit(5);
        send_all(c, 0, first);
        usleep(100000);
        send_all(c, first, second);
        if (!close_after) {
            char x;
            read(p[0], &x, 1);
        }
        close(c);
        _exit(0);
    }
    close(p[0]);
    *done = p[1];
    *child = pid;
    return accept(lfd, NULL, NULL);
}

static void finish(int s, int done, pid_t child)
{
    write(done, "x", 1);
    close(done);
    close(s);
    waitpid(child, NULL, 0);
}

static void report(const char *what, ssize_t n, const unsigned char *b, size_t from)
{
    if (n < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %zd, data %s\n", what, n, check(b, n, from) ? "right" : "WRONG");
}

int main(void)
{
    static unsigned char b[3 << 20];
    setvbuf(stdout, NULL, _IOLBF, 0);
    /* finish() writes to children that may already have exited */
    signal(SIGPIPE, SIG_IGN);
    int done, s;
    pid_t child;
    ssize_t n;
    lfd = socket(AF_INET, SOCK_STREAM, 0);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof addr;
    if (bind(lfd, (struct sockaddr *)&addr, sizeof addr) < 0 || listen(lfd, 8) < 0 ||
        getsockname(lfd, (struct sockaddr *)&addr, &len) < 0)
        return 1;

    s = start(10, 90, 0, &done, &child);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall 10 then 90", n, b, 0);
    finish(s, done, child);

    s = start(10, 90, 0, &done, &child);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall 10 then 90", n, b, 0);
    n = recv(s, b, 100, MSG_DONTWAIT);
    report("then read", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 1, &done, &child);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall at end of file", n, b, 0);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall after end of file", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 1, &done, &child);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall at end of file", n, b, 0);
    n = recv(s, b, 100, 0);
    report("then read", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    struct timeval tv = {0, 300000};
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall with SO_RCVTIMEO", n, b, 0);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall with SO_RCVTIMEO and nothing", n, b, 0);
    n = recv(s, b, 100, MSG_PEEK | MSG_WAITALL);
    report("peek|waitall with SO_RCVTIMEO and nothing", n, b, 0);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 0}, {0, 300000}};
    setitimer(ITIMER_REAL, &it, NULL);
    n = recv(s, b, 100, MSG_WAITALL);
    report("waitall cut by a SA_RESTART signal", n, b, 0);
    printf("handled: %d\n", handled);
    finish(s, done, child);

    s = start(10, 0, 0, &done, &child);
    usleep(50000);
    n = recv(s, b, 100, MSG_WAITALL | MSG_DONTWAIT);
    report("waitall|dontwait", n, b, 0);
    finish(s, done, child);

    s = start(1 << 20, 2 << 20, 0, &done, &child);
    n = recv(s, b, 3 << 20, MSG_WAITALL);
    report("waitall 3 MiB", n, b, 0);
    finish(s, done, child);

    s = start(1 << 20, 2 << 20, 0, &done, &child);
    struct iovec iov[3] = {{b, 1000}, {b + 1000, (3 << 20) - 2000}, {b + (3 << 20) - 1000, 1000}};
    struct msghdr m = {.msg_iov = iov, .msg_iovlen = 3};
    n = recvmsg(s, &m, MSG_WAITALL);
    report("recvmsg waitall 3 MiB over 3 iovecs", n, b, 0);
    finish(s, done, child);
    return 0;
}
C
gcc -O1 -o waitall waitall.c && ./waitall
rm -f waitall waitall.c
