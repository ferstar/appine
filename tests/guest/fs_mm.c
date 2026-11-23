/*
 * Regression tests for Package 2 (mount/VFS/mm/mmu/oom/exec/engine):
 * - fs.protected_symlinks applies to trailing-slash symlink paths (`link/`);
 * - access/faccessat(X_OK) honours noexec on regular files (while allowing directories);
 * - tmpfs keeps unlinked open files charged until last close, handles truncate/write without
 *   drift or underflow, and frees pages/inodes immediately on close;
 * - umount2(MNT_FORCE) requires initial CAP_SYS_ADMIN (EPERM in userns);
 * - umount2 returns EBUSY for mapped files, running executables, and open mqueue-mount fds;
 * - detached FUSE mount held only by an O_PATH fd aborts immediately on close;
 * - unattached open_tree(OPEN_TREE_CLONE) of a shared mount is excluded from mount propagation
 *   and torn down immediately on close;
 * - shmat below vm.mmap_min_addr without CAP_SYS_RAWIO returns EPERM;
 * - mremap updates moved SysV shm attach base (shmdt at new addr succeeds) and moved vDSO
 *   address ([vdso] label, MADV_DONTNEED refill, default signal restorer);
 * - /proc/self/pagemap clears MAP_EXCLUSIVE on CoW-lent pages in parent while child lives;
 * - FUSE on-demand mapping short reads do not mark unread tail pages as filled zeros;
 * - execve returns ETXTBSY for writable nested `#!` scripts and writable ELF PT_INTERP;
 * - traced execve of a doomed ELF dies by SIGSEGV without reporting PTRACE_EVENT_EXEC;
 * - pure Tier 0 single-stepped `ic ivau` on guest page 0 invalidates Tier 1 translations.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/ptrace.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
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

#ifndef MNT_FORCE
#define MNT_FORCE 1
#endif
#ifndef MNT_DETACH
#define MNT_DETACH 2
#endif
#ifndef OPEN_TREE_CLONE
#define OPEN_TREE_CLONE 1
#endif
#ifndef OPEN_TREE_CLOEXEC
#define OPEN_TREE_CLOEXEC O_CLOEXEC
#endif
#ifndef MOVE_MOUNT_F_EMPTY_PATH
#define MOVE_MOUNT_F_EMPTY_PATH 0x00000004
#endif
#ifndef SYS_open_tree
#define SYS_open_tree 428
#endif
#ifndef SYS_move_mount
#define SYS_move_mount 429
#endif
#ifndef PTRACE_O_TRACEEXEC
#define PTRACE_O_TRACEEXEC 0x00000010
#endif
#ifndef PTRACE_EVENT_EXEC
#define PTRACE_EVENT_EXEC 4
#endif
#ifndef NT_PRSTATUS
#define NT_PRSTATUS 1
#endif

static void write_file_bytes(const char *path, const void *buf, size_t len, mode_t mode)
{
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, mode);
    CHECK(fd >= 0, "open %s", path);
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, (const uint8_t *)buf + off, len - off);
        CHECK(n > 0, "write %s", path);
        off += (size_t)n;
    }
    close(fd);
}

static void write_proc_str(const char *path, const char *s)
{
    int fd = open(path, O_WRONLY);
    CHECK(fd >= 0, "open %s for write", path);
    ssize_t n = write(fd, s, strlen(s));
    CHECK(n == (ssize_t)strlen(s), "write %s", path);
    close(fd);
}

static void wait_ok(pid_t pid, const char *label)
{
    int st = 0;
    CHECK(waitpid(pid, &st, 0) == pid, "waitpid %s", label);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "%s failed (status=0x%x)", label, st);
}

static void copy_file(const char *src, const char *dst, mode_t mode)
{
    int in = open(src, O_RDONLY);
    CHECK(in >= 0, "open %s", src);
    int out = open(dst, O_WRONLY | O_CREAT | O_TRUNC, mode);
    CHECK(out >= 0, "create %s", dst);
    uint8_t buf[16384];
    for (;;) {
        ssize_t n = read(in, buf, sizeof(buf));
        CHECK(n >= 0, "read %s", src);
        if (n == 0)
            break;
        ssize_t off = 0;
        while (off < n) {
            ssize_t w = write(out, buf + off, (size_t)(n - off));
            CHECK(w > 0, "write %s", dst);
            off += w;
        }
    }
    close(in);
    close(out);
}

/* 1. fs.protected_symlinks applies to `link/` with trailing slash */
static void test_protected_symlinks_trailing_slash(void)
{
    CHECK(mkdir("/tmp/prot_dir", 01777) == 0, "mkdir prot_dir");
    CHECK(chmod("/tmp/prot_dir", 01777) == 0, "chmod 1777");
    CHECK(mkdir("/tmp/prot_target", 0755) == 0, "mkdir prot_target");

    pid_t creator = fork();
    CHECK(creator >= 0, "fork creator");
    if (creator == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0 && setresuid(1000, 1000, 1000) == 0, "setuid 1000");
        CHECK(symlink("/tmp/prot_target", "/tmp/prot_dir/link") == 0, "symlink");
        _exit(0);
    }
    wait_ok(creator, "symlink creator");

    pid_t other = fork();
    CHECK(other >= 0, "fork other");
    if (other == 0) {
        CHECK(setresgid(2000, 2000, 2000) == 0 && setresuid(2000, 2000, 2000) == 0, "setuid 2000");
        struct stat st;
        errno = 0;
        CHECK(stat("/tmp/prot_dir/link", &st) < 0 && errno == EACCES,
              "stat link must be EACCES");
        errno = 0;
        CHECK(lstat("/tmp/prot_dir/link/", &st) < 0 && errno == EACCES,
              "lstat link/ must be EACCES");
        errno = 0;
        CHECK(stat("/tmp/prot_dir/link/", &st) < 0 && errno == EACCES,
              "stat link/ must be EACCES");
        _exit(0);
    }
    wait_ok(other, "other user symlink/");

    pid_t owner = fork();
    CHECK(owner >= 0, "fork owner");
    if (owner == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0 && setresuid(1000, 1000, 1000) == 0, "setuid 1000");
        struct stat st;
        CHECK(stat("/tmp/prot_dir/link/", &st) == 0 && S_ISDIR(st.st_mode),
              "owner stat link/ must succeed");
        _exit(0);
    }
    wait_ok(owner, "owner symlink/");

    unlink("/tmp/prot_dir/link");
    rmdir("/tmp/prot_dir");
    rmdir("/tmp/prot_target");
}

