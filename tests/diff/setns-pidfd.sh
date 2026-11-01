# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined
# setns(2) 经由 pidfd 一次换成那个进程的几种命名空间（Linux 5.8 起）：标志的检查，按用户、
# 挂载、UTS、IPC、pid、cgroup、网络、time 的顺序逐个检查、全部通过才一起换，一起进了用户
# 命名空间时后面几种按那里的能力算，多线程、fs 共用、跟踪权限、已经结束的进程。util-linux
# 2.41 起的 nsenter -t 就是这样进去的，失败了不退回 /proc/<pid>/ns/*。原生一侧是有
# CAP_SYS_ADMIN 的 docker 容器
cd /tmp
python3 - <<'PY'
import ctypes, errno, functools, os, socket, threading, time

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
CLONE_VM = 0x100
CLONE_NEWTIME, CLONE_NEWNS, CLONE_NEWCGROUP, CLONE_NEWUTS = 0x80, 0x20000, 0x2000000, 0x4000000
CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWPID, CLONE_NEWNET = 0x8000000, 0x10000000, 0x20000000, 0x40000000
ALL = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID | CLONE_NEWNET
FIVE = CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWCGROUP
KINDS = ["mnt", "uts", "ipc", "net", "cgroup", "pid", "pid_for_children", "user", "time"]
CAP_SYS_PTRACE, CAP_SYS_ADMIN = 19, 21


def err(f, *a):
    try:
        f(*a)
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]


def ns(pid="self"):
    return {k: os.readlink(f"/proc/{pid}/ns/{k}") for k in KINDS}


def same(a, b):
    return " ".join(k for k in KINDS if a[k] == b[k])


def fork(body):
    """在子进程里运行 body 并等它结束；body 抛出的异常也打印出来。"""
    pid = os.fork()
    if pid == 0:
        try:
            body()
        except BaseException as e:
            print("child raised", repr(e))
        os._exit(0)
    os.waitpid(pid, 0)


def target(flags, hostname=None, pidns=False, nondumpable=False):
    """起一个 unshare 了 flags 的进程，返回 (子进程, 目标, 目标的 pidfd)；pidns 时目标是新的
    pid 命名空间里的 1 号进程（子进程 fork 出来的）。"""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            os.unshare(flags)
            if hostname:
                socket.sethostname(hostname)
            if nondumpable:
                libc.prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
            if pidns:
                c = os.fork()
                if c == 0:
                    time.sleep(100)
                    os._exit(0)
                os.write(w, b"%d\n" % c)
                os.waitpid(c, 0)
                os._exit(0)
            os.write(w, b"%d\n" % os.getpid())
            time.sleep(100)
        except BaseException as e:
            print("target raised", repr(e))
        os._exit(0)
    os.close(w)
    t = int(os.read(r, 64))
    os.close(r)
    return pid, t, os.pidfd_open(t)


def stop(t):
    for p in {t[0], t[1]}:
        try:
            os.kill(p, 9)
        except OSError:
            pass
    os.waitpid(t[0], 0)
    os.close(t[2])


class CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]


class CapData(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]


