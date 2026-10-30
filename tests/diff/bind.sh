# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# 仓库里的目录、文件 bind 挂载：`..`、getcwd 和 mountinfo 经由挂载点，只读、noexec、nosuid、
# nodev、nosymfollow 按经由的挂载算，跨挂载的 rename、link 是 EXDEV，--rbind 带上挂在里面
# 的，MS_MOVE，最后 pivot_root 换掉根目录。原生一侧是有 CAP_SYS_ADMIN 的 docker 容器，根
# 文件系统是 overlay：只比挂载点、看到的目录和挂载的选项，不比文件系统类型和 source
cd /tmp
rm -rf b
mkdir -p b/src/sub b/dst b/mv b/mv2 b/rb b/fd
echo hello >b/src/f
echo inner >b/src/sub/g
touch b/fd/t
mknod b/src/null c 1 3
cp /bin/busybox b/src/bb
ln -s f b/src/link
cat >b/euid.c <<'C'
#include <stdio.h>
#include <unistd.h>
int main(void) { printf("euid %d\n", geteuid()); return 0; }
C
gcc -O1 -static -o b/src/euid b/euid.c
chmod 4755 b/src/euid

# 挂在 /tmp/b 下的：编号换成出现的先后，第四列（看到的目录）、挂载点、选项
show() {
    awk '$5 ~ /^\/tmp\/b/ {
        id[$1] = ++n
        print id[$1], ($2 in id) ? id[$2] : "-", $4, $5, $6
    }' /proc/self/mountinfo
}

mount --bind /tmp/b/src /tmp/b/dst && echo "bind $?"
show
cat /tmp/b/dst/f /tmp/b/dst/sub/g
ls /tmp/b/dst | tr '\n' ' ' && echo
echo "same device: $([ "$(stat -c %d /tmp/b/src)" = "$(stat -c %d /tmp/b/dst)" ] && echo yes)"
echo "same inode: $([ "$(stat -c %i /tmp/b/src/f)" = "$(stat -c %i /tmp/b/dst/f)" ] && echo yes)"
echo written >/tmp/b/dst/new && cat /tmp/b/src/new
# .. 和 getcwd 经由挂载点
cd /tmp/b/dst/sub && pwd -P && cd .. && pwd -P && cd .. && pwd -P
echo "cwd link: $(cd /tmp/b/dst/sub && readlink /proc/self/cwd)"
cd /tmp

# 文件 bind 到文件上
mount --bind /tmp/b/src/f /tmp/b/fd/t && echo "bind a file $?"
cat /tmp/b/fd/t
show | tail -n 1
umount /tmp/b/fd/t && echo "umount the file $?"

# --rbind 带上挂在里面的；有挂载在里面时卸不下，MNT_DETACH 连同里面的一起卸
mount -t proc proc /tmp/b/src/sub && echo "proc on sub $?"
mount --rbind /tmp/b/src /tmp/b/rb && echo "rbind $?"
show
head -c 5 /tmp/b/rb/sub/version && echo
echo "plain bind sees the directory: $(cat /tmp/b/dst/sub/g)"
umount /tmp/b/rb; echo "with a mount inside $?"
umount -l /tmp/b/rb && echo "detach $?"
umount /tmp/b/src/sub
show

python3 - <<'PY'
import ctypes, errno, mmap, os

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_void_p] * 3 + [ctypes.c_ulong, ctypes.c_void_p]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.mprotect.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
libc.syscall.restype = ctypes.c_long

MS_RDONLY, MS_NOSUID, MS_NODEV, MS_NOEXEC = 1, 2, 4, 8
MS_REMOUNT, MS_BIND, MS_MOVE, MS_REC = 0x20, 0x1000, 0x2000, 0x4000
MS_NOSYMFOLLOW = 0x100
S, D = "/tmp/b/src", "/tmp/b/dst"


def err(e):
    return errno.errorcode[e.errno]


def attempt(name, f, *args):
    try:
        r = f(*args)
        print(name, "ok" if r is None or isinstance(r, (int, bool)) else r)
    except OSError as e:
        print(name, err(e))


def mount(source, target, fstype, flags):
    r = libc.mount(source, target, fstype, flags, None)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def remount(flags):
    return mount(None, D.encode(), None, MS_REMOUNT | MS_BIND | flags)