/* 2. access/faccessat(X_OK) honours MS_NOEXEC on regular files (but not directories) */
static void test_access_noexec(void)
{
    CHECK(mkdir("/tmp/noexec_mnt", 0755) == 0, "mkdir noexec_mnt");
    CHECK(mount("tmpfs", "/tmp/noexec_mnt", "tmpfs", MS_NOEXEC, "size=64k") == 0,
          "mount noexec tmpfs");
    write_file_bytes("/tmp/noexec_mnt/prog", "hi", 2, 0755);
    CHECK(mkdir("/tmp/noexec_mnt/sub", 0755) == 0, "mkdir sub");

    errno = 0;
    CHECK(access("/tmp/noexec_mnt/prog", X_OK) < 0 && errno == EACCES,
          "access(X_OK) on noexec regular file must be EACCES");
    errno = 0;
    CHECK(faccessat(AT_FDCWD, "/tmp/noexec_mnt/prog", X_OK, 0) < 0 && errno == EACCES,
          "faccessat(X_OK) on noexec regular file must be EACCES");
    CHECK(access("/tmp/noexec_mnt/prog", R_OK | W_OK) == 0,
          "access(R_OK|W_OK) on noexec file must succeed");
    CHECK(access("/tmp/noexec_mnt/sub", X_OK) == 0,
          "access(X_OK) on noexec directory must succeed");

    CHECK(umount2("/tmp/noexec_mnt", 0) == 0, "umount noexec_mnt");
    rmdir("/tmp/noexec_mnt");
}

