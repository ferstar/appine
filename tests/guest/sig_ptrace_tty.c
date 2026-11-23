/*
 * Regression tests for Package 2 (kernel/signal, kernel/ptrace, kernel/tty, kernel/time):
 * - group_stop notifies parent with CLD_STOPPED only after all threads in the group have stopped;
 * - SIGILL from an undefined instruction omits esr_context in the arm64 signal frame;
 * - rt_sigaction setting SIG_IGN discards pending signals in sibling CLONE_SIGHAND groups;
 * - rt_sigreturn rejects non-zero DAIF pstate bits and invalid/duplicate ESR_MAGIC records;
 * - settimeofday validates tv/tz arguments before EPERM, and clock_settime returns EPERM on CPU
 *   clocks;
 * - PTRACE_DETACH at an attach-time group stop re-enters group stop silently without duplicate
 *   CLD_STOPPED;
 * - PTRACE_SINGLESTEP over skipped syscall (-1), rt_sigreturn, and execve stops at syscall return
 *   rather than one instruction late;
 * - PTRACE_EVENT_EXIT is emitted for sibling threads killed by exit_group and for the leader
 *   killed by a non-leader execve;
 * - process_vm_readv/writev with non-empty local vector and zero remote bytes return 0 before PID
 *   lookup / access checks;
 * - TIOCSCTTY returns EPERM to a non-leader even when the terminal is already its ctty;
 * - TIOCSPGRP returns ESRCH for unknown pgid, checks EFAULT/EINVAL before ENOTTY, and returns
 *   ENOTTY (not EIO) for orphaned background callers;
 * - PTY slave writes clamp ONLCR and XTABS expansion to the 64 KiB output buffer limit;
 * - an exited (zombie) main thread's CPU clock remains readable and stopped until reaped.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            fprintf(stderr, "FAIL (%s:%d): ", __func__, __LINE__);                                 \
            fprintf(stderr, __VA_ARGS__);                                                          \
            fprintf(stderr, " (errno %d)\n", errno);                                               \
            _exit(1);                                                                              \
        }                                                                                          \
    } while (0)

#ifndef NT_PRSTATUS
#define NT_PRSTATUS 1
#endif
#ifndef NT_ARM_SYSTEM_CALL
#define NT_ARM_SYSTEM_CALL 0x404
#endif
#ifndef FPSIMD_MAGIC
#define FPSIMD_MAGIC 0x46508001U
#endif
#ifndef ESR_MAGIC
#define ESR_MAGIC 0x45535201U
#endif
#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif
#ifndef PTRACE_O_TRACECLONE
#define PTRACE_O_TRACECLONE 0x00000008
#endif
#ifndef PTRACE_O_TRACEEXEC
#define PTRACE_O_TRACEEXEC 0x00000010
#endif
#ifndef PTRACE_O_TRACEEXIT
#define PTRACE_O_TRACEEXIT 0x00000040
#endif
#ifndef PTRACE_EVENT_CLONE
#define PTRACE_EVENT_CLONE 3
#endif
#ifndef PTRACE_EVENT_EXEC
#define PTRACE_EVENT_EXEC 4
#endif
#ifndef PTRACE_EVENT_EXIT
#define PTRACE_EVENT_EXIT 6
#endif
#ifndef SYS_rt_sigreturn
#define SYS_rt_sigreturn 139
#endif
#ifndef SYS_settimeofday
#define SYS_settimeofday 170
#endif
#ifndef SYS_gettid
#define SYS_gettid 178
#endif
#ifndef SYS_process_vm_readv
#define SYS_process_vm_readv 270
#endif
#ifndef SYS_process_vm_writev
#define SYS_process_vm_writev 271
#endif

struct arm64_regs {
    uint64_t regs[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t pstate;
};

struct aarch64_ctx_hdr {
    uint32_t magic;
    uint32_t size;
};

static int get_regs(pid_t pid, struct arm64_regs *r)
{
    struct iovec iov = {.iov_base = r, .iov_len = sizeof(*r)};
    return (int)ptrace(PTRACE_GETREGSET, pid, (void *)(uintptr_t)NT_PRSTATUS, &iov);
}

static int set_regs(pid_t pid, const struct arm64_regs *r)
{
    struct iovec iov = {.iov_base = (void *)r, .iov_len = sizeof(*r)};
    return (int)ptrace(PTRACE_SETREGSET, pid, (void *)(uintptr_t)NT_PRSTATUS, &iov);
}

static int set_syscallno(pid_t pid, int nr)
{
    struct iovec iov = {.iov_base = &nr, .iov_len = sizeof(nr)};
    return (int)ptrace(PTRACE_SETREGSET, pid, (void *)(uintptr_t)NT_ARM_SYSTEM_CALL, &iov);
}

static void msleep(int ms)
{
    struct timespec ts = {.tv_sec = ms / 1000, .tv_nsec = (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

/* ---- 1. group_stop waits for all sibling threads before CLD_STOPPED ---- */

struct stop_shared {
    _Atomic int ready;
    _Atomic uint64_t ticks;
};

static void *spinner_thread(void *arg)
{
    struct stop_shared *sh = arg;
    atomic_store_explicit(&sh->ready, 1, memory_order_release);
    for (;;) {
        atomic_fetch_add_explicit(&sh->ticks, 1, memory_order_relaxed);
    }
    return NULL;
}

static void test_group_stop_waits_for_all_threads(void)
{
    struct stop_shared *sh = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                  MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(sh != MAP_FAILED, "mmap shared");
    atomic_init(&sh->ready, 0);
    atomic_init(&sh->ticks, 0);

    pid_t pid = fork();
    CHECK(pid >= 0, "fork");
    if (pid == 0) {
        pthread_t th;
        CHECK(pthread_create(&th, NULL, spinner_thread, sh) == 0, "pthread_create");
        while (atomic_load_explicit(&sh->ready, memory_order_acquire) == 0) {
            sched_yield();
        }
        for (;;) {
            pause();
        }
    }

    while (atomic_load_explicit(&sh->ready, memory_order_acquire) == 0) {
        msleep(1);
    }
    /* Ensure sibling thread is actively spinning before stopping the group. */
    uint64_t t0 = atomic_load_explicit(&sh->ticks, memory_order_relaxed);
    while (atomic_load_explicit(&sh->ticks, memory_order_relaxed) == t0) {
        msleep(1);
    }

    CHECK(kill(pid, SIGSTOP) == 0, "kill SIGSTOP");
    int st = 0;
    CHECK(waitpid(pid, &st, WUNTRACED) == pid, "waitpid WUNTRACED");
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, "expected SIGSTOP stop, got 0x%x", st);

    /* Once CLD_STOPPED / waitpid reports stopped, the spinning sibling must already be stopped. */
    uint64_t frozen = atomic_load_explicit(&sh->ticks, memory_order_acquire);
    msleep(40);
    uint64_t after = atomic_load_explicit(&sh->ticks, memory_order_acquire);
    CHECK(frozen == after, "sibling kept running after CLD_STOPPED (%llu -> %llu)",
          (unsigned long long)frozen, (unsigned long long)after);

    CHECK(kill(pid, SIGKILL) == 0, "kill SIGKILL");
    CHECK(waitpid(pid, &st, 0) == pid, "waitpid reap");
    munmap(sh, 4096);
}

