/*
 * Regression tests for Package 4A (locks, overlay, inotify, fuse events, fileio, files, pipe):
 * 1. Overlay leases taken before copy-up are broken on write open / truncate, and F_GETLEASE /
 *    F_SETLEASE(F_UNLCK) stay consistent after copy-up (e.g. via chmod).
 * 2. Overlay changes whose upper layer is on tmpfs are charged to tmpfs accounting (inodes,
 *    pages, copy-up, writes, unlinks, ENOSPC at limit).
 * 3. Overlay creations check permissions / type / whiteout before copying up parent directories.
 * 4. Overlay FIFO fstat matches stat before and after copy-up.
 * 5. Inotify queue overflow queues IN_Q_OVERFLOW even on a duplicate tail event; O_RDONLY|O_TRUNC
 *    emits IN_MODIFY; no-op chown(-1, -1) emits no IN_ATTRIB (while clearing setuid does);
 *    IN_EXCL_UNLINK excludes open/access/modify/close on unlinked names on both file and directory
 *    watches (including hard-linked files) while still delivering metadata events (fchmod);
 *    delayed delete-self cleans up when watches are removed before last close.
 * 6. FUSE inotify: IN_EXCL_UNLINK on a FUSE dir does not drop linked child events and excludes
 *    events after unlink; MKNOD fallback emits IN_CREATE once; FUSE chmod/chown/utimensat/
 *    truncate/ftruncate/setxattr/removexattr and O_TRUNC emit IN_ATTRIB/IN_MODIFY; O_SYNC FUSE
 *    writes trigger FUSE_FSYNC.
 * 7. fileio/files: RWF_NOWAIT and RWF_APPEND in preadv2/pwritev2; F_GETFL preserves O_DIRECT,
 *    O_NOATIME, O_ASYNC, O_DIRECTORY, O_NOFOLLOW; CLOSE_RANGE_UNSHARE enforces nr_open (EMFILE);
 *    remove_privs on write clears non-exec S_ISGID for non-group unprivileged writers (aligned
 *    with chown); writev on O_SYNC file, sendfile/copy_file_range/splice on O_SYNC/O_DSYNC
 *    destinations, and concurrent lseek + I/O position safety.
 * 8. Pipe I/O: read/readv/read-side vmsplice with faulting buffer (EFAULT) preserve unread pipe
 *    bytes; splice into file hitting RLIMIT_FSIZE raises SIGXFSZ; F_SETPIPE_SZ shrinking above
 *    pipe-max-size succeeds without EPERM while growing returns EPERM. Anonymous pipe ends
 *    contend for flock locks, with open-file ownership, conversion and procfs listings.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#define CHECK(cond, ...)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            fprintf(stderr, "FAIL: ");                                                             \
            fprintf(stderr, __VA_ARGS__);                                                          \
            fprintf(stderr, " (errno %d: %s)\n", errno, strerror(errno));                          \
            exit(1);                                                                               \
        }                                                                                          \
    } while (0)

#ifndef F_SETLEASE
#define F_SETLEASE 1024
#endif
#ifndef F_GETLEASE
#define F_GETLEASE 1025
#endif
#ifndef F_SETPIPE_SZ
#define F_SETPIPE_SZ 1031
#endif
#ifndef F_GETPIPE_SZ
#define F_GETPIPE_SZ 1032
#endif
#ifndef RWF_NOWAIT
#define RWF_NOWAIT 0x00000008
#endif
#ifndef RWF_APPEND
#define RWF_APPEND 0x00000010
#endif
#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE (1U << 1)
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef SYS_preadv2
#define SYS_preadv2 286
#endif
#ifndef SYS_pwritev2
#define SYS_pwritev2 287
#endif
#ifndef SYS_copy_file_range
#define SYS_copy_file_range 285
#endif
#ifndef IN_EXCL_UNLINK
#define IN_EXCL_UNLINK 0x04000000
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

static void read_proc_str(const char *path, char *buf, size_t cap)
{
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0, "open %s", path);
    ssize_t n = read(fd, buf, cap - 1);
    CHECK(n > 0, "read %s", path);
    buf[n] = '\0';
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
    pid_t r;
    do {
        r = waitpid(pid, &st, 0);
    } while (r < 0 && errno == EINTR);
    CHECK(r == pid, "waitpid %s", label);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "%s failed (status=0x%x)", label, st);
}

static void setup_overlay_dirs(const char *base)
{
    static const char *const subs[] = {"", "/lower", "/upper", "/work", "/merged"};
    for (size_t i = 0; i < sizeof(subs) / sizeof(subs[0]); i++) {
        char p[128];
        snprintf(p, sizeof(p), "%s%s", base, subs[i]);
        CHECK(mkdir(p, 0755) == 0, "mkdir %s", p);
    }
}

static void mount_overlay(const char *base)
{
    char merged[128], opts[256];
    snprintf(merged, sizeof(merged), "%s/merged", base);
    snprintf(opts, sizeof(opts), "lowerdir=%s/lower,upperdir=%s/upper,workdir=%s/work", base, base,
             base);
    CHECK(mount("overlay", merged, "overlay", 0, opts) == 0, "mount overlay %s", base);
}

/* ---- 1. Overlay leases before and across copy-up ---- */

static volatile sig_atomic_t lease_sigio_count = 0;
static volatile sig_atomic_t lease_unlock_fd = -1;

static void on_lease_sigio(int sig)
{
    (void)sig;
    lease_sigio_count++;
    int fd = lease_unlock_fd;
    if (fd >= 0) {
        (void)fcntl(fd, F_SETLEASE, F_UNLCK);
        lease_unlock_fd = -1;
    }
}

static void test_overlay_leases(void)
{
    setup_overlay_dirs("/tmp/ovl_l");
    write_file_bytes("/tmp/ovl_l/lower/a", "hello", 5, 0644);
    write_file_bytes("/tmp/ovl_l/lower/b", "world", 5, 0644);
    write_file_bytes("/tmp/ovl_l/lower/c", "12345", 5, 0644);
    mount_overlay("/tmp/ovl_l");

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_lease_sigio;
    CHECK(sigaction(SIGIO, &sa, NULL) == 0, "sigaction SIGIO");

    /* Part A: lease taken before copy-up survives copy-up (chmod + fstat follow_copy_up)
     * and can be queried and unlocked cleanly. */
    int fd = open("/tmp/ovl_l/merged/a", O_RDONLY);
    CHECK(fd >= 0, "open merged/a");
    CHECK(fcntl(fd, F_SETLEASE, F_RDLCK) == 0, "F_SETLEASE on lower overlay file");
    CHECK(fcntl(fd, F_GETLEASE) == F_RDLCK, "F_GETLEASE before copy-up");

    /* Copy up via chmod, then fstat to trigger follow_copy_up on fd */
    CHECK(chmod("/tmp/ovl_l/merged/a", 0640) == 0, "chmod merged/a to copy up");
    struct stat st;
    CHECK(fstat(fd, &st) == 0, "fstat merged/a after copy-up");
    CHECK((st.st_mode & 0777) == 0640, "fstat sees copied-up mode");

    CHECK(fcntl(fd, F_GETLEASE) == F_RDLCK,
          "F_GETLEASE must still report F_RDLCK after follow_copy_up");
    /* Nonblocking write open must fail with EAGAIN and notify holder */
    lease_sigio_count = 0;
    lease_unlock_fd = -1;
    errno = 0;
    int wfd = open("/tmp/ovl_l/merged/a", O_WRONLY | O_NONBLOCK);
    CHECK(wfd < 0 && errno == EAGAIN, "O_WRONLY|O_NONBLOCK must conflict with overlay lease");
    CHECK(lease_sigio_count >= 1, "holder must receive SIGIO on overlay write-open conflict");

    CHECK(fcntl(fd, F_SETLEASE, F_UNLCK) == 0,
          "F_SETLEASE(F_UNLCK) must succeed after copy-up");
    CHECK(fcntl(fd, F_GETLEASE) == F_UNLCK, "F_GETLEASE after unlock");
    close(fd);

    /* Part B: lease taken before copy-up is broken on blocking write open (no copy-up leak) */
    fd = open("/tmp/ovl_l/merged/b", O_RDONLY);
    CHECK(fd >= 0, "open merged/b");
    CHECK(fcntl(fd, F_SETLEASE, F_RDLCK) == 0, "F_SETLEASE merged/b");
    lease_sigio_count = 0;
    lease_unlock_fd = fd;
    pid_t pid = fork();
    CHECK(pid >= 0, "fork writer");
    if (pid == 0) {
        int cfd = open("/tmp/ovl_l/merged/b", O_WRONLY);
        CHECK(cfd >= 0, "child open merged/b O_WRONLY");
        CHECK(write(cfd, "Z", 1) == 1, "child write merged/b");
        close(cfd);
        _exit(0);
    }
    wait_ok(pid, "overlay lease break on write open");
    CHECK(lease_sigio_count >= 1, "holder must receive SIGIO on overlay write open before copy-up");
    CHECK(fcntl(fd, F_GETLEASE) == F_UNLCK, "lease unlocked by SIGIO handler");
    close(fd);

    /* Part C: lease taken before copy-up is broken on truncate() */
    fd = open("/tmp/ovl_l/merged/c", O_RDONLY);
    CHECK(fd >= 0, "open merged/c");
    CHECK(fcntl(fd, F_SETLEASE, F_RDLCK) == 0, "F_SETLEASE merged/c");
    lease_sigio_count = 0;
    lease_unlock_fd = fd;
    pid = fork();
    CHECK(pid >= 0, "fork truncator");
    if (pid == 0) {
        CHECK(truncate("/tmp/ovl_l/merged/c", 2) == 0, "child truncate merged/c");
        _exit(0);
    }
    wait_ok(pid, "overlay lease break on truncate");
    CHECK(lease_sigio_count >= 1, "holder must receive SIGIO on overlay truncate before copy-up");
    CHECK(stat("/tmp/ovl_l/merged/c", &st) == 0 && st.st_size == 2, "truncated size == 2");
    close(fd);

    signal(SIGIO, SIG_DFL);
    CHECK(umount2("/tmp/ovl_l/merged", 0) == 0, "umount ovl_l");
}