/* 3. tmpfs unlinked open file accounting: no drift, no double-uncharge, freed on close */
static void test_tmpfs_unlinked_open_accounting(void)
{
    CHECK(mkdir("/tmp/tmpfs_acct", 0755) == 0, "mkdir tmpfs_acct");
    CHECK(mount("tmpfs", "/tmp/tmpfs_acct", "tmpfs", 0, "size=16k,nr_inodes=4") == 0,
          "mount limited tmpfs");

    int fa = open("/tmp/tmpfs_acct/a", O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(fa >= 0, "open a");
    uint8_t page4[16384];
    memset(page4, 0x5a, sizeof(page4));
    CHECK(write(fa, page4, sizeof(page4)) == (ssize_t)sizeof(page4), "write 16k to a");

    int fb = open("/tmp/tmpfs_acct/b", O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(fb >= 0, "open b");
    errno = 0;
    CHECK(write(fb, page4, 4096) < 0 && errno == ENOSPC, "tmpfs should be full");

    /* Unlink a while still open: pages and inode must stay charged */
    CHECK(unlink("/tmp/tmpfs_acct/a") == 0, "unlink a");
    struct statvfs sv;
    CHECK(statvfs("/tmp/tmpfs_acct", &sv) == 0, "statvfs after unlink");
    CHECK(sv.f_bfree == 0, "unlinked open file must keep 4 pages charged (bfree=%lu)",
          (unsigned long)sv.f_bfree);
    CHECK(sv.f_ffree == 1, "unlinked open file must keep inode charged (ffree=%lu)",
          (unsigned long)sv.f_ffree);
    errno = 0;
    CHECK(write(fb, page4, 4096) < 0 && errno == ENOSPC,
          "write to b must still hit ENOSPC while unlinked a holds all 4 pages");

    /* Truncate unlinked open file to 0: frees its 4 pages cleanly without underflow */
    CHECK(ftruncate(fa, 0) == 0, "ftruncate unlinked a to 0");
    CHECK(statvfs("/tmp/tmpfs_acct", &sv) == 0, "statvfs after ftruncate");
    CHECK(sv.f_bfree == 4, "ftruncate(0) on unlinked file must free 4 pages (bfree=%lu)",
          (unsigned long)sv.f_bfree);

    /* Write 2 pages to unlinked a and 2 pages to b: fills all 4 pages, no bypass */
    CHECK(pwrite(fa, page4, 8192, 0) == 8192, "write 8k to unlinked a");
    CHECK(pwrite(fb, page4, 8192, 0) == 8192, "write 8k to b");
    errno = 0;
    CHECK(pwrite(fb, page4, 4096, 8192) < 0 && errno == ENOSPC,
          "b must not exceed tmpfs limit alongside unlinked a");

    /* Close unlinked a: its 2 pages and 1 inode are freed immediately */
    close(fa);
    CHECK(statvfs("/tmp/tmpfs_acct", &sv) == 0, "statvfs after close a");
    CHECK(sv.f_bfree == 2, "closing unlinked a must free 2 pages (bfree=%lu)",
          (unsigned long)sv.f_bfree);
    CHECK(sv.f_ffree == 2, "closing unlinked a must free 1 inode (ffree=%lu)",
          (unsigned long)sv.f_ffree);
    CHECK(pwrite(fb, page4, 8192, 8192) == 8192, "b can now use freed 8k");

    close(fb);
    CHECK(umount2("/tmp/tmpfs_acct", 0) == 0, "umount tmpfs_acct");
    rmdir("/tmp/tmpfs_acct");
}

/* 4. umount2(MNT_FORCE) cap check + EBUSY on mappings, running executables, mqueue fds */
static void test_mount_busy_and_force_cap(const char *self_path)
{
    /* Part A: umount2(MNT_FORCE) in a user namespace without init CAP_SYS_ADMIN -> EPERM */
    pid_t unpriv = fork();
    CHECK(unpriv >= 0, "fork userns");
    if (unpriv == 0) {
        CHECK(unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0, "unshare user+mnt");
        write_proc_str("/proc/self/setgroups", "deny\n");
        write_proc_str("/proc/self/uid_map", "0 0 1\n");
        write_proc_str("/proc/self/gid_map", "0 0 1\n");
        CHECK(mkdir("/tmp/umnt_force", 0755) == 0 || errno == EEXIST, "mkdir umnt_force");
        CHECK(mount("tmpfs", "/tmp/umnt_force", "tmpfs", 0, "size=64k") == 0, "mount in userns");
        errno = 0;
        CHECK(umount2("/tmp/umnt_force", MNT_FORCE) < 0 && errno == EPERM,
              "MNT_FORCE without init CAP_SYS_ADMIN must be EPERM");
        CHECK(umount2("/tmp/umnt_force", 0) == 0, "plain umount2 in userns must succeed");
        _exit(0);
    }
    wait_ok(unpriv, "userns MNT_FORCE");
    rmdir("/tmp/umnt_force");

    /* Part B: mapped file keeps mount busy even after fd is closed */
    CHECK(mkdir("/tmp/umnt_map", 0755) == 0, "mkdir umnt_map");
    CHECK(mount("tmpfs", "/tmp/umnt_map", "tmpfs", 0, "size=64k") == 0, "mount umnt_map");
    write_file_bytes("/tmp/umnt_map/f", "hello", 5, 0644);
    int fd = open("/tmp/umnt_map/f", O_RDONLY);
    CHECK(fd >= 0, "open mapped file");
    void *p = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(p != MAP_FAILED, "mmap file");
    close(fd);
    errno = 0;
    CHECK(umount2("/tmp/umnt_map", 0) < 0 && errno == EBUSY,
          "umount2 must return EBUSY while file is mapped");
    CHECK(munmap(p, 4096) == 0, "munmap");
    CHECK(umount2("/tmp/umnt_map", 0) == 0, "umount2 after munmap must succeed");
    rmdir("/tmp/umnt_map");

    /* Part C: running executable keeps mount busy */
    CHECK(mkdir("/tmp/umnt_exe", 0755) == 0, "mkdir umnt_exe");
    CHECK(mount("tmpfs", "/tmp/umnt_exe", "tmpfs", 0, "size=4m") == 0, "mount umnt_exe");
    copy_file(self_path, "/tmp/umnt_exe/self", 0755);
    int ready[2], stop[2];
    CHECK(pipe(ready) == 0 && pipe(stop) == 0, "pipe");
    pid_t runner = fork();
    CHECK(runner >= 0, "fork runner");
    if (runner == 0) {
        close(ready[0]);
        close(stop[1]);
        char rfd[16], sfd[16];
        snprintf(rfd, sizeof(rfd), "%d", ready[1]);
        snprintf(sfd, sizeof(sfd), "%d", stop[0]);
        char *argv[] = {"/tmp/umnt_exe/self", "--hold", rfd, sfd, NULL};
        execv("/tmp/umnt_exe/self", argv);
        _exit(99);
    }
    close(ready[1]);
    close(stop[0]);
    char ch = 0;
    CHECK(read(ready[0], &ch, 1) == 1, "wait runner exec");
    close(ready[0]);
    errno = 0;
    CHECK(umount2("/tmp/umnt_exe", 0) < 0 && errno == EBUSY,
          "umount2 must return EBUSY while executable on mount is running");
    CHECK(write(stop[1], "x", 1) == 1, "tell runner exit");
    close(stop[1]);
    wait_ok(runner, "runner exit");
    CHECK(umount2("/tmp/umnt_exe", 0) == 0, "umount2 after executable exits must succeed");
    rmdir("/tmp/umnt_exe");

    /* Part D: fd opened through an mqueue mount keeps that mount busy */
    CHECK(mkdir("/tmp/umnt_mq", 0755) == 0, "mkdir umnt_mq");
    CHECK(mount("mqueue", "/tmp/umnt_mq", "mqueue", 0, NULL) == 0, "mount mqueue");
    int mqfd = open("/tmp/umnt_mq/q_busy", O_RDWR | O_CREAT, 0600);
    CHECK(mqfd >= 0, "open mqueue file");
    errno = 0;
    CHECK(umount2("/tmp/umnt_mq", 0) < 0 && errno == EBUSY,
          "umount2 must return EBUSY while mqueue file is open through mount");
    close(mqfd);
    unlink("/tmp/umnt_mq/q_busy");
    CHECK(umount2("/tmp/umnt_mq", 0) == 0, "umount2 mqueue after close must succeed");
    rmdir("/tmp/umnt_mq");
}

/* Minimal FUSE daemon shared by tests 5 and 8 */
struct fuse_in_hdr {
    uint32_t len;
    uint32_t opcode;
    uint64_t unique;
    uint64_t nodeid;
    uint32_t uid;
    uint32_t gid;
    uint32_t pid;
    uint16_t total_extlen;
    uint16_t padding;
};

struct fuse_out_hdr {
    uint32_t len;
    int32_t error;
    uint64_t unique;
};

struct fuse_attr {
    uint64_t ino;
    uint64_t size;
    uint64_t blocks;
    uint64_t atime;
    uint64_t mtime;
    uint64_t ctime;
    uint32_t atimensec;
    uint32_t mtimensec;
    uint32_t ctimensec;
    uint32_t mode;
    uint32_t nlink;
    uint32_t uid;
    uint32_t gid;
    uint32_t rdev;
    uint32_t blksize;
    uint32_t flags;
};

struct fuse_entry_out {
    uint64_t nodeid;
    uint64_t generation;
    uint64_t entry_valid;
    uint64_t attr_valid;
    uint32_t entry_valid_nsec;
    uint32_t attr_valid_nsec;
    struct fuse_attr attr;
};

struct fuse_attr_out {
    uint64_t attr_valid;
    uint32_t attr_valid_nsec;
    uint32_t dummy;
    struct fuse_attr attr;
};

struct fuse_open_out {
    uint64_t fh;
    uint32_t open_flags;
    uint32_t padding;
};

struct fuse_read_in {
    uint64_t fh;
    uint64_t offset;
    uint32_t size;
    uint32_t read_flags;
    uint64_t lock_owner;
    uint32_t flags;
    uint32_t padding;
};

struct fuse_ctx {
    int devfd;
    uint64_t file_size;
    size_t max_reply; /* cap each FUSE_READ reply to simulate short reads */
    volatile int aborted;
    volatile int read_ops;
};

static void *fuse_short_daemon(void *arg)
{
    struct fuse_ctx *ctx = (struct fuse_ctx *)arg;
    uint8_t buf[16384];
    uint8_t out[16384];
    for (;;) {
        ssize_t n = read(ctx->devfd, buf, sizeof(buf));
        if (n < (ssize_t)sizeof(struct fuse_in_hdr)) {
            ctx->aborted = 1;
            break;
        }
        struct fuse_in_hdr in;
        memcpy(&in, buf, sizeof(in));

        memset(out, 0, sizeof(struct fuse_out_hdr));
        struct fuse_out_hdr *hdr = (struct fuse_out_hdr *)out;
        hdr->unique = in.unique;

        if (in.opcode == 26 /* FUSE_INIT */) {
            uint32_t init_out[16];
            memset(init_out, 0, sizeof(init_out));
            init_out[0] = 7;
            init_out[1] = 37;
            init_out[6] = 65536; /* max_write */
            hdr->len = (uint32_t)(sizeof(*hdr) + sizeof(init_out));
            memcpy(out + sizeof(*hdr), init_out, sizeof(init_out));
        } else if (in.opcode == 3 /* FUSE_GETATTR */) {
            struct fuse_attr_out a;
            memset(&a, 0, sizeof(a));
            a.attr.ino = in.nodeid;
            if (in.nodeid == 1) {
                a.attr.mode = S_IFDIR | 0755;
                a.attr.nlink = 2;
            } else {
                a.attr.mode = S_IFREG | 0444;
                a.attr.nlink = 1;
                a.attr.size = ctx->file_size;
            }
            hdr->len = (uint32_t)(sizeof(*hdr) + sizeof(a));
            memcpy(out + sizeof(*hdr), &a, sizeof(a));
        } else if (in.opcode == 1 /* FUSE_LOOKUP */) {
            const char *name = (const char *)(buf + sizeof(in));
            if (strcmp(name, "data") == 0) {
                struct fuse_entry_out e;
                memset(&e, 0, sizeof(e));
                e.nodeid = 2;
                e.entry_valid = 60;
                e.attr_valid = 60;
                e.attr.ino = 2;
                e.attr.mode = S_IFREG | 0444;
                e.attr.nlink = 1;
                e.attr.size = ctx->file_size;
                hdr->len = (uint32_t)(sizeof(*hdr) + sizeof(e));
                memcpy(out + sizeof(*hdr), &e, sizeof(e));
            } else {
                hdr->len = (uint32_t)sizeof(*hdr);
                hdr->error = -ENOENT;
            }
        } else if (in.opcode == 14 /* FUSE_OPEN */) {
            struct fuse_open_out o;
            memset(&o, 0, sizeof(o));
            o.fh = 1;
            hdr->len = (uint32_t)(sizeof(*hdr) + sizeof(o));
            memcpy(out + sizeof(*hdr), &o, sizeof(o));
        } else if (in.opcode == 15 /* FUSE_READ */) {
            ctx->read_ops++;
            struct fuse_read_in ri;
            memcpy(&ri, buf + sizeof(in), sizeof(ri));
            size_t avail = 0;
            if (ri.offset < ctx->file_size)
                avail = (size_t)(ctx->file_size - ri.offset);
            size_t want = ri.size;
            if (want > avail)
                want = avail;
            if (ctx->max_reply > 0 && want > ctx->max_reply)
                want = ctx->max_reply;
            if (want > sizeof(out) - sizeof(*hdr))
                want = sizeof(out) - sizeof(*hdr);
            for (size_t i = 0; i < want; i++) {
                uint64_t page = (ri.offset + i) / 4096;
                out[sizeof(*hdr) + i] = (uint8_t)(0x40 + (page & 0x3f));
            }
            hdr->len = (uint32_t)(sizeof(*hdr) + want);
        } else if (in.opcode == 18 /* FUSE_RELEASE */ || in.opcode == 25 /* FUSE_FLUSH */) {
            hdr->len = (uint32_t)sizeof(*hdr);
        } else {
            hdr->len = (uint32_t)sizeof(*hdr);
            hdr->error = -ENOSYS;
        }
        (void)write(ctx->devfd, out, hdr->len);
    }
    return NULL;
}

/* 5. Detached FUSE mount held only by O_PATH fd aborts on close + open_tree propagation isolation */
static void test_mount_collection_and_open_tree_propagation(void)
{
    /* Part A: O_PATH fd on lazily unmounted FUSE mount triggers collection right at close() */
    CHECK(mkdir("/tmp/fuse_opath", 0755) == 0, "mkdir fuse_opath");
    int devfd = open("/dev/fuse", O_RDWR);
    CHECK(devfd >= 0, "open /dev/fuse");
    struct fuse_ctx ctx = {.devfd = devfd, .file_size = 4096, .max_reply = 0, .aborted = 0};
    char opts[128];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=40755,user_id=0,group_id=0", devfd);
    CHECK(mount("f", "/tmp/fuse_opath", "fuse", 0, opts) == 0, "mount fuse");
    pthread_t th;
    CHECK(pthread_create(&th, NULL, fuse_short_daemon, &ctx) == 0, "spawn fuse daemon");
    int pfd = open("/tmp/fuse_opath/data", O_PATH);
    CHECK(pfd >= 0, "open O_PATH on fuse");
    CHECK(umount2("/tmp/fuse_opath", MNT_DETACH) == 0, "lazy umount fuse");
    CHECK(!ctx.aborted, "fuse must stay alive while O_PATH fd is open");
    close(pfd);
    /* Daemon must abort promptly without any unrelated umount2 or thread exit */
    pthread_join(th, NULL);
    CHECK(ctx.aborted == 1, "closing last O_PATH fd must abort detached FUSE mount");
    close(devfd);
    rmdir("/tmp/fuse_opath");

    /* Part B: unattached open_tree(OPEN_TREE_CLONE) of a shared mount must not receive
     * propagated mounts (IS_MNT_NEW) and must be torn down immediately on close */
    CHECK(mkdir("/tmp/ot_base", 0755) == 0, "mkdir ot_base");
    CHECK(mkdir("/tmp/ot_dst", 0755) == 0, "mkdir ot_dst");
    CHECK(mount("tmpfs", "/tmp/ot_base", "tmpfs", 0, "size=64k") == 0, "mount ot_base");
    CHECK(mount(NULL, "/tmp/ot_base", NULL, MS_SHARED, NULL) == 0, "make ot_base shared");
    CHECK(mkdir("/tmp/ot_base/sub", 0755) == 0, "mkdir ot_base/sub");

    int treefd = (int)syscall(SYS_open_tree, AT_FDCWD, "/tmp/ot_base",
                              OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC);
    CHECK(treefd >= 0, "open_tree clone");

    /* Mount a tmpfs on /tmp/ot_base/sub while treefd is still unattached */
    CHECK(mount("tmpfs", "/tmp/ot_base/sub", "tmpfs", 0, "size=64k") == 0, "mount sub");
    write_file_bytes("/tmp/ot_base/sub/marker", "1", 1, 0644);

    /* Attach treefd at /tmp/ot_dst: it must NOT contain the propagated sub-mount */
    CHECK(syscall(SYS_move_mount, treefd, "", AT_FDCWD, "/tmp/ot_dst", MOVE_MOUNT_F_EMPTY_PATH) == 0,
          "move_mount clone");
    close(treefd);
    errno = 0;
    CHECK(access("/tmp/ot_dst/sub/marker", F_OK) < 0 && errno == ENOENT,
          "unattached open_tree clone must not have received propagated mount");

    /* Unattached clone closed without attaching must free its mounts immediately (test with FUSE) */
    CHECK(mkdir("/tmp/ot_fuse", 0755) == 0, "mkdir ot_fuse");
    int fdev = open("/dev/fuse", O_RDWR);
    CHECK(fdev >= 0, "open /dev/fuse for open_tree");
    struct fuse_ctx fctx = {.devfd = fdev, .file_size = 4096, .max_reply = 0, .aborted = 0};
    char fopts[128];
    snprintf(fopts, sizeof(fopts), "fd=%d,rootmode=40755,user_id=0,group_id=0", fdev);
    CHECK(mount("f", "/tmp/ot_fuse", "fuse", 0, fopts) == 0, "mount ot_fuse");
    pthread_t fth;
    CHECK(pthread_create(&fth, NULL, fuse_short_daemon, &fctx) == 0, "spawn open_tree fuse");
    int ftree = (int)syscall(SYS_open_tree, AT_FDCWD, "/tmp/ot_fuse",
                             OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC);
    CHECK(ftree >= 0, "open_tree clone fuse");
    CHECK(umount2("/tmp/ot_fuse", MNT_DETACH) == 0, "detach ot_fuse");
    CHECK(!fctx.aborted, "fuse must stay alive while unattached open_tree clone holds it");
    close(ftree);
    pthread_join(fth, NULL);
    CHECK(fctx.aborted == 1, "closing unattached open_tree clone must abort FUSE mount");
    close(fdev);
    rmdir("/tmp/ot_fuse");

    CHECK(umount2("/tmp/ot_base/sub", 0) == 0, "umount sub");
    CHECK(umount2("/tmp/ot_dst", 0) == 0, "umount ot_dst");
    CHECK(umount2("/tmp/ot_base", 0) == 0, "umount ot_base");
    rmdir("/tmp/ot_base");
    rmdir("/tmp/ot_dst");
}

/* 6. shmat below mmap_min_addr + mremap updates SysV shm attach base and vDSO address */
static volatile sig_atomic_t got_usr1 = 0;
static void on_usr1(int sig)
{
    (void)sig;
    got_usr1 = 1;
}

static int find_vdso_range(uintptr_t *start_out, uintptr_t *end_out)
{
    FILE *f = fopen("/proc/self/maps", "r");
    CHECK(f != NULL, "fopen /proc/self/maps");
    char line[512];
    int found = 0;
    while (fgets(line, sizeof(line), f)) {
        if (strstr(line, "[vdso]")) {
            unsigned long s = 0, e = 0;
            if (sscanf(line, "%lx-%lx", &s, &e) == 2) {
                *start_out = (uintptr_t)s;
                *end_out = (uintptr_t)e;
                found = 1;
                break;
            }
        }
    }
    fclose(f);
    return found;
}

static void test_mremap_shm_vdso_and_shmat_min_addr(void)
{
    /* Part A: shmat below vm.mmap_min_addr without CAP_SYS_RAWIO -> EPERM */
    int shmid = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0666);
    CHECK(shmid >= 0, "shmget");

    pid_t unpriv = fork();
    CHECK(unpriv >= 0, "fork shmat unpriv");
    if (unpriv == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0 && setresuid(1000, 1000, 1000) == 0, "drop priv");
        errno = 0;
        void *r = shmat(shmid, (void *)0x1000, 0);
        CHECK(r == (void *)-1 && errno == EPERM,
              "shmat below mmap_min_addr must return EPERM (got %p errno=%d)", r, errno);
        _exit(0);
    }
    wait_ok(unpriv, "shmat min_addr");

    /* Part B: mremap moves SysV shm attach base so shmdt(new_addr) works */
    void *a = shmat(shmid, NULL, 0);
    CHECK(a != (void *)-1, "shmat");
    *(volatile uint32_t *)a = 0x12345678u;
    void *slot = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(slot != MAP_FAILED, "reserve mremap dst");
    void *b = mremap(a, 4096, 4096, MREMAP_MAYMOVE | MREMAP_FIXED, slot);
    CHECK(b == slot, "mremap shm");
    CHECK(*(volatile uint32_t *)b == 0x12345678u, "moved shm contents");
    errno = 0;
    CHECK(shmdt(a) < 0 && errno == EINVAL, "shmdt at old addr must fail EINVAL");
    CHECK(shmdt(b) == 0, "shmdt at new mremap addr must succeed");
    CHECK(shmctl(shmid, IPC_RMID, NULL) == 0, "shmctl IPC_RMID");

    /* Part C: mremap moves vDSO and keeps [vdso] label, MADV_DONTNEED refill, and signal restorer */
    pid_t vdso_child = fork();
    CHECK(vdso_child >= 0, "fork vdso child");
    if (vdso_child == 0) {
        uintptr_t v0 = 0, v0_end = 0;
        CHECK(find_vdso_range(&v0, &v0_end), "find initial [vdso]");
        size_t vlen = (size_t)(v0_end - v0);
        void *dst = mmap(NULL, vlen, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(dst != MAP_FAILED, "reserve vdso dst");
        void *moved = mremap((void *)v0, vlen, vlen, MREMAP_MAYMOVE | MREMAP_FIXED, dst);
        CHECK(moved == dst, "mremap vdso");

        uintptr_t v1 = 0, v1_end = 0;
        CHECK(find_vdso_range(&v1, &v1_end) && v1 == (uintptr_t)dst,
              "[vdso] label must follow mremap (got 0x%lx expected 0x%lx)",
              (unsigned long)v1, (unsigned long)(uintptr_t)dst);
        /* MADV_DONTNEED must refill the moved vDSO instead of zeroing it */
        CHECK(madvise(moved, vlen, MADV_DONTNEED) == 0, "madvise DONTNEED moved vdso");
        CHECK(memcmp(moved, "\x7f""ELF", 4) == 0, "moved vdso must retain ELF header after DONTNEED");
        /* Default signal restorer (no SA_RESTORER via raw rt_sigaction) must use moved vDSO */
        struct {
            void (*sa_handler_fn)(int);
            unsigned long sa_flags;
            void (*sa_restorer)(void);
            uint64_t sa_mask;
        } ksa;
        memset(&ksa, 0, sizeof(ksa));
        ksa.sa_handler_fn = on_usr1;
        CHECK(syscall(SYS_rt_sigaction, SIGUSR1, &ksa, NULL, sizeof(ksa.sa_mask)) == 0,
              "raw rt_sigaction SIGUSR1");
        got_usr1 = 0;
        CHECK(raise(SIGUSR1) == 0, "raise SIGUSR1");
        CHECK(got_usr1 == 1, "signal handler returned cleanly via moved vDSO restorer");
        _exit(0);
    }
    wait_ok(vdso_child, "moved vdso child");
}

/* 7. /proc/self/pagemap clears MAP_EXCLUSIVE (bit 56) on CoW-lent pages */
static uint64_t read_pagemap_entry(void *addr)
{
    int fd = open("/proc/self/pagemap", O_RDONLY);
    CHECK(fd >= 0, "open pagemap");
    uint64_t entry = 0;
    off_t off = (off_t)(((uintptr_t)addr / 4096) * 8);
    CHECK(pread(fd, &entry, sizeof(entry), off) == (ssize_t)sizeof(entry), "pread pagemap");
    close(fd);
    return entry;
}

static void test_pagemap_cow_lent_not_exclusive(void)
{
    const uint64_t PRESENT = 1ULL << 63;
    const uint64_t EXCLUSIVE = 1ULL << 56;
    uint8_t *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED, "mmap anon");
    p[0] = 0x42;
    uint64_t e0 = read_pagemap_entry(p);
    CHECK((e0 & (PRESENT | EXCLUSIVE)) == (PRESENT | EXCLUSIVE),
          "sole owner page must be PRESENT|EXCLUSIVE (got 0x%llx)", (unsigned long long)e0);

    int hold[2], ready[2];
    CHECK(pipe(hold) == 0 && pipe(ready) == 0, "pipe");
    pid_t child = fork();
    CHECK(child >= 0, "fork cow child");
    if (child == 0) {
        close(hold[1]);
        close(ready[0]);
        CHECK(p[0] == 0x42, "child reads lent page");
        CHECK(write(ready[1], "1", 1) == 1, "child ready");
        close(ready[1]);
        char c;
        (void)read(hold[0], &c, 1);
        close(hold[0]);
        _exit(0);
    }
    close(hold[0]);
    close(ready[1]);
    char c;
    CHECK(read(ready[0], &c, 1) == 1, "wait child");
    close(ready[0]);

    uint64_t e1 = read_pagemap_entry(p);
    CHECK((e1 & PRESENT) != 0 && (e1 & EXCLUSIVE) == 0,
          "lent page must NOT report EXCLUSIVE while child holds it (got 0x%llx)",
          (unsigned long long)e1);

    CHECK(write(hold[1], "x", 1) == 1, "release child");
    close(hold[1]);
    wait_ok(child, "cow child");

    uint64_t e2 = read_pagemap_entry(p);
    CHECK((e2 & (PRESENT | EXCLUSIVE)) == (PRESENT | EXCLUSIVE),
          "page must regain EXCLUSIVE after child exits (got 0x%llx)", (unsigned long long)e2);
    munmap(p, 4096);
}

