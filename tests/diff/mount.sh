# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# mount、umount：procfs、sysfs、devpts、cgroup2 挂到别的目录上，bind 挂载它们里面的目录，
# `..` 和 getcwd 经由挂载点，mountinfo 按进程的根目录列出，卸下、EBUSY 等错误。原生一侧是
# 有 CAP_SYS_ADMIN 的 docker 容器，它的 /sys 是只读的、/proc/sys 另有只读的 bind，所以只在
# 新挂的上面比较，不比超级块的选项（devpts 的新实例在 Linux 上默认 mode=600）
cd /tmp
rm -rf mnt
mkdir -p mnt/proc mnt/sys mnt/pts mnt/b mnt/rb mnt/dir "mnt/sp ace"
touch mnt/f

# 挂在 /tmp/mnt 下的：编号换成出现的先后（挂在别处的写 -），不比设备号和超级块的选项
show() {
    awk '$5 ~ /^\/tmp\/mnt/ {
        id[$1] = ++n
        for (i = 7; $i != "-"; i++) {}
        print id[$1], ($2 in id) ? id[$2] : "-", $4, $5, $6, $(i + 1), $(i + 2)
    }' /proc/self/mountinfo
}
# 设备号与哪个挂载的相同（新的 procfs 实例有自己的）。不比 devpts：Linux 上新的实例有自己的
# 设备号和一套终端，appine 的各个挂载共用开机时的那个
device() {
    d=$(stat -c %d "$1")
    for b in /proc /sys /sys/fs/cgroup /dev/pts /tmp/mnt/proc; do
        if [ "$b" != "$1" ] && [ "$d" = "$(stat -c %d $b)" ]; then
            echo "$1: same device as $b"
            return
        fi
    done
    echo "$1: a device of its own"
}

mount -t proc proc /tmp/mnt/proc && echo "proc $?"
mount -t sysfs sysfs /tmp/mnt/sys -o nosuid,nodev && echo "sysfs $?"
mount -t devpts devpts /tmp/mnt/pts -o noexec && echo "devpts $?"
# 新挂的 sysfs 里 fs/cgroup 是空的，cgroup2 要另外挂（只读）
echo "fs/cgroup: $(ls /tmp/mnt/sys/fs/cgroup | wc -l) entries"
mount -t cgroup2 -o ro cgroup /tmp/mnt/sys/fs/cgroup && echo "cgroup2 $?"
grep -qx memory.max /dev/stdin <<EOF && echo "cgroup2 has memory.max"
$(ls /tmp/mnt/sys/fs/cgroup)
EOF
mount -t proc other "/tmp/mnt/sp ace" && echo "space $?"
show
for p in /tmp/mnt/proc /tmp/mnt/sys /tmp/mnt/sys/fs/cgroup; do
    device $p
done
grep -E '^(proc|sysfs|devpts|cgroup|other) /tmp/mnt' /proc/mounts | cut -d ' ' -f 1-3
echo "fs types: $(stat -f -c %t /tmp/mnt/proc /tmp/mnt/sys /tmp/mnt/pts /tmp/mnt/sys/fs/cgroup | tr '\n' ' ')"
echo "ptmx: $(ls /tmp/mnt/pts | grep -c ptmx)"
head -c 5 /tmp/mnt/proc/version && echo

# .. 和 getcwd 经由挂载点回到上一层
cd /tmp/mnt/proc/self && /bin/pwd -P | sed "s/$$/PID/" && cd ../.. && pwd -P
cd /tmp/mnt/sys/fs/cgroup && cd .. && pwd -P && cd ../.. && pwd -P
cd /tmp
echo "proc/self/cwd: $(cd /tmp/mnt/sys/fs && readlink /proc/self/cwd)"

# 同一个实例：sysfs、cgroup2 不能再挂在自己的挂载的根上，procfs、devpts 每次是新的实例
mount -t sysfs sysfs /tmp/mnt/sys; echo "sysfs again $?"
mount -t cgroup2 -o ro cgroup /tmp/mnt/sys/fs/cgroup; echo "cgroup2 again $?"
mount -t proc proc /tmp/mnt/proc; echo "proc again $?"
show | tail -n 1
umount /tmp/mnt/proc; echo "umount the top $?"
mount -t appinefs nosuchsource /tmp/mnt/dir; echo "unknown type $?"
mount -t proc proc /tmp/mnt/f; echo "on a file $?"

