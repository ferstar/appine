# docker-run-options: --privileged
# The sequel to compat6.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): process_mrelease, /proc/<pid>/oom_score, /proc/swaps, /proc/locks, and ss
# reading /proc/net quietly when NETLINK_SOCK_DIAG refuses its queries
set -e
cd /tmp
# Linux's swap_show header, then the host's swap areas, which a container sees too
cat /proc/swaps

python3 - <<'PY'
import ctypes, errno, os, signal, sys
libc = ctypes.CDLL(None, use_errno=True)
PIDFD_OPEN, MRELEASE = 434, 448

def call(nr, *args):
    r = libc.syscall(ctypes.c_long(nr), *[ctypes.c_long(a) for a in args])
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]

def pidfd(pid):
    return libc.syscall(ctypes.c_long(PIDFD_OPEN), ctypes.c_long(pid), ctypes.c_long(0))

def rss(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1])

# process_mrelease's errors: not a pidfd (a /proc/<pid> directory neither), flags, a process that
# is not exiting, a zombie, a reaped process
print("bad fd", call(MRELEASE, -1, 0), "stdin", call(MRELEASE, 0, 0))
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    signal.pause()
    os._exit(0)
fd = pidfd(pid)
dfd = os.open(f"/proc/{pid}", os.O_RDONLY | os.O_DIRECTORY)
print("flags", call(MRELEASE, fd, 1), "running", call(MRELEASE, fd, 0), "proc dir", call(MRELEASE, dfd, 0))
os.kill(pid, signal.SIGKILL)
os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
print("zombie", call(MRELEASE, fd, 0))
os.waitpid(pid, 0)
print("reaped", call(MRELEASE, fd, 0))
os.close(fd)
os.close(dfd)

# A process stopped at PTRACE_EVENT_EXIT after exit_group: its memory goes, its maps stay, and it
# then exits as it meant to
PTRACE_TRACEME, PTRACE_CONT, PTRACE_SETOPTIONS, PTRACE_O_TRACEEXIT = 0, 7, 0x4200, 0x40
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    data = b"x" * (64 << 20)
    libc.ptrace(PTRACE_TRACEME, 0, 0, 0)
    os.kill(os.getpid(), signal.SIGSTOP)
    os._exit(len(data) & 3)
_, st = os.waitpid(pid, 0)
print("stopped", os.WIFSTOPPED(st) and os.WSTOPSIG(st) == signal.SIGSTOP)
fd = pidfd(pid)
libc.ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACEEXIT)
libc.ptrace(PTRACE_CONT, pid, 0, 0)
_, st = os.waitpid(pid, 0)
print("exit event", st >> 8 == signal.SIGTRAP | 6 << 8)
maps, before = open(f"/proc/{pid}/maps").read(), rss(pid)
print("release", call(MRELEASE, fd, 0), "again", call(MRELEASE, fd, 0))
print("64 MB freed", before - rss(pid) > 60 << 10, "maps kept", open(f"/proc/{pid}/maps").read() == maps)
libc.ptrace(PTRACE_CONT, pid, 0, 0)
_, st = os.waitpid(pid, 0)
print("exited", os.WIFEXITED(st), os.WEXITSTATUS(st))
os.close(fd)

# oom_score: 1000 plus the share of RAM and swap the process uses and oom_score_adj, times 2/3; 0
# for oom_score_adj -1000 and for a process with no memory left (a zombie)
def score(pid="self"):
    return int(open(f"/proc/{pid}/oom_score").read())

def adj(v):
    with open("/proc/self/oom_score_adj", "w") as f:
        f.write(str(v))

print("oom_score mode", oct(os.stat("/proc/self/oom_score").st_mode & 0o7777))
print("small process", 666 <= score() < 700)
adj(1000)
print("adj 1000", score() >= 1332)
adj(-1000)
print("adj -1000", score())
adj(0)
# The thread's directory has both too, the same as the process's
t = f"/proc/self/task/{os.getpid()}"
print("task oom files", [oct(os.stat(f"{t}/{n}").st_mode & 0o7777) for n in ["oom_score", "oom_score_adj"]])
with open(f"{t}/oom_score_adj", "w") as f:
    f.write("500")
print("task's oom_score_adj sets the process's", open("/proc/self/oom_score_adj").read().strip(),
      "task's score is the process's", abs(int(open(f"{t}/oom_score").read()) - score()) <= 1)
adj(0)
r, w = os.pipe()
sys.stdout.flush()
pid = os.fork()
if pid == 0:
    data = b"y" * (256 << 20)
    os.write(w, b"1")
    signal.pause()
    os._exit(len(data) & 3)
os.read(r, 1)
print("bigger child scores higher", score(pid) > score())
os.kill(pid, signal.SIGKILL)
os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
print("zombie", score(pid))
os.waitpid(pid, 0)
PY

# A vfork child is never the OOM killer's choice until it execs or exits (Linux's in_vfork: ending
# it frees nothing): its oom_score is 0, its waiting parent's is not
cat >vfork.c <<'C'
#include <time.h>
#include <unistd.h>
int main(void)
{
    if (vfork() == 0) {
        struct timespec t = {3, 0};
        nanosleep(&t, 0);
        _exit(0);
    }
    return 0;
}
C
gcc -o vfork vfork.c
./vfork &
p=$!
c=
for i in $(seq 50); do
    c=$(cat /proc/$p/task/$p/children 2>/dev/null || true)
    [ -z "$c" ] || break
    sleep 0.1