def options():
    with open("/proc/self/mountinfo") as f:
        return [l.split()[5] for l in f if l.split()[4] == D]


# 跨挂载：rename、link 是 EXDEV（同一个文件系统 bind 到两处也是），挂载里面照常
attempt("rename across", os.rename, D + "/new", S + "/new2")
attempt("link across", os.link, D + "/new", S + "/hard")
attempt("rename inside", os.rename, D + "/new", D + "/new2")
attempt("link inside", os.link, D + "/new2", D + "/hard")
os.unlink(D + "/hard")
# 改名、删除挂载点是 EBUSY（经由哪个挂载都算）
attempt("rmdir the mount point", os.rmdir, D)
attempt("rename the mount point", os.rename, D, "/tmp/b/dst2")

# 只读：经由这个挂载的修改都是 EROFS，源那边照常；已经存在的名字先报 EEXIST，设备节点照常
# 能写
print("remount ro", remount(MS_RDONLY), options())
print("f_flag", hex(os.statvfs(D).f_flag), hex(os.statvfs(S).f_flag))
attempt("open for write", os.open, D + "/f", os.O_WRONLY)
attempt("open truncating", os.open, D + "/f", os.O_RDONLY | os.O_TRUNC)
attempt("create", os.open, D + "/created", os.O_WRONLY | os.O_CREAT, 0o644)
attempt("create existing, read only", lambda: os.close(os.open(D + "/f", os.O_RDONLY | os.O_CREAT)))
attempt("mkdir", os.mkdir, D + "/d")
attempt("mkdir existing", os.mkdir, D + "/sub")
attempt("unlink", os.unlink, D + "/f")
attempt("unlink missing", os.unlink, D + "/missing")
attempt("rmdir", os.rmdir, D + "/sub")
attempt("rename", os.rename, D + "/f", D + "/f3")
attempt("symlink", os.symlink, "f", D + "/l2")
attempt("symlink existing", os.symlink, "f", D + "/link")
attempt("mknod", os.mknod, D + "/fifo", 0o10644)
attempt("chmod", os.chmod, D + "/f", 0o600)
attempt("chown", os.chown, D + "/f", 1, 1)
attempt("utime", os.utime, D + "/f", (1, 1))
attempt("truncate", os.truncate, D + "/f", 0)
attempt("setxattr", os.setxattr, D + "/f", "user.x", b"1")
attempt("removexattr", os.removexattr, D + "/f", "user.x")
fd = os.open(D + "/f", os.O_RDONLY)
attempt("fchmod", os.fchmod, fd, 0o600)
attempt("fchown", os.fchown, fd, 1, 1)
attempt("futimes", os.utime, fd, (1, 1))
os.close(fd)
print("access W_OK", os.access(D + "/f", os.W_OK), os.access(D + "/null", os.W_OK))
attempt("read", lambda: open(D + "/f").read().strip())
attempt("write the device", lambda: os.write(os.open(D + "/null", os.O_WRONLY), b"x"))
attempt("write through the source", lambda: open(S + "/f", "a").write("more\n"))
attempt("bind of a read-only mount", mount, D.encode(), b"/tmp/b/mv", None, MS_BIND)
print("inherits ro", [l.split()[5] for l in open("/proc/self/mountinfo") if l.split()[4] == "/tmp/b/mv"])
libc.umount2(b"/tmp/b/mv", 0)
print("remount rw", remount(0), options())

# noexec：执行是 EACCES，映射成可执行是 EPERM，以后也不能 mprotect 成可执行
print("remount noexec", remount(MS_NOEXEC), options())
pid = os.fork()
if pid == 0:
    try:
        os.execv(D + "/bb", ["bb", "true"])
    except OSError as e:
        print("exec", err(e), flush=True)
    os._exit(1)
os.waitpid(pid, 0)
fd = os.open(D + "/bb", os.O_RDONLY)
p = libc.mmap(None, 4096, mmap.PROT_READ | mmap.PROT_EXEC, mmap.MAP_PRIVATE, fd, 0)
print("mmap exec", "ok" if p not in (None, 2**64 - 1) else errno.errorcode[ctypes.get_errno()])
p = libc.mmap(None, 4096, mmap.PROT_READ, mmap.MAP_PRIVATE, fd, 0)
r = libc.mprotect(p, 4096, mmap.PROT_READ | mmap.PROT_EXEC)
print("mprotect exec", "ok" if r == 0 else errno.errorcode[ctypes.get_errno()])
with open("/proc/self/smaps") as f:
    lines = f.read().split("\n")
