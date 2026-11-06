# The sequel to compat2.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MSG_WAITALL and MSG_PEEK on TCP; arguments at the edges of their types;
# taking a controlling terminal; kill with a thread id
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

# Arguments at the edges of their types, as Linux takes them: mlock and munlock page-align the
# length with wrapping arithmetic (an empty range succeeds, one that wraps is EINVAL);
# F_SETOWN(INT_MIN) is EINVAL; record locks may end at OFFSET_MAX but not past it, and may not
# start below 0 (F_GETLK reports a lock to OFFSET_MAX with l_len 0); a negative iov_len is EINVAL,
# one past the address space EFAULT, and huge ones are cut to MAX_RW_COUNT; semtimedop's nsops is
# an unsigned int
cat > edges.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/sem.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static int fd;

static long lock(short type, short whence, int64_t start, int64_t len)
{
    struct flock l = {.l_type = type, .l_whence = whence, .l_start = start, .l_len = len};
    return fcntl(fd, F_SETLK, &l);
}

/* What another process sees: F_GETLK over the whole file */
static void probe(const char *what)
{
    pid_t pid = fork();
    if (pid == 0) {
        struct flock l = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
        if (fcntl(fd, F_GETLK, &l) < 0)
            printf("%s: %s\n", what, strerror(errno));
        else if (l.l_type == F_UNLCK)
            printf("%s: unlocked\n", what);
        else
            printf("%s: start %lld len %lld\n", what, (long long)l.l_start, (long long)l.l_len);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    char *p = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    show("mlock len SIZE_MAX", mlock(p, SIZE_MAX));
    /* Without CAP_IPC_LOCK (docker's root has none) RLIMIT_MEMLOCK is checked first */
    pid_t pid = fork();
    if (pid == 0) {
        /* capget/capset's version 3 header and data (CAP_IPC_LOCK is bit 14) */
        struct { uint32_t version; int pid; } h = {0x20080522, 0};
        struct { uint32_t effective, permitted, inheritable; } d[2];
        syscall(SYS_capget, &h, d);
        d[0].effective &= ~(1u << 14);
        syscall(SYS_capset, &h, d);
        show("mlock len that wraps, no CAP_IPC_LOCK", mlock(p + 4096, SIZE_MAX - 8191));
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("munlock len that wraps", munlock(p + 4096, SIZE_MAX - 8191));
    show("mlock 0 bytes inside a page", mlock(p + 1, 0));
    show("mlock 2 pages", mlock(p + 100, 4096));
    show("munlock 2 pages", munlock(p + 100, 4096));

    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    show("F_SETOWN INT_MIN", fcntl(sv[0], F_SETOWN, INT32_MIN));
    show("F_SETOWN self", fcntl(sv[0], F_SETOWN, getpid()));
    printf("F_GETOWN is self: %d\n", fcntl(sv[0], F_GETOWN) == getpid());
    show("F_SETOWN own group", fcntl(sv[0], F_SETOWN, -getpgrp()));
    printf("F_GETOWN is -group: %d\n", fcntl(sv[0], F_GETOWN) == -getpgrp());

    char b1[8] = {0}, b2[8] = {0};
    write(sv[1], "hello", 5);
    struct iovec bad[2] = {{b1, 2}, {b2, (size_t)1 << 63}};
    struct msghdr m = {.msg_iov = bad, .msg_iovlen = 2};
    show("recvmsg with a negative iov_len", recvmsg(sv[0], &m, MSG_DONTWAIT));
    struct iovec past[2] = {{b1, 2}, {b2, (size_t)1 << 48}};
    m.msg_iov = past;
    show("recvmsg with an iovec past the address space", recvmsg(sv[0], &m, MSG_DONTWAIT));
    /* Huge lengths that stay inside the address space: low addresses */
    char *low = mmap((void *)0x10000000, 4096, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    struct iovec big[3] = {{low, 2}, {low + 8, (size_t)1 << 46}, {low + 8, (size_t)1 << 46}};
    m.msg_iov = big;
    m.msg_iovlen = 3;
    long n = recvmsg(sv[0], &m, MSG_DONTWAIT);
    printf("recvmsg with huge iov_lens: %ld %.2s %.3s\n", n, low, low + 8);

    fd = open("edges.lock", O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(fd, "0123456789", 10);
    show("lock to OFFSET_MAX - 1", lock(F_WRLCK, SEEK_SET, 0, INT64_MAX));
    probe(" seen");
    show("lock to OFFSET_MAX", lock(F_WRLCK, SEEK_SET, 1, INT64_MAX));
    probe(" seen");
    show("lock past OFFSET_MAX", lock(F_WRLCK, SEEK_SET, 2, INT64_MAX));
    lock(F_UNLCK, SEEK_SET, 0, 0);
    show("lock INT64_MIN, INT64_MIN", lock(F_WRLCK, SEEK_SET, INT64_MIN, INT64_MIN));
    show("lock from the end past OFFSET_MAX", lock(F_WRLCK, SEEK_END, INT64_MAX, 1));
    show("lock from the end, negative", lock(F_WRLCK, SEEK_END, -11, 1));
    show("lock 10, -5", lock(F_WRLCK, SEEK_SET, 10, -5));
    probe(" seen");
    lock(F_UNLCK, SEEK_SET, 0, 0);
    show("lock 3, -5", lock(F_WRLCK, SEEK_SET, 3, -5));
    show("lock 5, INT64_MIN", lock(F_WRLCK, SEEK_SET, 5, INT64_MIN));
    unlink("edges.lock");

    int id = semget(IPC_PRIVATE, 1, 0600);
    struct sembuf op = {0, 1, 0};
    show("semop with nsops 2^32 + 1", syscall(SYS_semop, id, &op, (1UL << 32) + 1));
    printf("value: %d\n", semctl(id, 0, GETVAL));
    show("semop with nsops 2^32", syscall(SYS_semop, id, &op, 1UL << 32));
    semctl(id, 0, IPC_RMID);
    return 0;
}
C
gcc -O1 -o edges edges.c && ./edges
rm -f edges edges.c

# Taking a controlling terminal: TIOCSCTTY takes one from another session only with argument 1
# and CAP_SYS_ADMIN, and needs read access to the fd without CAP_SYS_ADMIN; opening a slave
# without O_NOCTTY makes it the controlling terminal only with read access. The tests drop
# CAP_SYS_ADMIN (docker's root has none)
cat > ctty.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static void show(const char *what, long r)
{
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        printf("%s: %ld\n", what, r);
}

static void drop_sys_admin(void)
{
    /* capget/capset's version 3 header and data (CAP_SYS_ADMIN is bit 21) */
    struct { uint32_t version; int pid; } h = {0x20080522, 0};
    struct { uint32_t effective, permitted, inheritable; } d[2];
    syscall(SYS_capget, &h, d);
    d[0].effective &= ~(1u << 21);
    syscall(SYS_capset, &h, d);
}

static char *new_pty(int *master)
{
    *master = posix_openpt(O_RDWR | O_NOCTTY);
    grantpt(*master);
    unlockpt(*master);
    return ptsname(*master);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    int m1, m2, done[2];
    char *name = strdup(new_pty(&m1));
    pipe(done);
    /* The victim's session owns the first terminal */
    pid_t victim = fork();
    if (victim == 0) {
        setsid();
        int fd = open(name, O_RDWR);
        printf("victim got it: %d\n", tcgetsid(fd) == getpid());
        char x;
        read(done[0], &x, 1);
        printf("victim still has it: %d\n", tcgetsid(fd) == getpid());
        _exit(0);
    }
    usleep(100000);
    pid_t pid = fork();
    if (pid == 0) {
        drop_sys_admin();
        setsid();
        int fd = open(name, O_RDWR | O_NOCTTY);
        show("TIOCSCTTY 0 on another session's terminal", ioctl(fd, TIOCSCTTY, 0));
        show("TIOCSCTTY 1 without CAP_SYS_ADMIN", ioctl(fd, TIOCSCTTY, 1));
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    write(done[1], "x", 1);
    waitpid(victim, NULL, 0);

    char *name2 = strdup(new_pty(&m2));
    pid = fork();
    if (pid == 0) {
        drop_sys_admin();
        setsid();
        int w = open(name2, O_WRONLY);
        show("after a write-only open, /dev/tty", open("/dev/tty", O_RDWR) >= 0 ? 0 : -1);
        show("TIOCSCTTY on a write-only fd", ioctl(w, TIOCSCTTY, 0));
        int r = open(name2, O_RDONLY);
        show("after a read-only open, /dev/tty", open("/dev/tty", O_RDWR) >= 0 ? 0 : -1);
        close(r);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o ctty ctty.c && ./ctty
rm -f ctty ctty.c

# kill with a thread's id signals that thread's process (Linux's kill_pid_info), as kill -0 does
# for checking it exists; tgkill with a wrong thread group is ESRCH
cat > killtid.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile pid_t tid;
static volatile int got;

static void on_usr1(int sig)
{
    (void)sig;
    got++;
}

static void *thread(void *arg)
{
    (void)arg;
    tid = syscall(SYS_gettid);
    for (;;)
        pause();
    return NULL;
}

int main(void)
{
    signal(SIGUSR1, on_usr1);
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    while (!tid)
        usleep(1000);
    printf("kill(tid, 0): %d\n", kill(tid, 0));
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    printf("kill(tid, SIGUSR1): %d\n", kill(tid, SIGUSR1));
    for (int i = 0; i < 200 && !got; i++)
        usleep(10000);
    printf("a thread handled it: %d\n", got);
    errno = 0;
    printf("tgkill with another group: %ld %s\n", syscall(SYS_tgkill, getppid(), tid, 0),
           strerror(errno));
    return 0;
}
C
gcc -O1 -pthread -o killtid killtid.c && ./killtid
rm -f killtid killtid.c