/* ---- 2. Overlay changes on tmpfs upper layer charged to tmpfs accounting ---- */

static void test_overlay_tmpfs_accounting(void)
{
    CHECK(mkdir("/tmp/ovl_t", 0755) == 0, "mkdir ovl_t");
    CHECK(mkdir("/tmp/ovl_t/lower", 0755) == 0, "mkdir ovl_t/lower");
    CHECK(mkdir("/tmp/ovl_t/tmp", 0755) == 0, "mkdir ovl_t/tmp");
    CHECK(mkdir("/tmp/ovl_t/merged", 0755) == 0, "mkdir ovl_t/merged");

    /* Lower file of 12 KiB (3 pages) */
    char big[12288];
    memset(big, 'L', sizeof(big));
    write_file_bytes("/tmp/ovl_t/lower/big", big, sizeof(big), 0644);

    /* Tight tmpfs: 32 KiB (8 pages) and 9 inodes total
     * (root + upper + work + work/work = 4 used, leaving 5 free) */
    CHECK(mount("tmpfs", "/tmp/ovl_t/tmp", "tmpfs", 0, "size=32k,nr_inodes=9") == 0,
          "mount upper tmpfs");
    CHECK(mkdir("/tmp/ovl_t/tmp/upper", 0755) == 0, "mkdir upper");
    CHECK(mkdir("/tmp/ovl_t/tmp/work", 0755) == 0, "mkdir work");
    CHECK(mount("overlay", "/tmp/ovl_t/merged", "overlay", 0,
                "lowerdir=/tmp/ovl_t/lower,upperdir=/tmp/ovl_t/tmp/upper,"
                "workdir=/tmp/ovl_t/tmp/work") == 0,
          "mount overlay over tmpfs");

    struct statvfs v0, v1, v2;
    CHECK(statvfs("/tmp/ovl_t/tmp", &v0) == 0, "statvfs initial");
    CHECK(v0.f_blocks == 8 && v0.f_files == 9, "tmpfs limits 8 pages / 9 inodes");
    CHECK(v0.f_ffree == 5 && v0.f_bfree == 8,
          "initial free: 5 inodes, 8 pages (got ffree=%lu bfree=%lu)",
          (unsigned long)v0.f_ffree, (unsigned long)v0.f_bfree);

    /* Create and write 16 KiB (4 pages) through overlay */
    int fd = open("/tmp/ovl_t/merged/f1", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0, "create merged/f1");
    char buf16k[16384];
    memset(buf16k, 'A', sizeof(buf16k));
    CHECK(write(fd, buf16k, sizeof(buf16k)) == (ssize_t)sizeof(buf16k), "write 16k through overlay");
    CHECK(statvfs("/tmp/ovl_t/tmp", &v1) == 0, "statvfs after 16k write");
    CHECK(v1.f_ffree == 4 && v1.f_bfree == 4,
          "overlay write must charge tmpfs (got ffree=%lu bfree=%lu)",
          (unsigned long)v1.f_ffree, (unsigned long)v1.f_bfree);

    /* Writing 20 KiB more exceeds remaining 16 KiB: must cap / ENOSPC */
    char buf20k[20480];
    memset(buf20k, 'B', sizeof(buf20k));
    ssize_t n = write(fd, buf20k, sizeof(buf20k));
    CHECK(n == 16384, "write hitting tmpfs limit through overlay stops at 16k (got %zd)", n);
    errno = 0;
    CHECK(write(fd, "x", 1) < 0 && errno == ENOSPC, "next write at full tmpfs must return ENOSPC");
    close(fd);

    /* Unlink recovers both pages and inode */
    CHECK(unlink("/tmp/ovl_t/merged/f1") == 0, "unlink merged/f1");
    CHECK(statvfs("/tmp/ovl_t/tmp", &v2) == 0, "statvfs after unlink");
    CHECK(v2.f_ffree == 5 && v2.f_bfree == 8,
          "overlay unlink must refund tmpfs (got ffree=%lu bfree=%lu)",
          (unsigned long)v2.f_ffree, (unsigned long)v2.f_bfree);

    /* Copy-up of 12 KiB lower file charges 3 pages and 1 inode */
    CHECK(chmod("/tmp/ovl_t/merged/big", 0600) == 0, "chmod merged/big copies up");
    CHECK(statvfs("/tmp/ovl_t/tmp", &v1) == 0, "statvfs after copy-up");
    CHECK(v1.f_ffree == 4 && v1.f_bfree == 5,
          "overlay copy-up must charge tmpfs (got ffree=%lu bfree=%lu)",
          (unsigned long)v1.f_ffree, (unsigned long)v1.f_bfree);

    /* Fill remaining 4 inodes; 5th creation must fail with ENOSPC */
    for (int i = 0; i < 4; i++) {
        char p[64];
        snprintf(p, sizeof(p), "/tmp/ovl_t/merged/n%d", i);
        int cfd = open(p, O_WRONLY | O_CREAT | O_EXCL, 0644);
        CHECK(cfd >= 0, "create %s", p);
        close(cfd);
    }
    errno = 0;
    CHECK(open("/tmp/ovl_t/merged/over", O_WRONLY | O_CREAT | O_EXCL, 0644) < 0 && errno == ENOSPC,
          "overlay create past tmpfs nr_inodes must fail with ENOSPC");

    CHECK(umount2("/tmp/ovl_t/merged", 0) == 0, "umount ovl_t/merged");
    CHECK(umount2("/tmp/ovl_t/tmp", 0) == 0, "umount ovl_t/tmp");
}

/* ---- 3. Overlay creations check permissions before copying up parents ---- */

static void test_overlay_perm_before_copy_up(void)
{
    setup_overlay_dirs("/tmp/ovl_p");
    CHECK(mkdir("/tmp/ovl_p/lower/dir", 0755) == 0, "mkdir lower/dir");
    CHECK(mkdir("/tmp/ovl_p/lower/dir/sub", 0755) == 0, "mkdir lower/dir/sub");
    mount_overlay("/tmp/ovl_p");

    /* Refused whiteout mknod must not copy up parent directories */
    errno = 0;
    CHECK(mknod("/tmp/ovl_p/merged/dir/sub/wh", S_IFCHR | 0600, makedev(0, 0)) < 0 &&
              errno == EPERM,
          "whiteout mknod in overlay must return EPERM");
    struct stat st;
    errno = 0;
    CHECK(stat("/tmp/ovl_p/upper/dir", &st) < 0 && errno == ENOENT,
          "refused whiteout mknod must not copy up parent dir");

    /* Refused unprivileged creations (open O_CREAT, mkdir, mknod, symlink) must not copy up */
    pid_t pid = fork();
    CHECK(pid >= 0, "fork unpriv creator");
    if (pid == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0, "setresgid");
        CHECK(setresuid(1000, 1000, 1000) == 0, "setresuid");

        errno = 0;
        CHECK(open("/tmp/ovl_p/merged/dir/sub/f", O_CREAT | O_WRONLY, 0644) < 0 && errno == EACCES,
              "unpriv open O_CREAT must fail with EACCES");
        errno = 0;
        CHECK(mkdir("/tmp/ovl_p/merged/dir/sub/d", 0755) < 0 && errno == EACCES,
              "unpriv mkdir must fail with EACCES");
        errno = 0;
        CHECK(mknod("/tmp/ovl_p/merged/dir/sub/p", S_IFIFO | 0644, 0) < 0 && errno == EACCES,
              "unpriv mknod must fail with EACCES");
        errno = 0;
        CHECK(symlink("target", "/tmp/ovl_p/merged/dir/sub/s") < 0 && errno == EACCES,
              "unpriv symlink must fail with EACCES");
        _exit(0);
    }
    wait_ok(pid, "unprivileged overlay creation refusals");

    errno = 0;
    CHECK(stat("/tmp/ovl_p/upper/dir", &st) < 0 && errno == ENOENT,
          "refused creations must leave upper/dir uncopied");

    CHECK(umount2("/tmp/ovl_p/merged", 0) == 0, "umount ovl_p");
}

/* ---- 4. Overlay FIFO fstat matches stat before and after copy-up ---- */

