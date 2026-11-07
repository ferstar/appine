# docker-run-options: --privileged
# The sequel to compat4.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): write opens of read-only procfs files; who signal-driven I/O may reach;
# ptrace memory requests on processes made non-dumpable; id maps written through another
# process's open file; procfs content regenerated through the opener's credentials; futexes on
# replaced memory, private and shared operations, and shared memory after fork
set -e
cd /tmp
# Root may open read-only procfs files for writing (only the write fails: EIO for the files of proc
# entries, EINVAL for per-process and namespace files), but not /proc/sys or sysfs ones; O_TRUNC
# asks for write permission, so another user cannot use it on a read-only file
cat > procwrite.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void try(const char *path, int flags, const char *how) {
    int fd = open(path, flags);
    if (fd < 0) {
        printf("%s %s: open %s\n", path, how, strerror(errno));
        return;
    }
    if ((flags & O_ACCMODE) == O_RDONLY) {
        printf("%s %s: open ok\n", path, how);
    } else {
        ssize_t n = write(fd, "1\n", 2);
        printf("%s %s: open ok, write %s\n", path, how, n < 0 ? strerror(errno) : "ok");
    }
    close(fd);
}

static void all(void) {
    const char *paths[] = {"/proc/cpuinfo", "/proc/self/status", "/proc/self/net/dev",
                           "/proc/sysvipc/shm", "/proc/self/fdinfo/0", "/proc/self/ns/net",
                           "/proc/sys/kernel/cap_last_cap", "/sys/devices/system/cpu/online"};
    for (unsigned i = 0; i < sizeof paths / sizeof *paths; i++) {
        try(paths[i], O_WRONLY, "write-only");
        try(paths[i], O_RDWR, "read-write");
        try(paths[i], O_RDONLY | O_TRUNC, "truncating");
    }
}

int main(void) {
    printf("root:\n");
    all();
    FILE *f = fopen("/proc/self/status", "r+");
    char line[64] = "";
    if (f && fgets(line, sizeof line, f)) printf("r+ status: %.5s\n", line);
    else printf("r+ status: %s\n", strerror(errno));
    if (f) fclose(f);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        if (setgid(4321) || setuid(4321)) return 1;
        printf("another user:\n");
        all();
        return 0;
    }
    waitpid(pid, NULL, 0);
    return 0;
}
C
gcc -O1 -o procwrite procwrite.c && ./procwrite
# Signal-driven I/O reaches a process only when whoever set the owner is root or shares a real or
# saved user ID with it (sigio_perm): another user's owner signals nothing, root's and the same
# user's do (SIGIO kills). An owner no pid has is ESRCH (F_SETOWN, a process group, F_SETOWN_EX);
# 0 removes it
cat > sigioperm.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t target(int uid) {
    pid_t t = fork();
    if (t == 0) {
        if (uid && (setgid(uid) || setuid(uid))) _exit(1);
        for (;;) pause();
    }
    return t;
}

// The pipe stays open a while: Linux signals at the write, Appine shortly after
static void signal_from(pid_t t, int uid) {
    pid_t p = fork();
    if (p == 0) {
        if (uid && (setgid(uid) || setuid(uid))) _exit(1);
        int fds[2];
        if (pipe(fds)) _exit(1);
        if (fcntl(fds[0], F_SETOWN, t)) {
            printf("F_SETOWN: %s\n", strerror(errno));
            _exit(1);
        }
        fcntl(fds[0], F_SETFL, O_ASYNC);
        write(fds[1], "x", 1);
        usleep(500000);
        _exit(0);
    }
    waitpid(p, NULL, 0);
}

