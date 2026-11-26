/*
 * Regression tests for low-level kernel stability fixes:
 * - kill(INT_MIN, sig) returns ESRCH/EINVAL without debug-build negation overflow;
 * - sigaltstack with unwritable uoss returns EFAULT without mutating altstack;
 * - nanosleep interrupted by signal or execve clears TaskState::restart;
 * - AF_UNIX partial stream send on peer close returns partial count without SIGPIPE;
 * - AF_UNIX recvmsg EFAULT on msg_name or read-only msghdr drops installed SCM_RIGHTS fds;
 * - AF_UNIX blocked syscall keeps FileRef alive across concurrent close and unix::collect;
 * - sched_setattr failing RLIMIT_NICE leaves policy and RESET_ON_FORK unchanged;
 * - sched_setscheduler/sched_setparam/sched_setattr enforce target RLIMIT_RTPRIO without CAP_SYS_NICE;
 * - pidfd_send_signal from a child pid namespace translates si_pid into the target namespace.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            fprintf(stderr, "FAIL: ");                                                             \
            fprintf(stderr, __VA_ARGS__);                                                          \
            fprintf(stderr, " (errno %d)\n", errno);                                               \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

struct sched_attr {
    uint32_t size;
    uint32_t sched_policy;
    uint64_t sched_flags;
    int32_t sched_nice;
    uint32_t sched_priority;
    uint64_t sched_runtime;
    uint64_t sched_deadline;
    uint64_t sched_period;
    uint32_t sched_util_min;
    uint32_t sched_util_max;
};

#ifndef SCHED_BATCH
#define SCHED_BATCH 3
#endif
#ifndef SCHED_IDLE
#define SCHED_IDLE 5
#endif
#ifndef SCHED_DEADLINE
#define SCHED_DEADLINE 6
#endif
#ifndef SCHED_FLAG_RESET_ON_FORK
#define SCHED_FLAG_RESET_ON_FORK 1
#endif
#ifndef SCHED_FLAG_KEEP_POLICY
#define SCHED_FLAG_KEEP_POLICY 0x08
#endif
#ifndef SCHED_FLAG_KEEP_PARAMS
#define SCHED_FLAG_KEEP_PARAMS 0x10
#endif
#ifndef SCHED_FLAG_UTIL_CLAMP_MIN
#define SCHED_FLAG_UTIL_CLAMP_MIN 0x20
#endif
#ifndef RLIMIT_RTPRIO
#define RLIMIT_RTPRIO 14
#endif

static volatile sig_atomic_t got_sigpipe = 0;
static volatile sig_atomic_t got_alrm = 0;
static volatile sig_atomic_t got_usr1_pid = -1;

static void on_sigpipe(int sig)
{
    (void)sig;
    got_sigpipe++;
}

static void on_alrm(int sig)
{
    (void)sig;
    got_alrm++;
}

static void on_usr1(int sig, siginfo_t *info, void *uctx)
{
    (void)sig;
    (void)uctx;
    got_usr1_pid = info->si_pid;
}

static void test_kill_int_min(void)
{
    errno = 0;
    CHECK(kill(INT_MIN, 0) == -1 && errno == ESRCH, "kill(INT_MIN, 0) should be ESRCH");
    errno = 0;
    CHECK(kill(INT_MIN, 999) == -1 && errno == EINVAL, "kill(INT_MIN, 999) should be EINVAL");
}

static void test_sigaltstack_uoss_fault(void)
{
    static char altbuf[16384];
    stack_t orig = {0}, got = {0};
    stack_t want = {.ss_sp = altbuf, .ss_size = sizeof(altbuf), .ss_flags = 0};
    CHECK(sigaltstack(NULL, &orig) == 0, "sigaltstack query");
    errno = 0;
    CHECK(sigaltstack(&want, (stack_t *)0x10) == -1 && errno == EFAULT,
          "sigaltstack with bad uoss must return EFAULT");
    CHECK(sigaltstack(NULL, &got) == 0, "sigaltstack query after EFAULT");
    CHECK(got.ss_sp == orig.ss_sp && got.ss_size == orig.ss_size && got.ss_flags == orig.ss_flags,
          "sigaltstack must not mutate state on uoss EFAULT");
}

static void test_restart_cleared(const char *self)
{
    struct sigaction sa = {0};
    sa.sa_handler = on_alrm;
    CHECK(sigaction(SIGALRM, &sa, NULL) == 0, "sigaction SIGALRM");

    struct itimerval it = {.it_value = {0, 20000}};
    CHECK(setitimer(ITIMER_REAL, &it, NULL) == 0, "setitimer");

    struct timespec req = {5, 0};
    errno = 0;
    CHECK(nanosleep(&req, NULL) == -1 && errno == EINTR, "nanosleep should be interrupted");
    CHECK(got_alrm == 1, "SIGALRM handler should have run");

    struct timespec t0, t1;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t0) == 0, "clock_gettime");
    errno = 0;
    long r = syscall(SYS_restart_syscall);
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t1) == 0, "clock_gettime");
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    CHECK(r == -1 && errno == EINTR && ms < 1000,
          "restart_syscall after EINTR must return EINTR immediately (r=%ld ms=%ld)", r, ms);

    pid_t pid = fork();
    CHECK(pid >= 0, "fork for execve restart test");
    if (pid == 0) {
        struct itimerval it2 = {.it_value = {0, 20000}};
        CHECK(setitimer(ITIMER_REAL, &it2, NULL) == 0, "child setitimer");
        struct timespec req2 = {5, 0};
        CHECK(nanosleep(&req2, NULL) == -1 && errno == EINTR, "child nanosleep EINTR");
        char *argv[] = {(char *)self, (char *)"--after-exec", NULL};
        execv(self, argv);
        _exit(99);
    }
    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid execve restart");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "execve child failed: %d", status);
}

static void *close_fd_later(void *arg)
{
    int fd = *(int *)arg;
    usleep(30000);
    close(fd);
    return NULL;
}

static void test_unix_partial_send_no_sigpipe(void)
{
    struct sigaction sa = {0};
    sa.sa_handler = on_sigpipe;
    CHECK(sigaction(SIGPIPE, &sa, NULL) == 0, "sigaction SIGPIPE");
    got_sigpipe = 0;

    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "socketpair");
    static char buf[65536];
    memset(buf, 'a', sizeof(buf));
    /* Fill channel completely, then drain 4096 bytes so blocking send writes one chunk then waits. */
    while (send(sv[0], buf, 4096, MSG_DONTWAIT) > 0) {
    }
    CHECK(errno == EAGAIN, "fill expected EAGAIN");
    CHECK(recv(sv[1], buf, 4096, 0) == 4096, "drain 4096");

    pthread_t th;
    int peer = sv[1];
    CHECK(pthread_create(&th, NULL, close_fd_later, &peer) == 0, "pthread_create");
    ssize_t n = send(sv[0], buf, sizeof(buf), 0);
    pthread_join(th, NULL);
    CHECK(n > 0 && n < (ssize_t)sizeof(buf), "partial stream send expected partial count, got %zd",
          n);
    CHECK(got_sigpipe == 0, "partial stream send must not raise SIGPIPE (got %d)", got_sigpipe);

    /* Next send writes 0 bytes and must fail with EPIPE and raise SIGPIPE. */
    errno = 0;
    CHECK(send(sv[0], "x", 1, 0) == -1 && errno == EPIPE, "subsequent send must EPIPE");
    CHECK(got_sigpipe == 1, "subsequent send must raise SIGPIPE");
    close(sv[0]);
}