for i, l in enumerate(lines):
    if l.startswith("%x-" % p):
        flags = next(x for x in lines[i:] if x.startswith("VmFlags")).split()
        print("may exec", "me" in flags, "may write", "mw" in flags)
os.close(fd)
print("exec from the source", os.system(S + "/bb true"))

# nosuid：setuid 位不起作用；nodev：设备节点打不开
print("remount nosuid,nodev", remount(MS_NOSUID | MS_NODEV), options())
for path in (S + "/euid", D + "/euid"):
    pid = os.fork()
    if pid == 0:
        os.setuid(65534)
        os.execv(path, [path])
    os.waitpid(pid, 0)
attempt("open the device", os.open, D + "/null", os.O_RDONLY)
attempt("open the device in the source", lambda: os.close(os.open(S + "/null", os.O_RDONLY)))

# nosymfollow：符号链接（包括魔法链接）不跟随，readlink 照常
print("remount nosymfollow", remount(MS_NOSYMFOLLOW), options())
attempt("follow", lambda: open(D + "/link").read().strip())
attempt("readlink", os.readlink, D + "/link")
attempt("follow in the source", lambda: open(S + "/link").read().strip())
print("remount plain", remount(0), options())

# MS_MOVE：连同里面的挂载一起挪；源要是挂载的根，目标不能在它里面
print("move", mount(D.encode(), b"/tmp/b/mv2", None, MS_MOVE))
print("moved", os.path.exists(D + "/f"), open("/tmp/b/mv2/f").read().split())
print("move a directory that is not a mount", mount(b"/tmp/b/mv2/sub", b"/tmp/b/mv", None, MS_MOVE))
print("move inside itself", mount(b"/tmp/b/mv2", b"/tmp/b/mv2/sub", None, MS_MOVE))
print("move the root", mount(b"/", b"/tmp/b/mv", None, MS_MOVE))
print("move back", mount(b"/tmp/b/mv2", D.encode(), None, MS_MOVE))

# pivot_root 的错误：要有 CAP_SYS_ADMIN，new_root 要是挂载的根、不能是根目录所在的挂载，
# put_old 要在 new_root 下面
def pivot(new, old):
    r = libc.syscall(41, new, old)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


print("pivot, not a mount", pivot(b"/tmp/b/mv", b"/tmp/b/mv"))
print("pivot onto the root", pivot(b"/", b"/tmp"))
print("pivot, put_old outside", pivot(D.encode(), b"/tmp/b/mv"))
print("pivot, not a directory", pivot(D.encode(), (D + "/f").encode()))
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    print("pivot, unprivileged", pivot(D.encode(), (D + "/sub").encode()), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
PY
show
umount /tmp/b/dst && echo "umount $?"

# 最后用 pivot_root 换掉根目录：新的根是 bind 到自己上的目录，原来的根挂到它里面的 old
# 上；所有根目录是原来的根的进程都换过去。之后只用新根里的 busybox
mkdir -p b/nr/bin b/nr/lib b/nr/old b/nr/proc b/nr/tmp
cp /bin/busybox b/nr/bin/
cp /lib/ld-musl-aarch64.so.1 b/nr/lib/
mount --bind /tmp/b/nr /tmp/b/nr && echo "bind onto itself $?"
cd /tmp/b/nr
/bin/busybox pivot_root . old && echo "pivot_root $?"
cd /
/bin/busybox echo "root: $(/bin/busybox ls / | /bin/busybox tr '\n' ' ')"
/bin/busybox echo "old: $(/bin/busybox ls /old/tmp/b | /bin/busybox tr '\n' ' ')"
/bin/busybox mount -t proc proc /proc && /bin/busybox echo "proc $?"
/bin/busybox echo "pwd: $(/bin/busybox pwd -P)"
/bin/busybox umount -l /old && /bin/busybox echo "umount old $?"
/bin/busybox awk '{ id[$1] = ++n; print id[$1], ($2 in id) ? id[$2] : "-", $4, $5 }' /proc/self/mountinfo
/bin/busybox echo "old is empty: $(/bin/busybox ls /old | /bin/busybox wc -l)"
