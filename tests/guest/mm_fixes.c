// Regression tests for kernel/mm fixes:
// 1. Range operations with unmapped gaps (madvise, msync, mlock/munlock, mincore).
// 2. Concurrent writable shared mappings (mmap, shmat, mremap) with cross-process futexes
//    (ensuring shared_maps is visible before PTEs so fast-path futexes never use private keys).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif
#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif
#ifndef MADV_REMOVE
#define MADV_REMOVE 9
#endif
#ifndef MADV_DONTFORK
#define MADV_DONTFORK 10
#endif
#ifndef MADV_WIPEONFORK
#define MADV_WIPEONFORK 18
#endif

static void die(const char *msg, int line) {
    fprintf(stderr, "FAIL line %d: %s (errno=%d: %s)\n", line, msg, errno, strerror(errno));
    _exit(1);
}

#define CHECK(cond) do { if (!(cond)) die(#cond, __LINE__); } while (0)

static long sys_futex(uint32_t *uaddr, int op, uint32_t val) {
    return syscall(SYS_futex, uaddr, op, val, NULL, NULL, 0);
}

static void test_madvise_gaps(size_t P) {
    // Layout: [p0: anon, p1: gap, p2: anon]
    char *base = mmap(NULL, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(base != MAP_FAILED);
    CHECK(munmap(base + P, P) == 0);

    // MADV_DONTNEED across [p0, gap, p2]: zeroes BOTH p0 and p2, returns ENOMEM.
    base[0] = 42;
    base[2 * P] = 99;
    errno = 0;
    CHECK(madvise(base, 3 * P, MADV_DONTNEED) == -1 && errno == ENOMEM);
    CHECK(base[0] == 0);
    CHECK(base[2 * P] == 0);

    // MADV_DONTNEED across [gap, p2]: zeroes p2, returns ENOMEM.
    base[2 * P] = 77;
    errno = 0;
    CHECK(madvise(base + P, 2 * P, MADV_DONTNEED) == -1 && errno == ENOMEM);
    CHECK(base[2 * P] == 0);

    // MADV_DONTNEED across [gap, locked p2]: EINVAL overrides earlier gap ENOMEM, p2 preserved.
    base[2 * P] = 55;
    CHECK(mlock(base + 2 * P, P) == 0);
    errno = 0;
    CHECK(madvise(base + P, 2 * P, MADV_DONTNEED) == -1 && errno == EINVAL);
    CHECK(base[2 * P] == 55);
    CHECK(munlock(base + 2 * P, P) == 0);

    // MADV_DONTNEED across [p0, locked p1, p2]: zeroes p0, stops at locked p1 (EINVAL), p2 preserved.
    char *m = mmap(NULL, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(m != MAP_FAILED);
    m[0] = 11;
    m[P] = 22;
    m[2 * P] = 33;
    CHECK(mlock(m + P, P) == 0);
    errno = 0;
    CHECK(madvise(m, 3 * P, MADV_DONTNEED) == -1 && errno == EINVAL);
    CHECK(m[0] == 0);
    CHECK(m[P] == 22);
    CHECK(m[2 * P] == 33);
    CHECK(munlock(m + P, P) == 0);
    CHECK(munmap(m, 3 * P) == 0);

    // MADV_WIPEONFORK across [p0, gap, p2]: sets flag on both p0 and p2, returns ENOMEM.
    base[0] = 12;
    base[2 * P] = 34;
    errno = 0;
    CHECK(madvise(base, 3 * P, MADV_WIPEONFORK) == -1 && errno == ENOMEM);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        if (base[0] != 0 || base[2 * P] != 0)
            _exit(2);
        _exit(0);
    }
    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(base[0] == 12 && base[2 * P] == 34);

    // MADV_DONTFORK across [p0, gap, p2]: sets flag on both p0 and p2, returns ENOMEM.
    errno = 0;
    CHECK(madvise(base, 3 * P, MADV_DONTFORK) == -1 && errno == ENOMEM);
    pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        unsigned char v = 0;
        if (mincore(base, P, &v) != -1 || errno != ENOMEM)
            _exit(3);
        if (mincore(base + 2 * P, P, &v) != -1 || errno != ENOMEM)
            _exit(4);
        _exit(0);
    }
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(munmap(base, P) == 0 && munmap(base + 2 * P, P) == 0);

    // MADV_REMOVE across [shared_p0, gap, shared_p2]: removes both, returns ENOMEM.
    int fd = memfd_create("mm_fixes_remove", 0);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, (off_t)(3 * P)) == 0);
    char *sh = mmap(NULL, 3 * P, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(sh != MAP_FAILED);
    memset(sh, 'Z', 3 * P);
    CHECK(munmap(sh + P, P) == 0);
    errno = 0;
    CHECK(madvise(sh, 3 * P, MADV_REMOVE) == -1 && errno == ENOMEM);
    CHECK(sh[0] == 0);
    CHECK(sh[2 * P] == 0);

    // MADV_REMOVE across [gap, ro_shared]: EACCES overrides earlier gap ENOMEM.
    char *ro = mmap(NULL, 2 * P, PROT_READ, MAP_SHARED, fd, 0);
    CHECK(ro != MAP_FAILED);
    // Remap a read-only fd at ro + P so may_write is false.
    char fdpath[64];
    snprintf(fdpath, sizeof(fdpath), "/proc/self/fd/%d", fd);
    int rfd = open(fdpath, O_RDONLY);
    CHECK(rfd >= 0);
    CHECK(munmap(ro, 2 * P) == 0);
    char *ro2 = mmap(ro + P, P, PROT_READ, MAP_SHARED | MAP_FIXED, rfd, 0);
    CHECK(ro2 == ro + P);
    errno = 0;
    CHECK(madvise(ro, 2 * P, MADV_REMOVE) == -1 && errno == EACCES);
    CHECK(munmap(ro2, P) == 0);
    close(rfd);

    // MADV_WILLNEED across [shared_p0, gap, shared_p2]: returns ENOMEM.
    errno = 0;
    CHECK(madvise(sh, 3 * P, MADV_WILLNEED) == -1 && errno == ENOMEM);

    CHECK(munmap(sh, P) == 0 && munmap(sh + 2 * P, P) == 0);
    close(fd);
}

