# docker-run-options: --privileged
# 新的挂载接口里的 open_tree、move_mount、mount_setattr（docs/rounds/168-open-tree.md）：crun 用
# 它们 bind 挂载（get_bind_mount），容器有用户命名空间（--uidmap）时在外面 open_tree 出一棵
# 树交给里面 move_mount。不带 OPEN_TREE_CLONE 是 O_PATH 的 fd；带的复制出一棵不挂在任何地方
# 的树（AT_RECURSIVE 连同下面的挂载），经由 fd 用得了，move_mount 接上；mount_setattr 改挂载
# 的标志。原生一侧是特权容器，只在 /tmp 下面比
cd /tmp
rm -rf nm
mkdir -p nm/src/sub nm/dst nm/dst2 nm/dst3 nm/dst4
echo hello > nm/src/file
touch nm/afile
mount -t tmpfs -o size=1m tmpfs /tmp/nm/src/sub && echo inside > /tmp/nm/src/sub/f
cat > /tmp/nm.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define OPEN_TREE_CLONE 1
#define AT_RECURSIVE 0x8000
#define MOVE_MOUNT_F_SYMLINKS 1
#define MOVE_MOUNT_F_EMPTY_PATH 4
#define MOVE_MOUNT_T_EMPTY_PATH 0x40
#define MOVE_MOUNT_SET_GROUP 0x100
#define MOUNT_ATTR_RDONLY 1
#define MOUNT_ATTR_NOSUID 2
#define MOUNT_ATTR_NOEXEC 8
#define MOUNT_ATTR__ATIME 0x70
#define MOUNT_ATTR_NOATIME 0x10
#define MOUNT_ATTR_STRICTATIME 0x20
#define MOUNT_ATTR_IDMAP 0x100000

struct mount_attr { uint64_t attr_set, attr_clr, propagation, userns_fd; };

static long open_tree(int dfd, const char *p, unsigned f) { return syscall(428, dfd, p, f); }
static long move_mount(int fd, const char *fp, int td, const char *tp, unsigned f)
{
    return syscall(429, fd, fp, td, tp, f);
}
static long setattr(int dfd, const char *p, unsigned f, struct mount_attr *a, size_t s)
{
    return syscall(442, dfd, p, f, a, s);
}
static const char *err(long r) { return r < 0 ? strerror(errno) : "ok"; }

// /tmp/nm 下面的挂载：挂载点、标志
static void mounts(const char *what)
{
    printf("%s:", what);
    FILE *f = fopen("/proc/self/mountinfo", "r");
    char line[512];
    while (fgets(line, sizeof line, f)) {
        char point[128], opts[128];
        if (sscanf(line, "%*s %*s %*s %*s %127s %127s", point, opts) == 2 && strncmp(point, "/tmp/nm/", 8) == 0)
            printf(" %s(%s)", point + 8, opts);
    }
    fclose(f);
    printf("\n");
}

