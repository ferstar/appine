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

# 用户命名空间
cat /proc/self/uid_map /proc/self/gid_map /proc/self/projid_map /proc/self/setgroups
echo "max_user_namespaces: $(test -r /proc/sys/user/max_user_namespaces && echo yes)"
echo "unshare -r: $(unshare -r sh -c 'id -u; id -g; cat /proc/self/uid_map' | tr -s ' \n' ' ')"
python3 - <<'PY2'
import ctypes, errno, functools, os, threading

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.syscall.restype = ctypes.c_long
CLONE_NEWNS, CLONE_NEWUSER, CLONE_FS = 0x20000, 0x10000000, 0x200


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def write(path, data):
    try:
        with open(path, "w") as f:
            f.write(data)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def read(path):
    with open(path) as f:
        return " ".join(f.read().split())


def capeff():
    with open("/proc/self/status") as f:
        return [l.split()[1] for l in f if l.startswith("CapEff")][0]


def fork(f):
    """在子进程里运行 f：出了什么错都打印出来，然后退出，不会接着跑父进程的代码。"""
    pid = os.fork()
    if pid == 0:
        try:
            f()
        except OSError as e:
            print("raised", errno.errorcode[e.errno])
        except BaseException as e:
            print("raised", repr(e))
        finally:
            os._exit(0)
    return pid


def child(f):
    pid = fork(f)
    os.waitpid(pid, 0)
    return pid


init_user = os.open("/proc/self/ns/user", os.O_RDONLY)
print("setns to own user namespace", call(libc.setns, init_user, CLONE_NEWUSER))


# root 新建的：没写映射时 ID 是 65534，有全部能力；自己写 0 0 1；gid_map 要先禁止 setgroups
def root_inside():
    os.unshare(CLONE_NEWUSER)
    print("new user ns", os.readlink("/proc/self/ns/user") != os.readlink("/proc/self/fd/%d" % init_user))
    # 别的用户命名空间里的进程（父进程）的链接看不了：不在同一个用户命名空间，又没有那边的
    # CAP_SYS_PTRACE
    try:
        os.readlink("/proc/%d/ns/user" % os.getppid())
    except OSError as e:
        print("parent's link", errno.errorcode[e.errno])
    print("unmapped", os.getuid(), os.getgid(), os.getresuid(), "map", repr(read("/proc/self/uid_map")))
    print("caps", capeff(), "setgroups", read("/proc/self/setgroups"))
    print("bad map", write("/proc/self/uid_map", "0 0\n"), write("/proc/self/uid_map", "0 0 0\n"))
    print("map someone else", write("/proc/self/uid_map", "0 1000 1\n"))
    print("uid_map", write("/proc/self/uid_map", "0 0 1\n"), os.getuid(), read("/proc/self/uid_map"))
    print("again", write("/proc/self/uid_map", "0 0 1\n"))
    print("gid_map before deny", write("/proc/self/gid_map", "0 0 1\n"))
    print("deny", write("/proc/self/setgroups", "deny"), read("/proc/self/setgroups"))
    print("allow again", write("/proc/self/setgroups", "allow"), write("/proc/self/setgroups", "bogus"))
    print("gid_map", write("/proc/self/gid_map", "0 0 1\n"), os.getgid())
    try:
        os.setgroups([])
        print("setgroups ok")
    except OSError as e:
        print("setgroups", errno.errorcode[e.errno])
    for uid in (1, 5000):
        try:
            os.setuid(uid)
        except OSError as e:
            print("setuid", uid, errno.errorcode[e.errno])
    print("stat /", os.stat("/").st_uid, "stat /proc/1", os.stat("/proc/1").st_uid)
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.dup2(w, 1)
        os.execv("/bin/sh", ["sh", "-c", "id -u; grep CapEff /proc/self/status"])
    os.close(w)
    print("exec", os.read(r, 100).split())
    os.waitpid(pid, 0)
    # 在这个用户命名空间里不能回到上一层的
    print("setns to the parent", call(libc.setns, init_user, CLONE_NEWUSER))
    # 外面的 root 看它的映射
    os.write(wfd, b"x")
    os.read(rfd2, 1)


rfd, wfd = os.pipe()
rfd2, wfd2 = os.pipe()
pid = fork(root_inside)
os.close(wfd)
os.read(rfd, 1)
print("parent sees", read("/proc/%d/uid_map" % pid), "|", read("/proc/%d/gid_map" % pid), "|", read("/proc/%d/setgroups" % pid))
with open("/proc/%d/status" % pid) as f:
    print("parent sees ids", [l.split()[1:3] for l in f if l.split()[0] in ("Uid:", "Gid:")])


# 外面的 root 可以 setns 进去
def enter():
    fd = os.open("/proc/%d/ns/user" % pid, os.O_RDONLY)
    print("setns into it", call(libc.setns, fd, CLONE_NEWUSER), os.getuid(), capeff())


child(enter)
os.write(wfd2, b"x")
os.waitpid(pid, 0)


# 普通用户自己建的用户命名空间不在这里比：Ubuntu 23.10 起的
# kernel.apparmor_restrict_unprivileged_userns（开发用的虚拟机、GitHub 的 runner 都开着）不让
# 它们在里面有能力，原生一侧比出来的不是上游 Linux 的行为。这部分在 hosts/cli/tests 里按
# 上游的行为检查（user_namespace_of_an_unprivileged_user）


# 错误：多线程不能 unshare，CLONE_NEWUSER 和 CLONE_FS 不能同用，chroot 了不能建，嵌套最多 32 层
ev = threading.Event()
t = threading.Thread(target=ev.wait)
t.start()
try:
    os.unshare(CLONE_NEWUSER)
except OSError as e:
    print("threaded unshare", errno.errorcode[e.errno])
ev.set()
t.join()
print("clone NEWUSER with FS", libc.syscall(220, CLONE_NEWUSER | CLONE_FS | 17, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])


def chrooted():
    os.chroot("/tmp")
    os.unshare(CLONE_NEWUSER)
    print("chrooted unshare ok")


child(chrooted)


def nested():
    for level in range(1, 40):
        try:
            os.unshare(CLONE_NEWUSER)
        except OSError as e:
            print("nesting stops at", level, errno.errorcode[e.errno])
            return
        write("/proc/self/uid_map", "0 0 1\n")
        write("/proc/self/setgroups", "deny")
        write("/proc/self/gid_map", "0 0 1\n")


child(nested)
PY2
rm -rf /tmp/ns