static void test_fifo_last_close(const char *dir, const char *path)
{
    CHECK(mkfifo(path, 0600) == 0, "create watched FIFO");
    for (int write_end = 0; write_end < 2; write_end++) {
        int fd = open(path, O_NONBLOCK | (write_end ? O_RDWR : O_RDONLY));
        int i = inotify_init1(IN_NONBLOCK);
        CHECK(fd >= 0 && i >= 0, "open FIFO and inotify");
        int wd = inotify_add_watch(i, path, IN_CLOSE);
        CHECK(wd >= 0, "watch already-open FIFO");
        int other = dup(fd);
        CHECK(other >= 0 && close(fd) == 0, "drop first FIFO reference");
        struct inotify_event event;
        CHECK(read(i, &event, sizeof(event)) == -1 && errno == EAGAIN, "premature FIFO close");
        CHECK(close(other) == 0, "drop final FIFO reference");
        CHECK(read(i, &event, sizeof(event)) == sizeof(event) && event.wd == wd &&
              event.mask == (write_end ? IN_CLOSE_WRITE : IN_CLOSE_NOWRITE), "missing FIFO close");
        close(i);
    }
    int fd = open(path, O_RDWR | O_NONBLOCK);
    int i = inotify_init1(IN_NONBLOCK);
    CHECK(fd >= 0 && i >= 0 && inotify_add_watch(i, path, IN_OPEN | IN_CLOSE) >= 0, "watch FIFO reopen");
    int excluded = inotify_init1(IN_NONBLOCK);
    CHECK(excluded >= 0 && inotify_add_watch(excluded, dir, IN_EXCL_UNLINK | IN_OPEN | IN_CLOSE) >= 0,
          "watch FIFO parent");
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    CHECK(unlink(path) == 0, "unlink open FIFO");
    int reopened = open(link, O_RDONLY | O_NONBLOCK);
    CHECK(reopened >= 0 && close(reopened) == 0 && close(fd) == 0, "reopen deleted FIFO");
    for (int n = 0; n < 3; n++) {
        struct inotify_event event;
        const unsigned expected[] = {IN_OPEN, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE};
        CHECK(read(i, &event, sizeof(event)) == sizeof(event) && event.mask == expected[n],
              "wrong event for deleted FIFO reopen");
    }
    struct inotify_event events[8];
    CHECK(read(excluded, events, sizeof(events)) == -1 && errno == EAGAIN, "reported unlinked FIFO");
    close(excluded);
    close(i);
}

static void test_overlay_fifo_fstat(void)
{
    setup_overlay_dirs("/tmp/ovl_f");
    CHECK(mkfifo("/tmp/ovl_f/lower/pipe", 0644) == 0, "mkfifo lower/pipe");
    mount_overlay("/tmp/ovl_f");

    int fd = open("/tmp/ovl_f/merged/pipe", O_RDWR);
    CHECK(fd >= 0, "open overlay fifo O_RDWR");

    struct stat st1, fst1;
    CHECK(stat("/tmp/ovl_f/merged/pipe", &st1) == 0, "stat fifo before copy-up");
    CHECK(fstat(fd, &fst1) == 0, "fstat fifo before copy-up");
    CHECK(fst1.st_dev == st1.st_dev && fst1.st_ino == st1.st_ino && fst1.st_mode == st1.st_mode,
          "fifo fstat must match stat before copy-up");

    /* Copy up the FIFO via chmod */
    CHECK(chmod("/tmp/ovl_f/merged/pipe", 0600) == 0, "chmod overlay fifo");

    struct stat st2, fst2;
    CHECK(stat("/tmp/ovl_f/merged/pipe", &st2) == 0, "stat fifo after copy-up");
    CHECK(fstat(fd, &fst2) == 0, "fstat fifo after copy-up");
    CHECK(fst2.st_dev == st2.st_dev && fst2.st_ino == st2.st_ino && fst2.st_mode == st2.st_mode,
          "fifo fstat must match stat after copy-up (fstat mode=%o stat mode=%o)",
          fst2.st_mode, st2.st_mode);
    CHECK((fst2.st_mode & 0777) == 0600, "fifo fstat reflects updated mode");

    close(fd);
    test_fifo_last_close("/tmp/ovl_f/merged", "/tmp/ovl_f/merged/watch-fifo");
    CHECK(umount2("/tmp/ovl_f/merged", 0) == 0, "umount ovl_f");
}

/* ---- 5. Inotify core regressions ---- */

struct ino_ev {
    int wd;
    uint32_t mask;
    char name[64];
};

static size_t read_inotify_events(int ifd, struct ino_ev *out, size_t max_out)
{
    uint8_t buf[4096];
    size_t count = 0;
    for (;;) {
        ssize_t n = read(ifd, buf, sizeof(buf));
        if (n < 0) {
            CHECK(errno == EAGAIN, "read inotify");
            break;
        }
        size_t off = 0;
        while (off < (size_t)n && count < max_out) {
            struct inotify_event ev;
            memcpy(&ev, buf + off, sizeof(ev));
            out[count].wd = ev.wd;
            out[count].mask = ev.mask;
            out[count].name[0] = '\0';
            if (ev.len > 0) {
                size_t copy = ev.len < sizeof(out[count].name) - 1 ? ev.len
                                                                    : sizeof(out[count].name) - 1;
                memcpy(out[count].name, buf + off + sizeof(ev), copy);
                out[count].name[copy] = '\0';
            }
            count++;
            off += sizeof(ev) + ev.len;
        }
    }
    return count;
}

static int has_event(const struct ino_ev *evs, size_t n, int wd, uint32_t mask, const char *name)
{
    for (size_t i = 0; i < n; i++) {
        if (wd >= 0 && evs[i].wd != wd)
            continue;
        if ((evs[i].mask & mask) != mask)
            continue;
        if (name && strcmp(evs[i].name, name) != 0)
            continue;
        return 1;
    }
    return 0;
}

static void expect_event(int ifd, int wd, uint32_t mask, const char *name, const char *msg)
{
    struct ino_ev evs[32];
    size_t n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, wd, mask, name), "%s", msg);
}

