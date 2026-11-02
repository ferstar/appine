# docker-run-options: --privileged
# 可写的 cgroup v2（docs/rounds/163-cgroup-tree.md）：建、删子 cgroup，迁移进程，
# /proc/<pid>/cgroup，cgroup.events、cgroup.stat，subtree_control 的规则，cgroup.kill，
# cgroup.max.depth、max.descendants，接口文件的格式，cgroup 命名空间里的路径，clone3 的
# CLONE_INTO_CGROUP。原生一侧是特权容器（cgroup 可写）。容器的根 cgroup 是 cgroup 命名空间的
# 根，里面有进程、开不了控制器，写它的大多数文件是 EPERM，appine 的根是真正的根，所以只在下面
# 建的 t 里比。不比文件的完整列表（新内核有 PSI、cpu.stat.local）和大小
cd /sys/fs/cgroup
rmdir t/a/x t/a t/b t 2>/dev/null
mkdir t && echo "mkdir t"
mkdir t 2>&1
stat -c '%A %U %n' t t/cgroup.procs t/cgroup.kill t/cgroup.controllers t/cgroup.subtree_control
for f in cgroup.controllers cgroup.events cgroup.freeze cgroup.kill cgroup.max.depth \
    cgroup.max.descendants cgroup.procs cgroup.stat cgroup.subtree_control cgroup.threads \
    cgroup.type cpu.stat memory.max pids.max; do
    [ -e t/$f ] && echo "has $f" || echo "no $f"
done
echo "controllers [$(cat t/cgroup.controllers)] subtree [$(cat t/cgroup.subtree_control)]"
echo "type $(cat t/cgroup.type)"
mkdir t/a t/b
# cgroup.stat 只比 6.1 就有的两项：新内核（CI 的）多了每个控制器的 nr_subsys_*
grep -vE '^nr_(dying_)?subsys_' t/cgroup.stat
cat t/cgroup.events
# 迁移进程
sleep 100 &
pid=$!
echo $pid > t/a/cgroup.procs && echo "moved to t/a"
cat /proc/$pid/cgroup
grep -qx $pid t/a/cgroup.procs && echo "t/a/cgroup.procs has it"
grep -qx $pid cgroup.procs || echo "the root no longer has it"
echo "t: $(tr '\n' ' ' < t/cgroup.events)/ t/a: $(tr '\n' ' ' < t/a/cgroup.events)/ t/b: $(tr '\n' ' ' < t/b/cgroup.events)"
rmdir t/a 2>&1
rmdir t 2>&1
echo $pid > t/b/cgroup.procs && cat /proc/$pid/cgroup
# 命名空间的根（原生是容器的根，appine 是真正的根）上只能写 cgroup.procs、cgroup.threads、
# cgroup.subtree_control
echo 1 > cgroup.max.depth
echo max > memory.max
echo 1 > memory.current
# 写错的值
echo $pid > t/a/cgroup.threads
echo abc > t/a/cgroup.procs
echo 999999 > t/a/cgroup.procs
echo -5 > t/a/cgroup.procs
for v in "+bogus" "memory" "+memory" "-memory" ""; do
    echo "$v" > t/cgroup.subtree_control && echo "subtree_control '$v' ok"
done
for f in cgroup.controllers cgroup.events cgroup.stat cpu.stat; do
    echo 1 > t/$f
done
echo 2 > t/cgroup.kill
echo abc > t/cgroup.kill
echo 2 > t/cgroup.freeze
echo 0 > t/cgroup.freeze && echo "freeze 0 ok"
echo x > t/cgroup.type
echo domain > t/cgroup.type
cat t/cgroup.kill
touch t/new
rm -f t/cgroup.procs
mknod t/fifo p
# 深度和个数的上限
echo -1 > t/cgroup.max.depth
echo 1 > t/cgroup.max.depth
mkdir t/a/x
echo "depth $(cat t/cgroup.max.depth)"
echo max > t/cgroup.max.depth
mkdir t/a/x && echo "t/a/x after max"
rmdir t/a/x
echo 2 > t/cgroup.max.descendants
mkdir t/c
echo max > t/cgroup.max.descendants
echo "descendants $(cat t/cgroup.max.descendants)"
# clone3 的 CLONE_INTO_CGROUP（放在 cgroup.kill 之前：6.8 写过 cgroup.kill 之后，经由它放进子树的子进程
# 也被结束，新内核改了）
cat > /tmp/into.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
struct clone_args { unsigned long long flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
static void into(const char *what, int fd)
{
    struct clone_args a = {.flags = 0x200000000ULL, .exit_signal = SIGCHLD, .cgroup = fd};
    fflush(stdout);
    long r = syscall(435, &a, sizeof a);
    if (r == 0) {
        char buf[64] = "";
        int f = open("/proc/self/cgroup", O_RDONLY);
        read(f, buf, sizeof buf - 1);
        printf("%s: child in %s", what, buf);
        fflush(stdout);
        _exit(0);
    }
    if (r < 0)
        printf("%s: %s\n", what, strerror(errno));
    else
        waitpid(r, NULL, 0);
}
int main(void)
{
    into("t/b", open("/sys/fs/cgroup/t/b", O_RDONLY | O_DIRECTORY));
    into("O_PATH t/a", open("/sys/fs/cgroup/t/a", O_PATH | O_DIRECTORY));
    into("not a cgroup", open("/tmp", O_RDONLY | O_DIRECTORY));
    into("bad fd", 1000);
    return 0;
}
C
gcc -o /tmp/into /tmp/into.c && /tmp/into
# cgroup.kill：子树里的进程都结束
echo 1 > t/cgroup.kill && echo "killed"
# 子进程结束得早的话 ash 在别处回收了它，不报 Killed
wait $pid 2>/dev/null
echo "status $?"
echo "t/b: $(tr '\n' ' ' < t/b/cgroup.events)"
# 子进程随父进程；cgroup 命名空间里看到的路径
sh -c "echo \$\$ > t/a/cgroup.procs; sleep 0.2; cat /proc/self/cgroup; exec unshare -C sh -c 'cat /proc/self/cgroup; echo 1 > /sys/fs/cgroup/t/a/cgroup.max.depth; sleep 100 & echo \$! > /sys/fs/cgroup/t/b/cgroup.procs; cat /proc/\$!/cgroup; kill \$!'"
# 普通用户
su nobody -s /bin/sh -c 'mkdir /sys/fs/cgroup/t/x; echo $$ > /sys/fs/cgroup/t/a/cgroup.procs' 2>&1
rmdir t/a t/b t 2>&1
ls -d t 2>&1
# 合成文件系统里已有的名字：mkdir 是 EEXIST，在目录能不能修改之前（mkdir -p 靠它）
for d in /sys /sys/fs /sys/fs/cgroup /proc/sys /proc/self /dev/pts; do
    mkdir $d 2>&1
done
mkdir -p /sys/fs/cgroup/t/p/q && echo "mkdir -p ok"
rmdir /sys/fs/cgroup/t/p/q /sys/fs/cgroup/t/p /sys/fs/cgroup/t