# bind：procfs 里的目录、文件，--rbind 带上挂在里面的
mount --bind /tmp/mnt/proc/sys /tmp/mnt/b && echo "bind $?"
ls /tmp/mnt/b | grep -x kernel
mount --bind /tmp/mnt/proc/version /tmp/mnt/f && echo "bind a file $?"
head -c 5 /tmp/mnt/f && echo
mount --bind /tmp/mnt/proc/version /tmp/mnt/dir; echo "file on a directory $?"
mount --rbind /tmp/mnt/sys /tmp/mnt/rb && echo "rbind $?"
show
device /tmp/mnt/b
device /tmp/mnt/rb
device /tmp/mnt/rb/fs/cgroup

# 在用的挂载卸不下，挂载点删不掉、不能改名
rmdir /tmp/mnt/b; echo "rmdir $?"
mv /tmp/mnt/b /tmp/mnt/b2; echo "rename $?"
rm /tmp/mnt/f; echo "unlink $?"
(cd /tmp/mnt/b && umount /tmp/mnt/b; echo "cwd inside $?")
umount /tmp/mnt/rb; echo "with a mount inside $?"
umount /tmp/mnt/rb/fs/cgroup /tmp/mnt/rb /tmp/mnt/b /tmp/mnt/f; echo "umount $?"
umount /tmp/mnt/dir; echo "not a mount point $?"

# 重新挂载成只读：写文件是 EROFS
mount -o remount,ro /tmp/mnt/proc && echo "remount $?"
show | head -n 1
echo x 2>/dev/null >/tmp/mnt/proc/self/comm; echo "write $?"
mount -o remount,rw /tmp/mnt/proc && echo "remount rw $?"

python3 - <<'PY'
import ctypes, errno, os, sys

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_void_p] * 3 + [ctypes.c_ulong, ctypes.c_void_p]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def mount(source, target, fstype, flags=0, data=None):
    return call(libc.mount, source, target, fstype, flags, data)


d = b"/tmp/mnt/py"
os.mkdir(d)
print("mount", mount(b"proc", d, b"proc"))
# 挂载编号与 fdinfo 的 mnt_id 一致
fd = os.open(d + b"/self/status", os.O_RDONLY)
with open("/proc/self/fdinfo/%d" % fd) as f:
    mnt = [l.split()[1] for l in f if l.startswith("mnt_id:")]
with open("/proc/self/mountinfo") as f:
    ids = [l.split()[0] for l in f if l.split()[4] == "/tmp/mnt/py"]
print("mnt_id matches mountinfo", ids == mnt)
os.close(fd)
# statfs 的 f_flags 按挂载的标志（ST_VALID、ST_RELATIME、ST_RDONLY 等）
print("f_flag /", hex(os.statvfs("/").f_flag))
print("f_flag proc", hex(os.statvfs(d).f_flag))
print("f_flag noexec", hex(os.statvfs("/tmp/mnt/pts").f_flag))
print("f_flag cgroup2", hex(os.statvfs("/tmp/mnt/sys/fs/cgroup").f_flag))
print("remount,bind,ro", mount(None, d, None, 0x20 | 0x1000 | 1))
print("f_flag after", hex(os.statvfs(d).f_flag))
print("remount,bind,noexec", mount(None, d, None, 0x20 | 0x1000 | 8))
with open("/proc/self/mountinfo") as f:
    print("options", [l.split()[5] for l in f if l.split()[4] == "/tmp/mnt/py"])
print("remount,bind,strictatime", mount(None, d, None, 0x20 | 0x1000 | (1 << 24)))
with open("/proc/self/mountinfo") as f:
    print("options", [l.split()[5] for l in f if l.split()[4] == "/tmp/mnt/py"])
# 传播类型
print("private", mount(None, d, None, 1 << 18))
print("rprivate", mount(None, d, None, (1 << 18) | 0x4000))
print("private and slave", mount(None, d, None, (1 << 18) | (1 << 19)))
print("private with nosuid", mount(None, d, None, (1 << 18) | 2))
print("private, not a mount point", mount(None, b"/tmp/mnt", None, 1 << 18))
print("remount, not a mount point", mount(None, b"/tmp/mnt", None, 0x20))
# 参数
print("no type", mount(b"x", d, None))
print("MS_NOUSER", mount(b"proc", d, b"proc", 1 << 31))
print("bad type pointer", mount(b"proc", d, 8))
print("bad data pointer", mount(b"proc", d, b"proc", 0, 8))
print("missing target", mount(b"proc", b"/tmp/mnt/none", b"proc"))
print("bind without source", mount(None, d, None, 0x1000))
print("bind a missing source", mount(b"/tmp/mnt/none", d, None, 0x1000))
# 经由 /proc/self/fd 到了已经删除的文件、管道上。挂在 /proc/self/fd/<n> 这一项本身上
# （LTP 的 mount08）不比：Linux 6.12 起是 ENOENT，原生的虚拟机是 6.8
fd = os.open("/tmp/mnt/foo", os.O_RDONLY | os.O_CREAT)
os.unlink("/tmp/mnt/foo")
print("on a deleted file", mount(d + b"/version", ("/proc/self/fd/%d" % fd).encode(), None, 0x1000))
r, w = os.pipe()
print("on a pipe", mount(d + b"/version", ("/proc/self/fd/%d" % r).encode(), None, 0x1000))
for x in (fd, r, w):
    os.close(x)