/* 8. FUSE short-read on-demand mapping does not permanently zero unread tail pages */
static void test_fuse_short_read_mapping(void)
{
    CHECK(mkdir("/tmp/fuse_short", 0755) == 0, "mkdir fuse_short");
    int devfd = open("/dev/fuse", O_RDWR);
    CHECK(devfd >= 0, "open /dev/fuse");

    /* 64 KiB file (16 pages), daemon caps every FUSE_READ reply to 8 KiB (2 pages) */
    struct fuse_ctx ctx = {
        .devfd = devfd,
        .file_size = 16 * 4096,
        .max_reply = 2 * 4096,
        .aborted = 0,
        .read_ops = 0,
    };
    char opts[128];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=40755,user_id=0,group_id=0", devfd);
    CHECK(mount("f", "/tmp/fuse_short", "fuse", 0, opts) == 0, "mount fuse_short");
    pthread_t th;
    CHECK(pthread_create(&th, NULL, fuse_short_daemon, &ctx) == 0, "spawn fuse short daemon");

    int fd = open("/tmp/fuse_short/data", O_RDONLY);
    CHECK(fd >= 0, "open fuse data");
    uint8_t *m = mmap(NULL, 16 * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(m != MAP_FAILED, "mmap fuse data");
    close(fd);

    /* Touch page 0, then page 2 (beyond the first 8 KiB short reply), then page 15 */
    CHECK(m[0 * 4096] == 0x40, "page 0 mismatch: 0x%02x", m[0 * 4096]);
    CHECK(m[2 * 4096] == 0x42, "page 2 mismatch (short read zeroed tail?): 0x%02x", m[2 * 4096]);
    CHECK(m[15 * 4096] == 0x4f, "page 15 mismatch: 0x%02x", m[15 * 4096]);
    CHECK(ctx.read_ops >= 3, "expected multiple FUSE_READs after short replies, got %d",
          ctx.read_ops);

    munmap(m, 16 * 4096);
    CHECK(umount2("/tmp/fuse_short", 0) == 0, "umount fuse_short");
    pthread_join(th, NULL);
    close(devfd);
    rmdir("/tmp/fuse_short");
}

/* 9. execve returns ETXTBSY for writable nested `#!` script and writable ELF PT_INTERP */
static void test_exec_etxtbsy_script_and_interp(const char *self_path)
{
    /* Part A: nested #! script s1 -> s2 -> self, with s2 open O_RDWR */
    char s2_body[256];
    snprintf(s2_body, sizeof(s2_body), "#!%s --exit-ok\n", self_path);
    write_file_bytes("/tmp/s2.sh", s2_body, strlen(s2_body), 0755);
    const char *s1_body = "#!/tmp/s2.sh\n";
    write_file_bytes("/tmp/s1.sh", s1_body, strlen(s1_body), 0755);

    int wfd = open("/tmp/s2.sh", O_RDWR);
    CHECK(wfd >= 0, "open s2.sh O_RDWR");
    char *argv[] = {"/tmp/s1.sh", NULL};
    char *envp[] = {NULL};
    errno = 0;
    CHECK(execve("/tmp/s1.sh", argv, envp) < 0 && errno == ETXTBSY,
          "execve nested writable script must return ETXTBSY");
    close(wfd);
    unlink("/tmp/s1.sh");
    unlink("/tmp/s2.sh");

    /* Part B: ELF with PT_INTERP pointing to /tmp/dyn_interp while /tmp/dyn_interp is O_RDWR */
    copy_file(self_path, "/tmp/dyn_interp", 0755);
    uint8_t elf[256];
    memset(elf, 0, sizeof(elf));
    const char *interp_path = "/tmp/dyn_interp";
    size_t interp_len = strlen(interp_path) + 1;
    memcpy(elf, "\x7f""ELF", 4);
    elf[4] = 2; /* ELFCLASS64 */
    elf[5] = 1; /* ELFDATA2LSB */
    elf[6] = 1; /* EV_CURRENT */
    uint16_t e_type = 2;    /* ET_EXEC */
    uint16_t e_machine = 183; /* EM_AARCH64 */
    uint32_t e_version = 1;
    uint64_t e_entry = 0x400000;
    uint64_t e_phoff = 64;
    uint16_t e_ehsize = 64;
    uint16_t e_phentsize = 56;
    uint16_t e_phnum = 1;
    memcpy(elf + 16, &e_type, 2);
    memcpy(elf + 18, &e_machine, 2);
    memcpy(elf + 20, &e_version, 4);
    memcpy(elf + 24, &e_entry, 8);
    memcpy(elf + 32, &e_phoff, 8);
    memcpy(elf + 52, &e_ehsize, 2);
    memcpy(elf + 54, &e_phentsize, 2);
    memcpy(elf + 56, &e_phnum, 2);
    uint32_t p_type = 3; /* PT_INTERP */
    uint32_t p_flags = 4;
    uint64_t p_offset = 128;
    uint64_t p_filesz = interp_len;
    uint64_t p_memsz = interp_len;
    memcpy(elf + 64, &p_type, 4);
    memcpy(elf + 68, &p_flags, 4);
    memcpy(elf + 72, &p_offset, 8);
    memcpy(elf + 96, &p_filesz, 8);
    memcpy(elf + 104, &p_memsz, 8);
    memcpy(elf + 128, interp_path, interp_len);
    write_file_bytes("/tmp/with_interp", elf, sizeof(elf), 0755);

    int iwfd = open("/tmp/dyn_interp", O_RDWR);
    CHECK(iwfd >= 0, "open dyn_interp O_RDWR");
    char *eargv[] = {"/tmp/with_interp", NULL};
    errno = 0;
    CHECK(execve("/tmp/with_interp", eargv, envp) < 0 && errno == ETXTBSY,
          "execve ELF with writable PT_INTERP must return ETXTBSY");
    close(iwfd);
    unlink("/tmp/with_interp");
    unlink("/tmp/dyn_interp");
}

/* 10. Traced execve of doomed ELF dies by SIGSEGV without PTRACE_EVENT_EXEC */
static void test_doomed_elf_ptrace_no_exec_event(void)
{
    uint8_t elf[128];
    memset(elf, 0, sizeof(elf));
    memcpy(elf, "\x7f""ELF", 4);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    uint16_t e_type = 2, e_machine = 183, e_ehsize = 64, e_phentsize = 56, e_phnum = 1;
    uint32_t e_version = 1;
    uint64_t e_entry = 0x400078, e_phoff = 64;
    memcpy(elf + 16, &e_type, 2);
    memcpy(elf + 18, &e_machine, 2);
    memcpy(elf + 20, &e_version, 4);
    memcpy(elf + 24, &e_entry, 8);
    memcpy(elf + 32, &e_phoff, 8);
    memcpy(elf + 52, &e_ehsize, 2);
    memcpy(elf + 54, &e_phentsize, 2);
    memcpy(elf + 56, &e_phnum, 2);
    /* Misaligned PT_LOAD (p_offset=1, p_vaddr=0x400000) -> doomed after point of no return */
    uint32_t p_type = 1, p_flags = 5;
    uint64_t p_offset = 1, p_vaddr = 0x400000, p_filesz = 16, p_memsz = 16, p_align = 4096;
    memcpy(elf + 64, &p_type, 4);
    memcpy(elf + 68, &p_flags, 4);
    memcpy(elf + 72, &p_offset, 8);
    memcpy(elf + 80, &p_vaddr, 8);
    memcpy(elf + 96, &p_filesz, 8);
    memcpy(elf + 104, &p_memsz, 8);
    memcpy(elf + 112, &p_align, 8);
    write_file_bytes("/tmp/doomed_elf", elf, sizeof(elf), 0755);

    pid_t child = fork();
    CHECK(child >= 0, "fork doomed tracee");
    if (child == 0) {
        CHECK(ptrace(PTRACE_TRACEME, 0, NULL, NULL) == 0, "PTRACE_TRACEME");
        raise(SIGSTOP);
        char *argv[] = {"/tmp/doomed_elf", NULL};
        char *envp[] = {NULL};
        execve("/tmp/doomed_elf", argv, envp);
        _exit(99);
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st), "initial stop");
    CHECK(ptrace(PTRACE_SETOPTIONS, child, NULL, (void *)PTRACE_O_TRACEEXEC) == 0,
          "PTRACE_O_TRACEEXEC");
    CHECK(ptrace(PTRACE_CONT, child, NULL, NULL) == 0, "PTRACE_CONT");

    int saw_exec_event = 0;
    int died_sigsegv = 0;
    for (;;) {
        CHECK(waitpid(child, &st, 0) == child, "waitpid doomed");
        if (WIFSTOPPED(st)) {
            int sig = WSTOPSIG(st);
            if (sig == SIGTRAP && ((st >> 16) == PTRACE_EVENT_EXEC)) {
                saw_exec_event = 1;
            }
            int deliver = (sig == SIGTRAP) ? 0 : sig;
            CHECK(ptrace(PTRACE_CONT, child, NULL, (void *)(uintptr_t)deliver) == 0, "PTRACE_CONT");
        } else if (WIFSIGNALED(st)) {
            died_sigsegv = (WTERMSIG(st) == SIGSEGV);
            break;
        } else {
            CHECK(0, "doomed child exited unexpectedly: status=0x%x", st);
        }
    }
    CHECK(!saw_exec_event, "doomed ELF must NOT report PTRACE_EVENT_EXEC");
    CHECK(died_sigsegv, "doomed ELF must die with SIGSEGV");
    unlink("/tmp/doomed_elf");
}