/* ---- 2. SIGILL from undefined instruction omits esr_context ---- */

static volatile sig_atomic_t sigill_seen = 0;
static volatile sig_atomic_t sigill_had_fpsimd = 0;
static volatile sig_atomic_t sigill_had_esr = 0;

static void on_sigill(int sig, siginfo_t *info, void *uctx)
{
    (void)sig;
    (void)info;
    ucontext_t *uc = uctx;
    sigill_seen = 1;
    const uint8_t *p = (const uint8_t *)uc->uc_mcontext.__reserved;
    size_t off = 0;
    while (off + sizeof(struct aarch64_ctx_hdr) <= sizeof(uc->uc_mcontext.__reserved)) {
        struct aarch64_ctx_hdr h;
        memcpy(&h, p + off, sizeof(h));
        if (h.magic == 0 && h.size == 0) {
            break;
        }
        if (h.magic == FPSIMD_MAGIC) {
            sigill_had_fpsimd = 1;
        } else if (h.magic == ESR_MAGIC) {
            sigill_had_esr = 1;
        }
        if (h.size < sizeof(h)) {
            break;
        }
        off += h.size;
    }
    uc->uc_mcontext.pc += 4;
}

static void test_sigill_omits_esr_context(void)
{
    struct sigaction sa = {0}, old = {0};
    sa.sa_sigaction = on_sigill;
    sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGILL, &sa, &old) == 0, "sigaction SIGILL");

    sigill_seen = 0;
    sigill_had_fpsimd = 0;
    sigill_had_esr = 0;
    __asm__ volatile(".inst 0x00000000" ::: "memory");

    CHECK(sigaction(SIGILL, &old, NULL) == 0, "restore SIGILL");
    CHECK(sigill_seen == 1, "SIGILL handler did not run");
    CHECK(sigill_had_fpsimd == 1, "SIGILL frame missing fpsimd_context");
    CHECK(sigill_had_esr == 0, "SIGILL from undefined insn must omit esr_context");
}

/* ---- 3. rt_sigaction SIG_IGN discards pending signals across CLONE_SIGHAND groups ---- */

struct sighand_shared {
    _Atomic int state;
    _Atomic int still_pending;
};

static int sighand_child_fn(void *arg)
{
    struct sighand_shared *sh = arg;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
        _exit(2);
    }
    atomic_store_explicit(&sh->state, 1, memory_order_release);
    /* Wait for parent to send SIGUSR1 and then ignore it. */
    while (atomic_load_explicit(&sh->state, memory_order_acquire) < 2) {
        msleep(1);
    }
    sigset_t pend;
    sigemptyset(&pend);
    if (sigpending(&pend) != 0) {
        _exit(3);
    }
    atomic_store_explicit(&sh->still_pending, sigismember(&pend, SIGUSR1) ? 1 : 0,
                          memory_order_release);
    _exit(0);
}

static void test_sigaction_clone_sighand_discard(void)
{
    struct sighand_shared sh;
    atomic_init(&sh.state, 0);
    atomic_init(&sh.still_pending, -1);

    size_t stack_sz = 64 * 1024;
    void *stack = mmap(NULL, stack_sz, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(stack != MAP_FAILED, "mmap clone stack");

    /* Reset SIGUSR1 to a caught handler so parent can send it while blocked in child. */
    struct sigaction sa = {0}, old = {0};
    sa.sa_handler = SIG_DFL;
    CHECK(sigaction(SIGUSR1, &sa, &old) == 0, "sigaction SIGUSR1 DFL");

    pid_t child = clone(sighand_child_fn, (char *)stack + stack_sz,
                        CLONE_VM | CLONE_SIGHAND | SIGCHLD, &sh);
    CHECK(child > 0, "clone CLONE_VM|CLONE_SIGHAND");

    while (atomic_load_explicit(&sh.state, memory_order_acquire) < 1) {
        msleep(1);
    }
    CHECK(kill(child, SIGUSR1) == 0, "kill(child, SIGUSR1)");
    msleep(5);

    /* Parent sets SIGUSR1 to SIG_IGN; child shares sighand, so child's pending SIGUSR1 must go. */
    sa.sa_handler = SIG_IGN;
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction SIGUSR1 IGN");

    atomic_store_explicit(&sh.state, 2, memory_order_release);
    int st = 0;
    CHECK(waitpid(child, &st, 0) == child, "waitpid sighand child");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "sighand child exit 0x%x", st);
    CHECK(atomic_load_explicit(&sh.still_pending, memory_order_acquire) == 0,
          "pending SIGUSR1 in sibling CLONE_SIGHAND group was not discarded");

    CHECK(sigaction(SIGUSR1, &old, NULL) == 0, "restore SIGUSR1");
    munmap(stack, stack_sz);
}

/* ---- 4. rt_sigreturn rejects DAIF bits and bogus/duplicate ESR_MAGIC ---- */

static sigjmp_buf sigret_jmp;
static volatile sig_atomic_t sigret_mode = 0;
static volatile sig_atomic_t sigret_got_segv = 0;

static void on_sigret_segv(int sig)
{
    (void)sig;
    sigret_got_segv = 1;
    siglongjmp(sigret_jmp, 1);
}

