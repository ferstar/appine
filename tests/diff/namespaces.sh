# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
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

# pid 命名空间：busybox 的 unshare -p -f --mount-proc，里面的 sh 是 1 号进程
unshare -p -f --mount-proc sh -c '
    echo "pid $$ ppid $PPID"
    echo /proc/[0-9]*
    echo "self $(readlink /proc/self) thread-self $(readlink /proc/thread-self)"
    sh -c "echo child \$\$ ppid \$PPID"
    cut -d" " -f1,4-6 /proc/self/stat
    grep -E "^(Tgid|Pid|PPid|NStgid|NSpid|NSpgid|NSsid)" /proc/self/status
    sleep 100 &
    kill -0 $! && echo "background $!"
    # ash 报告作业被结束（Terminated）的时机与快慢有关，不比
    kill $! 2>/dev/null; wait $! 2>/dev/null; echo "killed with status $?"
    kill -TERM 1; kill -KILL 1; echo "init survives its own SIGTERM and SIGKILL"
    echo "last pid $(cat /proc/sys/kernel/ns_last_pid)"
'
echo "unshare exits with $?"
# 在里面 strace -f：跟踪者拿到的新进程的编号（PTRACE_GETEVENTMSG）和 wait 的结果都是里面的
unshare -p -f --mount-proc strace -f -qq -e trace=exit_group,kill sh -c 'sh -c "exit 3"; kill -0 $$; echo "inner exit $?"'

python3 - <<'PY3'
import ctypes, errno, functools, os, signal, socket, struct, threading, time

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.umount.argtypes = [ctypes.c_char_p]
CLONE_NEWPID, CLONE_NEWNS, CLONE_THREAD, CLONE_SIGHAND, CLONE_VM, CLONE_PARENT = (
    0x20000000, 0x20000, 0x10000, 0x800, 0x100, 0x8000)
SIGCHLD = 17
os.makedirs("/tmp/ns/p", exist_ok=True)


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


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
    return os.waitpid(pid, 0)[1]


def try_fork():
    try:
        pid = os.fork()
    except OSError as e:
        return errno.errorcode[e.errno]
    if pid == 0:
        os._exit(0)
    os.waitpid(pid, 0)
    return "ok"


def link(path):
    try:
        return os.readlink(path)
    except OSError as e:
        return errno.errorcode[e.errno]


def status(pid, key, proc="/proc"):
    with open("%s/%s/status" % (proc, pid)) as f:
        return next(l.split(":", 1)[1].split() for l in f if l.startswith(key + ":"))


def pids(proc):
    return sorted(int(d) for d in os.listdir(proc) if d.isdigit())


def inner_global(init, local):
    """1 号进程是全局的 init 时，它的命名空间里编号为 local 的进程在外面的编号。"""
    for d in pids("/proc"):
        try:
            if status(d, "NSpid")[-1] == str(local) and init in (d, int(status(d, "PPid")[0])):
                return d
        except (OSError, StopIteration):
            pass


# unshare 之后自己不变，之后的子进程在新的命名空间里、是那里的 1 号进程；进去之前不能再
# unshare，也不能建线程；1 号进程退出之后不能再在里面建进程
def unshared():
    me = os.getpid()
    os.unshare(CLONE_NEWPID)
    print("pid unchanged", os.getpid() == me, "ns unchanged", link("/proc/self/ns/pid") == mine)
    print("pid_for_children before fork", link("/proc/self/ns/pid_for_children"))
    print("unshare again", err(os.unshare, CLONE_NEWPID))
    try:
        threading.Thread(target=lambda: None).start()
        print("thread started")
    except RuntimeError as e:
        print("thread:", e)
    r, w = os.pipe()

    def init():
        print("init pid", os.getpid(), "ppid", os.getppid(), "tid", threading.get_native_id())
        print("pgid", os.getpgid(0), "sid", os.getsid(0), "getpgid(1)", os.getpgid(1))
        os.write(w, link("/proc/self/ns/pid").encode())
        print("outside pid", err(os.kill, me, 0), "wait for it", err(os.waitpid, me, 0))
        print("CLONE_PARENT", libc.syscall(220, CLONE_PARENT | SIGCHLD, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])

    pid = fork(init)
    os.close(w)
    inner = os.read(r, 100).decode()
    os.waitpid(pid, 0)
    print("child pid is not 1", pid != 1)
    print("child's ns is pid_for_children", inner == link("/proc/self/ns/pid_for_children"))
    print("pid_for_children differs from pid", inner != mine)
    print("fork after init exited", try_fork())