static void test_mlock_and_mincore_gaps(size_t P) {
    // Layout: [p0, gap, p2]
    char *m = mmap(NULL, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(m != MAP_FAILED);
    CHECK(munmap(m + P, P) == 0);

    // mlock stops at the first gap: p0 is locked (not populated), p2 is NOT locked, returns ENOMEM.
    errno = 0;
    CHECK(mlock(m, 3 * P) == -1 && errno == ENOMEM);
    unsigned char v0 = 0xaa, v2 = 0xaa;
    CHECK(mincore(m, P, &v0) == 0 && (v0 & 1) == 0);
    CHECK(mincore(m + 2 * P, P, &v2) == 0 && (v2 & 1) == 0);
    errno = 0;
    CHECK(madvise(m, P, MADV_DONTNEED) == -1 && errno == EINVAL);
    CHECK(madvise(m + 2 * P, P, MADV_DONTNEED) == 0);

    // munlock stops at the first gap: p0 unlocked, p2 remains locked, returns ENOMEM.
    CHECK(mlock(m + 2 * P, P) == 0);
    errno = 0;
    CHECK(munlock(m, 3 * P) == -1 && errno == ENOMEM);
    CHECK(madvise(m, P, MADV_DONTNEED) == 0);
    errno = 0;
    CHECK(madvise(m + 2 * P, P, MADV_DONTNEED) == -1 && errno == EINVAL);
    CHECK(munlock(m + 2 * P, P) == 0);

    // mincore writes chunks up to the first gap, then returns ENOMEM without touching later bytes.
    m[0] = 1;
    m[2 * P] = 2;
    unsigned char vec[3] = {0xaa, 0xaa, 0xaa};
    errno = 0;
    CHECK(mincore(m, 3 * P, vec) == -1 && errno == ENOMEM);
    CHECK((vec[0] & 1) == 1);
    CHECK(vec[1] == 0xaa);
    CHECK(vec[2] == 0xaa);

    // mincore with NULL vec: EFAULT when start is mapped, ENOMEM when start is unmapped.
    errno = 0;
    CHECK(mincore(m, 2 * P, NULL) == -1 && errno == EFAULT);
    errno = 0;
    CHECK(mincore(m + P, 2 * P, NULL) == -1 && errno == ENOMEM);

    CHECK(munmap(m, P) == 0 && munmap(m + 2 * P, P) == 0);
}

static void test_msync_gaps(size_t P) {
    int fd = memfd_create("mm_fixes_msync", 0);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, (off_t)(3 * P)) == 0);
    char *s = mmap(NULL, 3 * P, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(s != MAP_FAILED);
    CHECK(munmap(s + P, P) == 0);
    s[0] = 'a';
    s[2 * P] = 'b';

    // msync(MS_SYNC) across [p0, gap, p2]: syncs mapped pages and returns ENOMEM.
    errno = 0;
    CHECK(msync(s, 3 * P, MS_SYNC) == -1 && errno == ENOMEM);

    // msync(MS_INVALIDATE) across [gap, locked p2]: EBUSY overrides earlier gap ENOMEM.
    CHECK(mlock(s + 2 * P, P) == 0);
    errno = 0;
    CHECK(msync(s + P, 2 * P, MS_INVALIDATE) == -1 && errno == EBUSY);
    errno = 0;
    CHECK(msync(s, 3 * P, MS_INVALIDATE | MS_SYNC) == -1 && errno == EBUSY);
    CHECK(munlock(s + 2 * P, P) == 0);

    // Zero-length msync on an unmapped address succeeds.
    CHECK(msync(s + P, 0, MS_SYNC) == 0);

    CHECK(munmap(s, P) == 0 && munmap(s + 2 * P, P) == 0);
    close(fd);
}