static void on_sigret_usr1(int sig, siginfo_t *info, void *uctx)
{
    (void)sig;
    (void)info;
    ucontext_t *uc = uctx;
    if (sigret_mode == 1) {
        /* Set IRQ mask bit (I = 1<<7 = 0x80) in pstate. */
        uc->uc_mcontext.pstate |= 0x80ULL;
    } else if (sigret_mode == 2) {
        /* Append an oversized ESR_MAGIC record (size 32 != 16) after fpsimd_context. */
        uint8_t *p = (uint8_t *)uc->uc_mcontext.__reserved;
        struct aarch64_ctx_hdr h;
        memcpy(&h, p, sizeof(h));
        if (h.magic == FPSIMD_MAGIC && h.size + 48 <= sizeof(uc->uc_mcontext.__reserved)) {
            struct aarch64_ctx_hdr bad = {.magic = ESR_MAGIC, .size = 32};
            struct aarch64_ctx_hdr end = {.magic = 0, .size = 0};
            memcpy(p + h.size, &bad, sizeof(bad));
            memcpy(p + h.size + 32, &end, sizeof(end));
        }
    } else if (sigret_mode == 3) {
        /* Append duplicate valid-sized ESR_MAGIC records after fpsimd_context. */
        uint8_t *p = (uint8_t *)uc->uc_mcontext.__reserved;
        struct aarch64_ctx_hdr h;
        memcpy(&h, p, sizeof(h));
        if (h.magic == FPSIMD_MAGIC && h.size + 48 <= sizeof(uc->uc_mcontext.__reserved)) {
            struct aarch64_ctx_hdr esr = {.magic = ESR_MAGIC, .size = 16};
            struct aarch64_ctx_hdr end = {.magic = 0, .size = 0};
            memcpy(p + h.size, &esr, sizeof(esr));
            memcpy(p + h.size + 16, &esr, sizeof(esr));
            memcpy(p + h.size + 32, &end, sizeof(end));
        }
    }
}

static void test_sigreturn_validation(void)
{
    struct sigaction sa_segv = {0}, old_segv = {0};
    sa_segv.sa_handler = on_sigret_segv;
    CHECK(sigaction(SIGSEGV, &sa_segv, &old_segv) == 0, "sigaction SIGSEGV");

    struct sigaction sa_usr1 = {0}, old_usr1 = {0};
    sa_usr1.sa_sigaction = on_sigret_usr1;
    sa_usr1.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGUSR1, &sa_usr1, &old_usr1) == 0, "sigaction SIGUSR1");

    for (int mode = 1; mode <= 3; mode++) {
        sigret_mode = mode;
        sigret_got_segv = 0;
        if (sigsetjmp(sigret_jmp, 1) == 0) {
            raise(SIGUSR1);
        }
        CHECK(sigret_got_segv == 1, "rt_sigreturn mode %d must force SIGSEGV", mode);
    }

    CHECK(sigaction(SIGUSR1, &old_usr1, NULL) == 0, "restore SIGUSR1");
    CHECK(sigaction(SIGSEGV, &old_segv, NULL) == 0, "restore SIGSEGV");
}

/* ---- 5. settimeofday validation and clock_settime on CPU clocks ---- */

static void test_time_setting_errors(void)
{
    struct timeval tv;
    struct timezone tz = {0, 0};

    tv.tv_sec = 100;
    tv.tv_usec = 1000000;
    errno = 0;
    CHECK(syscall(SYS_settimeofday, &tv, NULL) == -1 && errno == EINVAL,
          "settimeofday usec=1000000 -> EINVAL");

    tv.tv_sec = 100;
    tv.tv_usec = -1;
    errno = 0;
    CHECK(syscall(SYS_settimeofday, &tv, NULL) == -1 && errno == EINVAL,
          "settimeofday usec=-1 -> EINVAL");

    tv.tv_sec = -1;
    tv.tv_usec = 0;
    errno = 0;
    CHECK(syscall(SYS_settimeofday, &tv, NULL) == -1 && errno == EINVAL,
          "settimeofday sec=-1 -> EINVAL");

    errno = 0;
    CHECK(syscall(SYS_settimeofday, (void *)0x10, NULL) == -1 && errno == EFAULT,
          "settimeofday bad tv -> EFAULT");

    errno = 0;
    CHECK(syscall(SYS_settimeofday, NULL, (void *)0x10) == -1 && errno == EFAULT,
          "settimeofday bad tz -> EFAULT");

    tv.tv_sec = 100;
    tv.tv_usec = 500;
    errno = 0;
    CHECK(syscall(SYS_settimeofday, &tv, &tz) == -1 && errno == EPERM,
          "settimeofday valid -> EPERM");

    errno = 0;
    CHECK(syscall(SYS_settimeofday, NULL, NULL) == -1 && errno == EPERM,
          "settimeofday(NULL, NULL) -> EPERM");

    struct timespec ts = {.tv_sec = 1, .tv_nsec = 0};
    errno = 0;
    CHECK(clock_settime(CLOCK_PROCESS_CPUTIME_ID, &ts) == -1 && errno == EPERM,
          "clock_settime(CLOCK_PROCESS_CPUTIME_ID) -> EPERM");

    errno = 0;
    CHECK(clock_settime(CLOCK_THREAD_CPUTIME_ID, &ts) == -1 && errno == EPERM,
          "clock_settime(CLOCK_THREAD_CPUTIME_ID) -> EPERM");

    clockid_t pid_clk = 0;
    CHECK(clock_getcpuclockid(getpid(), &pid_clk) == 0, "clock_getcpuclockid(self)");
    errno = 0;
    CHECK(clock_settime(pid_clk, &ts) == -1 && errno == EPERM,
          "clock_settime(pid cpu clock) -> EPERM");

    clockid_t bad_clk = (clockid_t)((~(uint32_t)999999U << 3) | 2U);
    errno = 0;
    CHECK(clock_settime(bad_clk, &ts) == -1 && errno == EINVAL,
          "clock_settime(nonexistent pid cpu clock) -> EINVAL");

    errno = 0;
    CHECK(clock_settime(CLOCK_MONOTONIC, &ts) == -1 && errno == EINVAL,
          "clock_settime(CLOCK_MONOTONIC) -> EINVAL");
}

/* ---- 6. Detach at attach-time group stop does not duplicate CLD_STOPPED ---- */