/* 11. Single-stepped Tier 0 `ic ivau` on guest page 0 invalidates Tier 1 translations */
struct aarch64_regs {
    uint64_t regs[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t pstate;
};

static void test_page_zero_ic_ivau(void)
{
    write_proc_str("/proc/sys/vm/mmap_min_addr", "0\n");

    pid_t child = fork();
    CHECK(child >= 0, "fork page0");
    if (child == 0) {
        CHECK(ptrace(PTRACE_TRACEME, 0, NULL, NULL) == 0, "PTRACE_TRACEME");
        raise(SIGSTOP);

        void *p = mmap((void *)0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        CHECK(p == (void *)0, "mmap page 0");

        uintptr_t base = (uintptr_t)p;
        __asm__("" : "+r"(base));
        volatile uint32_t *code = (volatile uint32_t *)base;
        /* mov w0, #1; ret */
        code[0] = 0x52800020u;
        code[1] = 0xd65f03c0u;
        __asm__ volatile(
            "dc cvau, %0\n"
            "dsb ish\n"
            "ic ivau, %0\n"
            "dsb ish\n"
            "isb\n"
            :
            : "r"(base)
            : "memory");

        typedef int (*fn_t)(void);
        fn_t fn = (fn_t)base;
        CHECK(fn() == 1, "first page-0 call must return 1");

        /* Overwrite page 0 with: mov w0, #2; ret */
        code[0] = 0x52800040u;
        /* Pause at brk #0 so tracer single-steps `ic ivau` in Tier 0 (Exit::InvalidateCode{0}) */
        __asm__ volatile(
            "dc cvau, %0\n"
            "dsb ish\n"
            "brk #0\n"
            "ic ivau, %0\n"
            "dsb ish\n"
            "isb\n"
            :
            : "r"(base)
            : "memory");

        int v = fn();
        CHECK(v == 2, "page-0 call after Tier-0 single-stepped ic ivau must return 2 (got %d)", v);
        _exit(0);
    }

    int st = 0;
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP,
          "initial stop page0");
    CHECK(ptrace(PTRACE_CONT, child, NULL, NULL) == 0, "cont to brk");

    /* Child stops at `brk #0` right before `ic ivau, x0` */
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
          "expected SIGTRAP at brk #0");
    struct aarch64_regs regs;
    struct iovec iov = {.iov_base = &regs, .iov_len = sizeof(regs)};
    CHECK(ptrace(PTRACE_GETREGSET, child, (void *)NT_PRSTATUS, &iov) == 0, "GETREGSET");
    regs.pc += 4; /* advance past brk #0 to `ic ivau` */
    CHECK(ptrace(PTRACE_SETREGSET, child, (void *)NT_PRSTATUS, &iov) == 0, "SETREGSET");