struct FutexRaceCtx {
    _Atomic(uint32_t *) target;
    _Atomic int stop;
};

static void *futex_hammer(void *arg) {
    struct FutexRaceCtx *ctx = arg;
    while (!atomic_load_explicit(&ctx->stop, memory_order_acquire)) {
        uint32_t *p = atomic_load_explicit(&ctx->target, memory_order_acquire);
        if (p) {
            // Hammer FUTEX_WAKE (shared, not FUTEX_PRIVATE_FLAG) without holding any user lock.
            (void)sys_futex(p, FUTEX_WAKE, 1);
        }
    }
    return NULL;
}

static void verify_cross_proc_futex(uint32_t *slot) {
    atomic_store_explicit((_Atomic uint32_t *)slot, 0, memory_order_release);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        // Wait until parent publishes 1, then wake parent with 2 via shared futex.
        while (atomic_load_explicit((_Atomic uint32_t *)slot, memory_order_acquire) == 0) {
            (void)sys_futex(slot, FUTEX_WAIT, 0);
        }
        atomic_store_explicit((_Atomic uint32_t *)slot, 2, memory_order_release);
        (void)sys_futex(slot, FUTEX_WAKE, 1);
        _exit(0);
    }
    atomic_store_explicit((_Atomic uint32_t *)slot, 1, memory_order_release);
    (void)sys_futex(slot, FUTEX_WAKE, 1);
    while (atomic_load_explicit((_Atomic uint32_t *)slot, memory_order_acquire) != 2) {
        (void)sys_futex(slot, FUTEX_WAIT, 1);
    }
    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void test_shared_futex_maps(size_t P) {
    // 1. MAP_SHARED | MAP_ANONYMOUS + mremap move while a helper thread hammers futex.
    struct FutexRaceCtx ctx;
    atomic_init(&ctx.target, NULL);
    atomic_init(&ctx.stop, 0);
    pthread_t th;
    CHECK(pthread_create(&th, NULL, futex_hammer, &ctx) == 0);

    for (int i = 0; i < 8; i++) {
        uint32_t *p = mmap(NULL, P, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        CHECK(p != MAP_FAILED);
        atomic_store_explicit(&ctx.target, p, memory_order_release);
        verify_cross_proc_futex(p);

        // Move the shared mapping with mremap(MREMAP_MAYMOVE | MREMAP_FIXED) and verify again.
        void *reserve = mmap(NULL, P, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(reserve != MAP_FAILED);
        uint32_t *moved = mremap(p, P, P, MREMAP_MAYMOVE | MREMAP_FIXED, reserve);
        CHECK(moved == reserve);
        atomic_store_explicit(&ctx.target, moved, memory_order_release);
        verify_cross_proc_futex(moved);

        atomic_store_explicit(&ctx.target, NULL, memory_order_release);
        CHECK(munmap(moved, P) == 0);
    }

    // 2. SysV shmat while the helper thread is active.
    int shmid = shmget(IPC_PRIVATE, P, IPC_CREAT | 0600);
    CHECK(shmid >= 0);
    uint32_t *shm = shmat(shmid, NULL, 0);
    CHECK(shm != (void *)-1);
    CHECK(shmctl(shmid, IPC_RMID, NULL) == 0);
    atomic_store_explicit(&ctx.target, shm, memory_order_release);
    verify_cross_proc_futex(shm);
    atomic_store_explicit(&ctx.target, NULL, memory_order_release);
    CHECK(shmdt(shm) == 0);

    atomic_store_explicit(&ctx.stop, 1, memory_order_release);
    CHECK(pthread_join(th, NULL) == 0);
}

int main(void) {
    size_t P = (size_t)sysconf(_SC_PAGESIZE);
    test_madvise_gaps(P);
    test_mlock_and_mincore_gaps(P);
    test_msync_gaps(P);
    test_shared_futex_maps(P);
    puts("all ok");
    return 0;
}