mine = link("/proc/self/ns/pid")
child(unshared)
print("thread NEWPID", libc.syscall(220, CLONE_NEWPID | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, 0, 0, 0, 0), errno.errorcode[ctypes.get_errno()])


# 1 号进程：它的子进程、孤儿过继给它、setsid、线程、kill(-1)、ns_last_pid、新的 procfs 实例
def first():
    os.unshare(CLONE_NEWPID)

    def init():
        def grandchild():
            print("grandchild", os.getpid(), "ppid", os.getppid())
            orphan = fork(lambda: (time.sleep(1), print("orphan", os.getpid(), "adopted by", os.getppid())))
            print("orphan's parent exits, orphan is", orphan)

        print("grandchild status", child(grandchild))
        print("reaped orphan", os.waitpid(-1, 0))
        print("setsid", os.setsid(), "pgid", os.getpgid(0), "sid", os.getsid(0))
        ev = threading.Event()
        t = threading.Thread(target=ev.wait)
        t.start()
        print("thread", t.native_id, "pthread_kill", signal.pthread_kill(t.ident, 0))
        ev.set()
        t.join()
        sleepers = [fork(lambda: time.sleep(30)) for _ in range(2)]
        time.sleep(0.2)
        os.kill(-1, signal.SIGTERM)
        print("kill -1:", [os.waitpid(p, 0) for p in sleepers])
        print("mount proc", libc.mount(b"proc", b"/tmp/ns/p", b"proc", 0, None))
        print("pids inside", pids("/tmp/ns/p"), "self", link("/tmp/ns/p/self"))
        print("task", sorted(int(x) for x in os.listdir("/tmp/ns/p/self/task")))
        with open("/tmp/ns/p/sys/kernel/ns_last_pid") as f:
            print("ns_last_pid", f.read().strip())
        with open("/tmp/ns/p/sys/kernel/ns_last_pid", "w") as f:
            f.write("100")
        print("next child", child(lambda: print("child pid", os.getpid())))
        print("stat", open("/tmp/ns/p/1/stat").read().split()[:8:3])
        print("status", status(1, "Pid", "/tmp/ns/p"), status(1, "PPid", "/tmp/ns/p"), status(1, "NSpid", "/tmp/ns/p"))
        os.write(w, b"x")
        os.read(r2, 1)
        print("umount", libc.umount(b"/tmp/ns/p"))

    r, w = os.pipe()
    r2, w2 = os.pipe()
    pid = fork(init)
    os.read(r, 1)
    # 外面看里面：新的 procfs 实例里的编号是里面的，self 没有；全局的 procfs 里有两层编号
    print("outside: pids", pids("/tmp/ns/p"), "self", link("/tmp/ns/p/self"))
    print("outside: init status", status(1, "Pid", "/tmp/ns/p"), len(status(pid, "NStgid")), status(pid, "NStgid")[-1])
    print("outside: own NSpid", len(status("self", "NSpid")))
    os.write(w2, b"x")
    print("init status", os.waitpid(pid, 0)[1])


child(first)


# 信号：里面发给 1 号进程的 si_pid 是发送者在里面的编号，外面发的是 0；外面发的 SIGTERM
# 按默认处理时被忽略，SIGKILL 不会
def signals():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()

    def init():
        signal.pthread_sigmask(signal.SIG_BLOCK, [signal.SIGUSR1])
        sender = fork(lambda: os.kill(1, signal.SIGUSR1))
        info = signal.sigwaitinfo([signal.SIGUSR1])
        print("from inside: si_pid", info.si_pid, "sender", sender, "code", info.si_code)
        os.waitpid(sender, 0)
        os.write(w, b"x")
        info = signal.sigwaitinfo([signal.SIGUSR1])
        print("from outside: si_pid", info.si_pid, "code", info.si_code)
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(init)
    os.read(r, 1)
    os.kill(pid, signal.SIGUSR1)
    os.read(r, 1)
    os.kill(pid, signal.SIGTERM)
    time.sleep(0.3)
    print("after SIGTERM", os.waitpid(pid, os.WNOHANG))
    os.kill(pid, signal.SIGKILL)
    print("after SIGKILL", os.waitpid(pid, 0)[1])


