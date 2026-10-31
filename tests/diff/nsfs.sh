# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# 命名空间文件（nsfs）：打开着的、bind 挂载出去的命名空间文件让命名空间活着，里面的进程都
# 退出之后照样能 setns 进去；NS_GET_* 这些 ioctl；挂载命名空间的文件不能 bind 进不比它新的
# 命名空间，复制挂载命名空间时不带它们；busybox 的 unshare、nsenter 的持久命名空间。原生一侧
# 是有 CAP_SYS_ADMIN 的 docker 容器。命名空间的编号两边不同，只比异同
cd /tmp
rm -rf nsfs
mkdir -p nsfs/run nsfs/m

python3 - <<'PY'
import ctypes, errno, fcntl, functools, os, socket

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
CLONE_NEWNS, CLONE_NEWCGROUP, CLONE_NEWUTS, CLONE_NEWIPC = 0x20000, 0x2000000, 0x4000000, 0x8000000
CLONE_NEWUSER, CLONE_NEWPID, CLONE_NEWNET = 0x10000000, 0x20000000, 0x40000000
MS_BIND, MS_REC, MS_PRIVATE = 0x1000, 0x4000, 1 << 18
MNT_DETACH = 2
NS_GET_USERNS, NS_GET_PARENT, NS_GET_NSTYPE, NS_GET_OWNER_UID = 0xb701, 0xb702, 0xb703, 0xb704
RUN = "/tmp/nsfs/run"


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def ioctl(fd, req, arg=None):
    r = libc.ioctl(fd, req, arg)
    return r if r >= 0 else errno.errorcode[ctypes.get_errno()]


def link(fd):
    return os.readlink("/proc/self/fd/%d" % fd)


def rel(fd, req):
    """NS_GET_USERNS、NS_GET_PARENT 得到的命名空间的种类，或者错误。"""
    r = ioctl(fd, req)
    return r if isinstance(r, str) else link(r).split(":")[0]


def same(a, b):
    return "same" if a == b else "different"


def fork(f):
    """在子进程里运行 f，返回它的退出码。异常也结束子进程，不接着跑父进程的代码。"""
    pid = os.fork()
    if pid == 0:
        code = 0
        try:
            f()
        except BaseException as e:
            print("child:", repr(e))
            code = 1
        os._exit(code)
    return os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])


def mounted(path):
    with open("/proc/self/mountinfo") as f:
        return [l.split() for l in f if l.split()[4] == path]


libc.mount(None, b"/", None, MS_REC | MS_PRIVATE, None)

# 子进程建新的网络、UTS、IPC 命名空间，bind 挂载出去之后退出
def holder():
    assert libc.unshare(CLONE_NEWNET | CLONE_NEWUTS | CLONE_NEWIPC) == 0
    socket.sethostname("pinned")
    # 一个不删的共享内存段：IPC 命名空间还在它就在
    assert libc.shmget(0x5157, 4096, 0o1600) >= 0
    for k in ("net", "uts", "ipc"):
        open("%s/%s" % (RUN, k), "w").close()
        assert libc.mount(("/proc/self/ns/" + k).encode(), ("%s/%s" % (RUN, k)).encode(), None, MS_BIND, None) == 0

print("holder", fork(holder))
own = os.stat("/proc/self/ns/net")
for k in ("net", "uts", "ipc"):
    st = os.stat("%s/%s" % (RUN, k))
    print(k, "dev", same(st.st_dev, own.st_dev), "ino", same(st.st_ino, os.stat("/proc/self/ns/" + k).st_ino),
          "mode", oct(st.st_mode), "nlink", st.st_nlink)
for m in mounted(RUN + "/net"):
    # 编号、设备号之外的列
    print("mountinfo", m[2], m[3].split(":")[0], m[5], m[6:])

def enter():
    fd = os.open(RUN + "/net", os.O_RDONLY)
    # 经由 bind 挂载打开的，路径是挂载点
    print("open link", link(fd), "fdinfo", open("/proc/self/fdinfo/%d" % fd).read().split()[2:4])
    print("setns net", call(libc.setns, fd, CLONE_NEWNET))
    print("now in it", same(os.stat("/proc/self/ns/net").st_ino, os.fstat(fd).st_ino))
    print("interfaces", socket.if_nameindex())
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", 0))
        print("bind lo ok")
    except OSError as e:
        print("bind lo", errno.errorcode[e.errno])
    print("setns uts", call(libc.setns, os.open(RUN + "/uts", os.O_RDONLY), CLONE_NEWUTS), socket.gethostname())
    print("setns ipc", call(libc.setns, os.open(RUN + "/ipc", os.O_RDONLY), CLONE_NEWIPC),
          "shm", libc.shmget(0x5157, 0, 0) >= 0)
    print("wrong type", call(libc.setns, os.open(RUN + "/uts", os.O_RDONLY), CLONE_NEWNET))