static void test_inotify_core(void)
{
    CHECK(mkdir("/tmp/ino_core", 0755) == 0, "mkdir ino_core");
    struct ino_ev evs[32];

    /* Part A: overflow check precedes duplicate tail merge */
    char saved_max[32];
    read_proc_str("/proc/sys/fs/inotify/max_queued_events", saved_max, sizeof(saved_max));
    write_proc_str("/proc/sys/fs/inotify/max_queued_events", "2\n");

    write_file_bytes("/tmp/ino_core/q", "hello", 5, 0644);
    int ifd = inotify_init1(IN_NONBLOCK);
    CHECK(ifd >= 0, "inotify_init1");
    int wd = inotify_add_watch(ifd, "/tmp/ino_core/q", IN_MODIFY | IN_ATTRIB);
    CHECK(wd >= 0, "add watch q");
    CHECK(truncate("/tmp/ino_core/q", 3) == 0, "event 1: IN_MODIFY");
    CHECK(chmod("/tmp/ino_core/q", 0640) == 0, "event 2: IN_ATTRIB (queue full)");
    CHECK(chmod("/tmp/ino_core/q", 0640) == 0, "event 3: identical IN_ATTRIB must overflow");

    size_t n = read_inotify_events(ifd, evs, 32);
    CHECK(n == 3, "expected 3 events including IN_Q_OVERFLOW (got %zu)", n);
    CHECK((evs[0].mask & IN_MODIFY) != 0, "ev 0 IN_MODIFY");
    CHECK((evs[1].mask & IN_ATTRIB) != 0, "ev 1 IN_ATTRIB");
    CHECK(evs[2].wd == -1 && (evs[2].mask & IN_Q_OVERFLOW) != 0, "ev 2 IN_Q_OVERFLOW");
    close(ifd);
    write_proc_str("/proc/sys/fs/inotify/max_queued_events", saved_max);

    /* Part B: O_RDONLY | O_TRUNC emits IN_MODIFY */
    write_file_bytes("/tmp/ino_core/trunc", "abcdef", 6, 0644);
    ifd = inotify_init1(IN_NONBLOCK);
    CHECK(ifd >= 0, "inotify_init1 trunc");
    wd = inotify_add_watch(ifd, "/tmp/ino_core/trunc", IN_MODIFY | IN_OPEN);
    CHECK(wd >= 0, "watch trunc");
    int tfd = open("/tmp/ino_core/trunc", O_RDONLY | O_TRUNC);
    CHECK(tfd >= 0, "open O_RDONLY|O_TRUNC");
    close(tfd);
    n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, wd, IN_MODIFY, NULL), "O_RDONLY|O_TRUNC must emit IN_MODIFY");
    close(ifd);

    /* Part C: no-op chown(-1, -1) emits no IN_ATTRIB, clearing setuid does */
    write_file_bytes("/tmp/ino_core/own", "x", 1, 0644);
    ifd = inotify_init1(IN_NONBLOCK);
    wd = inotify_add_watch(ifd, "/tmp/ino_core/own", IN_ATTRIB);
    CHECK(chown("/tmp/ino_core/own", (uid_t)-1, (gid_t)-1) == 0, "no-op chown(-1,-1)");
    int ofd = open("/tmp/ino_core/own", O_RDONLY);
    CHECK(ofd >= 0, "open own");
    CHECK(fchown(ofd, (uid_t)-1, (gid_t)-1) == 0, "no-op fchown(-1,-1)");
    n = read_inotify_events(ifd, evs, 32);
    CHECK(n == 0, "no-op chown(-1,-1) must not emit IN_ATTRIB (got %zu)", n);

    CHECK(chmod("/tmp/ino_core/own", 04755) == 0, "chmod setuid");
    (void)read_inotify_events(ifd, evs, 32); /* drain chmod IN_ATTRIB */
    CHECK(chown("/tmp/ino_core/own", (uid_t)-1, (gid_t)-1) == 0, "chown(-1,-1) clearing setuid");
    struct stat st;
    CHECK(stat("/tmp/ino_core/own", &st) == 0 && (st.st_mode & S_ISUID) == 0, "setuid cleared");
    n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, wd, IN_ATTRIB, NULL),
          "chown(-1,-1) clearing setuid must emit IN_ATTRIB");
    close(ofd);
    close(ifd);

    /* Part D: IN_EXCL_UNLINK on both directory and file watches (including hard links) */
    CHECK(mkdir("/tmp/ino_core/d", 0755) == 0, "mkdir ino_core/d");
    write_file_bytes("/tmp/ino_core/d/a", "hello", 5, 0644);
    CHECK(link("/tmp/ino_core/d/a", "/tmp/ino_core/d/b") == 0, "hard link a -> b");

    ifd = inotify_init1(IN_NONBLOCK);
    int dwd = inotify_add_watch(ifd, "/tmp/ino_core/d",
                                IN_MODIFY | IN_CLOSE_WRITE | IN_ATTRIB | IN_EXCL_UNLINK);
    int fwd = inotify_add_watch(ifd, "/tmp/ino_core/d/a",
                                IN_MODIFY | IN_CLOSE_WRITE | IN_ATTRIB | IN_EXCL_UNLINK);
    CHECK(dwd >= 0 && fwd >= 0, "watches on d and d/a");

    int fda = open("/tmp/ino_core/d/a", O_RDWR);
    int fdb = open("/tmp/ino_core/d/b", O_RDWR);
    CHECK(fda >= 0 && fdb >= 0, "open d/a and d/b");

    /* Unlink d/a while d/b still keeps nlink == 1 */
    CHECK(unlink("/tmp/ino_core/d/a") == 0, "unlink d/a");
    (void)read_inotify_events(ifd, evs, 32); /* drain unlink IN_ATTRIB */

    CHECK(write(fda, "1", 1) == 1, "write unlinked d/a fd");
    close(fda);
    n = read_inotify_events(ifd, evs, 32);
    CHECK(!has_event(evs, n, dwd, IN_MODIFY, NULL) && !has_event(evs, n, fwd, IN_MODIFY, NULL),
          "IN_EXCL_UNLINK must suppress IN_MODIFY on unlinked hard-link name");
    CHECK(!has_event(evs, n, dwd, IN_CLOSE_WRITE, NULL) &&
              !has_event(evs, n, fwd, IN_CLOSE_WRITE, NULL),
          "IN_EXCL_UNLINK must suppress IN_CLOSE_WRITE on unlinked hard-link name");

    /* Writing through still-linked d/b MUST deliver IN_MODIFY to both watches */
    CHECK(write(fdb, "2", 1) == 1, "write linked d/b fd");
    n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, dwd, IN_MODIFY, "b"), "linked d/b must notify dir watch");
    CHECK(has_event(evs, n, fwd, IN_MODIFY, NULL), "linked d/b must notify file watch");

    /* Unlink d/b too: fchmod (metadata) still reports IN_ATTRIB, while write/close are excluded */
    CHECK(unlink("/tmp/ino_core/d/b") == 0, "unlink d/b");
    (void)read_inotify_events(ifd, evs, 32);
    CHECK(fchmod(fdb, 0600) == 0, "fchmod unlinked fdb");
    n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, fwd, IN_ATTRIB, NULL), "fchmod on unlinked fd still emits IN_ATTRIB");
    CHECK(write(fdb, "3", 1) == 1, "write unlinked fdb");
    close(fdb);
    n = read_inotify_events(ifd, evs, 32);
    CHECK(!has_event(evs, n, dwd, IN_MODIFY, NULL) && !has_event(evs, n, fwd, IN_MODIFY, NULL),
          "unlinked fdb write excluded");
    CHECK(!has_event(evs, n, dwd, IN_CLOSE_WRITE, NULL) &&
              !has_event(evs, n, fwd, IN_CLOSE_WRITE, NULL),
          "unlinked fdb close excluded");
    close(ifd);

    /* Part E: delayed delete-self cleans up when watch removed before last close */
    write_file_bytes("/tmp/ino_core/doomed", "x", 1, 0644);
    int dfd = open("/tmp/ino_core/doomed", O_RDONLY);
    CHECK(dfd >= 0, "open doomed");
    ifd = inotify_init1(IN_NONBLOCK);
    wd = inotify_add_watch(ifd, "/tmp/ino_core/doomed", IN_DELETE_SELF);
    CHECK(wd >= 0, "watch doomed");
    CHECK(unlink("/tmp/ino_core/doomed") == 0, "unlink doomed");
    CHECK(inotify_rm_watch(ifd, wd) == 0, "rm_watch before close");
    n = read_inotify_events(ifd, evs, 32);
    CHECK(has_event(evs, n, wd, IN_IGNORED, NULL), "IN_IGNORED on rm_watch");
    close(dfd);
    n = read_inotify_events(ifd, evs, 32);
    CHECK(n == 0, "no spurious event after close of unwatched doomed file");
    close(ifd);
}

/* ---- 6. FUSE inotify (IN_EXCL_UNLINK, MKNOD fallback, attrib/modify, O_SYNC fsync) ---- */

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

struct fuse_write_in {
    uint64_t fh;
    uint64_t offset;
    uint32_t size;
    uint32_t write_flags;
    uint64_t lock_owner;
    uint32_t flags;
    uint32_t padding;
};

struct fuse_write_out {
    uint32_t size;
    uint32_t padding;
};

struct fuse_setattr_in {
    uint32_t valid;
    uint32_t padding;
    uint64_t fh;
    uint64_t size;
    uint64_t lock_owner;
    uint64_t atime;
    uint64_t mtime;
    uint64_t ctime;
    uint32_t atimensec;
    uint32_t mtimensec;
    uint32_t ctimensec;
    uint32_t mode;
    uint32_t unused4;
    uint32_t uid;
    uint32_t gid;
    uint32_t unused5;
};

struct fuse_mknod_in {
    uint32_t mode;
    uint32_t rdev;
    uint32_t umask;
    uint32_t padding;
};

struct fuse_file {
    int present;
    char name[32];
    uint64_t nodeid;
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
};

struct fuse_test_ctx {
    int devfd;
    volatile int fail_create;
    volatile int fsync_ops;
    struct fuse_file files[4];
    uint64_t next_id;
};

static struct fuse_file *fuse_find_name(struct fuse_test_ctx *c, const char *name)
{
    for (int i = 0; i < 4; i++) {
        if (c->files[i].present && strcmp(c->files[i].name, name) == 0)
            return &c->files[i];
    }
    return NULL;
}

static struct fuse_file *fuse_find_id(struct fuse_test_ctx *c, uint64_t id)
{
    for (int i = 0; i < 4; i++) {
        if (c->files[i].nodeid == id)
            return &c->files[i];
    }
    return NULL;
}

enum {
    OP_LOOKUP = 1,
    OP_GETATTR = 3,
    OP_SETATTR = 4,
    OP_MKNOD = 8,
    OP_UNLINK = 10,
    OP_OPEN = 14,
    OP_WRITE = 16,
    OP_RELEASE = 18,
    OP_FSYNC = 20,
    OP_SETXATTR = 21,
    OP_REMOVEXATTR = 24,
    OP_FLUSH = 25,
    OP_INIT = 26,
    OP_CREATE = 35,

    FATTR_MODE = 1U << 0,
    FATTR_UID = 1U << 1,
    FATTR_GID = 1U << 2,
    FATTR_SIZE = 1U << 3,
};

static struct fuse_file *fuse_alloc_file(struct fuse_test_ctx *c, const char *name, uint32_t mode)
{
    for (int i = 0; i < 4; i++) {
        if (!c->files[i].present) {
            struct fuse_file *slot = &c->files[i];
            memset(slot, 0, sizeof(*slot));
            slot->present = 1;
            snprintf(slot->name, sizeof(slot->name), "%.31s", name);
            slot->nodeid = ++c->next_id;
            slot->mode = mode;
            return slot;
        }
    }
    return NULL;
}

static void fill_attr(struct fuse_attr *a, const struct fuse_file *f)
{
    memset(a, 0, sizeof(*a));
    a->ino = f->nodeid;
    a->mode = f->mode;
    a->nlink = f->present ? 1 : 0;
    a->uid = f->uid;
    a->gid = f->gid;
    a->size = f->size;
    a->blksize = 4096;
}

static void fill_entry_out(struct fuse_entry_out *e, const struct fuse_file *f)
{
    memset(e, 0, sizeof(*e));
    e->nodeid = f->nodeid;
    e->entry_valid = 1;
    e->attr_valid = 1;
    fill_attr(&e->attr, f);
}

static void reply_body(uint8_t *out, struct fuse_out_hdr *hdr, const void *body, size_t len)
{
    hdr->len = (uint32_t)(sizeof(*hdr) + len);
    if (len > 0)
        memcpy(out + sizeof(*hdr), body, len);
}

