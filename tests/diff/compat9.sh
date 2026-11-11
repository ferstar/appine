# docker-run-options: --privileged
# The small batch of the seventh map-bug triage: memfd's F_SEAL_EXEC keeps a memfd's execute bits
# (MFD_EXEC with MFD_NOEXEC_SEAL is EINVAL, and sealing an executable memfd's execute bits seals
# its contents too); personality(2)'s READ_IMPLIES_EXEC makes the heap executable (mmap and
# mprotect are Starnix's personality_test's), every execve clears it (arm64's SET_PERSONALITY)
# and a setuid one the other PER_CLEAR_ON_SETID flags too;
# a bind mount whose source name was removed shows `//deleted` after its root in mountinfo,
# whether the source was a directory, a file or one link of a file with another; and the tenth
# triage's POSIX timers on the CPU clock of a thread or process that has ended
set -e
cd /tmp
cat > small.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/personality.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define MFD_ALLOW_SEALING_ 0x2
#define MFD_NOEXEC_SEAL_ 0x8
#define MFD_EXEC_ 0x10
#define F_ADD_SEALS_ 1033
#define F_GET_SEALS_ 1034
#define F_SEAL_EXEC_ 0x20
#define READ_IMPLIES_EXEC_ 0x0400000
#define ADDR_NO_RANDOMIZE_ 0x0040000

static int memfd(const char *name, unsigned flags)
{
    return syscall(SYS_memfd_create, name, flags);
}

static void show(const char *what, long r)
{
    printf("%s: %ld %s\n", what, r, r < 0 ? strerror(errno) : "");
}

// The permissions /proc/self/maps shows for the mapping holding addr
static void perms(const char *what, unsigned long addr)
{
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    while (fgets(line, sizeof line, f)) {
        unsigned long lo, hi;
        char p[8];
        if (sscanf(line, "%lx-%lx %7s", &lo, &hi, p) == 3 && lo <= addr && addr < hi) {
            printf("%s: %s\n", what, p);
            fclose(f);
            return;
        }
    }
    printf("%s: unmapped\n", what);
    fclose(f);
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1) {
        printf("%s: personality %#x\n", argv[1], personality(0xffffffff));
        return 0;
    }

    show("MFD_NOEXEC_SEAL|MFD_EXEC", memfd("both", MFD_NOEXEC_SEAL_ | MFD_EXEC_));
    int n = memfd("noexec", MFD_NOEXEC_SEAL_);
    struct stat st;
    fstat(n, &st);
    printf("MFD_NOEXEC_SEAL: mode %o seals %#x\n", st.st_mode & 07777, fcntl(n, F_GET_SEALS_));
    show("chmod 0755 of it", fchmod(n, 0755));
    show("chmod 0600 of it", fchmod(n, 0600));
    int m = memfd("exec", MFD_ALLOW_SEALING_);
    show("F_SEAL_EXEC on a 0777 memfd", fcntl(m, F_ADD_SEALS_, F_SEAL_EXEC_));
    printf("its seals %#x\n", fcntl(m, F_GET_SEALS_));
    show("write to it", write(m, "x", 1));
    show("chmod 0755 of it (execute bits kept)", fchmod(m, 0755));
    show("chmod 0644 of it", fchmod(m, 0644));
    int w = memfd("nox", MFD_ALLOW_SEALING_);
    fchmod(w, 0644);
    show("F_SEAL_EXEC on a 0644 memfd", fcntl(w, F_ADD_SEALS_, F_SEAL_EXEC_));
    printf("its seals %#x\n", fcntl(w, F_GET_SEALS_));
    show("write to it", write(w, "x", 1));

    // The heap grown with READ_IMPLIES_EXEC is executable; without it, not
    unsigned long brk0 = syscall(SYS_brk, 0);
    unsigned long page = (brk0 + 4095) & ~4095UL;
    syscall(SYS_brk, page + 0x4000);
    perms("heap grown without READ_IMPLIES_EXEC", page);
    personality(READ_IMPLIES_EXEC_ | ADDR_NO_RANDOMIZE_);
    syscall(SYS_brk, page + 0x8000);
    perms("heap grown with READ_IMPLIES_EXEC", page + 0x4000);

    // Kept by an ordinary execve, cleared by a setuid one
    char self[256];
    ssize_t len = readlink("/proc/self/exe", self, sizeof self - 1);
    self[len] = 0;
    pid_t c = fork();
    if (c == 0) {
        execl(self, self, "plain execve", (char *)NULL);
        _exit(127);
    }
    waitpid(c, NULL, 0);
    c = fork();
    if (c == 0) {
        execl("/tmp/small-suid", "/tmp/small-suid", "setuid execve", (char *)NULL);
        _exit(127);
    }
    waitpid(c, NULL, 0);
    return 0;
}
C
gcc -O1 -o small small.c
cp small small-suid
chmod 4755 small-suid
./small