static int lowest_free_fd(void)
{
    int fd = dup(0);
    CHECK(fd >= 0, "dup(0)");
    close(fd);
    return fd;
}

static void send_one_fd(int sock, int fd_to_pass)
{
    char byte = 'Z';
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } u;
    memset(&u, 0, sizeof(u));
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = u.buf;
    msg.msg_controllen = sizeof(u.buf);
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &fd_to_pass, sizeof(int));
    CHECK(sendmsg(sock, &msg, 0) == 1, "sendmsg SCM_RIGHTS");
}

static void test_unix_recvmsg_fault_no_fd_leak(void)
{
    int sv[2], p[2];
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) == 0, "dgram socketpair");
    /* Bind sender so recvmsg receives a non-empty peer name. */
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, "\0appine_recvmsg_fault_test", 26);
    CHECK(bind(sv[0], (struct sockaddr *)&addr, offsetof(struct sockaddr_un, sun_path) + 26) == 0,
          "bind abstract");
    CHECK(pipe(p) == 0, "pipe");

    int base_fd = lowest_free_fd();

    /* Case A: unwritable msg_name faults after recv() has installed SCM_RIGHTS. */
    send_one_fd(sv[0], p[0]);
    char byte = 0;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = {0};
    msg.msg_name = (void *)0x10;
    msg.msg_namelen = 32;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf;
    msg.msg_controllen = sizeof(cbuf);
    errno = 0;
    CHECK(recvmsg(sv[1], &msg, 0) == -1 && errno == EFAULT, "recvmsg bad msg_name EFAULT");
    CHECK(lowest_free_fd() == base_fd, "SCM_RIGHTS fd leaked on msg_name EFAULT");

    /* Case B: read-only msghdr faults when writing msg_controllen/msg_flags back. */
    send_one_fd(sv[0], p[0]);
    struct msghdr *ro_msg =
        mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(ro_msg != MAP_FAILED, "mmap ro_msg");
    memset(ro_msg, 0, sizeof(*ro_msg));
    ro_msg->msg_iov = &iov;
    ro_msg->msg_iovlen = 1;
    ro_msg->msg_control = cbuf;
    ro_msg->msg_controllen = sizeof(cbuf);
    CHECK(mprotect(ro_msg, 4096, PROT_READ) == 0, "mprotect PROT_READ");
    errno = 0;
    CHECK(syscall(SYS_recvmsg, sv[1], ro_msg, 0) == -1 && errno == EFAULT,
          "recvmsg ro msghdr EFAULT");
    CHECK(lowest_free_fd() == base_fd, "SCM_RIGHTS fd leaked on msghdr EFAULT");
    munmap(ro_msg, 4096);

    close(p[0]);
    close(p[1]);
    close(sv[0]);
    close(sv[1]);
}