child(signals)


# 外面发的 SIGSTOP 让 1 号进程停下，里面发的 SIGCONT 虽然被丢掉也让它继续；按默认方式处理
# 的同步异常照常结束它
def stop_cont():
    os.unshare(CLONE_NEWPID)
    ready, go = os.pipe(), os.pipe()

    def init():
        fork(lambda: (os.read(go[0], 1), os.kill(1, signal.SIGCONT)))
        os.write(ready[1], b"x")
        time.sleep(2)
        print("init resumed")
        ctypes.string_at(0)

    def state(pid):
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0]

    def until(pid, f):
        for _ in range(200):
            if f(state(pid)):
                return True
            time.sleep(0.01)

    pid = fork(init)
    os.read(ready[0], 1)
    os.kill(pid, signal.SIGSTOP)
    print("stopped", until(pid, lambda s: s == "T"))
    os.write(go[1], b"x")
    print("continued", until(pid, lambda s: s != "T"))
    print("killed by", os.WTERMSIG(os.waitpid(pid, 0)[1]))


child(stop_cont)


# 1 号进程退出：里面别的进程都被结束、回收之后父进程才等到它。父进程在外面的（unshare、
# setns 之后 fork 的）结束了要等父进程回收
def zap():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()
    go = os.pipe()

    def init():
        fork(lambda: time.sleep(30))
        os.write(w, b"x")
        os.read(go[0], 1)

    pid = fork(init)
    os.read(r, 1)
    sleeper = inner_global(pid, 2)
    print("sleeper found", sleeper is not None)
    zombie = fork(lambda: None)
    time.sleep(0.2)
    os.write(go[1], b"x")
    time.sleep(0.3)
    print("init waits for the zombie", os.waitpid(pid, os.WNOHANG))
    print("zombie", os.waitpid(zombie, 0)[1])
    print("init status", os.waitpid(pid, 0)[1], "sleeper gone", err(os.kill, sleeper, 0))


child(zap)


# setns 到 pid 命名空间：之后的子进程在里面，看不到父进程；只能进自己的和后代命名空间
def entered():
    os.unshare(CLONE_NEWPID)
    r, w = os.pipe()
    outer = os.open("/proc/self/ns/pid", os.O_RDONLY)

    def init():
        print("setns to an ancestor", err(os.setns, outer, CLONE_NEWPID))
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(init)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/pid" % pid, os.O_RDONLY)
    print("wrong type", err(os.setns, fd, CLONE_NEWNS))
    print("setns back to own", err(os.setns, outer, CLONE_NEWPID))
    print("children in own", link("/proc/self/ns/pid_for_children") == mine)
    print("setns into the child's", err(os.setns, fd, CLONE_NEWPID))
    print("own unchanged", link("/proc/self/ns/pid") == mine)
    print("child", child(lambda: print("joined: pid", os.getpid(), "ppid", os.getppid())))
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)


child(entered)


# SO_PEERCRED：外面监听、里面连接，里面看到的对方 pid 是 0
def peercred():
    os.unshare(CLONE_NEWPID)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind("\0appine-pidns")
    srv.listen()

    def init():
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect("\0appine-pidns")
        cred = c.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        print("inside sees peer pid", struct.unpack("3i", cred)[0])
        c.send(b"y")
        c.recv(1)

    pid = fork(init)
    conn, _ = srv.accept()
    conn.recv(1)
    cred = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
    print("outside sees the init", struct.unpack("3i", cred)[0] == pid)
    conn.send(b"x")
    os.waitpid(pid, 0)


child(peercred)


# clone3 的 set_tid：指定新进程在各层的编号，从它所在的那层起往外。只比里面两层：docker 的
# 容器本身在一个 pid 命名空间里，再往外的层两边不同
class CloneArgs(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint64) for n in (
        "flags", "pidfd", "child_tid", "parent_tid", "exit_signal", "stack", "stack_size",
        "tls", "set_tid", "set_tid_size", "cgroup")]