print("enter", fork(enter))
print("shm outside", libc.shmget(0x5157, 0, 0) >= 0)

# 卸下之后文件是原来那个普通文件
held = os.open(RUN + "/net", os.O_RDONLY)
print("umount", call(libc.umount2, (RUN + "/net").encode(), MNT_DETACH), "mounted", len(mounted(RUN + "/net")))
fd = os.open(RUN + "/net", os.O_RDONLY)
print("plain file", call(libc.setns, fd, 0))
os.close(fd)
# 卸下之前打开的仍然指着那个命名空间
print("held", fork(lambda: print("setns held", call(libc.setns, held, CLONE_NEWNET), socket.if_nameindex())))
os.close(held)
for k in ("uts", "ipc"):
    libc.umount2(("%s/%s" % (RUN, k)).encode(), MNT_DETACH)

# 只靠打开着的 fd：子进程退出之后经由它 setns
r1, w1 = os.pipe()
r2, w2 = os.pipe()
pid = os.fork()
if pid == 0:
    libc.unshare(CLONE_NEWNET | CLONE_NEWNS)
    libc.mount(b"tmpfs", b"/tmp/nsfs/m", b"tmpfs", 0, None)
    open("/tmp/nsfs/m/inside", "w").close()
    os.write(w1, b"x")
    os.read(r2, 1)
    os._exit(0)
os.read(r1, 1)
net = os.open("/proc/%d/ns/net" % pid, os.O_RDONLY)
mnt = os.open("/proc/%d/ns/mnt" % pid, os.O_RDONLY)
print("fd link", link(net).split(":")[0], same(int(link(net).split("[")[1][:-1]), os.fstat(net).st_ino))
os.write(w2, b"x")
os.waitpid(pid, 0)
def enter_fds():
    print("setns net fd", call(libc.setns, net, CLONE_NEWNET), socket.if_nameindex())
    print("setns mnt fd", call(libc.setns, mnt, CLONE_NEWNS), os.listdir("/tmp/nsfs/m"), os.getcwd())
print("enter fds", fork(enter_fds))
print("outside", os.listdir("/tmp/nsfs/m"))

# 挂载命名空间的文件只能 bind 进比它旧的命名空间（Linux 的 mnt_ns_loop）
open(RUN + "/mnt", "w").close()
print("bind own mnt", call(libc.mount, b"/proc/self/ns/mnt", (RUN + "/mnt").encode(), None, MS_BIND, None))
print("bind newer mnt", call(libc.mount, b"/proc/self/fd/%d" % mnt, (RUN + "/mnt").encode(), None, MS_BIND, None))
open(RUN + "/net2", "w").close()
print("bind net", call(libc.mount, b"/proc/self/fd/%d" % net, (RUN + "/net2").encode(), None, MS_BIND, None))
# 复制挂载命名空间时不带挂载命名空间的文件，别的照样复制
def copied():
    libc.unshare(CLONE_NEWNS)
    print("copied mnt", len(mounted(RUN + "/mnt")), "net", len(mounted(RUN + "/net2")))
    # 原来的比自己的旧：不能 bind 进来
    open(RUN + "/mnt2", "w").close()
    print("bind older mnt", call(libc.mount, b"/proc/%d/ns/mnt" % os.getppid(), (RUN + "/mnt2").encode(), None, MS_BIND, None))
print("copy", fork(copied))
os.close(mnt)
os.close(net)
def enter_bound():
    fd = os.open(RUN + "/mnt", os.O_RDONLY)
    print("setns bound mnt", call(libc.setns, fd, CLONE_NEWNS), os.listdir("/tmp/nsfs/m"))
print("enter bound", fork(enter_bound))
libc.umount2((RUN + "/mnt").encode(), MNT_DETACH)
libc.umount2((RUN + "/net2").encode(), MNT_DETACH)

# NS_GET_*
own = {k: os.open("/proc/self/ns/" + k, os.O_RDONLY) for k in ("cgroup", "ipc", "mnt", "net", "pid", "pid_for_children", "user", "uts")}
for k, fd in own.items():
    print(k, "type", hex(ioctl(fd, NS_GET_NSTYPE)), "userns", rel(fd, NS_GET_USERNS), "parent", rel(fd, NS_GET_PARENT))
print("owner uid of net", ioctl(own["net"], NS_GET_OWNER_UID, ctypes.byref(ctypes.c_uint())))
uid = ctypes.c_uint(1234)
print("owner uid of init", ioctl(own["user"], NS_GET_OWNER_UID, ctypes.byref(uid)), uid.value)
print("unknown", ioctl(own["net"], 0xb7ff), "tcgets", ioctl(own["net"], 0x5401, ctypes.byref(ctypes.create_string_buffer(64))))

