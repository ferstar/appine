# The sequel to compat3.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MADV_DONTNEED on private file mappings; signal frames that cannot be written
# and bad rt_sigreturn frames
set -e
cd /tmp
# MADV_DONTNEED on a private file mapping drops the written copies: the pages read the file again
# (zeros past its end in the last page), also for a mapping made writable after being read-only, in
# a forked child, and for a range starting inside a page run
cat > dontneedfile.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>

static void show(const char *what, const unsigned char *p, size_t len) {
    size_t a = 0, b = 0, z = 0, other = 0;
    for (size_t i = 0; i < len; i++) {
        if (p[i] == 'a') a++;
        else if (p[i] == 'b') b++;
        else if (p[i] == 0) z++;
        else other++;
    }
    printf("%s: a=%zu b=%zu zero=%zu other=%zu\n", what, a, b, z, other);
}

int main(void) {
    // 3 pages and a bit: the last page holds 100 bytes of the file
    size_t flen = 3 * 4096 + 100, mlen = 4 * 4096;
    int fd = open("dnfile", O_RDWR | O_CREAT | O_TRUNC, 0644);
    unsigned char *buf = malloc(flen);
    memset(buf, 'a', flen);
    if (write(fd, buf, flen) != (ssize_t)flen) return 1;
    // Old enough that a read-only mapping may share a cached host mapping
    sleep(3);

    unsigned char *p = mmap(NULL, mlen, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    memset(p, 'b', mlen);
    show("written", p, mlen);
    printf("dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
    show("after", p, mlen);
    // Only a middle part
    memset(p, 'b', mlen);
    printf("dontneed middle %d\n", madvise(p + 4096, 4096, MADV_DONTNEED));
    show("page 0", p, 4096);
    show("page 1", p + 4096, 4096);
    show("page 2", p + 8192, 4096);

    // Read-only first, then writable
    unsigned char *q = mmap(NULL, mlen, PROT_READ, MAP_PRIVATE, fd, 0);
    show("read-only", q, mlen);
    mprotect(q, mlen, PROT_READ | PROT_WRITE);
    memset(q, 'b', mlen);
    printf("dontneed %d\n", madvise(q, mlen, MADV_DONTNEED));
    show("made writable", q, mlen);

    // In a forked child, the child's copies go and the parent's stay
    memset(p, 'b', mlen);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        printf("child dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
        show("child", p, mlen);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("parent", p, mlen);

    // The file changed: the dropped pages show the new contents
    memset(p, 'b', mlen);
    pwrite(fd, "bbbb", 4, 0);
    madvise(p, 4096, MADV_DONTNEED);
    printf("first bytes %.6s\n", p);
    return 0;
}
C
gcc -O1 -o dontneedfile dontneedfile.c && ./dontneedfile
# A signal whose frame cannot be written (its alternate stack is read-only) forces a SIGSEGV
# (SI_KERNEL) that a handler may catch; one for SIGSEGV itself ends the process with SIGSEGV. A bad
# rt_sigreturn frame returns 0 and forces a SIGSEGV at the stack pointer (SEGV_ACCERR below a
# mapping)
cat > badframe.c <<'C'
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static sigjmp_buf back;
static volatile int usr1;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    printf("SIGSEGV handler: sig %d code %d addr %s\n", sig, si->si_code,
           si->si_code == SI_KERNEL ? "-" : "set");
    fflush(stdout);
    siglongjmp(back, 1);
}

static void on_usr1(int sig) {
    (void)sig;
    usr1 = 1;
}

static void bad_sigreturn(void *sp) {
    __asm__ volatile("mov x1, sp\n\tmov sp, %0\n\tmov x8, #139\n\tsvc #0\n\tmov sp, x1"
                     :
                     : "r"(sp)
                     : "x0", "x1", "x8", "memory");
}

static void *ro_stack(size_t len) {
    return mmap(NULL, len, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

int main(void) {
    size_t len = 64 * 1024;
    stack_t ss = {.ss_sp = ro_stack(len), .ss_size = len};
    sigaltstack(&ss, NULL);
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR1, &sa, NULL);
    struct sigaction segv = {0};
    segv.sa_sigaction = on_segv;
    segv.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &segv, NULL);
    if (sigsetjmp(back, 1) == 0) {
        raise(SIGUSR1);
        printf("not reached\n");
    }
    printf("after the forced SIGSEGV: usr1 ran %d\n", usr1);

    // A bad frame at rt_sigreturn: SIGSEGV handled on a good alternate stack
    stack_t good = {.ss_sp = malloc(len), .ss_size = len};
    sigaltstack(&good, NULL);
    segv.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGSEGV, &segv, NULL);
    void *none = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sigsetjmp(back, 1) == 0) {
        bad_sigreturn(none);
        printf("not reached\n");
    }
    printf("after the bad sigreturn\n");
    fflush(stdout);

    // SIGSEGV's own frame cannot be written either: the process ends with SIGSEGV
    pid_t pid = fork();
    if (pid == 0) {
        struct rlimit no = {0, 0};
        setrlimit(RLIMIT_CORE, &no);
        stack_t bad = {.ss_sp = ro_stack(len), .ss_size = len};
        sigaltstack(&bad, NULL);
        raise(SIGUSR1);
        printf("child not reached\n");
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("child: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    return 0;
}
C
gcc -O1 -o badframe badframe.c && ./badframe
