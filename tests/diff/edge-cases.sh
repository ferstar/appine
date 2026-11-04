# Kernel corners: another process truncating a mapped file (SIGBUS, which must not take down the
# whole app), MADV_DONTNEED, large PROT_NONE reservations, nanosleep sleeping only the remaining
# time after a stop and continue, waiters getting EOWNERDEAD after a process holding a robust
# lock exits, EPOLLEXCLUSIVE argument checks, jumping to an unaligned address (SIGBUS), timeouts
# too long for Linux to represent saturating
set -e
mkdir -p /tmp/diff-edges
cd /tmp/diff-edges
cat > edges.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static sigjmp_buf env;
static volatile int last_sig, last_code;
static void *volatile last_addr;

static void on_fault(int sig, siginfo_t *si, void *ctx)
{
    (void)ctx;
    last_sig = sig;
    last_code = si->si_code;
    last_addr = si->si_addr;
    siglongjmp(env, 1);
}

/* Read one byte and print the result; on a fault print the signal and si_code */
static void probe(const char *label, volatile char *p)
{
    if (sigsetjmp(env, 1)) {
        printf(" %s:%s/%d", label, last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code);
        return;
    }
    int v = *p;
    printf(" %s:%d", label, v);
}

static void run_child(void (*fn)(void))
{
    pid_t pid = fork();
    if (pid == 0) {
        fn();
        _exit(0);
    }
    waitpid(pid, NULL, 0);
}

static void child_ftruncate(void)
{
    int fd = open("data", O_RDWR);
    ftruncate(fd, 4096 + 10);
}

static void child_truncate(void) { truncate("data", 0); }

static void child_otrunc(void) { close(open("data", O_WRONLY | O_TRUNC)); }

static long ms_since(struct timespec *t0)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0->tv_sec) * 1000 + (t.tv_nsec - t0->tv_nsec) / 1000000;
}

static void on_usr1(int sig) { (void)sig; }

/* Sends signal `sig` to this process in 100 ms */
static pid_t poke_soon(int sig)
{
    pid_t parent = getpid(), poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, sig);
        _exit(0);
    }
    return poker;
}

