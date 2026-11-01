# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# 容器运行时（podman、crun、docker、runc）用到的内核接口（docs/rounds/157-container-survey.md
# 起）：capget 探测版本、开机时的命名空间里拉起已经起着的 lo、合成文件系统根目录的 inode
# 编号、线程目录里的 fd；不带只读地挂 cgroup2；mqueue 文件系统
set -e
cd /tmp

# capget：数据指针为空时只是探测版本，不认识的版本也返回 0 并写回内核的版本，不看 pid
# （moby/sys/capability 这样取版本）；有数据指针时才查版本、pid
cat > capget.c <<'C'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

// 仓库里没有内核的头文件，照 linux/capability.h 定义
#define _LINUX_CAPABILITY_VERSION_3 0x20080522
struct __user_cap_header_struct {
    unsigned version;
    int pid;
};
struct __user_cap_data_struct {
    unsigned effective, permitted, inheritable;
};

static void probe(const char *what, unsigned version, int pid, int with_data)
{
    struct __user_cap_header_struct h = {version, pid};
    struct __user_cap_data_struct d[2];
    long r = syscall(SYS_capget, &h, with_data ? d : NULL);
    printf("%-28s ret=%ld %-6s version=%#x\n", what, r, r ? strerror(errno) : "", h.version);
}

int main(void)
{
    probe("probe", 0, 0, 0);
    probe("probe pid -1", 0, -1, 0);
    probe("probe unknown version", 0x12345678, 0, 0);
    probe("v3 no data pid -1", _LINUX_CAPABILITY_VERSION_3, -1, 0);
    probe("v3 no data no such pid", _LINUX_CAPABILITY_VERSION_3, 999999, 0);
    probe("unknown version with data", 0, 0, 1);
    probe("v3 pid -1 with data", _LINUX_CAPABILITY_VERSION_3, -1, 1);
    probe("v3 no such pid with data", _LINUX_CAPABILITY_VERSION_3, 999999, 1);
    probe("v3 self with data", _LINUX_CAPABILITY_VERSION_3, 0, 1);
    return 0;
}
C
gcc -D_GNU_SOURCE -o capget capget.c && ./capget

# 开机时的网络命名空间里修改 lo：什么也不改的成功（dockerd 给默认网络沙箱拉起 lo），错误码
# 照 Linux 6.1 的 rtnl_setlink、__rtnl_newlink。真要改的请求在原生上会改掉容器的 lo，不比
cat > nlset.c <<'C'
#include <errno.h>
#include <net/if.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

// 照 linux/netlink.h、linux/rtnetlink.h 定义
#define NETLINK_ROUTE 0
#define NLM_F_REQUEST 1
#define NLM_F_ACK 4
#define NLM_F_REPLACE 0x100
#define NLM_F_EXCL 0x200
#define NLM_F_CREATE 0x400
#define NLMSG_ERROR 2
#define RTM_NEWLINK 16
#define RTM_SETLINK 19
#define IFLA_IFNAME 3
#define IFLA_MTU 4
struct nlmsghdr {
    unsigned nlmsg_len;
    unsigned short nlmsg_type, nlmsg_flags;
    unsigned nlmsg_seq, nlmsg_pid;
};
struct nlmsgerr {
    int error;
    struct nlmsghdr msg;
};
struct ifinfomsg {
    unsigned char ifi_family, pad;
    unsigned short ifi_type;
    int ifi_index;
    unsigned ifi_flags, ifi_change;
};
struct rtattr {
    unsigned short rta_len, rta_type;
};
#define NLMSG_LENGTH(n) ((n) + sizeof(struct nlmsghdr))
#define NLMSG_DATA(h) ((void *)((char *)(h) + sizeof(struct nlmsghdr)))
#define RTA_ALIGN(n) (((n) + 3) & ~3)
#define RTA_LENGTH(n) (sizeof(struct rtattr) + (n))
#define RTA_DATA(a) ((void *)((char *)(a) + sizeof(struct rtattr)))

static int sock;