static void report(const char *what, pid_t t) {
    int st;
    usleep(100000);
    if (waitpid(t, &st, WNOHANG) == 0) printf("%s: alive\n", what);
    else if (WIFSIGNALED(st)) printf("%s: killed by %d\n", what, WTERMSIG(st));
    else printf("%s: exited %d\n", what, WEXITSTATUS(st));
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    pid_t t = target(0);
    usleep(100000);
    signal_from(t, 4321);
    report("root target, another user's owner", t);
    signal_from(t, 0);
    report("root target, root's owner", t);
    t = target(4321);
    usleep(100000);
    signal_from(t, 4321);
    report("target of the same user", t);

    pid_t gone = fork();
    if (gone == 0) _exit(0);
    waitpid(gone, NULL, 0);
    int fds[2];
    pipe(fds);
    printf("F_SETOWN of a reaped pid: %s\n",
           fcntl(fds[0], F_SETOWN, gone) ? strerror(errno) : "ok");
    printf("F_SETOWN of its process group: %s\n",
           fcntl(fds[0], F_SETOWN, -gone) ? strerror(errno) : "ok");
    struct f_owner_ex ex = {F_OWNER_TID, gone};
    printf("F_SETOWN_EX of the tid: %s\n", fcntl(fds[0], F_SETOWN_EX, &ex) ? strerror(errno) : "ok");
    printf("F_SETOWN 0: %s\n", fcntl(fds[0], F_SETOWN, 0) ? strerror(errno) : "ok");
    printf("F_SETOWN self: %s, F_GETOWN is self: %d\n",
           fcntl(fds[0], F_SETOWN, getpid()) ? strerror(errno) : "ok",
           fcntl(fds[0], F_GETOWN) == getpid());
    return 0;
}
C
gcc -O1 -o sigioperm sigioperm.c && ./sigioperm
# PTRACE_PEEKDATA/POKEDATA on a tracee that made itself non-dumpable are EIO for a tracer without
# CAP_SYS_PTRACE at attach (ptrace_access_vm), and work for a dumpable one or a privileged tracer
# (the tracee sets its dumpable state itself: leaving root made the other user's processes
# non-dumpable)
cat > accessvm.c <<'C'
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile long secret = 0x1234;

static void trace(const char *who) {
    for (int dumpable = 1; dumpable >= 0; dumpable--) {
        pid_t c = fork();
        if (c == 0) {
            ptrace(PTRACE_TRACEME, 0, 0, 0);
            prctl(PR_SET_DUMPABLE, dumpable);
            raise(SIGSTOP);
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        errno = 0;
        long v = ptrace(PTRACE_PEEKDATA, c, &secret, 0);
        printf("%s, dumpable %d: peek %s", who, dumpable,
               errno ? strerror(errno) : v == 0x1234 ? "ok" : "wrong value");
        long r = ptrace(PTRACE_POKEDATA, c, &secret, 5);
        printf(", poke %s\n", r ? strerror(errno) : "ok");
        fflush(stdout);
        kill(c, SIGKILL);
        waitpid(c, &st, 0);
    }
}

int main(void) {
    trace("root");
    pid_t u = fork();
    if (u == 0) {
        if (setgid(4321) || setuid(4321)) return 1;
        trace("another user");
        return 0;
    }
    waitpid(u, NULL, 0);
    return 0;
}
C
gcc -O1 -o accessvm accessvm.c && ./accessvm
# uid_map written by root through a descriptor an unprivileged user opened is judged by the opener
# too: mapping uid 0 needs the opener's CAP_SETFCAP, more than its own ID the opener's CAP_SETUID;
# its own ID alone works. Contents read through a descriptor follow its opener's user namespace,
# not the reader's
cat > idmapopener.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static void send_fd(int s, int fd) {
    char c[CMSG_SPACE(sizeof(int))] = {0};
    char b = 'f';
    struct iovec iov = {&b, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = c,
                       .msg_controllen = sizeof c};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &fd, sizeof fd);
    sendmsg(s, &m, 0);
}

static int recv_fd(int s) {
    char c[CMSG_SPACE(sizeof(int))] = {0};
    char b;
    struct iovec iov = {&b, 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = c,
                       .msg_controllen = sizeof c};
    if (recvmsg(s, &m, 0) <= 0 || !CMSG_FIRSTHDR(&m)) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&m)), sizeof fd);
    return fd;
}

static const char *w(int fd, const char *s) {
    return write(fd, s, strlen(s)) < 0 ? strerror(errno) : "ok";
}