static void *fuse_reg_daemon(void *arg)
{
    struct fuse_test_ctx *c = (struct fuse_test_ctx *)arg;
    uint8_t buf[16384];
    uint8_t out[16384];
    for (;;) {
        ssize_t n = read(c->devfd, buf, sizeof(buf));
        if (n < (ssize_t)sizeof(struct fuse_in_hdr))
            break;
        struct fuse_in_hdr in;
        memcpy(&in, buf, sizeof(in));

        struct fuse_out_hdr *hdr = (struct fuse_out_hdr *)out;
        memset(hdr, 0, sizeof(*hdr));
        hdr->unique = in.unique;
        hdr->len = (uint32_t)sizeof(*hdr);

        switch (in.opcode) {
        case OP_INIT: {
            uint32_t init_out[16] = {0};
            init_out[0] = 7;
            init_out[1] = 37;
            init_out[6] = 65536;
            reply_body(out, hdr, init_out, sizeof(init_out));
            break;
        }
        case OP_GETATTR: {
            struct fuse_attr_out a = {.attr_valid = 1};
            if (in.nodeid == 1) {
                a.attr.ino = 1;
                a.attr.mode = S_IFDIR | 0755;
                a.attr.nlink = 2;
            } else {
                struct fuse_file *f = fuse_find_id(c, in.nodeid);
                if (!f) {
                    hdr->error = -ENOENT;
                    break;
                }
                fill_attr(&a.attr, f);
            }
            reply_body(out, hdr, &a, sizeof(a));
            break;
        }
        case OP_LOOKUP: {
            struct fuse_file *f = fuse_find_name(c, (const char *)(buf + sizeof(in)));
            if (!f) {
                hdr->error = -ENOENT;
            } else {
                struct fuse_entry_out e;
                fill_entry_out(&e, f);
                reply_body(out, hdr, &e, sizeof(e));
            }
            break;
        }
        case OP_CREATE: {
            if (c->fail_create) {
                hdr->error = -ENOSYS;
                break;
            }
            struct fuse_file *slot =
                fuse_alloc_file(c, (const char *)(buf + sizeof(in) + 16), S_IFREG | 0644);
            if (!slot) {
                hdr->error = -ENOSPC;
                break;
            }
            struct {
                struct fuse_entry_out e;
                struct fuse_open_out o;
            } resp = {0};
            fill_entry_out(&resp.e, slot);
            resp.o.fh = slot->nodeid;
            reply_body(out, hdr, &resp, sizeof(resp));
            break;
        }
        case OP_MKNOD: {
            struct fuse_mknod_in mi;
            memcpy(&mi, buf + sizeof(in), sizeof(mi));
            uint32_t mode = (mi.mode & S_IFMT) ? mi.mode : (S_IFREG | (mi.mode & 07777));
            struct fuse_file *slot =
                fuse_alloc_file(c, (const char *)(buf + sizeof(in) + sizeof(mi)), mode);
            if (!slot) {
                hdr->error = -ENOSPC;
                break;
            }
            struct fuse_entry_out e;
            fill_entry_out(&e, slot);
            reply_body(out, hdr, &e, sizeof(e));
            break;
        }
        case OP_OPEN: {
            struct fuse_open_out o = {.fh = in.nodeid};
            reply_body(out, hdr, &o, sizeof(o));
            break;
        }
        case OP_WRITE: {
            struct fuse_write_in wi;
            memcpy(&wi, buf + sizeof(in), sizeof(wi));
            struct fuse_file *f = fuse_find_id(c, in.nodeid);
            if (f && wi.offset + wi.size > f->size)
                f->size = wi.offset + wi.size;
            struct fuse_write_out wo = {.size = wi.size};
            reply_body(out, hdr, &wo, sizeof(wo));
            break;
        }
        case OP_SETATTR: {
            struct fuse_setattr_in si;
            memcpy(&si, buf + sizeof(in), sizeof(si));
            struct fuse_file *f = fuse_find_id(c, in.nodeid);
            if (!f) {
                hdr->error = -ENOENT;
                break;
            }
            if (si.valid & FATTR_MODE)
                f->mode = (f->mode & S_IFMT) | (si.mode & 07777);
            if (si.valid & FATTR_UID)
                f->uid = si.uid;
            if (si.valid & FATTR_GID)
                f->gid = si.gid;
            if (si.valid & FATTR_SIZE)
                f->size = si.size;
            struct fuse_attr_out a = {.attr_valid = 1};
            fill_attr(&a.attr, f);
            reply_body(out, hdr, &a, sizeof(a));
            break;
        }
        case OP_UNLINK: {
            struct fuse_file *f = fuse_find_name(c, (const char *)(buf + sizeof(in)));
            if (!f)
                hdr->error = -ENOENT;
            else
                f->present = 0;
            break;
        }
        case OP_FSYNC:
            c->fsync_ops++;
            break;
        case OP_SETXATTR:
        case OP_REMOVEXATTR:
        case OP_RELEASE:
        case OP_FLUSH:
            break;
        default:
            hdr->error = -ENOSYS;
            break;
        }
        (void)write(c->devfd, out, hdr->len);
    }
    return NULL;
}

static void test_fuse_inotify_and_sync(void)
{
    CHECK(mkdir("/tmp/fuse_ino", 0755) == 0, "mkdir fuse_ino");
    int devfd = open("/dev/fuse", O_RDWR);
    CHECK(devfd >= 0, "open /dev/fuse");
    struct fuse_test_ctx ctx;
    memset(&ctx, 0, sizeof(ctx));
    ctx.devfd = devfd;
    ctx.fail_create = 1; /* force MKNOD fallback on first create */
    ctx.next_id = 1;

    char opts[128];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=40755,user_id=0,group_id=0", devfd);
    CHECK(mount("f", "/tmp/fuse_ino", "fuse", 0, opts) == 0, "mount fuse_ino");

    pthread_t th;
    CHECK(pthread_create(&th, NULL, fuse_reg_daemon, &ctx) == 0, "spawn fuse daemon");

    int ifd = inotify_init1(IN_NONBLOCK);
    CHECK(ifd >= 0, "inotify_init1 fuse");
    int dwd = inotify_add_watch(ifd, "/tmp/fuse_ino",
                                IN_CREATE | IN_OPEN | IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE |
                                    IN_DELETE | IN_EXCL_UNLINK);
    CHECK(dwd >= 0, "watch fuse dir with IN_EXCL_UNLINK");

    /* MKNOD fallback: must emit IN_CREATE once and IN_OPEN (not dropped by IN_EXCL_UNLINK!) */
    int fd = open("/tmp/fuse_ino/f", O_CREAT | O_RDWR, 0644);
    CHECK(fd >= 0, "create /tmp/fuse_ino/f via MKNOD fallback");

    struct ino_ev evs[32];
    size_t n = read_inotify_events(ifd, evs, 32);
    int creates = 0;
    for (size_t i = 0; i < n; i++) {
        if ((evs[i].mask & IN_CREATE) && strcmp(evs[i].name, "f") == 0)
            creates++;
    }
    CHECK(creates == 1, "MKNOD fallback must emit IN_CREATE once (got %d)", creates);
    CHECK(has_event(evs, n, dwd, IN_OPEN, "f"),
          "IN_EXCL_UNLINK on FUSE dir must NOT drop linked child IN_OPEN");

    /* Write on linked FUSE child must emit IN_MODIFY under IN_EXCL_UNLINK */
    CHECK(write(fd, "hello", 5) == 5, "write fuse f");
    expect_event(ifd, dwd, IN_MODIFY, "f",
                 "IN_EXCL_UNLINK on FUSE dir must NOT drop linked child IN_MODIFY");

    /* FUSE chmod, chown, utimensat, truncate, ftruncate, setxattr, removexattr, O_TRUNC */
    CHECK(chmod("/tmp/fuse_ino/f", 0600) == 0, "fuse chmod");
    expect_event(ifd, dwd, IN_ATTRIB, "f", "fuse chmod must emit IN_ATTRIB");

    CHECK(chown("/tmp/fuse_ino/f", 0, 0) == 0, "fuse chown");
    expect_event(ifd, dwd, IN_ATTRIB, "f", "fuse chown must emit IN_ATTRIB");

    CHECK(utimensat(AT_FDCWD, "/tmp/fuse_ino/f", NULL, 0) == 0, "fuse utimensat");
    expect_event(ifd, dwd, IN_ATTRIB, "f", "fuse utimensat must emit IN_ATTRIB");

    CHECK(truncate("/tmp/fuse_ino/f", 2) == 0, "fuse truncate");
    expect_event(ifd, dwd, IN_MODIFY, "f", "fuse truncate must emit IN_MODIFY");

    CHECK(ftruncate(fd, 1) == 0, "fuse ftruncate");
    expect_event(ifd, dwd, IN_MODIFY, "f", "fuse ftruncate must emit IN_MODIFY");

    CHECK(setxattr("/tmp/fuse_ino/f", "user.k", "v", 1, 0) == 0, "fuse setxattr");
    expect_event(ifd, dwd, IN_ATTRIB, "f", "fuse setxattr must emit IN_ATTRIB");

    CHECK(removexattr("/tmp/fuse_ino/f", "user.k") == 0, "fuse removexattr");
    expect_event(ifd, dwd, IN_ATTRIB, "f", "fuse removexattr must emit IN_ATTRIB");

    int rfd = open("/tmp/fuse_ino/f", O_RDONLY | O_TRUNC);
    CHECK(rfd >= 0, "fuse O_RDONLY|O_TRUNC");
    close(rfd);
    expect_event(ifd, dwd, IN_MODIFY, "f", "fuse O_TRUNC open must emit IN_MODIFY");

    /* After unlink on FUSE, IN_EXCL_UNLINK suppresses write and close events */
    CHECK(unlink("/tmp/fuse_ino/f") == 0, "fuse unlink f");
    expect_event(ifd, dwd, IN_DELETE, "f", "fuse unlink emits IN_DELETE");

    CHECK(write(fd, "z", 1) == 1, "write unlinked fuse fd");
    close(fd);
    n = read_inotify_events(ifd, evs, 32);
    CHECK(!has_event(evs, n, dwd, IN_MODIFY, NULL), "IN_EXCL_UNLINK filters unlinked FUSE modify");
    CHECK(!has_event(evs, n, dwd, IN_CLOSE_WRITE, NULL),
          "IN_EXCL_UNLINK filters unlinked FUSE close");
    close(ifd);

    /* O_SYNC FUSE write sends FUSE_FSYNC (and splice/sendfile to O_SYNC FUSE file too) */
    ctx.fail_create = 0;
    ctx.fsync_ops = 0;
    int sfd = open("/tmp/fuse_ino/syncf", O_CREAT | O_RDWR | O_SYNC, 0644);
    CHECK(sfd >= 0, "open fuse syncf O_SYNC");
    CHECK(write(sfd, "hello", 5) == 5, "write fuse O_SYNC");
    CHECK(ctx.fsync_ops >= 1, "O_SYNC FUSE write must call FUSE_FSYNC (got %d)", ctx.fsync_ops);

    int prev_fsync = ctx.fsync_ops;
    int sp[2];
    CHECK(pipe(sp) == 0, "pipe for fuse splice");
    CHECK(write(sp[1], "xyz", 3) == 3, "write pipe");
    CHECK(splice(sp[0], NULL, sfd, NULL, 3, 0) == 3, "splice into O_SYNC fuse file");
    CHECK(ctx.fsync_ops > prev_fsync, "splice into O_SYNC FUSE file must call FUSE_FSYNC");
    close(sp[0]);
    close(sp[1]);
    close(sfd);

    CHECK(umount2("/tmp/fuse_ino", 0) == 0, "umount fuse_ino");
    pthread_join(th, NULL);
    close(devfd);
}

