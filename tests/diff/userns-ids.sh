# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# 别的用户命名空间里的进程看到的 ID（from_kuid_munged）：SO_PEERCRED、SCM_CREDENTIALS（声明
# 的凭据按 scm_check_creds 检查，没声明时是真实 ID）、信号的 si_uid（发送者按自己的命名空间
# 填，目标在别的命名空间里时再换算；停止、继续时的 SIGCHLD 因此再换算一次，退出时的不）、
# waitid 的 si_uid、SysV IPC 的属主（IPC_STAT、IPC_SET、/proc/sysvipc）、/proc/net/tcp 的
# uid 和 auxv 的 AT_UID。子进程的用户命名空间里 0 是外面的 1000、100 是外面的 0
python3 - <<'PY'
import ctypes, errno, functools, os, signal, socket, struct, sys

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
IPC_RMID, IPC_SET, IPC_STAT, IPC_CREAT = 0, 1, 2, 0o1000
SOCK = "/tmp/ids.sock"


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def creds(pid, uid, gid):
    return [(socket.SOL_SOCKET, socket.SCM_CREDENTIALS, struct.pack("iII", pid, uid, gid))]


def recv_creds(s):
    msg, anc, _, _ = s.recvmsg(100, socket.CMSG_SPACE(12))
    for lvl, typ, data in anc:
        if (lvl, typ) == (socket.SOL_SOCKET, socket.SCM_CREDENTIALS):
            return msg, struct.unpack("iII", data)
    return msg, None


def peercred(s):
    return struct.unpack("iII", s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[1:]


def msgctl(qid, cmd, buf):
    if libc.msgctl(qid, cmd, buf) < 0:
        return errno.errorcode[ctypes.get_errno()]
    return "ok"


def owner(qid):
    buf = ctypes.create_string_buffer(120)
    r = msgctl(qid, IPC_STAT, buf)
    return r if r != "ok" else struct.unpack_from("5I", buf.raw, 4)


def set_owner(qid, uid, gid):
    buf = ctypes.create_string_buffer(120)
    msgctl(qid, IPC_STAT, buf)
    struct.pack_into("II", buf, 4, uid & 0xffffffff, gid & 0xffffffff)
    return msgctl(qid, IPC_SET, buf)


def sysvipc(keys):
    out = []
    with open("/proc/sysvipc/msg") as f:
        for line in f.read().splitlines()[1:]:
            w = line.split()
            if int(w[0]) in keys:
                out.append((hex(int(w[0])), w[7], w[8], w[9], w[10]))
    return sorted(out)


def tcp_uid(port):
    with open("/proc/net/tcp") as f:
        for line in f.read().splitlines()[1:]:
            w = line.split()
            if int(w[1].split(":")[1], 16) == port:
                return w[7]


def siginfo(info):
    return "%s code=%d uid=%d" % (signal.Signals(info.si_signo).name, info.si_code, info.si_uid)


# 同一个命名空间里：没声明时 SCM_CREDENTIALS 是真实 ID（SO_PEERCRED 是有效 ID）；只能声明自己的
# pid 和真实、有效、保存的 ID 之一；root 什么都能声明，pid 不存在是 ESRCH；长度不对是 EINVAL
a, b = socket.socketpair()
b.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
pid = os.fork()
if pid == 0:
    os.setresgid(5001, 6001, 6001)
    os.setresuid(5000, 6000, 6000)
    a.send(b"default")
    a.sendmsg([b"euid"], creds(os.getpid(), 6000, 6001))
    print("declare root", err(a.sendmsg, [b"x"], creds(os.getpid(), 0, 6001)),
          "declare parent", err(a.sendmsg, [b"x"], creds(os.getppid(), 5000, 5001)),
          "short", err(a.sendmsg, [b"x"], [(socket.SOL_SOCKET, socket.SCM_CREDENTIALS, struct.pack("ii", 1, 2))]))
    os._exit(0)
os.waitpid(pid, 0)
for _ in range(2):
    msg, c = recv_creds(b)
    print("received", msg, c[0] == pid, c[1:])
a.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
b.sendmsg([b"root"], creds(os.getpid(), 4242, 4343))
print("root declares", recv_creds(a)[1][1:], "missing pid", err(b.sendmsg, [b"x"], creds(999999, 0, 0)))
a.close()
b.close()

# 别的用户命名空间里的子进程
try:
    os.unlink(SOCK)
except FileNotFoundError:
    pass
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(SOCK)
os.chmod(SOCK, 0o777)
listener.listen(1)
pa, pc = socket.socketpair()
q1 = libc.msgget(0x1234, IPC_CREAT | 0o644)
q2 = libc.msgget(0x1235, IPC_CREAT | 0o644)
print("set q2 owner", set_owner(q2, 5000, 5001), "unmapped", set_owner(q1, -1, 0))
signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGCHLD})
r1, w1 = os.pipe()
r2, w2 = os.pipe()
# 新程序继续之后等父进程用 waitid 取走继续的事件再退出：退出了的子进程取不到（ECHILD）
r3, w3 = os.pipe()
child = os.fork()
if child == 0:
    try:
        os.unshare(CLONE_NEWUSER)
        os.write(w1, b"x")
        os.read(r2, 1)
        print("inside", os.getresuid(), os.getresgid())
        os.setresgid(0, 0, 0)
        os.setresuid(0, 0, 0)
        print("after setresuid", os.getresuid(), os.getresgid())
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(SOCK)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
        print("child SO_PEERCRED", peercred(s))
        s.send(b"a")
        s.sendmsg([b"b"], creds(os.getpid(), 100, 100))
        print("declare unmapped", err(s.sendmsg, [b"x"], creds(os.getpid(), 7, 0)),
              "declare parent's pid", err(s.sendmsg, [b"x"], creds(os.getppid(), 0, 0)))
        print("child IPC_STAT", owner(q1), owner(q2))
        q3 = libc.msgget(0x1236, IPC_CREAT | 0o644)
        print("child IPC_SET", set_owner(q3, 7, 0), set_owner(q3, 100, 100), owner(q3))
        print("child sysvipc", sysvipc({0x1234, 0x1235, 0x1236}))
        t = socket.socket()
        t.bind(("127.0.0.1", 0))
        t.listen(1)
        port = t.getsockname()[1]
        print("child tcp uid", tcp_uid(port))
        pc.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
        rt = signal.SIGRTMIN
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1, signal.SIGUSR2, rt})
        os.write(w1, struct.pack("i", port))
        os.read(r2, 1)
        for _ in range(2):
            msg, c = recv_creds(pc)
            print("child received", msg, c[0] == os.getppid(), c[1:])
        for _ in range(3):
            print("child got", siginfo(signal.sigwaitinfo({signal.SIGUSR1, signal.SIGUSR2, rt})))
        # 新程序的 AT_UID、AT_EUID、AT_GID、AT_EGID；之后停住、继续、退出
        os.set_inheritable(r3, True)
        code = ("import os, signal, struct, sys\n"
                "a = open('/proc/self/auxv', 'rb').read()\n"
                "v = dict(struct.iter_unpack('QQ', a))\n"
                "print('auxv', v[11], v[12], v[13], v[14], flush=True)\n"
                "os.kill(os.getpid(), signal.SIGSTOP)\n"
                "os.read(int(sys.argv[1]), 1)\n")
        os.execv(sys.executable, [sys.executable, "-c", code, str(r3)])
    except BaseException as e:
        print("child raised", repr(e))
    os._exit(1)