def clone3(*set_tid):
    tids = (ctypes.c_int * len(set_tid))(*set_tid)
    args = CloneArgs(exit_signal=SIGCHLD, set_tid=ctypes.addressof(tids), set_tid_size=len(set_tid))
    pid = libc.syscall(435, ctypes.byref(args), ctypes.sizeof(args))
    if pid == 0:
        print("  clone3 child: pid", os.getpid())
        os._exit(0)
    if pid < 0:
        return errno.errorcode[ctypes.get_errno()]
    os.waitpid(pid, 0)
    return pid


def set_tid():
    os.unshare(CLONE_NEWPID)
    print("no init yet", clone3(5))
    r, w = os.pipe()
    pid = fork(lambda: os.read(r, 1))
    print("taken", clone3(1))
    print("inner only", clone3(50) not in (50, "EINVAL"))
    print("two levels", clone3(50, 30000))
    print("out of range", clone3(-1), clone3(0))
    os.write(w, b"x")
    os.waitpid(pid, 0)


child(set_tid)


def unprivileged_set_tid():
    os.setuid(65534)
    print("unprivileged", clone3(30000))


child(unprivileged_set_tid)


# 最多嵌套 32 层。docker 的容器本身在一个 pid 命名空间里，原生一侧少嵌套一层，只比错误
# （层数在单元测试 pid_namespaces 里）
def nested():
    for level in range(1, 40):
        try:
            os.unshare(CLONE_NEWPID)
        except OSError as e:
            print("nesting stops:", errno.errorcode[e.errno], "pid", os.getpid(), "deep", level > 30)
            return
        pid = os.fork()
        if pid:
            os.waitpid(pid, 0)
            os._exit(0)


child(nested)
PY3

# UTS 命名空间：busybox 的 unshare -u 里改的主机名外面看不到
before=$(hostname)
unshare -u sh -c '
    hostname inner
    echo "inside: $(hostname) $(uname -n) $(cat /proc/sys/kernel/hostname)"
    echo "different ns: $([ "$(readlink /proc/self/ns/uts)" != "$(readlink /proc/$PPID/ns/uts)" ] && echo yes)"
'
echo "outside unchanged: $([ "$(hostname)" = "$before" ] && echo yes)"

python3 - <<'PY4'
import ctypes, errno, functools, os, socket, struct, time

print = functools.partial(print, flush=True)

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
libc.shmat.restype = ctypes.c_void_p
libc.shmat.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
libc.shmdt.argtypes = [ctypes.c_void_p]
libc.mq_open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_uint, ctypes.c_void_p]
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_void_p]
libc.umount.argtypes = [ctypes.c_char_p]
CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_SYSVSEM = 0x04000000, 0x08000000, 0x10000000, 0x40000
IPC_CREAT, IPC_EXCL, IPC_RMID, IPC_STAT, SEM_UNDO, GETVAL = 0o1000, 0o2000, 0, 2, 0x1000, 12
KEY = 0x4170704E
os.makedirs("/tmp/ns/p", exist_ok=True)


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def c(r):
    return r if r != -1 else errno.errorcode[ctypes.get_errno()]


def fork(f):
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
    return os.waitpid(fork(f), 0)[1]


def link(kind, pid="self"):
    return os.readlink("/proc/%s/ns/%s" % (pid, kind))


def sysvipc(which):
    with open("/proc/sysvipc/" + which) as f:
        return [l.split()[:2] for l in f.readlines()[1:]]


def semop(sem, op, flg):
    buf = struct.pack("HhH", 0, op, flg)
    return c(libc.semop(sem, buf, 1))


name = socket.gethostname()


# 新的 UTS 命名空间复制原来的名字；里面改的外面看不到；名字太长是 EINVAL；没有能力是 EPERM
def uts():
    os.unshare(CLONE_NEWUTS)
    print("copied", socket.gethostname() == name, "domain", os.uname().nodename == name)
    print("too long", err(socket.sethostname, "x" * 65))
    socket.sethostname("inside")
    print("domain", c(libc.setdomainname(b"dom", 3)), open("/proc/sys/kernel/domainname").read().strip())
    print("inside", socket.gethostname(), os.uname().nodename, open("/proc/sys/kernel/hostname").read().strip())
    r, w = os.pipe()
    pid = fork(lambda: (os.write(w, b"x"), time.sleep(30)))
    os.read(r, 1)
    print("child shares it", link("uts", pid) == link("uts"))
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(uts)
print("outside", socket.gethostname() == name, open("/proc/sys/kernel/domainname").read().strip())
child(lambda: (os.setuid(65534), print("unprivileged sethostname", err(socket.sethostname, "x"),
                                     "unshare", err(os.unshare, CLONE_NEWUTS))))