def drop_caps(effective=(), permitted=()):
    """capget、capset：从有效集（permitted 的也从允许集）里去掉这些能力。"""
    h = CapHeader(0x20080522, 0)
    d = (CapData * 2)()
    assert libc.syscall(90, ctypes.byref(h), d) == 0
    for c in list(effective) + list(permitted):
        d[c // 32].effective &= ~(1 << (c % 32))
    for c in permitted:
        d[c // 32].permitted &= ~(1 << (c % 32))
    assert libc.syscall(91, ctypes.byref(h), d) == 0


def parked_thread():
    ev = threading.Event()
    t = threading.Thread(target=ev.wait)
    t.start()
    return ev, t


# 主机名两边不同：先换进自己的 UTS 命名空间
os.unshare(CLONE_NEWUTS)
socket.sethostname("outer")
me = os.pidfd_open(os.getpid())

print("== flags")
print("zero", err(os.setns, me, 0))
print("not a namespace flag", err(os.setns, me, CLONE_VM))
print("own uts", err(os.setns, me, CLONE_NEWUTS))
print("own user", err(os.setns, me, CLONE_NEWUSER))
print("own all but user", err(os.setns, me, ALL))
print("own time", err(os.setns, me, CLONE_NEWTIME))
reg = os.open("/etc/hostname", os.O_RDONLY)
print("regular file", err(os.setns, reg, CLONE_NEWUTS))
os.close(reg)
print("bad fd", err(os.setns, 1000, CLONE_NEWUTS))
uts = os.open("/proc/self/ns/uts", os.O_RDONLY)
print("uts file with flag 0", err(os.setns, uts, 0), "with pid", err(os.setns, uts, CLONE_NEWPID))
os.close(uts)

print("== several at once")
t = target(FIVE, "inner")


def several():
    os.chdir("/tmp")
    before = ns()
    print("setns", err(os.setns, t[2], FIVE))
    after = ns()
    print("same as target:", same(after, ns(t[1])))
    print("unchanged:", same(after, before))
    print("hostname", socket.gethostname(), "cwd", os.getcwd())


fork(several)


def only_mnt():
    os.chdir("/tmp")
    print("mnt only", err(os.setns, t[2], CLONE_NEWNS), "cwd", os.getcwd())


fork(only_mnt)


# 只换挂载命名空间时 fs 不能和别的线程共用；一起换别的时 Linux 查的是临时复制的 fs，照样成功
def threaded():
    ev, th = parked_thread()
    print("threaded mnt only", err(os.setns, t[2], CLONE_NEWNS))
    print("threaded mnt and uts", err(os.setns, t[2], CLONE_NEWNS | CLONE_NEWUTS), socket.gethostname())
    print("threaded time", err(os.setns, t[2], CLONE_NEWTIME))
    time_ns = os.open("/proc/self/ns/time", os.O_RDONLY)
    print("threaded time file", err(os.setns, time_ns, 0))
    ev.set()
    th.join()


fork(threaded)
stop(t)

print("== pid")
t = target(CLONE_NEWPID, pidns=True)


def enter_pid():
    before = ns()
    print("setns", err(os.setns, t[2], CLONE_NEWPID))
    after = ns()
    print("own pid unchanged", after["pid"] == before["pid"], "children's is the target's", after["pid_for_children"] == ns(t[1])["pid"])
    r, w = os.pipe()
    c = os.fork()
    if c == 0:
        os.write(w, b"%d" % os.getpid())
        os._exit(0)
    os.waitpid(c, 0)
    print("child pid inside", os.read(r, 16).decode())


fork(enter_pid)
outer = os.pidfd_open(os.getpid())
other = target(CLONE_NEWUTS, "other")


# 从里层的 pid 命名空间进外层的是 EINVAL，一起要求的 UTS 也不换
def from_inside():
    os.unshare(CLONE_NEWPID)
    c = os.fork()
    if c == 0:
        print("outer pid ns", err(os.setns, outer, CLONE_NEWPID))
        print("outer pid ns and uts", err(os.setns, other[2], CLONE_NEWPID | CLONE_NEWUTS), socket.gethostname())
        print("uts alone", err(os.setns, other[2], CLONE_NEWUTS), socket.gethostname())
        os._exit(0)
    os.waitpid(c, 0)


fork(from_inside)
stop(other)
stop(t)

print("== user")
t = target(CLONE_NEWUSER | CLONE_NEWUTS, "userns")


# 没有 CAP_SYS_ADMIN 也是那个用户命名空间的属主：只进 UTS 不行，连同用户命名空间一起进可以
def without_sys_admin():
    drop_caps(effective=[CAP_SYS_ADMIN])
    print("uts alone", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname())
    print("user and uts", err(os.setns, t[2], CLONE_NEWUSER | CLONE_NEWUTS), socket.gethostname())
    print("user now the target's", ns()["user"] == ns(t[1])["user"])
    print("CapEff", [l.split()[1] for l in open("/proc/self/status") if l.startswith("CapEff")][0])


fork(without_sys_admin)
fork(lambda: print("root uts alone", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname()))


def threaded_user():
    ev, th = parked_thread()
    print("threaded user and uts", err(os.setns, t[2], CLONE_NEWUSER | CLONE_NEWUTS), socket.gethostname())
    ev.set()
    th.join()


fork(threaded_user)
stop(t)

print("== access")
t = target(CLONE_NEWUTS, "hidden", nondumpable=True)


def no_ptrace():
    drop_caps(permitted=[CAP_SYS_PTRACE])
    print("nondumpable target", err(os.setns, t[2], CLONE_NEWUTS), socket.gethostname())


fork(no_ptrace)
stop(t)

print("== exited")
c = os.fork()
if c == 0:
    os._exit(0)
fd = os.pidfd_open(c)
while open(f"/proc/{c}/stat").read().split()[2] != "Z":
    time.sleep(0.01)
print("zombie", err(os.setns, fd, CLONE_NEWUTS))
os.waitpid(c, 0)
print("reaped", err(os.setns, fd, CLONE_NEWUTS))
PY

# util-linux 的 nsenter -t、unshare
echo "== nsenter $(nsenter --version | grep -c util-linux)"
rm -f /tmp/ready*
wait_for() { while [ ! -e "$1" ]; do sleep 0.01; done; }
unshare -u -n -i -m sh -c 'hostname inner; touch /tmp/ready1; exec sleep 100' &
p1=$!
wait_for /tmp/ready1
echo "uts: $(nsenter -t $p1 -u hostname)"
echo "net links: $(nsenter -t $p1 -n ip -o link | wc -l)"
cmp='hostname; for k in mnt uts ipc net pid user cgroup time; do [ "$(readlink /proc/self/ns/$k)" = "$(readlink /proc/'$p1'/ns/$k)" ] && echo "  $k same"; done'
nsenter -t $p1 -m -u -i -n sh -c "$cmp"
echo "-a:"
nsenter -t $p1 -a sh -c "$cmp"
unshare -p -f --mount-proc sh -c 'touch /tmp/ready2; exec sleep 100' &
p2=$!
wait_for /tmp/ready2
nsenter -t "$(pgrep -P $p2)" -p -m sh -c 'echo "pid inside: $$"'
unshare -U -r -u sh -c 'hostname u; touch /tmp/ready3; exec sleep 100' &
p3=$!
wait_for /tmp/ready3
nsenter -t $p3 -U -u sh -c 'echo "user: $(hostname) uid $(id -u) map $(tr -s " " </proc/self/uid_map)"'
nsenter -t $p3 -a sh -c 'echo "user -a: $(hostname) uid $(id -u)"'
echo "unshare -r: $(unshare -r sh -c 'id -u; id -g; cat /proc/self/setgroups' | tr '\n' ' ')"
# unshare -f 的父进程等子进程时屏蔽了 SIGTERM，子进程是新的 pid 命名空间里的 1 号进程，
# 祖先命名空间发来的信号只有 SIGKILL、SIGSTOP 一定送到：用 SIGKILL 杀它，不然 wait 要等
# 里面的 sleep 100 自己结束
kill $p1 $p2 $p3
kill -9 $(pgrep -P $p2)
wait
nsenter -t $p1 -u true 2>/dev/null
echo "exited: $?"