os.read(r1, 1)
for f in ("uid_map", "gid_map"):
    with open("/proc/%d/%s" % (child, f), "w") as m:
        m.write("0 1000 1\n100 0 1\n")
os.write(w2, b"x")
port = struct.unpack("i", os.read(r1, 4))[0]
conn, _ = listener.accept()
conn.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
print("parent SO_PEERCRED", peercred(conn))
for _ in range(2):
    msg, c = recv_creds(conn)
    print("parent received", msg, c[0] == child, c[1:])
print("parent tcp uid", tcp_uid(port))
q3 = libc.msgget(0x1236, 0)
print("parent IPC_STAT", owner(q3), "sysvipc", sysvipc({0x1234, 0x1235, 0x1236}))
pa.sendmsg([b"p"], [])
pa.sendmsg([b"q"], creds(os.getpid(), 4242, 4343))
os.kill(child, signal.SIGUSR1)
helper = os.fork()
if helper == 0:
    os.setresuid(5000, 1000, 1000)
    os.kill(child, signal.SIGUSR2)
    os._exit(0)
os.waitpid(helper, 0)
print("helper SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
# rt_sigqueueinfo：si_uid 是调用者给的、按它的命名空间
info = struct.pack("iiiiiIq", signal.SIGRTMIN, 0, -1, 0, os.getpid(), 0, 7).ljust(128, b"\0")
libc.syscall.argtypes = [ctypes.c_long, ctypes.c_long, ctypes.c_long, ctypes.c_char_p]
print("sigqueue", libc.syscall(138, child, signal.SIGRTMIN, info))
os.write(w2, b"x")
print("stop SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WSTOPPED)
print("waitid stopped", w.si_code, w.si_uid)
os.kill(child, signal.SIGCONT)
print("continue SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WCONTINUED)
print("waitid continued", w.si_code, w.si_uid)
os.write(w3, b"x")
print("exit SIGCHLD", siginfo(signal.sigwaitinfo({signal.SIGCHLD})))
w = os.waitid(os.P_PID, child, os.WEXITED)
print("waitid exited", w.si_code, w.si_uid, w.si_status)
for q in (q1, q2, q3):
    libc.msgctl(q, IPC_RMID, None)
os.unlink(SOCK)
PY

# 用户命名空间里的 root 没有 no_new_privs 也能装 seccomp 过滤器：要的是自己所在的命名空间里的
# CAP_SYS_ADMIN（Linux 的 seccomp_prepare_filter 用 ns_capable），进了新的用户命名空间之后
# 开机时的那个里的能力就没有了；开机时的命名空间里的普通用户不行。用 root 建用户命名空间：
# 原生的 Ubuntu 宿主开着 apparmor_restrict_unprivileged_userns，普通用户建的里面能力被拒绝
cat > /tmp/sfilter.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    if (argc > 1 && unshare(CLONE_NEWUSER) != 0) {
        printf("unshare: %s\n", strerror(errno));
        return 0;
    }
    // 没有内核头文件：BPF_RET | BPF_K 返回 SECCOMP_RET_ALLOW，SECCOMP_SET_MODE_FILTER 是 1
    struct { uint16_t code; uint8_t jt, jf; uint32_t k; } allow = {0x06, 0, 0, 0x7fff0000};
    struct { unsigned short len; void *filter; } prog = {1, &allow};
    long r = syscall(SYS_seccomp, 1, 0, &prog);
    printf("%s: %s\n", argc > 1 ? "in a new user namespace" : "as is", r ? strerror(errno) : "ok");
    return 0;
}
C
gcc -o /tmp/sfilter /tmp/sfilter.c && su nobody -s /bin/sh -c /tmp/sfilter && /tmp/sfilter userns