# root 建的用户命名空间里的 UTS 命名空间：属主是新的用户命名空间，在里面能改名字；
# setns 进去之后看到的是它的名字
def owned():
    os.unshare(CLONE_NEWUSER | CLONE_NEWUTS)
    socket.sethostname("owned")
    print("in a user namespace", socket.gethostname())


child(owned)


def joined():
    r, w = os.pipe()

    def other():
        os.unshare(CLONE_NEWUTS)
        socket.sethostname("other")
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(other)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/uts" % pid, os.O_RDONLY)
    print("setns uts", err(os.setns, fd, CLONE_NEWUTS), socket.gethostname())
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(joined)


# IPC 命名空间：里面看不到外面的 System V IPC 对象和 POSIX 消息队列，id 重新编号；
# shmat 的段在 unshare 之后照样能 shmdt；换命名空间时执行 SEM_UNDO 的撤销值
shm = libc.shmget(KEY, 4096, IPC_CREAT | 0o600)
sem = libc.semget(KEY, 1, IPC_CREAT | 0o600)
msq = libc.msgget(KEY, IPC_CREAT | 0o600)
mq = libc.mq_open(b"/appine-ns", os.O_CREAT | os.O_RDONLY, 0o600, None)
print("outside objects", shm >= 0, sem >= 0, msq >= 0, mq >= 0)


step, go = os.pipe(), os.pipe()


def ipc():
    addr = libc.shmat(shm, None, 0)
    print("semop with undo", semop(sem, 1, SEM_UNDO), "value", libc.semctl(sem, 0, GETVAL))
    me = link("ipc")
    os.unshare(CLONE_NEWIPC)
    os.write(step[1], b"x")
    os.read(go[0], 1)
    print("new ns", link("ipc") != me)
    print("shmget", c(libc.shmget(KEY, 4096, 0)), "semget", c(libc.semget(KEY, 1, 0)),
          "msgget", c(libc.msgget(KEY, 0)), "mq_open", c(libc.mq_open(b"/appine-ns", os.O_RDONLY, 0, None)))
    print("sysvipc", sysvipc("shm"), sysvipc("sem"), sysvipc("msg"))
    mine = libc.shmget(0, 4096, IPC_CREAT | 0o600)
    print("first id", mine, "same key again", c(libc.shmget(KEY, 8192, IPC_CREAT | IPC_EXCL | 0o600)) >= 0)
    print("shmdt the old one", c(libc.shmdt(addr)))
    print("mount proc", libc.mount(b"proc", b"/tmp/ns/p", b"proc", 0, None))
    with open("/tmp/ns/p/sys/kernel/msgmax", "w") as f:
        f.write("1234")
    print("msgmax", open("/tmp/ns/p/sys/kernel/msgmax").read().strip())
    print("umount", libc.umount(b"/tmp/ns/p"))


pid = fork(ipc)
os.read(step[0], 1)
print("undone at unshare", libc.semctl(sem, 0, GETVAL))
os.write(go[1], b"x")
os.waitpid(pid, 0)
print("outside after: nattch",
      [l.split()[6] for l in open("/proc/sysvipc/shm") if l.split()[1:2] == [str(shm)]],
      "msgmax", open("/proc/sys/kernel/msgmax").read().strip())
print("clone NEWIPC with SYSVSEM", libc.syscall(220, CLONE_NEWIPC | CLONE_SYSVSEM | 17, 0, 0, 0, 0),
      errno.errorcode[ctypes.get_errno()])
child(lambda: (os.setuid(65534), print("unprivileged unshare", err(os.unshare, CLONE_NEWIPC))))