/* ---- 7. fileio & files regressions ---- */

struct cr_arg {
    int ready_pipe[2];
    int go_pipe[2];
    int high_fd;
    int err_keep;
    int res_drop;
};

static void *close_range_worker(void *arg)
{
    struct cr_arg *a = (struct cr_arg *)arg;
    char c = 1;
    CHECK(write(a->ready_pipe[1], &c, 1) == 1, "signal ready");
    CHECK(read(a->go_pipe[0], &c, 1) == 1, "wait go");

    /* Trying to unshare while keeping high_fd > nr_open must fail with EMFILE */
    errno = 0;
    long r1 = syscall(SYS_close_range, 0U, 2U, CLOSE_RANGE_UNSHARE);
    a->err_keep = (r1 < 0) ? errno : 0;

    /* Closing from high_fd onward drops it before copying, so unshare succeeds */
    errno = 0;
    long r2 = syscall(SYS_close_range, (unsigned int)a->high_fd, ~0U, CLOSE_RANGE_UNSHARE);
    a->res_drop = (int)r2;
    return NULL;
}

static void *lseek_racer(void *arg)
{
    int fd = (int)(intptr_t)arg;
    for (int i = 0; i < 400; i++) {
        off_t pos = lseek(fd, 0, SEEK_SET);
        CHECK(pos == 0, "lseek SEEK_SET");
        char b[4];
        ssize_t n = read(fd, b, sizeof(b));
        CHECK(n >= 0, "concurrent read");
    }
    return NULL;
}

static void test_fileio_and_files(void)
{
    CHECK(mkdir("/tmp/fio", 0755) == 0, "mkdir fio");

    /* Part A: RWF_APPEND and RWF_NOWAIT in preadv2/pwritev2 */
    int fd = open("/tmp/fio/rwf", O_RDWR | O_CREAT | O_TRUNC, 0644);
    CHECK(fd >= 0, "open rwf");
    CHECK(write(fd, "abc", 3) == 3, "write abc");
    CHECK(lseek(fd, 0, SEEK_SET) == 0, "seek 0");

    struct iovec iov = {.iov_base = (void *)"DE", .iov_len = 2};
    ssize_t n = syscall(SYS_pwritev2, fd, &iov, 1, 0L, 0L, RWF_APPEND);
    CHECK(n == 2, "pwritev2 RWF_APPEND");
    int fl = fcntl(fd, F_GETFL);
    CHECK(fl >= 0 && (fl & O_APPEND) == 0, "RWF_APPEND must not mutate O_APPEND in F_GETFL");
    char rbuf[16];
    memset(rbuf, 0, sizeof(rbuf));
    CHECK(pread(fd, rbuf, sizeof(rbuf), 0) == 5, "pread rwf");
    CHECK(memcmp(rbuf, "abcDE", 5) == 0, "RWF_APPEND wrote at EOF (got %.5s)", rbuf);
    close(fd);

    /* RWF_NOWAIT on an empty blocking pipe returns EAGAIN without mutating O_NONBLOCK */
    int p[2];
    CHECK(pipe(p) == 0, "pipe for RWF_NOWAIT");
    struct iovec riov = {.iov_base = rbuf, .iov_len = 4};
    errno = 0;
    n = syscall(SYS_preadv2, p[0], &riov, 1, -1L, -1L, RWF_NOWAIT);
    CHECK(n < 0 && errno == EAGAIN, "preadv2 RWF_NOWAIT on empty pipe must return EAGAIN");
    fl = fcntl(p[0], F_GETFL);
    CHECK(fl >= 0 && (fl & O_NONBLOCK) == 0, "RWF_NOWAIT must not mutate O_NONBLOCK");
    close(p[0]);
    close(p[1]);

    /* Part B: F_GETFL preserves O_DIRECT, O_NOATIME, O_ASYNC, O_DIRECTORY, O_NOFOLLOW */
    fd = open("/tmp/fio/flags", O_RDWR | O_CREAT | O_DIRECT | O_NOATIME | O_ASYNC, 0644);
    CHECK(fd >= 0, "open with O_DIRECT|O_NOATIME|O_ASYNC");
    fl = fcntl(fd, F_GETFL);
    CHECK(fl >= 0, "F_GETFL");
    CHECK((fl & O_DIRECT) != 0, "F_GETFL must preserve O_DIRECT");
    CHECK((fl & O_NOATIME) != 0, "F_GETFL must preserve O_NOATIME");
    CHECK((fl & O_ASYNC) != 0, "F_GETFL must preserve O_ASYNC");
    close(fd);

    int dfd = open("/tmp/fio", O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
    CHECK(dfd >= 0, "open dir with O_DIRECTORY|O_NOFOLLOW");
    fl = fcntl(dfd, F_GETFL);
    CHECK(fl >= 0 && (fl & O_DIRECTORY) != 0, "F_GETFL must preserve O_DIRECTORY");
    close(dfd);

    /* Part C: CLOSE_RANGE_UNSHARE enforces nr_open */
    char saved_nr[32];
    read_proc_str("/proc/sys/fs/nr_open", saved_nr, sizeof(saved_nr));
    struct rlimit rl_orig, rl_hi;
    CHECK(getrlimit(RLIMIT_NOFILE, &rl_orig) == 0, "getrlimit NOFILE");
    rl_hi = rl_orig;
    if (rl_hi.rlim_cur < 2048)
        rl_hi.rlim_cur = 2048;
    if (rl_hi.rlim_max < 2048)
        rl_hi.rlim_max = 2048;
    CHECK(setrlimit(RLIMIT_NOFILE, &rl_hi) == 0, "raise RLIMIT_NOFILE");

    int base = open("/dev/null", O_RDONLY);
    CHECK(base >= 0, "open /dev/null");
    int high = dup2(base, 1024);
    CHECK(high == 1024, "dup2 to fd 1024");
    close(base);

    struct cr_arg cra;
    memset(&cra, 0, sizeof(cra));
    cra.high_fd = high;
    CHECK(pipe(cra.ready_pipe) == 0 && pipe(cra.go_pipe) == 0, "pipes for close_range");
    pthread_t cth;
    CHECK(pthread_create(&cth, NULL, close_range_worker, &cra) == 0, "spawn close_range worker");
    char sig = 0;
    CHECK(read(cra.ready_pipe[0], &sig, 1) == 1, "wait worker ready");

    /* Lower nr_open to 1024 so a table with fd 1024 (size 1088) exceeds nr_open */
    write_proc_str("/proc/sys/fs/nr_open", "1024\n");
    CHECK(write(cra.go_pipe[1], &sig, 1) == 1, "release worker");
    pthread_join(cth, NULL);
    write_proc_str("/proc/sys/fs/nr_open", saved_nr);
    close(high);
    close(cra.ready_pipe[0]);
    close(cra.ready_pipe[1]);
    close(cra.go_pipe[0]);
    close(cra.go_pipe[1]);
    CHECK(setrlimit(RLIMIT_NOFILE, &rl_orig) == 0, "restore RLIMIT_NOFILE");
    CHECK(cra.err_keep == EMFILE,
          "CLOSE_RANGE_UNSHARE keeping fd >= nr_open must fail with EMFILE (got %d)", cra.err_keep);
    CHECK(cra.res_drop == 0,
          "CLOSE_RANGE_UNSHARE closing high fds must succeed (got %d)", cra.res_drop);

    /* Part D: remove_privs on write clears non-exec S_ISGID (Linux 6.2+, aligned with chown) */
    write_file_bytes("/tmp/fio/sgid_other", "0000", 4, 0666);
    CHECK(chown("/tmp/fio/sgid_other", 0, 0) == 0, "chown sgid_other");
    CHECK(chmod("/tmp/fio/sgid_other", S_ISGID | 0666) == 0, "chmod 2666 sgid_other");

    write_file_bytes("/tmp/fio/sgid_member", "0000", 4, 0666);
    CHECK(chown("/tmp/fio/sgid_member", 0, 1000) == 0, "chown sgid_member");
    CHECK(chmod("/tmp/fio/sgid_member", S_ISGID | 0666) == 0, "chmod 2666 sgid_member");

    pid_t pid = fork();
    CHECK(pid >= 0, "fork unpriv writer");
    if (pid == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0, "setresgid 1000");
        CHECK(setresuid(1000, 1000, 1000) == 0, "setresuid 1000");
        int w1 = open("/tmp/fio/sgid_other", O_WRONLY);
        CHECK(w1 >= 0 && write(w1, "X", 1) == 1, "write sgid_other");
        close(w1);
        int w2 = open("/tmp/fio/sgid_member", O_WRONLY);
        CHECK(w2 >= 0 && write(w2, "Y", 1) == 1, "write sgid_member");
        close(w2);
        _exit(0);
    }
    wait_ok(pid, "unpriv write remove_privs");

    struct stat st;
    CHECK(stat("/tmp/fio/sgid_other", &st) == 0, "stat sgid_other");
    CHECK((st.st_mode & S_ISGID) == 0,
          "write by non-group unpriv user must clear non-exec S_ISGID (mode=%o)", st.st_mode);
    CHECK(stat("/tmp/fio/sgid_member", &st) == 0, "stat sgid_member");
    CHECK((st.st_mode & S_ISGID) != 0,
          "write by group member must keep non-exec S_ISGID (mode=%o)", st.st_mode);

    /* Part E: synchronous writev, O_SYNC/O_DSYNC sendfile/copy_file_range/splice, lseek + I/O */
    int sfd = open("/tmp/fio/sync_out", O_RDWR | O_CREAT | O_TRUNC | O_SYNC, 0644);
    CHECK(sfd >= 0, "open sync_out");
    struct iovec wiov[2] = {
        {.iov_base = (void *)"hello ", .iov_len = 6},
        {.iov_base = (void *)"world", .iov_len = 5},
    };
    CHECK(writev(sfd, wiov, 2) == 11, "writev on O_SYNC file");
    close(sfd);

    write_file_bytes("/tmp/fio/src", "0123456789", 10, 0644);
    int in = open("/tmp/fio/src", O_RDONLY);
    int out1 = open("/tmp/fio/dst_sf", O_RDWR | O_CREAT | O_TRUNC | O_DSYNC, 0644);
    int out2 = open("/tmp/fio/dst_cfr", O_RDWR | O_CREAT | O_TRUNC | O_SYNC, 0644);
    CHECK(in >= 0 && out1 >= 0 && out2 >= 0, "open copy fds");
    CHECK(sendfile(out1, in, NULL, 10) == 10, "sendfile to O_DSYNC");
    CHECK(lseek(in, 0, SEEK_SET) == 0, "rewind in");
    CHECK(syscall(SYS_copy_file_range, in, NULL, out2, NULL, 10, 0) == 10,
          "copy_file_range to O_SYNC");
    close(in);
    close(out1);
    close(out2);

    /* Concurrent lseek + read/writev on shared fd */
    int rfd = open("/tmp/fio/src", O_RDONLY);
    CHECK(rfd >= 0, "open src for race test");
    pthread_t t1, t2;
    CHECK(pthread_create(&t1, NULL, lseek_racer, (void *)(intptr_t)rfd) == 0, "spawn racer 1");
    CHECK(pthread_create(&t2, NULL, lseek_racer, (void *)(intptr_t)rfd) == 0, "spawn racer 2");
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    close(rfd);
}