static void line(int fd, const char *what, const char *prefix) {
    char buf[4096];
    lseek(fd, 0, SEEK_SET);
    ssize_t n = read(fd, buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    char *p = strstr(buf, prefix);
    if (p) *strchrnul(p, '\n') = 0;
    printf("%s: %s\n", what, p ? p : "(none)");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t u = fork();
    if (u == 0) {
        close(sv[0]);
        setgroups(0, NULL);
        if (setgid(4321) || setuid(4321)) return 1;
        // Leaving root made it non-dumpable, and the child's /proc files would belong to root
        prctl(PR_SET_DUMPABLE, 1);
        int ready[2], hold[2];
        pipe(ready);
        pipe(hold);
        pid_t c = fork();
        char x;
        if (c == 0) {
            if (unshare(CLONE_NEWUSER)) printf("unshare: %s\n", strerror(errno));
            write(ready[1], "r", 1);
            read(hold[0], &x, 1);
            printf("inside: uid %d\n", (int)getuid());
            _exit(0);
        }
        read(ready[0], &x, 1);
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/uid_map", c);
        int fd = open(path, O_WRONLY);
        if (fd < 0) {
            printf("open %s\n", strerror(errno));
            write(sv[1], "n", 1);
        } else {
            send_fd(sv[1], fd);
        }
        read(sv[1], &x, 1);
        write(hold[1], "g", 1);
        waitpid(c, NULL, 0);
        _exit(0);
    }
    close(sv[1]);
    int fd = recv_fd(sv[0]);
    if (fd >= 0) {
        printf("root writes 0 0 1: %s\n", w(fd, "0 0 1\n"));
        printf("root writes 0 4321 2: %s\n", w(fd, "0 4321 2\n"));
        printf("root writes 0 4321 1: %s\n", w(fd, "0 4321 1\n"));
    }
    write(sv[0], "d", 1);
    waitpid(u, NULL, 0);

    // Opened here, read again by a child in a new user namespace with no map
    int status = open("/proc/self/status", O_RDONLY);
    int map = open("/proc/self/uid_map", O_RDONLY);
    pid_t c = fork();
    if (c == 0) {
        if (unshare(CLONE_NEWUSER)) printf("unshare: %s\n", strerror(errno));
        line(status, "status read in the child", "Uid:");
        line(map, "uid_map read in the child", " ");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O1 -o idmapopener idmapopener.c && ./idmapopener
# A futex waiter is found at its address after the memory there was replaced (MAP_FIXED); private
# and shared operations on one private address do not meet; shared anonymous memory still meets
# across fork
cat > futexkeys.c <<'C'
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// The store has no kernel headers
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_WAIT_PRIVATE 128
#define FUTEX_WAKE_PRIVATE 129

static long futex(int *a, int op, int val, const struct timespec *t) {
    return syscall(SYS_futex, a, op, val, t, NULL, 0);
}

static int *page;
static int waited_op;
static const char *result;

static void *waiter(void *arg) {
    (void)arg;
    struct timespec t = {3, 0};
    result = futex(page, waited_op, 0, &t) == 0 ? "woken" : strerror(errno);
    return NULL;
}

// Wakes until somebody is woken, for at most 2 s
static long wake_loop(int op) {
    for (int i = 0; i < 200; i++) {
        long n = futex(page, op, 1, NULL);
        if (n) return n;
        usleep(10000);
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_t th;
    waited_op = FUTEX_WAIT_PRIVATE;
    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    mmap(page, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    printf("wake after MAP_FIXED: %ld\n", wake_loop(FUTEX_WAKE_PRIVATE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);

    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    printf("shared wake of a private waiter: %ld\n", futex(page, FUTEX_WAKE, 1, NULL));
    printf("private wake: %ld\n", wake_loop(FUTEX_WAKE_PRIVATE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);
    waited_op = FUTEX_WAIT;
    pthread_create(&th, NULL, waiter, NULL);
    usleep(300000);
    printf("private wake of a shared waiter: %ld\n", futex(page, FUTEX_WAKE_PRIVATE, 1, NULL));
    printf("shared wake: %ld\n", wake_loop(FUTEX_WAKE));
    pthread_join(th, NULL);
    printf("waiter: %s\n", result);

    page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t c = fork();
    if (c == 0) {
        struct timespec t = {3, 0};
        printf("child waiting on shared memory: %s\n",
               futex(page, FUTEX_WAIT, 0, &t) == 0 ? "woken" : strerror(errno));
        _exit(0);
    }
    long n = wake_loop(FUTEX_WAKE);
    waitpid(c, NULL, 0);
    printf("parent's shared wake: %ld\n", n);
    return 0;
}
C
gcc -O1 -pthread -o futexkeys futexkeys.c && ./futexkeys