static void test_ptrace_detach_at_attach_group_stop(void)
{
    int pfd[2];
    CHECK(pipe(pfd) == 0, "pipe");

    pid_t child = fork();
    CHECK(child >= 0, "fork child");
    if (child == 0) {
        close(pfd[0]);
        /* Put child in its own process group so it is not an orphaned pgrp (SIGTSTP stops it). */
        CHECK(setpgid(0, 0) == 0, "setpgid");
        raise(SIGTSTP);
        /* After parent resumes us with SIGCONT, signal readiness and exit 0. */
        char c = 'k';
        (void)write(pfd[1], &c, 1);
        _exit(0);
    }
    close(pfd[1]);

    int st = 0;
    CHECK(waitpid(child, &st, WUNTRACED) == child, "parent waitpid WUNTRACED");
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTSTP, "child stopped by SIGTSTP");

    /* A separate tracer attaches to the already-stopped child, observes the group stop,
     * and detaches without continuing the group. */
    pid_t tracer = fork();
    CHECK(tracer >= 0, "fork tracer");
    if (tracer == 0) {
        CHECK(ptrace(PTRACE_ATTACH, child, 0, 0) == 0, "PTRACE_ATTACH");
        int tst = 0;
        CHECK(waitpid(child, &tst, 0) == child, "tracer waitpid");
        CHECK(WIFSTOPPED(tst) && WSTOPSIG(tst) == SIGTSTP,
              "tracer expected SIGTSTP group stop, got 0x%x", tst);
        CHECK(ptrace(PTRACE_DETACH, child, 0, 0) == 0, "PTRACE_DETACH");
        _exit(0);
    }

    CHECK(waitpid(tracer, &st, 0) == tracer, "waitpid tracer");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "tracer exit 0x%x", st);

    /* Give the detached child time to settle; parent must NOT see a second stop report. */
    msleep(20);
    pid_t r = waitpid(child, &st, WUNTRACED | WNOHANG);
    CHECK(r == 0, "duplicate CLD_STOPPED after PTRACE_DETACH: waitpid=%d st=0x%x", r, st);

    /* Resume child with SIGCONT and confirm it runs and exits cleanly. */
    CHECK(kill(child, SIGCONT) == 0, "kill SIGCONT");
    char buf = 0;
    CHECK(read(pfd[0], &buf, 1) == 1, "read child post-SIGCONT byte");
    close(pfd[0]);
    CHECK(waitpid(child, &st, 0) == child, "waitpid child exit");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "child exit 0x%x", st);
}

/* ---- 7. Single-step over skipped syscall (-1), rt_sigreturn, and execve ---- */

static void test_singlestep_skipped_syscall(void)
{
    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        CHECK(ptrace(PTRACE_TRACEME, 0, 0, 0) == 0, "PTRACE_TRACEME");
        kill(getpid(), SIGSTOP);
        register uint64_t x8 __asm__("x8") = SYS_getpid;
        register int64_t x0 __asm__("x0") = 0;
        __asm__ volatile(
            "svc #0\n"
            "nop\n"
            "nop\n"
            : "+r"(x0)
            : "r"(x8)
            : "memory");
        _exit(x0 == -EPERM ? 0 : 2);
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "initial SIGSTOP");

    /* Run until syscall entry of getpid. */
    struct arm64_regs entry_regs;
    for (;;) {
        CHECK(ptrace(PTRACE_SYSCALL, child, 0, 0) == 0, "PTRACE_SYSCALL");
        CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
              "syscall entry stop");
        CHECK(get_regs(child, &entry_regs) == 0, "get_regs entry");
        if (entry_regs.regs[8] == SYS_getpid) {
            break;
        }
    }
    uint64_t after_svc_pc = entry_regs.pc;

    /* Skip syscall by setting nr = -1 and return value x0 = -EPERM, then SINGLESTEP. */
    CHECK(set_syscallno(child, -1) == 0, "set_syscallno(-1)");
    entry_regs.regs[0] = (uint64_t)(int64_t)(-EPERM);
    CHECK(set_regs(child, &entry_regs) == 0, "set_regs x0=-EPERM");
    CHECK(ptrace(PTRACE_SINGLESTEP, child, 0, 0) == 0, "PTRACE_SINGLESTEP over skipped svc");

    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
          "step trap after skipped svc");
    struct arm64_regs step_regs;
    CHECK(get_regs(child, &step_regs) == 0, "get_regs step");
    CHECK(step_regs.pc == after_svc_pc,
          "single-step over skipped syscall reported late: pc=0x%llx expected 0x%llx",
          (unsigned long long)step_regs.pc, (unsigned long long)after_svc_pc);

    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0, "PTRACE_CONT");
    CHECK(waitpid(child, &st, 0) == child && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "skipped syscall child exit 0x%x", st);
}

static volatile sig_atomic_t step_sigret_hit = 0;
static void on_step_usr1(int sig)
{
    (void)sig;
    step_sigret_hit = 1;
}

static void test_singlestep_rt_sigreturn(void)
{
    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        struct sigaction sa = {0};
        sa.sa_handler = on_step_usr1;
        CHECK(sigaction(SIGUSR1, &sa, NULL) == 0, "sigaction");
        CHECK(ptrace(PTRACE_TRACEME, 0, 0, 0) == 0, "PTRACE_TRACEME");
        kill(getpid(), SIGSTOP);
        __asm__ volatile(
            "nop\n"
            "nop\n"
            "nop\n"
            ::: "memory");
        _exit(step_sigret_hit ? 0 : 2);
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "initial SIGSTOP");

    struct arm64_regs before_sig;
    CHECK(get_regs(child, &before_sig) == 0, "get_regs before SIGUSR1");
    uint64_t expected_return_pc = before_sig.pc;

    /* Inject SIGUSR1 while single-stepping -> stops at first instruction of handler. */
    CHECK(ptrace(PTRACE_SINGLESTEP, child, 0, (void *)(uintptr_t)SIGUSR1) == 0,
          "PTRACE_SINGLESTEP with SIGUSR1");
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
          "handler entry stop");
    struct arm64_regs hregs;
    CHECK(get_regs(child, &hregs) == 0, "get_regs handler entry");
    CHECK(hregs.pc == (uint64_t)(uintptr_t)on_step_usr1,
          "expected handler entry 0x%llx, got 0x%llx",
          (unsigned long long)(uintptr_t)on_step_usr1, (unsigned long long)hregs.pc);

    /* Single-step through the handler and the vDSO sigreturn trampoline until pc returns to
     * expected_return_pc; if rt_sigreturn skipped syscall_stepped, the first stop back in main
     * code would be at expected_return_pc + 4 instead of expected_return_pc! */
    bool returned_exact = false;
    for (int i = 0; i < 400; i++) {
        struct arm64_regs r;
        CHECK(get_regs(child, &r) == 0, "get_regs step");
        if (r.pc == expected_return_pc) {
            returned_exact = true;
            break;
        }
        CHECK(r.pc != expected_return_pc + 4,
              "single-step over rt_sigreturn stopped one instruction late (pc=0x%llx expected 0x%llx)",
              (unsigned long long)r.pc, (unsigned long long)expected_return_pc);
        CHECK(ptrace(PTRACE_SINGLESTEP, child, 0, 0) == 0, "PTRACE_SINGLESTEP");
        CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
              "step trap in handler");
    }
    CHECK(returned_exact, "did not observe exact return PC from rt_sigreturn");

    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0, "PTRACE_CONT");
    CHECK(waitpid(child, &st, 0) == child && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "rt_sigreturn child exit 0x%x", st);
}