/* ---- 8. Pipe regressions ---- */

static volatile sig_atomic_t got_xfsz = 0;
static void on_xfsz(int sig)
{
    (void)sig;
    got_xfsz = 1;
}

static void check_flock_listing(int fd)
{
    char path[64], info[1024], id[96];
    struct stat st;
    CHECK(fstat(fd, &st) == 0, "stat locked file");
    snprintf(id, sizeof(id), "FLOCK  ADVISORY  WRITE %d %02x:%02x:%llu 0 EOF",
             getpid(), major(st.st_dev), minor(st.st_dev), (unsigned long long)st.st_ino);
    snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
    read_proc_str(path, info, sizeof(info));
    CHECK(strstr(info, id) != NULL, "lock in fdinfo: %s", info);
    read_proc_str("/proc/locks", info, sizeof(info));
    CHECK(strstr(info, id) != NULL, "lock in /proc/locks: %s", info);
}

static void test_pipe_flock(void)
{
    int p[2], other[2];
    CHECK(pipe(p) == 0 && pipe(other) == 0, "create pipes for flock");
    int alias = dup(p[0]);
    CHECK(alias >= 0, "dup pipe read end");
    CHECK(flock(p[0], LOCK_EX | LOCK_NB) == 0, "lock pipe read end");
    CHECK(flock(p[1], LOCK_EX | LOCK_NB) < 0 && errno == EWOULDBLOCK,
          "pipe write end must conflict with read end");
    CHECK(flock(other[0], LOCK_EX | LOCK_NB) == 0, "lock independent pipe");
    close(p[0]);
    CHECK(flock(p[1], LOCK_SH | LOCK_NB) < 0 && errno == EWOULDBLOCK,
          "dup must keep exclusive lock alive");

    check_flock_listing(alias);

    CHECK(flock(alias, LOCK_SH | LOCK_NB) == 0, "downgrade to shared lock");
    CHECK(flock(p[1], LOCK_SH | LOCK_NB) == 0, "share lock with write end");
    CHECK(flock(alias, LOCK_EX | LOCK_NB) < 0 && errno == EWOULDBLOCK,
          "upgrade must conflict with the other shared holder");
    CHECK(flock(p[1], LOCK_EX | LOCK_NB) == 0, "failed upgrade released the old shared lock");
    CHECK(flock(p[1], LOCK_UN) == 0, "unlock pipe write end");
    CHECK(flock(alias, LOCK_EX | LOCK_NB) == 0, "acquire after unlock");
    close(alias);
    CHECK(flock(p[1], LOCK_EX | LOCK_NB) == 0, "acquire after last close");
    close(p[1]);
    close(other[0]);
    close(other[1]);
}

/* POSIX and OFD locks conflict even in one process. Closing any descriptor for the inode
 * drops the process's POSIX locks. The peer can be the opposite end of an anonymous pipe. */
static void check_record_lock(int writer, int reader)
{
    int alias = dup(reader);
    CHECK(alias >= 0, "duplicate descriptor for lock close");
    struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 3, .l_len = 7};
    CHECK(fcntl(writer, F_SETLK, &lock) == 0, "set POSIX lock");
    lock.l_type = F_RDLCK;
    CHECK(fcntl(reader, F_OFD_GETLK, &lock) == 0 && lock.l_type == F_WRLCK &&
          lock.l_pid == getpid() && lock.l_start == 3 && lock.l_len == 7, "query conflicting lock");
    lock.l_type = F_RDLCK;
    lock.l_pid = 0;
    CHECK(fcntl(reader, F_OFD_SETLK, &lock) < 0 && errno == EAGAIN, "OFD lock must conflict");
    close(alias);
    CHECK(fcntl(reader, F_OFD_SETLK, &lock) == 0, "close releases POSIX lock");
    lock.l_type = F_UNLCK;
    CHECK(fcntl(reader, F_OFD_SETLK, &lock) == 0, "unlock OFD lock");
}