int main(void)
{
    struct sigaction sa = {.sa_sigaction = on_fault, .sa_flags = SA_SIGINFO};
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    long page = 4096;

    /* Another process truncates a mapped file: pages beyond the new end get SIGBUS */
    void (*truncators[])(void) = {child_ftruncate, child_truncate, child_otrunc};
    const char *names[] = {"ftruncate", "truncate", "O_TRUNC"};
    for (int i = 0; i < 3; i++) {
        int fd = open("data", O_RDWR | O_CREAT | O_TRUNC, 0644);
        char buf[3 * 4096];
        memset(buf, 'x', sizeof buf);
        write(fd, buf, sizeof buf);
        char *shared = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        char *private = mmap(NULL, 3 * page, PROT_READ, MAP_PRIVATE, fd, 0);
        printf("%s before:", names[i]);
        probe("shared2", shared + 2 * page);
        probe("private2", private + 2 * page);
        run_child(truncators[i]);
        printf("\n%s after:", names[i]);
        for (int p = 0; p < 3; p++) {
            char label[16];
            snprintf(label, sizeof label, "shared%d", p);
            probe(label, shared + p * page);
            snprintf(label, sizeof label, "private%d", p);
            probe(label, private + p * page);
        }
        printf("\n");
        munmap(shared, 3 * page);
        munmap(private, 3 * page);
        close(fd);
    }

    /* MADV_DONTNEED: private anonymous memory reads back as zeros, shared memory is unchanged */
    char *anon = mmap(NULL, 8 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *shm = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(anon, 7, 8 * page);
    memset(shm, 9, 2 * page);
    madvise(anon + page, 5 * page, MADV_DONTNEED);
    madvise(shm, 2 * page, MADV_DONTNEED);
    printf("dontneed %d %d %d %d shared %d\n", anon[0], anon[page], anon[6 * page - 1], anon[6 * page],
           shm[page]);

    /* An 8 GB PROT_NONE reservation, with one page in the middle made writable */
    char *reserve = mmap(NULL, 1L << 33, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    char *mid = reserve + (1L << 32);
    mprotect(mid, page, PROT_READ | PROT_WRITE);
    mid[0] = 5;
    printf("reserve");
    probe("mid", mid);
    probe("next", mid + page);
    mprotect(mid, page, PROT_NONE);
    probe("protected", mid);
    mprotect(mid, page, PROT_READ);
    probe("readable", mid);
    munmap(reserve, 1L << 33);
    probe("unmapped", mid);
    printf("\n");

    /* Reading a procfs file again from offset 0 gives the current contents (seq_file): after a
     * rename, seek back to 0, and pread at 0 */
    {
        int st = open("/proc/self/status", O_RDONLY);
        char sb[64] = "";
        read(st, sb, sizeof sb - 1);
        prctl(PR_SET_NAME, "renamed");
        lseek(st, 0, SEEK_SET);
        memset(sb, 0, sizeof sb);
        read(st, sb, sizeof sb - 1);
        printf("status after seek: %.*s\n", (int)strcspn(sb, "\n"), sb);
        prctl(PR_SET_NAME, "again");
        memset(sb, 0, sizeof sb);
        pread(st, sb, sizeof sb - 1, 0);
        printf("status by pread: %.*s\n", (int)strcspn(sb, "\n"), sb);
        close(st);
    }

    /* Stop and continue: nanosleep sleeps on for the remaining time, not the full time again */
    int pipefd[2];
    pipe(pipefd);
    pid_t sleeper = fork();
    if (sleeper == 0) {
        struct timespec t0, req = {0, 600 * 1000000L};
        clock_gettime(CLOCK_MONOTONIC, &t0);
        nanosleep(&req, NULL);
        long ms = ms_since(&t0);
        write(pipefd[1], &ms, sizeof ms);
        _exit(0);
    }
    usleep(150000);
    kill(sleeper, SIGSTOP);
    usleep(150000);
    kill(sleeper, SIGCONT);
    long slept;
    read(pipefd[0], &slept, sizeof slept);
    waitpid(sleeper, NULL, 0);
    printf("stopped sleep %s\n", slept >= 600 && slept < 850 ? "kept its deadline" : "restarted");

    /* Stop and continue: poll (the remaining time is written back to the timespec) and futex
       (restart_block) go on waiting until the original deadline, epoll_wait is EINTR (signal(7);
       the same after a cgroup is frozen and thawed) */
    for (int which = 0; which < 3; which++) {
        pid_t waiter = fork();
        if (waiter == 0) {
            int q[2];
            pipe(q);
            struct timespec t0;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            long r;
            if (which == 0) {
                struct pollfd p = {q[0], POLLIN, 0};
                r = poll(&p, 1, 600);
            } else if (which == 2) {
                static int word;
                struct timespec rel = {0, 600 * 1000000L};
                r = syscall(SYS_futex, &word, 128 /* FUTEX_WAIT | FUTEX_PRIVATE_FLAG */, 0, &rel, NULL, 0);
            } else {
                int ep = epoll_create1(0);
                struct epoll_event ev = {.events = EPOLLIN};
                epoll_ctl(ep, EPOLL_CTL_ADD, q[0], &ev);
                r = epoll_wait(ep, &ev, 1, 600);
            }
            long out[3] = {r, r < 0 ? errno : 0, ms_since(&t0)};
            write(pipefd[1], out, sizeof out);
            _exit(0);
        }
        usleep(150000);
        kill(waiter, SIGSTOP);
        usleep(150000);
        kill(waiter, SIGCONT);
        long out[3];
        read(pipefd[0], out, sizeof out);
        waitpid(waiter, NULL, 0);
        printf("stopped %s: %ld %s %s\n", which == 2 ? "futex" : which ? "epoll_wait" : "poll", out[0],
               out[1] ? strerror(out[1]) : "", out[2] >= 600 && out[2] < 850 ? "kept its deadline"
               : out[2] < 600 ? "returned early" : "restarted");
    }

    /* With a handler it returns EINTR, and rem is the remaining time */
    struct sigaction su = {.sa_handler = on_usr1};
    sigaction(SIGUSR1, &su, NULL);
    pid_t parent = getpid();
    pid_t poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    struct timespec req = {2, 0}, rem = {0, 0};
    int r = nanosleep(&req, &rem);
    printf("interrupted sleep %d %s rem<2s %d\n", r, strerror(errno), rem.tv_sec < 2 && (rem.tv_sec || rem.tv_nsec));
    waitpid(poker, NULL, 0);

    /* A futex wait with a timeout that a handler interrupts is EINTR, even with SA_RESTART
     * (restart_block) */
    su.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &su, NULL);
    poker = fork();
    if (poker == 0) {
        usleep(100000);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    static int fword;
    struct timespec ft = {2, 0};
    r = syscall(SYS_futex, &fword, 128, 0, &ft, NULL, 0);
    printf("interrupted futex with SA_RESTART %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);

    /* A process exits while holding a process-shared robust lock: the waiter gets EOWNERDEAD */
    pthread_mutex_t *m = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(m, &ma);
    pid_t owner = fork();
    if (owner == 0) {
        pthread_mutex_lock(m);
        _exit(0);
    }
    waitpid(owner, NULL, 0);
    alarm(10);
    r = pthread_mutex_lock(m);
    printf("robust lock %s\n", r == EOWNERDEAD ? "EOWNERDEAD" : strerror(r));
    if (r == EOWNERDEAD)
        pthread_mutex_consistent(m);
    printf("unlock %d\n", pthread_mutex_unlock(m));
    alarm(0);

    /* EPOLLEXCLUSIVE can be used only when adding, not on an epoll, and not with other flags */
    int ep = epoll_create1(0), ep2 = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLEXCLUSIVE};
    printf("exclusive add %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &ev));
    printf("exclusive mod %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN;
    printf("mod exclusive interest %d %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, pipefd[0], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT;
    printf("exclusive oneshot %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[1], &ev), errno);
    ev.events = EPOLLIN | EPOLLEXCLUSIVE;
    printf("exclusive epoll %d %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, ep2, &ev), errno);

    /* Timeouts too long for Linux to represent saturate at about 292 years (GNU `sleep infinity`
     * sleeps for LONG_MAX seconds): the waits go on until a handler interrupts them, the timers
     * report the saturated time left, and such a socket timeout reads back as none */
    const struct timespec huge = {LONG_MAX, 999999999};
    struct timespec t = huge, left = {0, 0};
    su.sa_flags = 0;
    sigaction(SIGUSR1, &su, NULL);
    poker = poke_soon(SIGUSR1);
    r = syscall(SYS_nanosleep, &t, &left);
    printf("huge nanosleep %d %s left>9e9s %d\n", r, strerror(errno), left.tv_sec > 9000000000L);
    waitpid(poker, NULL, 0);
    poker = poke_soon(SIGUSR1);
    r = clock_nanosleep(CLOCK_REALTIME, 0, &t, &left);
    printf("huge clock_nanosleep %s left>9e9s %d\n", strerror(r), left.tv_sec > 9000000000L);
    waitpid(poker, NULL, 0);
    poker = poke_soon(SIGUSR1);
    r = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, NULL);
    printf("huge clock_nanosleep abs %s\n", strerror(r));
    waitpid(poker, NULL, 0);
    static int hword;
    poker = poke_soon(SIGUSR1);
    r = syscall(SYS_futex, &hword, 128, 0, &t, NULL, 0);
    printf("huge futex %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    int hq[2];
    pipe(hq);
    struct pollfd hp = {hq[0], POLLIN, 0};
    poker = poke_soon(SIGUSR1);
    r = ppoll(&hp, 1, &t, NULL);
    printf("huge ppoll %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(hq[0], &rd);
    t = huge;
    poker = poke_soon(SIGUSR1);
    r = pselect(hq[0] + 1, &rd, NULL, NULL, &t, NULL);
    printf("huge pselect %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    t = huge;
    poker = poke_soon(SIGUSR1);
    r = syscall(441 /* epoll_pwait2 */, ep2, &ev, 1, &t, NULL, 0);
    printf("huge epoll_pwait2 %d %s\n", r, strerror(errno));
    waitpid(poker, NULL, 0);
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    sigprocmask(SIG_BLOCK, &usr2, NULL);
    t = huge;
    poker = poke_soon(SIGUSR2);
    r = sigtimedwait(&usr2, NULL, &t);
    printf("huge sigtimedwait %s\n", r == SIGUSR2 ? "SIGUSR2" : strerror(errno));
    waitpid(poker, NULL, 0);
    sigprocmask(SIG_UNBLOCK, &usr2, NULL);

    struct itimerval iv = {{0, 0}, {LONG_MAX, 999999}}, oiv;
    printf("huge setitimer %d", setitimer(ITIMER_REAL, &iv, NULL));
    getitimer(ITIMER_REAL, &oiv);
    printf(" left>9e9s %d\n", oiv.it_value.tv_sec > 9000000000L);
    iv.it_value.tv_sec = iv.it_value.tv_usec = 0;
    setitimer(ITIMER_REAL, &iv, NULL);
    struct itimerspec its = {huge, huge}, oits;
    timer_t tid;
    struct sigevent sev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGUSR1};
    timer_create(CLOCK_MONOTONIC, &sev, &tid);
    printf("huge timer_settime %d", timer_settime(tid, 0, &its, NULL));
    timer_gettime(tid, &oits);
    printf(" left>9e9s %d interval>9e9s %d\n", oits.it_value.tv_sec > 9000000000L,
           oits.it_interval.tv_sec > 9000000000L);
    timer_delete(tid);
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    printf("huge timerfd_settime %d", timerfd_settime(tfd, 0, &its, NULL));
    timerfd_gettime(tfd, &oits);
    printf(" left>9e9s %d interval>9e9s %d\n", oits.it_value.tv_sec > 9000000000L,
           oits.it_interval.tv_sec > 9000000000L);

    int us[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, us);
    int socks[] = {us[0], socket(AF_INET, SOCK_DGRAM, 0)};
    for (int i = 0; i < 2; i++) {
        struct timeval tv = {LONG_MAX, 0}, got = {1, 1};
        socklen_t len = sizeof got;
        r = setsockopt(socks[i], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        getsockopt(socks[i], SOL_SOCKET, SO_RCVTIMEO, &got, &len);
        printf("huge SO_RCVTIMEO %s %d reads %ld.%06ld", i ? "inet" : "unix", r, (long)got.tv_sec,
               (long)got.tv_usec);
        char c;
        poker = poke_soon(SIGUSR1);
        r = recv(socks[i], &c, 1, 0);
        printf(" recv %d %s\n", r, strerror(errno));
        waitpid(poker, NULL, 0);
    }

    /* Jumping to an unaligned address: SIGBUS (BUS_ADRALN) on instruction fetch, si_addr is the
     * target. Through a call via a function pointer and through ret */
    uintptr_t targets[] = {1, 2, 0x1001};
    for (int i = 0; i < 3; i++) {
        for (int how = 0; how < 2; how++) {
            if (sigsetjmp(env, 1)) {
                printf("unaligned %s %#lx: %s/%d addr=%p\n", how ? "ret" : "call", (unsigned long)targets[i],
                       last_sig == SIGBUS ? "SIGBUS" : "SIGSEGV", last_code, last_addr);
                continue;
            }
            if (how)
                __asm__ volatile("mov x30, %0\n ret" ::"r"(targets[i]) : "x30");
            else
                ((void (*)(void))targets[i])();
        }
    }
    return 0;
}
C
gcc -O2 -o edges edges.c
./edges
