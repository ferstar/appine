# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# 命名空间：/proc/<pid>/ns 的链接和打开之后的文件，挂载命名空间（unshare、clone 的
# CLONE_NEWNS，两边的挂载互不可见，/proc/<pid>/mountinfo 看那个进程的，setns 回去，在新的
# 命名空间里 pivot_root 不影响外面），以及各种错误。原生一侧是有 CAP_SYS_ADMIN 的 docker 容器。
# 命名空间的编号两边不同，只比格式和异同
cd /tmp
rm -rf ns
mkdir -p ns/a ns/b ns/root

echo "ns: $(ls /proc/self/ns | tr '\n' ' ')"
for n in /proc/self/ns/*; do
    readlink "$n" | sed 's/[0-9][0-9]*/N/'
done
stat -L -c '%F %a %h %u %s' /proc/self/ns/mnt
echo "same device for all: $(stat -L -c %d /proc/self/ns/* | sort -u | wc -l)"
echo "not the procfs device: $([ "$(stat -L -c %d /proc/self/ns/mnt)" != "$(stat -c %d /proc/self)" ] && echo yes)"
echo "inode is the number: $([ "$(stat -L -c %i /proc/self/ns/mnt)" = "$(readlink /proc/self/ns/mnt | tr -dc 0-9)" ] && echo yes)"
echo "same for threads: $([ "$(readlink /proc/$$/ns/mnt)" = "$(readlink /proc/$$/task/$$/ns/mnt)" ] && echo yes)"
stat -f -c %t /proc/self/ns/mnt

# busybox 的 unshare -m：里面挂的外面看不到
unshare -m sh -c '
    mount --make-rprivate /
    mount -t tmpfs inside /tmp/ns/a && echo "inside: $(grep -c " /tmp/ns/a " /proc/self/mountinfo)"
    echo "different ns: $([ "$(readlink /proc/self/ns/mnt)" != "$(readlink /proc/$PPID/ns/mnt)" ] && echo yes)"
'
echo "outside: $(grep -c ' /tmp/ns/a ' /proc/self/mountinfo)"

python3 - <<'PY'
import ctypes, errno, functools, os, threading

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.syscall.restype = ctypes.c_long
CLONE_NEWNS, CLONE_FS, CLONE_NEWUTS, CLONE_NEWUSER = 0x20000, 0x200, 0x04000000, 0x10000000
MS_REC, MS_PRIVATE, MS_BIND = 0x4000, 1 << 18, 0x1000


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def mounted(path, pid="self"):
    with open("/proc/%s/mountinfo" % pid) as f:
        return any(l.split()[4] == path for l in f)


def ns(pid="self"):
    return os.readlink("/proc/%s/ns/mnt" % pid)


def private():
    return call(libc.mount, None, b"/", None, MS_REC | MS_PRIVATE, None)


mine = os.open("/proc/self/ns/mnt", os.O_RDONLY)
print("fd link is the namespace", os.readlink("/proc/self/fd/%d" % mine) == ns())
try:
    os.read(mine, 10)
except OSError as e:
    print("read", errno.errorcode[e.errno])
print("setns, wrong type", call(libc.setns, mine, CLONE_NEWUTS))
print("setns, not a namespace", call(libc.setns, 0, 0))
print("setns, own namespace", call(libc.setns, mine, CLONE_NEWNS))
print("cwd after setns", os.getcwd())
os.chdir("/tmp")
print("clone NEWNS with FS", libc.syscall(220, CLONE_NEWNS | CLONE_FS | 17, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])

# 子进程在新的挂载命名空间里：两边后来挂的互不可见，从外面看它的 mountinfo 看得到它的
r1, w1 = os.pipe()
r2, w2 = os.pipe()
pid = os.fork()
if pid == 0:
    os.unshare(CLONE_NEWNS)
    print("child private", private(), flush=True)
    print("child new namespace", ns() != ns(os.getppid()), flush=True)
    print("child mount", call(libc.mount, b"child", b"/tmp/ns/a", b"tmpfs", 0, None), flush=True)
    os.write(w1, b"x")
    os.read(r2, 1)
    print("child sees parent's mount", mounted("/tmp/ns/b"), flush=True)
    print("child sees own mount", mounted("/tmp/ns/a"), flush=True)
    # setns 回到原来的命名空间：根目录、当前目录换成那里的根
    os.chdir("/tmp/ns")
    print("child setns back", call(libc.setns, mine, CLONE_NEWNS), flush=True)
    print("child back", ns() == ns(os.getppid()), os.getcwd(), mounted("/tmp/ns/a"), mounted("/tmp/ns/b"), flush=True)
    os._exit(0)
os.read(r1, 1)
print("parent mount", call(libc.mount, b"parent", b"/tmp/ns/b", b"tmpfs", 0, None))
print("parent sees child's mount", mounted("/tmp/ns/a"))
print("child's mountinfo", mounted("/tmp/ns/a", pid), mounted("/tmp/ns/b", pid))
# 别的命名空间里的挂载：经由 /proc/<pid>/root 也不能 bind、卸下
print("bind from another namespace", call(libc.mount, ("/proc/%d/root/tmp/ns/a" % pid).encode(), b"/tmp/ns/root", None, MS_BIND, None))
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
print("umount in another namespace", call(libc.umount2, ("/proc/%d/root/tmp/ns/a" % pid).encode(), 0))
os.write(w2, b"x")
os.waitpid(pid, 0)
libc.umount2(b"/tmp/ns/b", 0)

# 在新的命名空间里 pivot_root：外面不受影响
pid = os.fork()
if pid == 0:
    os.unshare(CLONE_NEWNS)
    private()
    libc.mount(b"newroot", b"/tmp/ns/root", b"tmpfs", 0, None)
    os.mkdir("/tmp/ns/root/old")
    os.chdir("/tmp/ns/root")
    print("pivot_root inside", call(libc.syscall, 41, b".", b"old"), flush=True)
    os.chdir("/")
    print("new root", sorted(os.listdir("/")), sorted(os.listdir("/old/tmp/ns")), flush=True)
    print("umount old", call(libc.umount2, b"/old", 2), sorted(os.listdir("/old")), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
print("outside root", os.path.isdir("/tmp/ns/root"), mounted("/tmp/ns/root"), os.path.exists("/bin/sh"))

# 多线程的进程不能 setns 挂载命名空间；没有 CAP_SYS_ADMIN 不能 unshare、setns
ev = threading.Event()
t = threading.Thread(target=ev.wait)
t.start()
print("setns with threads", call(libc.setns, mine, CLONE_NEWNS))
ev.set()
t.join()
pid = os.fork()
if pid == 0:
    os.setuid(65534)
    try:
        os.unshare(CLONE_NEWNS)
        print("unprivileged unshare ok", flush=True)
    except OSError as e:
        print("unprivileged unshare", errno.errorcode[e.errno], flush=True)
    print("unprivileged setns", call(libc.setns, mine, CLONE_NEWNS), flush=True)
    os._exit(0)
os.waitpid(pid, 0)
PY
echo "left: $(grep -c ' /tmp/ns' /proc/self/mountinfo)"
rm -rf /tmp/ns