static void ask(const char *what, int type, int flags, int index, unsigned ifi_flags, unsigned change,
                const char *name, int mtu)
{
    struct {
        struct nlmsghdr h;
        struct ifinfomsg i;
        char attrs[64];
    } m;
    memset(&m, 0, sizeof m);
    m.h.nlmsg_type = type;
    m.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    m.h.nlmsg_seq = 1;
    m.i.ifi_index = index;
    m.i.ifi_flags = ifi_flags;
    m.i.ifi_change = change;
    int len = NLMSG_LENGTH(sizeof m.i);
    if (name) {
        struct rtattr *a = (struct rtattr *)((char *)&m + len);
        a->rta_type = IFLA_IFNAME;
        a->rta_len = RTA_LENGTH(strlen(name) + 1);
        strcpy(RTA_DATA(a), name);
        len += RTA_ALIGN(a->rta_len);
    }
    if (mtu) {
        struct rtattr *a = (struct rtattr *)((char *)&m + len);
        a->rta_type = IFLA_MTU;
        a->rta_len = RTA_LENGTH(4);
        memcpy(RTA_DATA(a), &mtu, 4);
        len += RTA_ALIGN(a->rta_len);
    }
    m.h.nlmsg_len = len;
    if (send(sock, &m, len, 0) < 0) {
        printf("%-30s send: %s\n", what, strerror(errno));
        return;
    }
    char buf[4096];
    int n = recv(sock, buf, sizeof buf, 0);
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    if (n < 0 || h->nlmsg_type != NLMSG_ERROR) {
        printf("%-30s no ack\n", what);
        return;
    }
    int e = -((struct nlmsgerr *)NLMSG_DATA(h))->error;
    printf("%-30s %s\n", what, e ? strerror(e) : "ok");
}