# setns 到别的进程的 IPC 命名空间：看到的是它的对象
def joined_ipc():
    r, w = os.pipe()

    def other():
        os.unshare(CLONE_NEWIPC)
        libc.msgget(KEY + 1, IPC_CREAT | 0o600)
        os.write(w, b"x")
        time.sleep(30)

    pid = fork(other)
    os.read(r, 1)
    fd = os.open("/proc/%d/ns/ipc" % pid, os.O_RDONLY)
    print("setns ipc", err(os.setns, fd, CLONE_NEWIPC), "msgget", libc.msgget(KEY + 1, 0) >= 0,
          "outer one", c(libc.msgget(KEY, 0)))
    os.kill(pid, 9)
    os.waitpid(pid, 0)


child(joined_ipc)
libc.shmctl(shm, IPC_RMID, None)
libc.semctl(sem, 0, IPC_RMID)
libc.msgctl(msq, IPC_RMID, None)
libc.mq_unlink(b"/appine-ns")
PY4

# 网络命名空间：busybox 的 unshare -n 里只有关着的 lo，ip 把它打开之后有 127.0.0.1/8、::1
# 和 local 表里的路由，外面的地址到不了
unshare -n sh -c '
    ip link show
    ip -o addr
    ip link set lo up
    ip link show
    ip -o addr
    ip route show table local
    sed 1,2d /proc/net/dev | cut -d: -f1
    ping -c 1 -W 1 10.0.0.1 2>&1 | tail -1
'

python3 - <<'PY5'
import ctypes, errno, functools, os, socket, struct, time

print = functools.partial(print, flush=True)

CLONE_NEWNET, CLONE_NEWUSER = 0x40000000, 0x10000000


def err(f, *args):
    try:
        f(*args)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def fork(f):
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
    return os.waitpid(fork(f), 0)[1]


def tcp(family=socket.AF_INET):
    return socket.socket(family, socket.SOCK_STREAM)


def udp(family=socket.AF_INET):
    return socket.socket(family, socket.SOCK_DGRAM)


def lo_up():
    os.system("ip link set lo up")


def rtattr(ty, data):
    return struct.pack("=HH", 4 + len(data), ty) + data + b"\0" * (-len(data) % 4)


def rtnl(*msgs):
    """经由 NETLINK_ROUTE 发请求，返回每个请求的确认里的错误码。"""
    s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0)
    s.bind((0, 0))
    out = []
    for seq, (ty, flags, body) in enumerate(msgs, 1):
        s.send(struct.pack("=LHHLL", 16 + len(body), ty, flags | 1, seq, 0) + body)
        r = s.recv(4096)
        out.append(struct.unpack("=i", r[16:20])[0] if struct.unpack("=H", r[4:6])[0] == 2 else None)
    return out


# bubblewrap 的 loopback_setup：RTM_NEWADDR 加 127.0.0.1/8，再 RTM_NEWLINK 把 lo 打开，都要确认
NEWADDR = (20, 0x4 | 0x200 | 0x400, struct.pack("=BBBBI", socket.AF_INET, 8, 0x80, 254, 1)
           + rtattr(2, bytes([127, 0, 0, 1])) + rtattr(1, bytes([127, 0, 0, 1])))
NEWLINK = (16, 0x4, struct.pack("=BBHiII", 0, 0, 0, 1, 1, 1))


# 外面监听的回环端口和抽象地址：里面都连不到
outer = tcp()
outer.bind(("127.0.0.1", 0))
outer.listen()
outer_port = outer.getsockname()[1]
outer_unix = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
outer_unix.bind("\0appine-netns")
outer_unix.listen()
me = os.readlink("/proc/self/ns/net")