static uint64_t elf_entry_of(const char *path)
{
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0, "open %s", path);
    Elf64_Ehdr eh;
    CHECK(read(fd, &eh, sizeof(eh)) == (ssize_t)sizeof(eh), "read ehdr");
    close(fd);
    return eh.e_entry;
}

static void test_singlestep_execve(const char *self)
{
    uint64_t entry = elf_entry_of(self);
    int sync_p[2];
    CHECK(pipe(sync_p) == 0, "pipe");

    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        close(sync_p[1]);
        char c;
        CHECK(read(sync_p[0], &c, 1) == 1, "wait seize");
        close(sync_p[0]);
        /* Stop so tracer can switch to PTRACE_SINGLESTEP right before execve svc. */
        kill(getpid(), SIGSTOP);
        char *argv[] = {(char *)self, "--exec-helper", NULL};
        char *envp[] = {NULL};
        syscall(SYS_execve, self, argv, envp);
        _exit(99);
    }
    close(sync_p[0]);

    /* Seize WITHOUT PTRACE_O_TRACEEXEC so execve itself does not emit SIGTRAP or EVENT_EXEC;
     * single-step over execve must still stop right at the new image's entry point. */
    CHECK(ptrace(PTRACE_SEIZE, child, 0, 0) == 0, "PTRACE_SEIZE");
    char go = 1;
    CHECK(write(sync_p[1], &go, 1) == 1, "signal child");
    close(sync_p[1]);

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "wait SIGSTOP");
    /* Suppress SIGSTOP and step until we hit execve's entry point. */
    CHECK(ptrace(PTRACE_SINGLESTEP, child, 0, 0) == 0, "start SINGLESTEP");

    bool hit_entry = false;
    for (int i = 0; i < 600; i++) {
        CHECK(waitpid(child, &st, 0) == child, "waitpid step execve");
        CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
              "expected SIGTRAP stepping to execve, got 0x%x", st);
        struct arm64_regs r;
        CHECK(get_regs(child, &r) == 0, "get_regs");
        if (r.pc == entry) {
            hit_entry = true;
            break;
        }
        CHECK(r.pc != entry + 4,
              "single-step over execve stopped one instruction late (pc=0x%llx entry=0x%llx)",
              (unsigned long long)r.pc, (unsigned long long)entry);
        CHECK(ptrace(PTRACE_SINGLESTEP, child, 0, 0) == 0, "PTRACE_SINGLESTEP");
    }
    CHECK(hit_entry, "did not observe stop at exact execve entry 0x%llx",
          (unsigned long long)entry);

    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0, "PTRACE_CONT");
    CHECK(waitpid(child, &st, 0) == child && WIFEXITED(st) && WEXITSTATUS(st) == 42,
          "execve helper exit 0x%x", st);
}

/* ---- 8. PTRACE_EVENT_EXIT for exit_group siblings and non-leader execve ---- */

static _Atomic int exit_group_th_ready = 0;

static void *sleep_forever(void *arg)
{
    (void)arg;
    atomic_store_explicit(&exit_group_th_ready, 1, memory_order_release);
    for (;;) {
        pause();
    }
    return NULL;
}

static void test_ptrace_event_exit_on_exit_group(void)
{
    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        CHECK(ptrace(PTRACE_TRACEME, 0, 0, 0) == 0, "PTRACE_TRACEME");
        raise(SIGSTOP);
        pthread_t th;
        CHECK(pthread_create(&th, NULL, sleep_forever, NULL) == 0, "pthread_create");
        while (atomic_load_explicit(&exit_group_th_ready, memory_order_acquire) == 0) {
            msleep(1);
        }
        syscall(SYS_exit_group, 7);
        _exit(1);
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "initial SIGSTOP");
    CHECK(ptrace(PTRACE_SETOPTIONS, child, 0,
                 (void *)(uintptr_t)(PTRACE_O_TRACECLONE | PTRACE_O_TRACEEXIT)) == 0,
          "PTRACE_SETOPTIONS");
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0, "PTRACE_CONT");

    pid_t sibling = -1;
    int exit_events = 0;
    bool saw_leader_exit = false;
    bool saw_sibling_exit = false;
    bool leader_reaped = false;

    while (!leader_reaped) {
        pid_t w = waitpid(-1, &st, __WALL);
        CHECK(w > 0, "waitpid __WALL");
        if (WIFEXITED(st)) {
            if (w == child) {
                CHECK(WEXITSTATUS(st) == 7, "leader exit status %d", WEXITSTATUS(st));
                leader_reaped = true;
            }
            continue;
        }
        CHECK(WIFSTOPPED(st), "unexpected status 0x%x for %d", st, w);
        int ev = st >> 16;
        if (ev == PTRACE_EVENT_CLONE) {
            unsigned long msg = 0;
            CHECK(ptrace(PTRACE_GETEVENTMSG, w, 0, &msg) == 0, "GETEVENTMSG clone");
            sibling = (pid_t)msg;
        } else if (ev == PTRACE_EVENT_EXIT) {
            unsigned long msg = 0;
            CHECK(ptrace(PTRACE_GETEVENTMSG, w, 0, &msg) == 0, "GETEVENTMSG exit");
            CHECK(msg == (7UL << 8), "expected exit_group eventmsg 0x700, got 0x%lx (pid %d)",
                  msg, w);
            exit_events++;
            if (w == child) {
                saw_leader_exit = true;
            } else if (w == sibling) {
                saw_sibling_exit = true;
            }
        }
        int sig = WSTOPSIG(st);
        int inject = (sig == SIGTRAP || sig == SIGSTOP) ? 0 : sig;
        CHECK(ptrace(PTRACE_CONT, w, 0, (void *)(uintptr_t)inject) == 0, "PTRACE_CONT %d", w);
    }

    CHECK(sibling > 0, "never saw clone sibling");
    CHECK(saw_leader_exit && saw_sibling_exit && exit_events == 2,
          "expected PTRACE_EVENT_EXIT for both leader and sibling (leader=%d sibling=%d count=%d)",
          saw_leader_exit, saw_sibling_exit, exit_events);
}

struct exec_arg {
    const char *self;
};

static void *nonleader_exec_thread(void *arg)
{
    const char *self = ((struct exec_arg *)arg)->self;
    char *argv[] = {(char *)self, "--exec-helper", NULL};
    char *envp[] = {NULL};
    syscall(SYS_execve, self, argv, envp);
    _exit(99);
}