def nested():
    # root 建的用户命名空间，里面再建网络、pid 命名空间（Ubuntu 的宿主不让普通用户建的
    # 用户命名空间里有能力）。在外面拿着它们的文件
    r1, w1 = os.pipe()
    r2, w2 = os.pipe()
    pid = os.fork()
    if pid == 0:
        assert libc.unshare(CLONE_NEWUSER) == 0
        with open("/proc/self/uid_map", "w") as f:
            f.write("0 0 1")
        assert libc.unshare(CLONE_NEWNET | CLONE_NEWPID) == 0
        user = os.open("/proc/self/ns/user", os.O_RDONLY)
        u = ioctl(os.open("/proc/self/ns/net", os.O_RDONLY), NS_GET_USERNS)
        # 自己外面的用户命名空间看不到
        print("inside: parent", rel(user, NS_GET_PARENT), "userns", rel(user, NS_GET_USERNS),
              "net's userns", same(os.fstat(u).st_ino, os.fstat(user).st_ino))
        kid = os.fork()
        if kid == 0:
            os.read(r2, 1)
            os._exit(0)
        os.write(w1, b"%d" % kid)
        os.waitpid(kid, 0)
        os._exit(0)
    kid = int(os.read(r1, 16))
    user = os.open("/proc/%d/ns/user" % pid, os.O_RDONLY)
    net = os.open("/proc/%d/ns/net" % pid, os.O_RDONLY)
    u = ioctl(net, NS_GET_USERNS)
    print("net's userns", link(u).split(":")[0], same(os.fstat(u).st_ino, os.fstat(user).st_ino),
          "cloexec", fcntl.fcntl(u, fcntl.F_GETFD), "flags", oct(fcntl.fcntl(u, fcntl.F_GETFL)))
    p = ioctl(user, NS_GET_PARENT)
    print("user's parent", same(os.fstat(p).st_ino, os.stat("/proc/self/ns/user").st_ino),
          "userns", same(os.fstat(ioctl(user, NS_GET_USERNS)).st_ino, os.stat("/proc/self/ns/user").st_ino),
          "parent of that", rel(p, NS_GET_PARENT))
    owner = ctypes.c_uint(1234)
    print("owner uid", ioctl(user, NS_GET_OWNER_UID, ctypes.byref(owner)), owner.value)
    child = os.open("/proc/%d/ns/pid" % kid, os.O_RDONLY)
    pp = ioctl(child, NS_GET_PARENT)
    print("pid parent", link(pp).split(":")[0], same(os.fstat(pp).st_ino, os.stat("/proc/self/ns/pid").st_ino),
          "parent of that", rel(pp, NS_GET_PARENT), "type", hex(ioctl(child, NS_GET_NSTYPE)),
          "userns", same(os.fstat(ioctl(child, NS_GET_USERNS)).st_ino, os.fstat(user).st_ino))
    os.write(w2, b"x")
    os.waitpid(pid, 0)
    # 1 号进程退出之后：还能 setns 进去，但建不了进程
    print("setns dead pid", call(libc.setns, child, CLONE_NEWPID))
    try:
        if os.fork() == 0:
            os._exit(0)
        print("fork ok")
    except OSError as e:
        print("fork", errno.errorcode[e.errno])
    # 用户命名空间里的进程都退出了：照样能进去
    print("setns user", call(libc.setns, user, CLONE_NEWUSER), os.getuid())
print("nested", fork(nested))
PY

# busybox 的 unshare、nsenter：命名空间文件的路径
touch nsfs/run/bbnet nsfs/run/bbuts
busybox unshare --net=/tmp/nsfs/run/bbnet --uts=/tmp/nsfs/run/bbuts sh -c 'hostname bb; ip link show | grep -c "^[0-9]"'
echo "outside: $(hostname | grep -c bb)"
busybox nsenter --net=/tmp/nsfs/run/bbnet --uts=/tmp/nsfs/run/bbuts sh -c 'hostname; ip -o link show | cut -d: -f2'
umount /tmp/nsfs/run/bbnet /tmp/nsfs/run/bbuts
busybox nsenter --net=/tmp/nsfs/run/bbnet true; echo "after umount: $?"

# iproute2 的 ip netns：add 把 unshare 出来的网络命名空间 bind 到 /var/run/netns 下面（原生
# 一侧的镜像里没有 /var、/run）
[ -e /var/run ] || { mkdir -p /var /run && ln -s ../run /var/run; }
ip netns add appine-a
ip netns list
ip netns exec appine-a ip -o link show | cut -d: -f2 | cut -d' ' -f2
ip netns exec appine-a sh -c 'ip link set lo up && ip -4 -o addr show lo | cut -d" " -f2,7'
ip netns exec appine-a ip -o link show lo | grep -o 'state [A-Z]*'
ip -n appine-a -o link show | wc -l
ip netns identify $$ | wc -c
ip netns exec appine-a sh -c 'ip netns identify $$'
ip netns delete appine-a
ip netns list | wc -l
ip netns exec appine-a true 2>&1 | sed 's/: .*$//'