static void cat(const char *path)
{
    char b[32] = "";
    int fd = open(path, O_RDONLY);
    int n = fd < 0 ? -1 : read(fd, b, sizeof b - 1);
    if (n > 0 && b[n - 1] == '\n')
        b[n - 1] = 0;
    printf("  %s: %s\n", path, n < 0 ? strerror(errno) : b);
    if (fd >= 0)
        close(fd);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    // 不带 CLONE：O_PATH 的 fd
    int p = open_tree(AT_FDCWD, "/tmp/nm/src", 0);
    printf("open_tree plain: %s, O_PATH %d, cloexec %d\n", err(p), (fcntl(p, F_GETFL) & O_PATH) != 0,
           (fcntl(p, F_GETFD) & FD_CLOEXEC) != 0);
    char link[64] = "", path[32];
    snprintf(path, sizeof path, "/proc/self/fd/%d", p);
    readlink(path, link, sizeof link - 1);
    printf("  link %s\n", link);
    printf("move_mount of a plain directory: %s\n", err(move_mount(p, "", AT_FDCWD, "/tmp/nm/dst", MOVE_MOUNT_F_EMPTY_PATH)));
    close(p);
    // 错误的参数
    printf("recursive without clone: %s\n", err(open_tree(AT_FDCWD, "/tmp/nm/src", AT_RECURSIVE)));
    // 用一个不会有定义的位：新内核（CI 的）给 open_tree 加了标志 2
    printf("bad flags: %s\n", err(open_tree(AT_FDCWD, "/tmp/nm/src", 1u << 30)));
    printf("missing: %s\n", err(open_tree(AT_FDCWD, "/tmp/nm/missing", OPEN_TREE_CLONE)));
    printf("empty path without AT_EMPTY_PATH: %s\n", err(open_tree(AT_FDCWD, "", OPEN_TREE_CLONE)));

    // CLONE：不挂在任何地方，经由 fd 用得了
    int t = open_tree(AT_FDCWD, "/tmp/nm/src", OPEN_TREE_CLONE | O_CLOEXEC);
    printf("open_tree clone: %s, cloexec %d\n", err(t), (fcntl(t, F_GETFD) & FD_CLOEXEC) != 0);
    mounts("mounts after clone");
    int f = openat(t, "file", O_RDONLY);
    char b[16] = "";
    read(f, b, sizeof b - 1);
    printf("  file through the clone: %s", b);
    close(f);
    f = openat(t, "sub/f", O_RDONLY);
    printf("  sub/f without AT_RECURSIVE: %s\n", f < 0 ? strerror(errno) : "there");
    close(f);
    struct stat a, s;
    fstat(t, &a);
    stat("/tmp/nm/src", &s);
    printf("  same inode as the source %d\n", a.st_ino == s.st_ino && a.st_dev == s.st_dev);
    // mount_setattr：只读、nosuid
    struct mount_attr ma = {.attr_set = MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID};
    printf("setattr rdonly nosuid: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &ma, sizeof ma)));
    struct mount_attr bad = {.attr_set = 0x4000};
    printf("setattr unknown bit: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &bad, sizeof bad)));
    bad = (struct mount_attr){.propagation = 12345};
    printf("setattr bad propagation: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &bad, sizeof bad)));
    bad = (struct mount_attr){.attr_set = MOUNT_ATTR_NOATIME};
    printf("setattr atime without clearing: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &bad, sizeof bad)));
    bad = (struct mount_attr){.attr_set = MOUNT_ATTR_IDMAP, .userns_fd = 999};
    printf("setattr idmap with a bad fd: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &bad, sizeof bad)));
    printf("setattr small size: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &ma, 16)));
    printf("setattr bad flags: %s\n", err(setattr(t, "", 2, &ma, sizeof ma)));
    struct mount_attr nop = {0};
    printf("setattr nothing: %s\n", err(setattr(t, "", AT_EMPTY_PATH, &nop, sizeof nop)));
    printf("setattr not a mount root: %s\n", err(setattr(AT_FDCWD, "/tmp/nm/src", 0, &ma, sizeof ma)));
    // move_mount 接上
    printf("move_mount bad flags: %s\n", err(move_mount(t, "", AT_FDCWD, "/tmp/nm/dst", 0x1000)));
    printf("move_mount set group: %s\n", err(move_mount(t, "", AT_FDCWD, "/tmp/nm/dst", MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_SET_GROUP)));
    printf("move_mount onto a file: %s\n", err(move_mount(t, "", AT_FDCWD, "/tmp/nm/afile", MOVE_MOUNT_F_EMPTY_PATH)));
    printf("move_mount: %s\n", err(move_mount(t, "", AT_FDCWD, "/tmp/nm/dst", MOVE_MOUNT_F_EMPTY_PATH)));
    mounts("mounts after moving");
    cat("/tmp/nm/dst/file");
    int w = open("/tmp/nm/dst/new", O_CREAT | O_WRONLY, 0600);
    printf("  create in it: %s\n", err(w));
    // 已经接上的再 move_mount 一次：挪过去
    printf("move it again: %s\n", err(move_mount(t, "", AT_FDCWD, "/tmp/nm/dst2", MOVE_MOUNT_F_EMPTY_PATH)));
    mounts("mounts after moving again");
    close(t);

    // AT_RECURSIVE：连同下面的挂载；递归地改标志
    int r = open_tree(AT_FDCWD, "/tmp/nm/src", OPEN_TREE_CLONE | AT_RECURSIVE);
    f = openat(r, "sub/f", O_RDONLY);
    printf("recursive clone: sub/f %s\n", f < 0 ? strerror(errno) : "there");
    close(f);
    ma = (struct mount_attr){.attr_set = MOUNT_ATTR_NOEXEC, .attr_clr = MOUNT_ATTR__ATIME,
                             .propagation = MS_PRIVATE};
    ma.attr_set |= MOUNT_ATTR_STRICTATIME;
    printf("setattr recursive noexec strictatime: %s\n", err(setattr(r, "", AT_EMPTY_PATH | AT_RECURSIVE, &ma, sizeof ma)));
    printf("move_mount with T_EMPTY_PATH: %s\n", err(move_mount(r, "", open("/tmp/nm/dst3", O_PATH), "", MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH)));
    mounts("mounts after the recursive one");
    cat("/tmp/nm/dst3/sub/f");
    close(r);
    // 关掉没接上的：树没了，什么也不挂
    int g = open_tree(AT_FDCWD, "/tmp/nm/src", OPEN_TREE_CLONE);
    close(g);
    mounts("mounts after closing an unattached clone");
    // 不在调用者的命名空间里挂着的挂载不能再 clone（open_tree 出来还没接上的）：6.1、6.8 是
    // EINVAL，新内核（CI 的）可以，不比
    g = open_tree(AT_FDCWD, "/tmp/nm/src", OPEN_TREE_CLONE);
    int gg = open_tree(g, "", OPEN_TREE_CLONE | AT_EMPTY_PATH);
    if (gg >= 0)
        close(gg);
    printf("plain open_tree of it: %s\n", err(open_tree(g, "", AT_EMPTY_PATH)));
    close(g);
    // 普通用户
    if (fork() == 0) {
        setuid(65534);
        printf("nobody open_tree clone: %s\n", err(open_tree(AT_FDCWD, "/tmp/nm/src", OPEN_TREE_CLONE)));
        printf("nobody open_tree plain: %s\n", err(open_tree(AT_FDCWD, "/tmp/nm/src", 0)));
        printf("nobody move_mount: %s\n", err(move_mount(AT_FDCWD, "/tmp/nm/dst2", AT_FDCWD, "/tmp/nm/dst4", 0)));
        printf("nobody setattr: %s\n", err(setattr(AT_FDCWD, "/tmp/nm/dst2", 0, &ma, sizeof ma)));
        exit(0);
    }
    wait(NULL);
    // move_mount 已经挂着的（目录路径，不带 EMPTY_PATH），不是挂载的根是 EINVAL
    printf("move_mount by path: %s\n", err(move_mount(AT_FDCWD, "/tmp/nm/dst2", AT_FDCWD, "/tmp/nm/dst4", 0)));
    printf("move_mount not a mount root: %s\n", err(move_mount(AT_FDCWD, "/tmp/nm/src/file", AT_FDCWD, "/tmp/nm/afile", 0)));
    mounts("mounts at the end");
    return 0;
}
C
gcc -o /tmp/nm/nm /tmp/nm.c 2>&1 | grep -i error
/tmp/nm/nm
umount /tmp/nm/dst3/sub /tmp/nm/dst3 /tmp/nm/dst4 /tmp/nm/src/sub
echo "left: $(grep -c ' /tmp/nm/' /proc/self/mountinfo)"