static void test_ptrace_event_exit_on_nonleader_execve(const char *self)
{
    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        CHECK(ptrace(PTRACE_TRACEME, 0, 0, 0) == 0, "PTRACE_TRACEME");
        raise(SIGSTOP);
        struct exec_arg ea = {.self = self};
        pthread_t th;
        CHECK(pthread_create(&th, NULL, nonleader_exec_thread, &ea) == 0, "pthread_create");
        for (;;) {
            pause();
        }
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "initial SIGSTOP");
    CHECK(ptrace(PTRACE_SETOPTIONS, child, 0,
                 (void *)(uintptr_t)(PTRACE_O_TRACECLONE | PTRACE_O_TRACEEXEC |
                                     PTRACE_O_TRACEEXIT)) == 0,
          "PTRACE_SETOPTIONS");
    CHECK(ptrace(PTRACE_CONT, child, 0, 0) == 0, "PTRACE_CONT");

    pid_t worker = -1;
    bool saw_leader_exit_event = false;
    bool saw_exec_event = false;
    bool done = false;

    while (!done) {
        pid_t w = waitpid(-1, &st, __WALL);
        CHECK(w > 0, "waitpid __WALL");
        if (WIFEXITED(st)) {
            if (w == child) {
                CHECK(WEXITSTATUS(st) == 42, "exec helper exit status %d", WEXITSTATUS(st));
                done = true;
            }
            continue;
        }
        CHECK(WIFSTOPPED(st), "unexpected status 0x%x for %d", st, w);
        int ev = st >> 16;
        if (ev == PTRACE_EVENT_CLONE) {
            unsigned long msg = 0;
            CHECK(ptrace(PTRACE_GETEVENTMSG, w, 0, &msg) == 0, "GETEVENTMSG clone");
            worker = (pid_t)msg;
        } else if (ev == PTRACE_EVENT_EXIT && w == child && !saw_exec_event) {
            unsigned long msg = 0;
            CHECK(ptrace(PTRACE_GETEVENTMSG, w, 0, &msg) == 0, "GETEVENTMSG leader exit");
            CHECK(msg == 0, "leader killed by non-leader execve should report msg 0, got 0x%lx",
                  msg);
            saw_leader_exit_event = true;
        } else if (ev == PTRACE_EVENT_EXEC) {
            unsigned long msg = 0;
            CHECK(ptrace(PTRACE_GETEVENTMSG, w, 0, &msg) == 0, "GETEVENTMSG exec");
            CHECK(w == child, "execing thread should take over leader pid %d (got %d)", child, w);
            CHECK((pid_t)msg == worker, "exec eventmsg should be former tid %d (got %lu)",
                  worker, msg);
            CHECK(saw_leader_exit_event,
                  "leader PTRACE_EVENT_EXIT must arrive before PTRACE_EVENT_EXEC");
            saw_exec_event = true;
        }
        int sig = WSTOPSIG(st);
        int inject = (sig == SIGTRAP || sig == SIGSTOP) ? 0 : sig;
        CHECK(ptrace(PTRACE_CONT, w, 0, (void *)(uintptr_t)inject) == 0, "PTRACE_CONT %d", w);
    }

    CHECK(saw_leader_exit_event && saw_exec_event,
          "missing events: leader_exit=%d exec=%d", saw_leader_exit_event, saw_exec_event);
}

/* ---- 9. process_vm_readv/writev zero remote bytes before PID/permission checks ---- */

static void test_process_vm_zero_remote(void)
{
    char buf[16] = {0};
    struct iovec local = {.iov_base = buf, .iov_len = sizeof(buf)};
    struct iovec remote_zero = {.iov_base = (void *)0x1000, .iov_len = 0};

    errno = 0;
    CHECK(syscall(SYS_process_vm_readv, 999999, &local, 1, &remote_zero, 1, 0) == 0,
          "process_vm_readv zero-len remote must return 0 before ESRCH");
    errno = 0;
    CHECK(syscall(SYS_process_vm_readv, 999999, &local, 1, &remote_zero, 0, 0) == 0,
          "process_vm_readv empty remote must return 0 before ESRCH");
    errno = 0;
    CHECK(syscall(SYS_process_vm_writev, 999999, &local, 1, &remote_zero, 1, 0) == 0,
          "process_vm_writev zero-len remote must return 0 before ESRCH");
    errno = 0;
    CHECK(syscall(SYS_process_vm_writev, 999999, &local, 1, &remote_zero, 0, 0) == 0,
          "process_vm_writev empty remote must return 0 before ESRCH");
}

/* ---- 10 & 11. TIOCSCTTY non-leader & TIOCSPGRP error codes ---- */

static int open_pty_pair(int *master_out, int *slave_out, int slave_flags)
{
    int m = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    if (m < 0) {
        return -1;
    }
    int unlock = 0;
    if (ioctl(m, TIOCSPTLCK, &unlock) != 0) {
        close(m);
        return -1;
    }
    unsigned int ptn = 0;
    if (ioctl(m, TIOCGPTN, &ptn) != 0) {
        close(m);
        return -1;
    }
    char path[64];
    snprintf(path, sizeof(path), "/dev/pts/%u", ptn);
    int s = open(path, slave_flags);
    if (s < 0) {
        close(m);
        return -1;
    }
    *master_out = m;
    *slave_out = s;
    return 0;
}