struct gc_ctx {
    int sock;
    int peer;
    int pipe_r;
};

static void *gc_closer_thread(void *arg)
{
    struct gc_ctx *ctx = arg;
    usleep(30000);
    /* Closing sock triggers unix::collect while main thread is blocked in send(sock). */
    close(ctx->sock);
    /* The pipe write end inside sock's inbox must NOT have been collected yet. */
    struct pollfd pfd = {.fd = ctx->pipe_r, .events = POLLIN};
    int pr = poll(&pfd, 1, 0);
    CHECK(pr >= 0, "poll pipe_r");
    CHECK((pfd.revents & POLLHUP) == 0,
          "unix::collect emptied blocked socket's inbox while syscall was waiting");
    /* Drain peer so the blocked send finishes cleanly. */
    static char drain[4096];
    CHECK(recv(ctx->peer, drain, sizeof(drain), 0) == 4096, "drain peer");
    return NULL;
}

static void test_unix_blocked_syscall_pinned_during_gc(void)
{
    int sv[2], p[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "gc socketpair");
    CHECK(pipe(p) == 0, "gc pipe");

    /* Put both sv[1] (self-cycle) and p[1] (liveness probe) into sv[1]'s inbox. */
    char byte = 'G';
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    int fds[2] = {sv[1], p[1]};
    union {
        char buf[CMSG_SPACE(sizeof(fds))];
        struct cmsghdr align;
    } u;
    memset(&u, 0, sizeof(u));
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = u.buf;
    msg.msg_controllen = sizeof(u.buf);
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(fds));
    memcpy(CMSG_DATA(cmsg), fds, sizeof(fds));
    CHECK(sendmsg(sv[0], &msg, 0) == 1, "sendmsg self-cycle + pipe");
    close(p[1]); /* Only sv[1]'s inbox now holds p[1]. */

    /* Fill sv[1] -> sv[0] so a blocking send on sv[1] sleeps. */
    static char fill[4096];
    while (send(sv[1], fill, sizeof(fill), MSG_DONTWAIT) > 0) {
    }
    CHECK(errno == EAGAIN, "fill sv[1] expected EAGAIN");

    struct gc_ctx ctx = {.sock = sv[1], .peer = sv[0], .pipe_r = p[0]};
    pthread_t th;
    CHECK(pthread_create(&th, NULL, gc_closer_thread, &ctx) == 0, "pthread_create gc");
    ssize_t n = send(sv[1], fill, sizeof(fill), 0);
    pthread_join(th, NULL);
    CHECK(n == (ssize_t)sizeof(fill), "blocked send expected %zu, got %zd", sizeof(fill), n);
    close(sv[0]);
    close(p[0]);
}