done
c=$(echo $c)
echo "vfork parent scores $([ "$(cat /proc/$p/oom_score)" -gt 0 ] && echo above 0), child $(cat /proc/$c/oom_score)"
wait $p
rm -f vfork vfork.c

python3 - <<'PY'
import fcntl, os, re, signal, struct, subprocess, sys
# Locks of each kind, some taken by a child, as /proc/locks lists them: the kind and type in their
# columns, who took them, the file by device and inode number as stat shows them, the range
signal.signal(signal.SIGIO, signal.SIG_IGN)
names = ["flock", "shared", "posix", "ofd", "lease", "child"]
files = {}
for n in names:
    open(f"/tmp/lk-{n}", "w").close()
    files[n] = open(f"/tmp/lk-{n}", "r" if n == "lease" else "r+")
ids = {}
for n, f in files.items():
    st = os.fstat(f.fileno())
    ids[f"{os.major(st.st_dev):02x}:{os.minor(st.st_dev):02x}:{st.st_ino}"] = n
fcntl.flock(files["flock"], fcntl.LOCK_EX)
fcntl.flock(files["shared"], fcntl.LOCK_SH)
fcntl.lockf(files["posix"], fcntl.LOCK_EX, 100, 10)
fcntl.lockf(files["posix"], fcntl.LOCK_SH, 0, 200)
fcntl.fcntl(files["ofd"], fcntl.F_OFD_SETLK, struct.pack("hhqqi", fcntl.F_WRLCK, 0, 5, 1, 0))
fcntl.fcntl(files["lease"], fcntl.F_SETLEASE, fcntl.F_RDLCK)
r, w = os.pipe()
r2, w2 = os.pipe()
sys.stdout.flush()
child = os.fork()
if child == 0:
    f, g = open("/tmp/lk-child", "r+"), open("/tmp/lk-shared")
    fcntl.lockf(f, fcntl.LOCK_EX, 0, 0)
    fcntl.flock(g, fcntl.LOCK_SH)
    # An open for writing breaks the lease (without waiting, being nonblocking)
    try:
        os.open("/tmp/lk-lease", os.O_WRONLY | os.O_NONBLOCK)
    except OSError as e:
        print("open of the leased file", e.strerror)
    sys.stdout.flush()
    os.write(w, b"1")
    os.read(r2, 1)
    os._exit(0)
os.read(r, 1)
lines = open("/proc/locks").read().splitlines()
# Numbered by position in the list (with gaps natively, for the locks of other pid namespaces)
nums = [int(l.split(":")[0]) for l in lines]
print("numbered", nums == sorted(set(nums)) and all(n > 0 for n in nums))
out = []
for l in lines:
    m = re.match(r"\d+: (.{17})(\S+) (-?\d+) (\S+) (.*)$", l)
    if not m or m.group(4) not in ids:
        continue
    kind, ty, pid, fid, rng = m.groups()
    who = {str(os.getpid()): "self", str(child): "child"}.get(pid, pid)
    out.append(f"{kind!r} {ty} {who} {ids[fid]} {rng}")
for l in sorted(out):
    print(" ", l)
# fdinfo's lock lines, numbered per file: the locks held through that open file (a POSIX lock
# through its table's), which lslocks reads to tell who holds OFD locks
who = {str(os.getpid()): "self", str(child): "child"}
for n in names:
    for l in open(f"/proc/self/fdinfo/{files[n].fileno()}").read().splitlines():
        m = re.match(r"lock:\t(\d+): (.{17})(\S+) (-?\d+) (\S+) (.*)$", l)
        if m:
            num, kind, ty, pid, fid, rng = m.groups()
            print(" ", n, num, repr(kind), ty, who.get(pid, pid), ids.get(fid), rng)
for pid in [os.getpid(), child]:
    p = subprocess.run(["lslocks", "-n", "-o", "COMMAND,TYPE,MODE,M,START,END,PATH", "-p", str(pid)],
                       capture_output=True, text=True)
    # In the reverse of /proc/locks' order, which Linux keeps newest first
    print("lslocks", who[str(pid)], sorted(" ".join(l.split()) for l in p.stdout.splitlines()),
          repr(p.stderr), p.returncode)
os.write(w2, b"1")
os.waitpid(child, 0)
fcntl.fcntl(files["lease"], fcntl.F_SETLEASE, fcntl.F_UNLCK)
for n in names:
    os.unlink(f"/tmp/lk-{n}")
PY

python3 - <<'PY'
import os, socket, subprocess
# NETLINK_SOCK_DIAG opens; ss lists the listeners, from it or from /proc/net, without errors
s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 4)
s.close()
tcp = socket.socket()
tcp.bind(("127.0.0.1", 38211))
tcp.listen()
if os.path.exists("/tmp/ss.sock"):
    os.unlink("/tmp/ss.sock")
unix = socket.socket(socket.AF_UNIX)
unix.bind("/tmp/ss.sock")
unix.listen()
for args, col in [(["-tlnH"], 3), (["-xlH"], 4)]:
    p = subprocess.run(["ss"] + args, capture_output=True, text=True)
    mine = [l.split() for l in p.stdout.splitlines()]
    mine = [f[:2] + [f[col]] for f in mine if f[col] in ("127.0.0.1:38211", "/tmp/ss.sock")]
    print("ss", *args, mine, "stderr", repr(p.stderr), "exit", p.returncode)
os.unlink("/tmp/ss.sock")
PY