int main(void)
{
    sock = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    int lo = if_nametoindex("lo");
    ask("newlink up", RTM_NEWLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("setlink up", RTM_SETLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("newlink up by name", RTM_NEWLINK, 0, 0, IFF_UP, IFF_UP, "lo", 0);
    ask("setlink up by name", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, "lo", 0);
    ask("no flags", RTM_NEWLINK, 0, lo, 0, 0, NULL, 0);
    ask("whole flags", RTM_NEWLINK, 0, lo, IFF_UP | IFF_LOOPBACK | IFF_RUNNING, 0, NULL, 0);
    ask("same name and mtu", RTM_NEWLINK, NLM_F_CREATE, lo, IFF_UP, IFF_UP, "lo", 65536);
    ask("no such index", RTM_NEWLINK, 0, 9999, IFF_UP, IFF_UP, NULL, 0);
    ask("no such name", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, "nosuch0", 0);
    ask("create unknown type", RTM_NEWLINK, NLM_F_CREATE, 0, 0, 0, "nosuch0", 0);
    ask("excl", RTM_NEWLINK, NLM_F_EXCL, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("replace", RTM_NEWLINK, NLM_F_REPLACE, lo, IFF_UP, IFF_UP, NULL, 0);
    ask("newlink nothing named", RTM_NEWLINK, 0, 0, IFF_UP, IFF_UP, NULL, 0);
    ask("setlink nothing named", RTM_SETLINK, 0, 0, IFF_UP, IFF_UP, NULL, 0);
    ask("negative index", RTM_NEWLINK, 0, -1, IFF_UP, IFF_UP, NULL, 0);
    if (setuid(65534) == 0)
        ask("nobody", RTM_NEWLINK, 0, lo, IFF_UP, IFF_UP, NULL, 0);
    return 0;
}
C
gcc -D_GNU_SOURCE -o nlset nlset.c && ./nlset
ip link set lo up && echo "ip link set lo up: ok"
ip link set dev lo mtu 65536 && echo "ip link set lo mtu 65536: ok"
ip link set nosuch0 up 2>&1 || true

# 合成文件系统根目录的 inode 编号是 1（runc 打开 /proc 之后检查它）。docker 的
# /sys/fs/cgroup 是 cgroup 命名空间里的子目录，不比
stat -c '%n %i' /proc /sys /dev/pts
mkdir -p mnt/proc mnt/sys
mount -t proc proc mnt/proc && mount -t sysfs sysfs mnt/sys
stat -c '%n %i' mnt/proc mnt/sys
umount mnt/proc mnt/sys

# 线程目录里的 fd、fdinfo：runc 照 pathrs-lite 的做法，经由 openat2 从 /proc 打开
# thread-self/fd/（不跟随魔法链接、不越过挂载），再读里面的链接。主线程、另一个线程，以及
# 在新的 pid 命名空间里（runc init 那样，看到的还是外面的 procfs）
cat > threadfd.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

// 照 linux/openat2.h 定义
struct open_how {
    unsigned long long flags, mode, resolve;
};
#define RESOLVE_NO_XDEV 0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_BENEATH 0x08
#ifndef SYS_openat2
#define SYS_openat2 437
#endif

static void check(const char *who)
{
    int proc = open("/proc", O_PATH | O_DIRECTORY | O_CLOEXEC);
    struct open_how how = {
        .flags = O_PATH | O_NOFOLLOW | O_CLOEXEC,
        .resolve = RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS,
    };
    int dir = syscall(SYS_openat2, proc, "thread-self/fd/", &how, sizeof how);
    if (dir < 0) {
        printf("%s: openat2 thread-self/fd/: %s\n", who, strerror(errno));
        return;
    }
    char a[256] = "", b[256] = "";
    readlinkat(dir, "2", a, sizeof a - 1);
    readlink("/proc/self/fd/2", b, sizeof b - 1);
    struct stat st;
    fstatat(dir, "", &st, AT_EMPTY_PATH);
    int info = syscall(SYS_openat2, proc, "thread-self/fdinfo/2", &how, sizeof how);
    printf("%s: dir=%d same link=%d fdinfo=%d\n", who, S_ISDIR(st.st_mode), strcmp(a, b) == 0, info >= 0);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%ld/fd", syscall(SYS_gettid));
    printf("%s: %s exists=%d\n", who, "/proc/self/task/<tid>/fd", access(path, F_OK) == 0);
}

static void *thread(void *arg)
{
    check("thread");
    return arg;
}

int main(void)
{
    check("main");
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    pthread_join(t, NULL);
    fflush(stdout);
    if (unshare(CLONE_NEWPID) == 0) {
        pid_t c = fork();
        if (c == 0) {
            check("in a new pid namespace");
            return 0;
        }
        waitpid(c, NULL, 0);
    }
    return 0;
}
C
gcc -D_GNU_SOURCE -pthread -o threadfd threadfd.c && ./threadfd
ls /proc/$$/task/$$/ | grep -x 'fd\|fdinfo'

# cgroup2 可以不带只读挂上（crun 给容器挂 /sys/fs/cgroup 时这样挂），之后 bind 地重新挂成
# 只读、可写。写文件、建子 cgroup 在原生上真会改，不比；不带 bind 的 remount 改的是超级块，
# 原生上会改到 docker 给容器的 cgroup，也不做
mkdir -p mnt/cg
mount -t cgroup2 cgroup2 mnt/cg && echo "cgroup2 rw: ok"
opts() { awk -v p="$PWD/$1" '$5 == p {split($6, o, ","); print $5, o[1]}' /proc/self/mountinfo; }
opts mnt/cg
python3 -c 'import os; print("ST_RDONLY", os.statvfs("mnt/cg").f_flag & 1)'
[ -f mnt/cg/cgroup.procs ] && echo "has cgroup.procs"
mount -o remount,bind,ro mnt/cg && opts mnt/cg
python3 -c 'import os; print("ST_RDONLY", os.statvfs("mnt/cg").f_flag & 1)'
mount -o remount,bind,rw mnt/cg && opts mnt/cg
umount mnt/cg

# mqueue 文件系统（crun 给每个容器挂 /dev/mqueue）：列出的是挂上它的进程所在的 IPC 命名
# 空间里的 POSIX 消息队列。根目录 01777、大小按队列数；队列文件读到状态行；在里面新建的
# 是队列（默认的大小），fd 可以当 mq 描述符用；unlink 删除队列（粘滞位：别人的是 EPERM，
# libc 的 mq_unlink 换成 EACCES）；mkdir、mknod、symlink、rename 是 EPERM；同一个命名
# 空间在自己的根上再挂是 EBUSY，新的 IPC 命名空间里是另一个实例
mkdir -p mnt/mq mnt/mq2
mount -t mqueue mqueue mnt/mq && echo "mqueue: ok"
awk -v p="$PWD/mnt/mq" '$5 == p {for (i = 7; $i != "-"; i++) {}; split($6, o, ","); print "mountinfo", o[1], $(i + 1), $(i + 2)}' /proc/self/mountinfo
stat -c '%A %h %s %u' mnt/mq
stat -f -c 'type %t' mnt/mq
mount -t mqueue mqueue mnt/mq 2>&1 || true
cat > mqfs.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <dirent.h>
#include <unistd.h>

#define D "/tmp/mnt/mq/"

static void show(const char *what, long r)
{
    printf("%-34s %s\n", what, r < 0 ? strerror(errno) : "ok");
}

static void ls(const char *dir)
{
    DIR *d = opendir(dir);
    struct dirent *e;
    printf("ls %s:", dir);
    while (d && (e = readdir(d)))
        if (e->d_name[0] != '.')
            printf(" %s(%s)", e->d_name, e->d_type == DT_REG ? "reg" : "?");
    printf("\n");
    if (d)
        closedir(d);
}

int main(void)
{
    umask(022);
    struct mq_attr a = {.mq_maxmsg = 4, .mq_msgsize = 64};
    mqd_t q1 = mq_open("/q1", O_CREAT | O_RDWR, 0640, &a);
    show("mq_open /q1", q1);
    show("mq_send", mq_send(q1, "hello", 5, 3));
    struct stat st, fst;
    stat(D "q1", &st);
    fstat(q1, &fst);
    printf("q1: mode %o nlink %d size %lld uid %d same ino %d same dev %d\n", st.st_mode, (int)st.st_nlink,
           (long long)st.st_size, st.st_uid, st.st_ino == fst.st_ino, st.st_dev == fst.st_dev);
    struct stat dst;
    stat(D, &dst);
    printf("dir: size %lld, queue on the same device %d\n", (long long)dst.st_size, dst.st_dev == st.st_dev);
    char buf[128] = "";
    int fd = open(D "q1", O_RDONLY);
    int n = read(fd, buf, sizeof buf - 1);
    printf("read %d: %s", n, buf);
    close(fd);
    ls(D);

    // 在文件系统里新建的是队列
    int q2 = open(D "q2", O_CREAT | O_RDWR, 0666);
    show("open O_CREAT q2", q2);
    stat(D "q2", &st);
    printf("q2: mode %o\n", st.st_mode);
    struct mq_attr ga;
    show("mq_getattr on the open fd", mq_getattr(q2, &ga));
    printf("q2 attr: maxmsg %ld msgsize %ld\n", ga.mq_maxmsg, ga.mq_msgsize);
    show("mq_send on the open fd", mq_send(q2, "x", 1, 0));
    mqd_t q2b = mq_open("/q2", O_RDONLY | O_NONBLOCK);
    char m[8192];
    show("mq_receive via mq_open", mq_receive(q2b, m, sizeof m, NULL));
    show("write to the queue fd", write(q2, "x", 1));
    struct statfs sf;
    fstatfs(q2, &sf);
    printf("fstatfs type %lx\n", (unsigned long)sf.f_type);
    show("open O_CREAT|O_EXCL existing", open(D "q2", O_CREAT | O_EXCL | O_RDWR, 0600));
    show("open O_DIRECTORY on a queue", open(D "q2", O_RDONLY | O_DIRECTORY));
    stat(D, &dst);
    printf("dir: size %lld\n", (long long)dst.st_size);

    // 元数据
    show("chmod q1 0600", chmod(D "q1", 0600));
    fstat(q1, &fst);
    printf("q1 mode via mq fd: %o\n", fst.st_mode);
    show("fchmod on the mq fd 0640", fchmod(q1, 0640));
    stat(D "q1", &st);
    printf("q1 mode: %o\n", st.st_mode);
    show("chown q1 1000:1000", chown(D "q1", 1000, 1000));
    stat(D "q1", &st);
    printf("q1 owner: %d %d\n", st.st_uid, st.st_gid);
    struct timespec ts[2] = {{1000, 0}, {2000, 0}};
    show("utimensat q1", utimensat(AT_FDCWD, D "q1", ts, 0));
    stat(D "q1", &st);
    printf("q1 times: %lld %lld\n", (long long)st.st_atime, (long long)st.st_mtime);

    // 不支持的操作
    show("mkdir", mkdir(D "d", 0755));
    show("mknod fifo", mknod(D "f", S_IFIFO | 0644, 0));
    show("symlink", symlink("q1", D "l"));
    show("rename", rename(D "q1", D "q3"));
    show("rmdir a queue", rmdir(D "q1"));
    show("unlink q1/", unlink(D "q1/"));
    show("unlink missing", unlink(D "nosuch"));

    // 别人的队列：粘滞位
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setgid(65534);
        setuid(65534);
        show("nobody unlink q1", unlink(D "q1"));
        show("nobody raw mq_unlink q1", syscall(SYS_mq_unlink, "q1"));
        show("nobody libc mq_unlink q1", mq_unlink("/q1"));
        int fd = open(D "q4", O_CREAT | O_RDWR, 0600);
        show("nobody creates q4", fd);
        struct stat st;
        stat(D "q4", &st);
        printf("q4 owner %d\n", st.st_uid);
        show("nobody unlinks q4", unlink(D "q4"));
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);

    show("unlink q2", unlink(D "q2"));
    show("mq_open /q2 after unlink", mq_open("/q2", O_RDONLY));
    ls(D);

    // 新的 IPC 命名空间里另外挂一个：是空的，设备号不同
    fflush(stdout);
    c = fork();
    if (c == 0) {
        show("unshare CLONE_NEWIPC", unshare(CLONE_NEWIPC));
        show("mount in the new ns", mount("mqueue", "/tmp/mnt/mq2", "mqueue", 0, NULL));
        ls("/tmp/mnt/mq2");
        struct stat a, b;
        stat("/tmp/mnt/mq", &a);
        stat("/tmp/mnt/mq2", &b);
        printf("other instance has its own device: %d\n", a.st_dev != b.st_dev);
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, NULL, 0);
    ls(D);
    mq_unlink("/q1");
    return 0;
}
C
gcc -D_GNU_SOURCE -o mqfs mqfs.c && ./mqfs
umount mnt/mq2 2>/dev/null; umount mnt/mq