static void test_sched_setattr_atomic(void)
{
    pid_t pid = fork();
    CHECK(pid >= 0, "fork for sched_setattr");
    if (pid == 0) {
        struct rlimit rl = {.rlim_cur = 0, .rlim_max = 0};
        CHECK(setrlimit(RLIMIT_NICE, &rl) == 0, "setrlimit RLIMIT_NICE");
        /* Drop privileges so CAP_SYS_NICE does not bypass RLIMIT_NICE. */
        CHECK(setuid(65534) == 0, "setuid(65534)");

        struct sched_attr before = {0};
        CHECK(syscall(SYS_sched_getattr, 0, &before, sizeof(before), 0) == 0, "sched_getattr");
        CHECK(before.sched_policy == SCHED_OTHER && before.sched_flags == 0,
              "unexpected initial policy=%u flags=%llu", before.sched_policy,
              (unsigned long long)before.sched_flags);

        struct sched_attr req = {0};
        req.size = sizeof(req);
        req.sched_policy = SCHED_BATCH;
        req.sched_flags = SCHED_FLAG_RESET_ON_FORK;
        req.sched_nice = -5;
        errno = 0;
        CHECK(syscall(SYS_sched_setattr, 0, &req, 0) == -1 && errno == EPERM,
              "sched_setattr exceeding RLIMIT_NICE must fail with EPERM");

        struct sched_attr after = {0};
        CHECK(syscall(SYS_sched_getattr, 0, &after, sizeof(after), 0) == 0,
              "sched_getattr after fail");
        CHECK(after.sched_policy == SCHED_OTHER && after.sched_flags == 0,
              "sched_setattr mutated policy=%u flags=%llu on failure", after.sched_policy,
              (unsigned long long)after.sched_flags);
        _exit(0);
    }
    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid sched_setattr");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "sched_setattr child failed: %d", status);
}

static void run_child(const char *name, void (*fn)(void))
{
    pid_t pid = fork();
    CHECK(pid >= 0, "fork for %s", name);
    if (pid == 0) {
        fn();
        _exit(0);
    }
    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid, "waitpid %s", name);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "%s child failed: %d", name, status);
}

static void set_rlimit(int res, rlim_t val, const char *what)
{
    struct rlimit rl = {.rlim_cur = val, .rlim_max = val};
    CHECK(setrlimit(res, &rl) == 0, "%s", what);
}

static void check_deadline_eperm(const char *what)
{
    struct sched_attr dl = {0};
    dl.size = sizeof(dl);
    dl.sched_policy = SCHED_DEADLINE;
    dl.sched_runtime = 10000;
    dl.sched_deadline = 20000;
    dl.sched_period = 20000;
    errno = 0;
    CHECK(syscall(SYS_sched_setattr, 0, &dl, 0) == -1 && errno == EPERM, "%s", what);
}

static void rtprio_zero(void)
{
    /* Default RLIMIT_RTPRIO is 0: unprivileged RT transitions fail with EPERM. */
    set_rlimit(RLIMIT_RTPRIO, 0, "setrlimit RTPRIO=0");
    CHECK(setuid(65534) == 0, "setuid(65534)");

    struct sched_param sp = {.sched_priority = 1};
    errno = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == -1 && errno == EPERM,
          "RTPRIO=0 FIFO must EPERM");
    errno = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == -1 && errno == EPERM,
          "RTPRIO=0 RR must EPERM");
    struct sched_attr attr = {0};
    attr.size = sizeof(attr);
    attr.sched_policy = SCHED_FIFO;
    attr.sched_priority = 1;
    errno = 0;
    CHECK(syscall(SYS_sched_setattr, 0, &attr, 0) == -1 && errno == EPERM,
          "RTPRIO=0 sched_setattr FIFO must EPERM");
}