static void test_anonymous_locks(void)
{
    int p[2], socks[2];
    CHECK(pipe(p) == 0, "create pipe for record locks");
    check_record_lock(p[1], p[0]);
    close(p[0]);
    close(p[1]);

    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, socks) == 0, "socket pair for locks");
    check_record_lock(socks[0], socks[0]);
    CHECK(flock(socks[0], LOCK_EX | LOCK_NB) == 0 && flock(socks[1], LOCK_EX | LOCK_NB) == 0,
          "socket pair ends lock independently");
    check_flock_listing(socks[0]);
    close(socks[0]);
    close(socks[1]);

    int master = posix_openpt(O_RDWR | O_NOCTTY);
    int other = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(master >= 0 && other >= 0 && unlockpt(master) == 0, "open two PTY masters");
    CHECK(flock(master, LOCK_EX | LOCK_NB) == 0, "lock master");
    CHECK(flock(other, LOCK_EX | LOCK_NB) < 0 && errno == EWOULDBLOCK, "same ptmx inode");
    check_record_lock(master, other);
    int slave = open(ptsname(master), O_RDWR | O_NOCTTY);
    int again = open(ptsname(master), O_RDWR | O_NOCTTY);
    CHECK(slave >= 0 && again >= 0, "open same slave twice");
    CHECK(flock(slave, LOCK_EX | LOCK_NB) == 0, "slave independent of master");
    CHECK(flock(again, LOCK_EX | LOCK_NB) < 0 && errno == EWOULDBLOCK, "same slave inode");
    int pathfd = open(ptsname(master), O_PATH);
    CHECK(pathfd >= 0, "open slave O_PATH");
    struct flock record = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    CHECK(fcntl(slave, F_SETLK, &record) == 0, "lock before O_PATH close");
    close(pathfd);
    CHECK(fcntl(again, F_OFD_GETLK, &record) == 0 && record.l_type == F_WRLCK,
          "O_PATH close keeps the POSIX lock");
    record.l_type = F_UNLCK;
    CHECK(fcntl(slave, F_SETLK, &record) == 0, "unlock slave record");
    check_record_lock(slave, again);
    check_flock_listing(slave);

    /* A character device node can open the same terminal through a different inode. */
    struct stat st;
    const char *alias_path = "/tmp/pty-lock-alias";
    CHECK(fstat(slave, &st) == 0 && mknod(alias_path, S_IFCHR | 0600, st.st_rdev) == 0,
          "create slave device alias");
    int alias = open(alias_path, O_RDWR | O_NOCTTY);
    int same_alias = open(alias_path, O_RDWR | O_NOCTTY);
    CHECK(alias >= 0 && same_alias >= 0, "open slave through store inode");
    CHECK(flock(alias, LOCK_EX | LOCK_NB) == 0, "device alias lock independent of devpts");
    CHECK(flock(same_alias, LOCK_EX | LOCK_NB) < 0 && errno == EWOULDBLOCK, "alias inode conflicts");
    close(same_alias);
    close(alias);
    unlink(alias_path);
    close(again);
    close(slave);
    close(other);
    close(master);
}

static void test_pipe_regressions(void)
{
    /* Part A: read, readv, and read-side vmsplice with faulting buffer do not consume bytes */
    int p[2];
    CHECK(pipe(p) == 0, "pipe");
    CHECK(write(p[1], "ABCDEFGH", 8) == 8, "write pipe");

    errno = 0;
    CHECK(read(p[0], (void *)0x1, 4) < 0 && errno == EFAULT, "pipe read EFAULT");

    struct iovec bad_iov = {.iov_base = (void *)0x1, .iov_len = 4};
    errno = 0;
    CHECK(readv(p[0], &bad_iov, 1) < 0 && errno == EFAULT, "pipe readv EFAULT");

    errno = 0;
    CHECK(vmsplice(p[0], &bad_iov, 1, 0) < 0 && errno == EFAULT, "pipe read-side vmsplice EFAULT");

    /* Partial readv: first iov valid (2 bytes), second faults -> consumes ONLY first 2 bytes */
    char part[4] = {0};
    struct iovec mixed[2] = {
        {.iov_base = part, .iov_len = 2},
        {.iov_base = (void *)0x1, .iov_len = 4},
    };
    ssize_t rn = readv(p[0], mixed, 2);
    CHECK(rn == 2 && memcmp(part, "AB", 2) == 0,
          "partial readv before EFAULT must keep remaining bytes (rn=%zd)", rn);

    char rest[16] = {0};
    rn = read(p[0], rest, sizeof(rest));
    CHECK(rn == 6 && memcmp(rest, "CDEFGH", 6) == 0,
          "pipe bytes preserved across EFAULT calls (got rn=%zd '%s')", rn, rest);
    close(p[0]);
    close(p[1]);

    /* Part B: splice into file hitting RLIMIT_FSIZE raises SIGXFSZ */
    pid_t pid = fork();
    CHECK(pid >= 0, "fork splice fsize");
    if (pid == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = on_xfsz;
        CHECK(sigaction(SIGXFSZ, &sa, NULL) == 0, "sigaction SIGXFSZ");

        struct rlimit rl = {.rlim_cur = 5, .rlim_max = 1024};
        CHECK(setrlimit(RLIMIT_FSIZE, &rl) == 0, "setrlimit RLIMIT_FSIZE");

        int sp[2];
        CHECK(pipe(sp) == 0, "pipe");
        CHECK(write(sp[1], "0123456789", 10) == 10, "write 10 to pipe");
        int out = open("/tmp/fio/fsize_splice", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        CHECK(out >= 0, "open fsize_splice");

        got_xfsz = 0;
        ssize_t n1 = splice(sp[0], NULL, out, NULL, 10, 0);
        CHECK(n1 == 5, "first splice must stop at RLIMIT_FSIZE=5 (got %zd)", n1);
        CHECK(got_xfsz == 1, "first short splice hitting RLIMIT_FSIZE must raise SIGXFSZ");
        got_xfsz = 0;
        errno = 0;
        ssize_t n2 = splice(sp[0], NULL, out, NULL, 5, 0);
        CHECK(n2 < 0 && errno == EFBIG, "second splice at RLIMIT_FSIZE must return EFBIG");
        CHECK(got_xfsz == 1, "second splice hitting RLIMIT_FSIZE must raise SIGXFSZ");
        close(out);
        close(sp[0]);
        close(sp[1]);
        _exit(0);
    }
    wait_ok(pid, "splice RLIMIT_FSIZE SIGXFSZ");

    /* Part C: F_SETPIPE_SZ shrinking above pipe-max-size succeeds without EPERM */
    char saved_max[32];
    read_proc_str("/proc/sys/fs/pipe-max-size", saved_max, sizeof(saved_max));
    CHECK(pipe(p) == 0, "pipe for resize");
    /* Root raises capacity to 256 KiB, then lowers pipe-max-size to 64 KiB */
    int sz = fcntl(p[0], F_SETPIPE_SZ, 256 * 1024);
    CHECK(sz == 256 * 1024, "root F_SETPIPE_SZ to 256 KiB (got %d)", sz);
    write_proc_str("/proc/sys/fs/pipe-max-size", "65536\n");

    pid = fork();
    CHECK(pid >= 0, "fork pipe shrinker");
    if (pid == 0) {
        CHECK(setresgid(1000, 1000, 1000) == 0, "setresgid");
        CHECK(setresuid(1000, 1000, 1000) == 0, "setresuid");

        /* Growing above pipe-max-size without CAP_SYS_RESOURCE must fail with EPERM */
        errno = 0;
        CHECK(fcntl(p[0], F_SETPIPE_SZ, 512 * 1024) < 0 && errno == EPERM,
              "growing pipe above pipe-max-size without CAP_SYS_RESOURCE must return EPERM");

        /* Shrinking from 256 KiB to 128 KiB (still > 64 KiB pipe-max-size) must succeed */
        int shrunk = fcntl(p[0], F_SETPIPE_SZ, 128 * 1024);
        CHECK(shrunk == 128 * 1024,
              "shrinking pipe above pipe-max-size must succeed without EPERM (got %d)", shrunk);
        _exit(0);
    }
    wait_ok(pid, "F_SETPIPE_SZ shrink above pipe-max-size");
    write_proc_str("/proc/sys/fs/pipe-max-size", saved_max);
    close(p[0]);
    close(p[1]);
}

static void rm_rf(const char *path)
{
    struct stat st;
    if (lstat(path, &st) < 0)
        return;
    if (S_ISDIR(st.st_mode)) {
        DIR *d = opendir(path);
        if (d) {
            struct dirent *e;
            while ((e = readdir(d)) != NULL) {
                if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0)
                    continue;
                char sub[512];
                snprintf(sub, sizeof(sub), "%s/%s", path, e->d_name);
                rm_rf(sub);
            }
            closedir(d);
        }
        rmdir(path);
    } else {
        unlink(path);
    }
}

int main(void)
{
    CHECK(mkdir("/tmp", 0777) == 0 || errno == EEXIST, "mkdir /tmp");
    CHECK(chmod("/tmp", 0777) == 0, "chmod /tmp");

    /* Clean leftovers so Tier 0 and Tier 1 can reuse the same rootfs image */
    static const char *const dirs[] = {
        "/tmp/ovl_l", "/tmp/ovl_t",    "/tmp/ovl_p", "/tmp/ovl_f",
        "/tmp/ino_core", "/tmp/fuse_ino", "/tmp/fio",
    };
    for (size_t i = 0; i < sizeof(dirs) / sizeof(dirs[0]); i++)
        rm_rf(dirs[i]);

    test_overlay_leases();
    test_overlay_tmpfs_accounting();
    test_overlay_perm_before_copy_up();
    test_overlay_fifo_fstat();
    test_fifo_last_close("/tmp", "/tmp/watch-fifo");
    test_inotify_core();
    test_fuse_inotify_and_sync();
    test_fileio_and_files();
    test_pipe_flock();
    test_anonymous_locks();
    test_pipe_regressions();

    printf("all ok\n");
    return 0;
}