static void test_tty_job_control_ioctls(void)
{
    /* Test TIOCSCTTY and TIOCSPGRP in a fresh session. */
    pid_t leader = fork();
    CHECK(leader >= 0, "fork leader");
    if (leader == 0) {
        CHECK(setsid() > 0, "setsid");
        int m = -1, s = -1;
        CHECK(open_pty_pair(&m, &s, O_RDWR) == 0, "open_pty_pair");

        /* Session leader calling TIOCSCTTY on its own ctty succeeds. */
        CHECK(ioctl(s, TIOCSCTTY, 0) == 0, "leader TIOCSCTTY own ctty");

        /* TIOCSPGRP with pgid=0 or nonexistent pgid returns ESRCH. */
        pid_t pg = 0;
        errno = 0;
        CHECK(ioctl(s, TIOCSPGRP, &pg) == -1 && errno == ESRCH,
              "TIOCSPGRP(0) -> ESRCH");
        pg = 999999;
        errno = 0;
        CHECK(ioctl(s, TIOCSPGRP, &pg) == -1 && errno == ESRCH,
              "TIOCSPGRP(unknown) -> ESRCH");

        /* TIOCSPGRP with a pgid in another session returns EPERM. */
        pid_t parent_pg = getpgid(getppid());
        errno = 0;
        CHECK(ioctl(s, TIOCSPGRP, &parent_pg) == -1 && errno == EPERM,
              "TIOCSPGRP(other session) -> EPERM");

        /* Non-ctty PTY: bad pointer -> EFAULT, negative -> EINVAL, valid -> ENOTTY. */
        int m2 = -1, s2 = -1;
        CHECK(open_pty_pair(&m2, &s2, O_RDWR | O_NOCTTY) == 0, "open second pty");
        errno = 0;
        CHECK(ioctl(s2, TIOCSPGRP, (void *)0x10) == -1 && errno == EFAULT,
              "non-ctty TIOCSPGRP bad ptr -> EFAULT");
        pg = -1;
        errno = 0;
        CHECK(ioctl(s2, TIOCSPGRP, &pg) == -1 && errno == EINVAL,
              "non-ctty TIOCSPGRP negative -> EINVAL");
        pg = getpgrp();
        errno = 0;
        CHECK(ioctl(s2, TIOCSPGRP, &pg) == -1 && errno == ENOTTY,
              "non-ctty TIOCSPGRP valid -> ENOTTY");
        close(m2);
        close(s2);

        /* Non-leader child inherits ctty: TIOCSCTTY must return EPERM (not 0). */
        pid_t nonleader = fork();
        CHECK(nonleader >= 0, "fork nonleader");
        if (nonleader == 0) {
            errno = 0;
            CHECK(ioctl(s, TIOCSCTTY, 0) == -1 && errno == EPERM,
                  "non-leader TIOCSCTTY on own ctty -> EPERM");
            _exit(0);
        }
        int st = 0;
        CHECK(waitpid(nonleader, &st, 0) == nonleader && WIFEXITED(st) && WEXITSTATUS(st) == 0,
              "nonleader exit 0x%x", st);

        /* Orphaned background process group calling TIOCSPGRP gets ENOTTY (not EIO). */
        int pfd[2];
        CHECK(pipe(pfd) == 0, "pipe");
        pid_t mid = fork();
        CHECK(mid >= 0, "fork mid");
        if (mid == 0) {
            close(pfd[0]);
            /* Put grandchild in its own pgrp within the same session, then exit mid so grandchild's
             * parent becomes init (different session) -> orphaned pgrp! */
            pid_t mid_pid = getpid();
            pid_t gc = fork();
            CHECK(gc >= 0, "fork grandchild");
            if (gc == 0) {
                CHECK(setpgid(0, 0) == 0, "grandchild setpgid");
                /* Wait until mid exits and we are reparented away from mid. */
                while (getppid() == mid_pid) {
                    msleep(1);
                }
                pid_t mypg = getpgrp();
                errno = 0;
                int rc = ioctl(s, TIOCSPGRP, &mypg);
                int err = errno;
                int out[2] = {rc, err};
                CHECK(write(pfd[1], out, sizeof(out)) == (ssize_t)sizeof(out), "write result");
                _exit(0);
            }
            (void)setpgid(gc, gc);
            close(pfd[1]);
            _exit(0);
        }
        close(pfd[1]);
        CHECK(waitpid(mid, &st, 0) == mid && WIFEXITED(st) && WEXITSTATUS(st) == 0,
              "waitpid mid");
        int res[2] = {0, 0};
        CHECK(read(pfd[0], res, sizeof(res)) == (ssize_t)sizeof(res), "read orphan result");
        close(pfd[0]);
        CHECK(res[0] == -1 && res[1] == ENOTTY,
              "orphaned background TIOCSPGRP expected -1/ENOTTY, got %d/%d", res[0], res[1]);

        /* Closing the last master sends SIGHUP to the session leader; ignore it before cleanup. */
        signal(SIGHUP, SIG_IGN);
        close(s);
        close(m);
        _exit(0);
    }

    int st = 0;
    CHECK(waitpid(leader, &st, 0) == leader && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "tty job control leader exit 0x%x", st);
}

/* ---- 12. PTY slave write clamps ONLCR and XTABS expansion to OUTPUT_MAX ---- */

static void drain_fd(int fd)
{
    char tmp[4096];
    for (;;) {
        ssize_t n = read(fd, tmp, sizeof(tmp));
        if (n <= 0) {
            break;
        }
    }
}

static void test_tty_output_clamp(void)
{
    int m = -1, s = -1;
    CHECK(open_pty_pair(&m, &s, O_RDWR | O_NOCTTY | O_NONBLOCK) == 0, "open_pty_pair");
    int fl = fcntl(m, F_GETFL);
    CHECK(fl >= 0 && fcntl(m, F_SETFL, fl | O_NONBLOCK) == 0, "master O_NONBLOCK");

    struct termios tio;
    CHECK(tcgetattr(s, &tio) == 0, "tcgetattr");
    tio.c_oflag = OPOST | ONLCR | XTABS;
    CHECK(tcsetattr(s, TCSANOW, &tio) == 0, "tcsetattr");

    /* Fill with newlines: ONLCR doubles bytes, so output queue must never exceed 65536. */
    char *nls = malloc(65536);
    CHECK(nls != NULL, "malloc");
    memset(nls, '\n', 65536);
    ssize_t w = write(s, nls, 65536);
    CHECK(w == 32768, "ONLCR write expected 32768, got %zd", w);
    int queued = 0;
    CHECK(ioctl(m, FIONREAD, &queued) == 0 && queued == 65536,
          "ONLCR queued expected 65536, got %d", queued);

    /* With 1 byte free, writing '\n' must return EAGAIN and not overshoot 65536. */
    char one;
    CHECK(read(m, &one, 1) == 1, "read 1 byte");
    errno = 0;
    CHECK(write(s, "\n", 1) == -1 && errno == EAGAIN,
          "ONLCR write with 1 byte free must return EAGAIN");
    CHECK(ioctl(m, FIONREAD, &queued) == 0 && queued == 65535,
          "queued must remain 65535, got %d", queued);

    /* Fill with tabs from column 0: XTABS expands each tab to 8 spaces -> at most 8192 tabs. */
    drain_fd(m);
    CHECK(write(s, "\r", 1) == 1, "reset column");
    drain_fd(m);
    memset(nls, '\t', 65536);
    w = write(s, nls, 65536);
    CHECK(w == 8192, "XTABS write expected 8192, got %zd", w);
    CHECK(ioctl(m, FIONREAD, &queued) == 0 && queued == 65536,
          "XTABS queued expected 65536, got %d", queued);

    free(nls);
    close(s);
    close(m);
}

/* ---- 13. Exited (zombie) main thread's CPU clock stays readable & stopped until reaped ---- */

struct zombie_clock_arg {
    int release_fd;
    int leader_exited_fd;
};

static void *keep_process_alive(void *arg)
{
    struct zombie_clock_arg *a = arg;
    /* Wait until parent tells us to exit. */
    char c;
    (void)read(a->release_fd, &c, 1);
    _exit(0);
}