static void rtprio_from_sched_idle_denied(void)
{
    /* Leaving SCHED_IDLE into SCHED_FIFO still requires RLIMIT_NICE for task_nice(p) on Linux 6.1. */
    set_rlimit(RLIMIT_NICE, 0, "setrlimit NICE=0");
    set_rlimit(RLIMIT_RTPRIO, 10, "setrlimit RTPRIO=10");
    struct sched_param sp = {.sched_priority = 0};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_IDLE, &sp) == 0, "enter SCHED_IDLE");
    CHECK(setuid(65534) == 0, "setuid(65534)");

    sp.sched_priority = 10;
    errno = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == -1 && errno == EPERM,
          "SCHED_IDLE->FIFO with RLIMIT_NICE=0 must EPERM");
    CHECK(syscall(SYS_sched_getscheduler, 0) == SCHED_IDLE, "policy remains SCHED_IDLE");
}

static void rtprio_within_limit(void)
{
    set_rlimit(RLIMIT_NICE, 20, "setrlimit NICE=20");
    set_rlimit(RLIMIT_RTPRIO, 10, "setrlimit RTPRIO=10");
    /* Enter SCHED_IDLE before dropping privileges so we also cover SCHED_IDLE -> SCHED_FIFO. */
    struct sched_param sp = {.sched_priority = 0};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_IDLE, &sp) == 0, "enter SCHED_IDLE");
    CHECK(setuid(65534) == 0, "setuid(65534)");

    /* Leaving SCHED_IDLE into SCHED_FIFO succeeds when both RLIMIT_NICE and RLIMIT_RTPRIO permit. */
    sp.sched_priority = 10;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == 0, "SCHED_IDLE->FIFO prio 10");
    CHECK(syscall(SYS_sched_getscheduler, 0) == SCHED_FIFO, "getscheduler FIFO");
    struct sched_param got = {0};
    CHECK(syscall(SYS_sched_getparam, 0, &got) == 0 && got.sched_priority == 10, "getparam 10");

    /* Raising above RLIMIT_RTPRIO via sched_setparam fails with EPERM. */
    sp.sched_priority = 11;
    errno = 0;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == -1 && errno == EPERM, "setparam 11 must EPERM");

    /* Switching to SCHED_RR within limit succeeds. */
    sp.sched_priority = 5;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == 0, "FIFO->RR prio 5");
    CHECK(syscall(SYS_sched_getscheduler, 0) == SCHED_RR, "getscheduler RR");

    /* sched_setattr within limit succeeds; above limit fails without mutating policy/prio. */
    struct sched_attr attr = {0};
    attr.size = sizeof(attr);
    attr.sched_policy = SCHED_FIFO;
    attr.sched_priority = 8;
    CHECK(syscall(SYS_sched_setattr, 0, &attr, 0) == 0, "sched_setattr FIFO 8");
    attr.sched_policy = SCHED_RR;
    attr.sched_priority = 11;
    errno = 0;
    CHECK(syscall(SYS_sched_setattr, 0, &attr, 0) == -1 && errno == EPERM,
          "sched_setattr RR 11 must EPERM");
    struct sched_attr after = {0};
    CHECK(syscall(SYS_sched_getattr, 0, &after, sizeof(after), 0) == 0, "sched_getattr");
    CHECK(after.sched_policy == SCHED_FIFO && after.sched_priority == 8,
          "failed sched_setattr mutated policy=%u prio=%u", after.sched_policy,
          after.sched_priority);

    /* SCHED_DEADLINE always requires CAP_SYS_NICE regardless of RLIMIT_RTPRIO. */
    check_deadline_eperm("SCHED_DEADLINE without CAP_SYS_NICE must EPERM");
}