# The fourth column (the mount's root) of a bind's mountinfo line, once its source is removed
root() {
    awk -v p="$1" '$5 == p { print $4 }' /proc/self/mountinfo
}
mkdir bd bdt
mount --bind bd bdt
rmdir bd
echo "directory: $(root /tmp/bdt)"
echo x > bf
touch bft
mount --bind bf bft
rm bf
echo "file: $(root /tmp/bft)"
echo y > hl
ln hl hl2
touch hlt
mount --bind hl hlt
rm hl
echo "one of two links: $(root /tmp/hlt), still read: $(cat hlt)"
umount bdt bft hlt

# POSIX timers on the CPU clock of a thread or process that ends (the tenth map-bug triage): the
# timer of an exited thread or a reaped process reads as all zeros and cannot be set (ESRCH), and
# does not follow a process that takes the number over; a zombie's clock still counts as there.
# Clock ids name pids as the caller's pid namespace numbers them, so it all repeats in a new one.
cat > deadclock.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *where;
static int go[2];
static volatile int started;

static void *idle(void *arg)
{
    (void)arg;
    started = 1;
    char x;
    read(go[0], &x, 1);
    return NULL;
}

static void show(const char *what, timer_t t)
{
    struct itimerspec v;
    if (timer_gettime(t, &v) != 0) {
        printf("%s, %s: gettime %s\n", where, what, strerror(errno));
        return;
    }
    printf("%s, %s: interval %ld.%09ld, %s\n", where, what, (long)v.it_interval.tv_sec,
           v.it_interval.tv_nsec, v.it_value.tv_sec || v.it_value.tv_nsec ? "armed" : "disarmed");
}

static void set(const char *what, timer_t t)
{
    struct itimerspec v = {{1, 0}, {100, 0}};
    printf("%s, %s: settime %s\n", where, what,
           timer_settime(t, 0, &v, NULL) == 0 ? "ok" : strerror(errno));
}

static timer_t make(clockid_t c)
{
    struct sigevent ev = {.sigev_notify = SIGEV_NONE};
    timer_t t = 0;
    if (timer_create(c, &ev, &t) != 0)
        printf("%s: timer_create %s\n", where, strerror(errno));
    return t;
}

static void thread_clock(void)
{
    pipe(go);
    started = 0;
    pthread_t th;
    pthread_create(&th, NULL, idle, NULL);
    while (!started)
        usleep(1000);
    clockid_t c;
    pthread_getcpuclockid(th, &c);
    struct timespec now;
    printf("%s, thread: clock %s\n", where, clock_gettime(c, &now) == 0 ? "read" : strerror(errno));
    timer_t t = make(c);
    set("thread", t);
    show("thread", t);
    write(go[1], "x", 1);
    pthread_join(th, NULL);
    // The clock goes once the thread has left the kernel
    for (int i = 0; i < 3000 && clock_gettime(c, &now) == 0; i++)
        usleep(1000);
    show("exited thread", t);
    set("exited thread", t);
    printf("%s, exited thread: delete %s\n", where, timer_delete(t) == 0 ? "ok" : strerror(errno));
    close(go[0]);
    close(go[1]);
}

static void process_clock(void)
{
    pid_t p = fork();
    if (p == 0) {
        pause();
        _exit(0);
    }
    clockid_t c;
    int e = clock_getcpuclockid(p, &c);
    printf("%s, child: clock %s\n", where, e ? strerror(e) : "found");
    timer_t t = make(c);
    set("child", t);
    kill(p, SIGKILL);
    siginfo_t si;
    waitid(P_PID, p, &si, WEXITED | WNOWAIT);
    show("zombie", t);
    set("zombie", t);
    waitpid(p, NULL, 0);
    show("reaped", t);
    set("reaped", t);
    // A new process with the same number
    int f = open("/proc/sys/kernel/ns_last_pid", O_WRONLY);
    dprintf(f, "%d", p - 1);
    close(f);
    pid_t q = fork();
    if (q == 0) {
        pause();
        _exit(0);
    }
    printf("%s, new child: %s number\n", where, q == p ? "same" : "another");
    show("new child", t);
    set("new child", t);
    kill(q, SIGKILL);
    waitpid(q, NULL, 0);
    timer_delete(t);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    where = "here";
    thread_clock();
    process_clock();
    if (unshare(CLONE_NEWPID) != 0) {
        perror("unshare");
        return 1;
    }
    pid_t p = fork();
    if (p == 0) {
        where = "pid namespace";
        thread_clock();
        process_clock();
        _exit(0);
    }
    waitpid(p, NULL, 0);
    return 0;
}
C
gcc -O1 -pthread -o deadclock deadclock.c
./deadclock
rm -f deadclock deadclock.c