# MNT_EXPIRE：第一次只标记
print("expire", call(libc.umount2, d, 4))
print("expire again", call(libc.umount2, d, 4))
print("mount", mount(b"proc", d, b"proc"))
print("unknown umount flag", call(libc.umount2, d, 16))
print("expire and detach", call(libc.umount2, d, 6))
# 没有 CAP_SYS_ADMIN
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    print("unprivileged mount", mount(b"proc", d, b"proc"))
    print("unprivileged umount", call(libc.umount2, d, 0), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
# MNT_DETACH：当前目录在里面也卸得下，之后 getcwd 系统调用以 (unreachable) 开头（musl 当作
# ENOENT），/proc/self/cwd 是从卸下的挂载的根算起的路径，文件照样能读
os.chdir(b"/tmp/mnt/py/self")
print("busy", call(libc.umount2, d, 0))
print("detach", call(libc.umount2, d, 2))
buf = ctypes.create_string_buffer(4096)
n = libc.syscall(17, buf, 4096)
print("getcwd syscall", buf.value.replace(str(os.getpid()).encode(), b"PID") if n > 0 else n)
try:
    os.getcwd()
except FileNotFoundError:
    print("getcwd ENOENT")
print("cwd link", os.readlink("/proc/self/cwd").replace(str(os.getpid()), "PID"))
with open("comm") as f:
    print("comm", f.read().strip())
os.chdir("..")
print("still a directory", os.path.isdir("self"))
os.chdir("/")
print("gone", call(libc.umount2, d, 0))
PY

# chroot 进去挂 procfs、devpts：mountinfo 只列根目录下面的，路径相对它；经由新挂的 devpts
# 打开的终端，ttyname 找得到（/proc/self/fd 里的名字与 fstat 的设备号、inode 对得上）
mkdir -p mnt/root/bin mnt/root/lib mnt/root/proc mnt/root/dev/pts
cp /bin/busybox mnt/root/bin/
cp /lib/ld-musl-aarch64.so.1 mnt/root/lib/
ln -s pts/ptmx mnt/root/dev/ptmx
cat > mnt/pty.c <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) {
        perror("ptmx");
        return 1;
    }
    char *name = ptsname(m);
    int s = open(name, O_RDWR | O_NOCTTY);
    struct stat a, b;
    if (s < 0 || stat(name, &a) || fstat(s, &b)) {
        perror(name);
        return 1;
    }
    char *tty = ttyname(s);
    printf("ttyname %s\n", tty && strcmp(tty, name) == 0 ? "is ptsname" : "fails");
    printf("same device and inode %d\n", a.st_dev == b.st_dev && a.st_ino == b.st_ino);
    return 0;
}
C
gcc -O1 -static -o mnt/root/bin/pty mnt/pty.c
chroot /tmp/mnt/root /bin/busybox sh -c '
    /bin/busybox mount -t proc proc /proc && echo "proc in chroot $?"
    /bin/busybox mount -t devpts devpts /dev/pts && echo "devpts in chroot $?"
    /bin/pty
    /bin/busybox awk "{ print \$4, \$5, \$6, \$8, \$9 }" /proc/self/mountinfo
    /bin/busybox cut -d " " -f 1-3 /proc/self/mounts
    cd /proc/self && /bin/busybox pwd -P | /bin/busybox sed "s/[0-9][0-9]*/PID/" && cd ../.. && /bin/busybox pwd -P
    echo "root: $(/bin/busybox readlink /proc/self/root)"
    # 根目录以外的：与 Linux 的 d_path 一样是从真正的根算起的路径，不加 (unreachable)
    echo "outside cwd: $(/bin/busybox readlink /proc/$PPID/cwd)"
    echo "outside root: $(/bin/busybox readlink /proc/$PPID/root)"
    echo "processes: $(/bin/busybox ps -o comm | /bin/busybox grep -c busybox)"
'
grep -c ' /tmp/mnt/root/proc ' /proc/self/mountinfo
umount /tmp/mnt/root/proc /tmp/mnt/root/dev/pts /tmp/mnt/sys/fs/cgroup /tmp/mnt/sys /tmp/mnt/pts /tmp/mnt/proc "/tmp/mnt/sp ace"
echo "left: $(show | wc -l)"
rm -rf /tmp/mnt