static void rtprio_lowering_and_keep_flags(void)
{
    /* Start as SCHED_FIFO prio 20 while privileged, then drop privileges with RLIMIT_RTPRIO=0. */
    struct sched_param sp = {.sched_priority = 20};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == 0, "root FIFO 20");
    set_rlimit(RLIMIT_RTPRIO, 0, "setrlimit RTPRIO=0");
    CHECK(setuid(65534) == 0, "setuid(65534)");

    /* Lowering priority under the same policy is allowed even with RLIMIT_RTPRIO=0. */
    sp.sched_priority = 15;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == 0, "setparam lower 20->15");
    sp.sched_priority = 10;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == 0, "setscheduler lower 15->10");

    /* Raising priority above current (and above 0) fails with EPERM. */
    sp.sched_priority = 12;
    errno = 0;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == -1 && errno == EPERM,
          "setparam raise 10->12 with RTPRIO=0 must EPERM");

    /* Changing RT policy (FIFO->RR) with RLIMIT_RTPRIO=0 fails with EPERM even when lowering. */
    sp.sched_priority = 5;
    errno = 0;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == -1 && errno == EPERM,
          "FIFO->RR with RTPRIO=0 must EPERM");

    /* sched_setattr KEEP_POLICY|KEEP_PARAMS updating uclamp succeeds on an existing RT task. */
    struct sched_attr keep = {0};
    keep.size = sizeof(keep);
    keep.sched_flags = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN;
    keep.sched_util_min = 256;
    CHECK(syscall(SYS_sched_setattr, 0, &keep, 0) == 0,
          "sched_setattr KEEP_POLICY|KEEP_PARAMS on RT task with RTPRIO=0");
    struct sched_attr got = {0};
    CHECK(syscall(SYS_sched_getattr, 0, &got, sizeof(got), 0) == 0, "sched_getattr after keep");
    CHECK(got.sched_policy == SCHED_FIFO && got.sched_priority == 10 && got.sched_util_min == 256,
          "unexpected state after keep: policy=%u prio=%u util_min=%u", got.sched_policy,
          got.sched_priority, got.sched_util_min);

    /* sched_setattr KEEP_PARAMS with same policy adding RESET_ON_FORK also succeeds. */
    struct sched_attr reset = {0};
    reset.size = sizeof(reset);
    reset.sched_policy = SCHED_FIFO;
    reset.sched_flags = SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_RESET_ON_FORK;
    CHECK(syscall(SYS_sched_setattr, 0, &reset, 0) == 0,
          "sched_setattr KEEP_PARAMS|RESET_ON_FORK on RT task with RTPRIO=0");
    memset(&got, 0, sizeof(got));
    CHECK(syscall(SYS_sched_getattr, 0, &got, sizeof(got), 0) == 0, "sched_getattr after reset");
    CHECK(got.sched_policy == SCHED_FIFO && got.sched_priority == 10 &&
              (got.sched_flags & SCHED_FLAG_RESET_ON_FORK) != 0,
          "unexpected state after reset: policy=%u prio=%u flags=%llu", got.sched_policy,
          got.sched_priority, (unsigned long long)got.sched_flags);
}

static void rtprio_policy_switch(void)
{
    /* Start as SCHED_FIFO prio 20 with RLIMIT_RTPRIO=5, then drop privileges. */
    struct sched_param sp = {.sched_priority = 20};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == 0, "root FIFO 20");
    set_rlimit(RLIMIT_RTPRIO, 5, "setrlimit RTPRIO=5");
    CHECK(setuid(65534) == 0, "setuid(65534)");

    /* With RTPRIO=5 (>0), switching FIFO->RR while lowering 20->15 is allowed on Linux 6.1. */
    sp.sched_priority = 15;
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_RR, &sp) == 0, "FIFO->RR lower 20->15");
    sp.sched_priority = 16;
    errno = 0;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == -1 && errno == EPERM,
          "raise 15->16 above RTPRIO=5 must EPERM");
    sp.sched_priority = 4;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == 0, "lower 15->4");
    sp.sched_priority = 5;
    CHECK(syscall(SYS_sched_setparam, 0, &sp) == 0, "raise 4->5 within RTPRIO=5");
}