def netns():
    os.unshare(CLONE_NEWNET)
    print("new ns", os.readlink("/proc/self/ns/net") != me)
    print("interfaces", socket.if_nameindex(), "lo", socket.if_nametoindex("lo"),
          "eth0", err(socket.if_nametoindex, "eth0"))
    # lo 关着：通配地址能绑，127.0.0.1 绑不上、连不到
    print("down: bind any", err(tcp().bind, ("0.0.0.0", 0)), "bind lo", err(tcp().bind, ("127.0.0.1", 0)),
          "connect", err(tcp().connect, ("127.0.0.1", 9)), "v6", err(tcp(socket.AF_INET6).connect, ("::1", 9)))
    r, w = os.pipe()
    pid = fork(lambda: (os.setuid(65534), os.write(w, str(rtnl(NEWADDR)).encode())))
    os.waitpid(pid, 0)
    print("unprivileged rtnl", os.read(r, 100).decode())
    print("bwrap's loopback_setup", rtnl(NEWADDR, NEWLINK), "again", rtnl(NEWADDR))
    # 回环上的 TCP：两端的地址都是 127.0.0.1
    srv = tcp()
    srv.bind(("127.0.0.1", 0))
    srv.listen()
    port = srv.getsockname()[1]
    c = tcp()
    c.connect(("127.0.0.1", port))
    conn, peer = srv.accept()
    c.sendall(b"hello")
    print("tcp", srv.getsockname()[0], c.getpeername() == ("127.0.0.1", port), peer[0],
          conn.getsockname() == c.getpeername(), conn.recv(5))
    # 通配地址：getsockname 是 0.0.0.0，连 127.0.0.1 也连得上
    any4 = tcp()
    any4.bind(("0.0.0.0", 0))
    any4.listen()
    print("any", any4.getsockname()[0], err(tcp().connect, ("127.0.0.1", any4.getsockname()[1])))
    # IPv6 的 ::1 和 ::
    s6 = tcp(socket.AF_INET6)
    s6.bind(("::", 0))
    s6.listen()
    c6 = tcp(socket.AF_INET6)
    c6.connect(("::1", s6.getsockname()[1]))
    a6, p6 = s6.accept()
    print("v6", s6.getsockname()[0], p6[0], c6.getpeername()[0])
    # UDP 和自动绑定
    u = udp()
    u.bind(("127.0.0.1", 0))
    v = udp()
    v.sendto(b"x", u.getsockname())
    data, frm = u.recvfrom(10)
    print("udp", data, frm[0], frm == v.getsockname()[:0] or frm[1] == v.getsockname()[1], v.getsockname()[0])
    # 外面的地址到不了；外面的回环端口和抽象地址在这里是另一套
    print("outside", err(tcp().connect, ("10.0.0.1", 80)), err(udp().sendto, b"x", ("8.8.8.8", 53)),
          err(udp().connect, ("192.0.2.1", 9)), "bind", err(tcp().bind, ("10.0.0.1", 0)))
    print("outer port", err(tcp().connect, ("127.0.0.1", outer_port)))
    un = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    print("outer abstract", err(un.connect, "\0appine-netns"),
          "bind it here", err(socket.socket(socket.AF_UNIX, socket.SOCK_STREAM).bind, "\0appine-netns"))
    # /proc/net 是这个命名空间的：dev 只有 lo，tcp 里只有这里的 socket
    with open("/proc/self/net/dev") as f:
        print("dev", [l.split(":")[0].strip() for l in f.readlines()[2:]])
    with open("/proc/self/net/tcp") as f:
        listed = {l.split()[1] for l in f.readlines()[1:]}
    mine = {"0100007F:%04X" % port, "00000000:%04X" % any4.getsockname()[1],
            "0100007F:%04X" % c.getsockname()[1], "0100007F:%04X" % conn.getsockname()[1]}
    print("tcp listing has ours", mine <= listed, "not the outer one", "0100007F:%04X" % outer_port not in listed)
    # setns 回原来的命名空间：之后建的 socket 连得上外面的端口
    fd = os.open("/proc/%d/ns/net" % os.getppid(), os.O_RDONLY)
    print("setns back", err(os.setns, fd, CLONE_NEWNET), err(tcp().connect, ("127.0.0.1", outer_port)),
          "old socket", err(c.send, b"y"))


child(netns)
child(lambda: (os.setuid(65534), print("unprivileged unshare", err(os.unshare, CLONE_NEWNET))))


# root 建的用户命名空间里的网络命名空间：属主是新的用户命名空间，在里面能配 lo
def owned():
    os.unshare(CLONE_NEWUSER | CLONE_NEWNET)
    lo_up()
    s = tcp()
    s.bind(("127.0.0.1", 0))
    s.listen()
    print("in a user namespace", err(tcp().connect, s.getsockname()))


child(owned)
PY5
rm -rf /tmp/ns