    /* Single-step the `ic ivau` instruction in pure Tier 0 */
    CHECK(ptrace(PTRACE_SINGLESTEP, child, NULL, NULL) == 0, "SINGLESTEP ic ivau");
    CHECK(waitpid(child, &st, 0) == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGTRAP,
          "expected SIGTRAP after singlestep ic ivau");

    /* Resume normal Tier 1 execution */
    CHECK(ptrace(PTRACE_CONT, child, NULL, NULL) == 0, "CONT after singlestep");
    wait_ok(child, "page0 child");

    write_proc_str("/proc/sys/vm/mmap_min_addr", "32768\n");
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "--exit-ok") == 0)
        return 0;
    if (argc == 4 && strcmp(argv[1], "--hold") == 0) {
        int rfd = atoi(argv[2]);
        int sfd = atoi(argv[3]);
        char c = '1';
        if (write(rfd, &c, 1) != 1)
            return 2;
        close(rfd);
        (void)read(sfd, &c, 1);
        close(sfd);
        return 0;
    }

    mkdir("/tmp", 01777);

    test_protected_symlinks_trailing_slash();
    test_access_noexec();
    test_tmpfs_unlinked_open_accounting();
    test_mount_busy_and_force_cap(argv[0]);
    test_mount_collection_and_open_tree_propagation();
    test_mremap_shm_vdso_and_shmat_min_addr();
    test_pagemap_cow_lent_not_exclusive();
    test_fuse_short_read_mapping();
    test_exec_etxtbsy_script_and_interp(argv[0]);
    test_doomed_elf_ptrace_no_exec_event();
    test_page_zero_ic_ivau();

    printf("all ok\n");
    return 0;
}