static void rtprio_infinity_and_cross_process(void)
{
    /* RLIM_INFINITY allows up to priority 99, while SCHED_DEADLINE still fails with EPERM. */
    set_rlimit(RLIMIT_RTPRIO, RLIM_INFINITY, "setrlimit RTPRIO=INF");
    CHECK(setuid(65534) == 0, "setuid(65534)");
    struct sched_param sp = {.sched_priority = 99};
    CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &sp) == 0, "RTPRIO=INF FIFO 99");
    check_deadline_eperm("RTPRIO=INF SCHED_DEADLINE must still EPERM");

    /* Cross-process check uses the target's RLIMIT_RTPRIO, not the caller's. */
    int pfd[2];
    CHECK(pipe(pfd) == 0, "pipe");
    pid_t target = fork();
    CHECK(target >= 0, "fork target");
    if (target == 0) {
        close(pfd[0]);
        set_rlimit(RLIMIT_RTPRIO, 7, "target setrlimit RTPRIO=7");
        sp.sched_priority = 0;
        CHECK(syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &sp) == 0, "target SCHED_OTHER");
        char ready = 'R';
        CHECK(write(pfd[1], &ready, 1) == 1, "signal ready");
        pause();
        _exit(0);
    }
    close(pfd[1]);
    char ready = 0;
    CHECK(read(pfd[0], &ready, 1) == 1 && ready == 'R', "wait target ready");
    close(pfd[0]);

    /* Lower caller's own RLIMIT_RTPRIO to 0; target still has 7. */
    set_rlimit(RLIMIT_RTPRIO, 0, "caller setrlimit RTPRIO=0");
    sp.sched_priority = 7;
    CHECK(syscall(SYS_sched_setscheduler, target, SCHED_FIFO, &sp) == 0,
          "cross-process FIFO 7 within target RTPRIO=7");
    sp.sched_priority = 8;
    errno = 0;
    CHECK(syscall(SYS_sched_setscheduler, target, SCHED_FIFO, &sp) == -1 && errno == EPERM,
          "cross-process FIFO 8 above target RTPRIO=7 must EPERM");
    kill(target, SIGKILL);
    waitpid(target, NULL, 0);
}

static void test_sched_rlimit_rtprio(void)
{
    run_child("rtprio_zero", rtprio_zero);
    run_child("rtprio_from_sched_idle_denied", rtprio_from_sched_idle_denied);
    run_child("rtprio_within_limit", rtprio_within_limit);
    run_child("rtprio_lowering_and_keep_flags", rtprio_lowering_and_keep_flags);
    run_child("rtprio_policy_switch", rtprio_policy_switch);
    run_child("rtprio_infinity_and_cross_process", rtprio_infinity_and_cross_process);
}

static int pidns_child(void *arg)
{
    int pfd = *(int *)arg;
    CHECK(syscall(SYS_pidfd_send_signal, pfd, SIGUSR1, NULL, 0) == 0, "pidfd_send_signal");
    return 0;
}

static void test_cross_pidns_si_pid(void)
{
    struct sigaction sa = {0};
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction SIGUSR1");
    got_usr1_pid = -1;

    int pfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
    CHECK(pfd >= 0, "pidfd_open");

    static char stack[65536] __attribute__((aligned(16)));
    pid_t child = clone(pidns_child, stack + sizeof(stack), CLONE_NEWPID | SIGCHLD, &pfd);
    CHECK(child > 0, "clone CLONE_NEWPID");
    int status = 0;
    pid_t w;
    while ((w = waitpid(child, &status, 0)) == -1 && errno == EINTR) {
    }
    CHECK(w == child, "waitpid pidns child");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "pidns child status %d", status);
    CHECK(got_usr1_pid == child, "expected si_pid=%d from child pidns, got %d", (int)child,
          (int)got_usr1_pid);
    close(pfd);
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "--after-exec") == 0) {
        errno = 0;
        long r = syscall(SYS_restart_syscall);
        CHECK(r == -1 && errno == EINTR, "restart_syscall after execve must return EINTR");
        return 0;
    }

    test_kill_int_min();
    puts("ok kill_int_min");
    fflush(stdout);
    test_sigaltstack_uoss_fault();
    puts("ok sigaltstack_uoss_fault");
    fflush(stdout);
    test_restart_cleared(argv[0]);
    puts("ok restart_cleared");
    fflush(stdout);
    test_unix_partial_send_no_sigpipe();
    puts("ok unix_partial_send_no_sigpipe");
    fflush(stdout);
    test_unix_recvmsg_fault_no_fd_leak();
    puts("ok unix_recvmsg_fault_no_fd_leak");
    fflush(stdout);
    test_unix_blocked_syscall_pinned_during_gc();
    puts("ok unix_blocked_syscall_pinned_during_gc");
    fflush(stdout);
    test_sched_setattr_atomic();
    puts("ok sched_setattr_atomic");
    fflush(stdout);
    test_sched_rlimit_rtprio();
    puts("ok sched_rlimit_rtprio");
    fflush(stdout);
    test_cross_pidns_si_pid();
    puts("ok cross_pidns_si_pid");
    fflush(stdout);

    puts("all ok");
    return 0;
}