static clockid_t thread_cpu_clock(pid_t tid)
{
    /* MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED) = (~tid << 3) | 4 | 2 */
    return (clockid_t)((~(uint32_t)tid << 3) | 6U);
}

static void test_zombie_leader_cpu_clock(void)
{
    int p_ready[2], p_go[2], p_release[2];
    CHECK(pipe(p_ready) == 0 && pipe(p_go) == 0 && pipe(p_release) == 0, "pipes");

    pid_t child = fork();
    CHECK(child >= 0, "fork");
    if (child == 0) {
        close(p_ready[0]);
        close(p_go[1]);
        close(p_release[1]);
        /* Burn a tiny bit of CPU so leader's CPU clock is > 0. */
        volatile uint64_t spin = 0;
        for (int i = 0; i < 200000; i++) {
            spin += (uint64_t)i;
        }
        (void)spin;

        struct zombie_clock_arg arg = {
            .release_fd = p_release[0],
            .leader_exited_fd = -1,
        };
        pthread_t th;
        CHECK(pthread_create(&th, NULL, keep_process_alive, &arg) == 0, "pthread_create");

        /* Tell parent we're ready so parent can create a POSIX timer on our thread CPU clock
         * before we exit. */
        char c = 1;
        CHECK(write(p_ready[1], &c, 1) == 1, "signal ready");
        CHECK(read(p_go[0], &c, 1) == 1, "wait go");
        close(p_ready[1]);
        close(p_go[0]);
        /* Exit ONLY the main thread so it becomes a zombie leader while the worker runs. */
        syscall(SYS_exit, 0);
        __builtin_unreachable();
    }
    close(p_ready[1]);
    close(p_go[0]);
    close(p_release[0]);

    char c;
    CHECK(read(p_ready[0], &c, 1) == 1, "read ready");
    close(p_ready[0]);

    clockid_t clk = thread_cpu_clock(child);
    struct timespec t_live = {0};
    CHECK(clock_gettime(clk, &t_live) == 0, "clock_gettime live leader");

    /* Create and arm a POSIX timer on the leader's thread CPU clock while alive. */
    timer_t tid_timer;
    struct sigevent sev = {0};
    sev.sigev_notify = SIGEV_NONE;
    CHECK(timer_create(clk, &sev, &tid_timer) == 0, "timer_create on leader clock");
    struct itimerspec its = {
        .it_interval = {.tv_sec = 0, .tv_nsec = 50000000},
        .it_value = {.tv_sec = 10, .tv_nsec = 0},
    };
    CHECK(timer_settime(tid_timer, 0, &its, NULL) == 0, "timer_settime live leader");

    /* Let the leader exit while its worker thread keeps the process alive. */
    CHECK(write(p_go[1], &c, 1) == 1, "tell leader to exit");
    close(p_go[1]);

    /* Wait for leader to become a zombie (check /proc/<child>/stat shows 'Z' or sleep briefly). */
    for (int i = 0; i < 200; i++) {
        char path[64], buf[256];
        snprintf(path, sizeof(path), "/proc/%d/stat", child);
        int fd = open(path, O_RDONLY);
        if (fd >= 0) {
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);
            if (n > 0) {
                buf[n] = '\0';
                char *rp = strrchr(buf, ')');
                if (rp && rp[1] == ' ' && rp[2] == 'Z') {
                    break;
                }
            }
        }
        msleep(2);
    }

    /* Zombie leader's thread CPU clock must STILL be readable, non-decreasing, and stopped! */
    struct timespec t_z1 = {0}, t_z2 = {0};
    CHECK(clock_gettime(clk, &t_z1) == 0, "clock_gettime zombie leader while worker runs");
    msleep(15);
    CHECK(clock_gettime(clk, &t_z2) == 0, "clock_gettime zombie leader second read");
    CHECK(t_z1.tv_sec == t_z2.tv_sec && t_z1.tv_nsec == t_z2.tv_nsec,
          "zombie leader CPU clock must be stopped");

    /* Timer on zombie leader must still report remaining time and accept timer_settime! */
    struct itimerspec cur = {0};
    CHECK(timer_gettime(tid_timer, &cur) == 0, "timer_gettime on zombie leader");
    CHECK(cur.it_value.tv_sec > 0 || cur.it_value.tv_nsec > 0,
          "timer_gettime on zombie leader must not read all zeros");
    CHECK(cur.it_interval.tv_nsec == 50000000, "timer_gettime interval preserved");
    CHECK(timer_settime(tid_timer, 0, &its, NULL) == 0,
          "timer_settime on zombie leader must succeed (not ESRCH)");

    /* Also verify creating a NEW timer on an already-zombie leader works. */
    timer_t tid_timer2;
    CHECK(timer_create(clk, &sev, &tid_timer2) == 0, "timer_create on zombie leader");
    CHECK(timer_delete(tid_timer2) == 0, "timer_delete second timer");

    /* Now release the worker so the whole process exits, then reap it. */
    CHECK(write(p_release[1], &c, 1) == 1, "release worker");
    close(p_release[1]);
    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFEXITED(st) && WEXITSTATUS(st) == 0,
          "reap child");

    /* Once reaped, the leader's CPU clock and timer are gone. */
    errno = 0;
    CHECK(clock_gettime(clk, &t_z1) == -1 && errno == EINVAL,
          "clock_gettime after reap must return EINVAL");
    errno = 0;
    CHECK(timer_settime(tid_timer, 0, &its, NULL) == -1 && errno == ESRCH,
          "timer_settime after reap must return ESRCH");
    CHECK(timer_gettime(tid_timer, &cur) == 0 &&
          cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0 &&
          cur.it_interval.tv_sec == 0 && cur.it_interval.tv_nsec == 0,
          "timer_gettime after reap must read all zeros");
    CHECK(timer_delete(tid_timer) == 0, "timer_delete");
}

int main(int argc, char **argv)
{
    if (argc >= 2 && strcmp(argv[1], "--exec-helper") == 0) {
        return 42;
    }
    const char *self = argv[0];

    test_group_stop_waits_for_all_threads();
    test_sigill_omits_esr_context();
    test_sigaction_clone_sighand_discard();
    test_sigreturn_validation();
    test_time_setting_errors();
    test_ptrace_detach_at_attach_group_stop();
    test_singlestep_skipped_syscall();
    test_singlestep_rt_sigreturn();
    test_singlestep_execve(self);
    test_ptrace_event_exit_on_exit_group();
    test_ptrace_event_exit_on_nonleader_execve(self);
    test_process_vm_zero_remote();
    test_tty_job_control_ioctls();
    test_tty_output_clamp();
    test_zombie_leader_cpu_clock();

    puts("all ok");
    return 0;
}
