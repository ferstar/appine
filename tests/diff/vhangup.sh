# docker-run-options: --cap-add SYS_ADMIN --cap-add SYS_TTY_CONFIG
# vhangup(2) 和 TIOCVHANGUP（Linux 的 tty_vhangup）：挂断时已经打开着的文件读到 EOF、写和
# ioctl 是 EIO，会话组长收到 SIGHUP 和 SIGCONT，会话失去控制终端，没读的输入丢掉；节点还在，
# 重新打开就能照常用（login、agetty 靠这个赶走终端上留下的进程）。原生一侧加了
# CAP_SYS_TTY_CONFIG（vhangup 要它）和 CAP_SYS_ADMIN（TIOCVHANGUP 要它）
python3 - <<'PY'
import ctypes, errno, fcntl, functools, os, select, signal, struct, termios, time

print = functools.partial(print, flush=True)
libc = ctypes.CDLL(None, use_errno=True)
TIOCVHANGUP, TIOCSCTTY, TIOCGSID = 0x5437, 0x540e, 0x5429
POLL = {select.POLLIN: "IN", select.POLLOUT: "OUT", select.POLLERR: "ERR", select.POLLHUP: "HUP",
        select.POLLRDNORM: "RDNORM", select.POLLWRNORM: "WRNORM"}


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]
    except termios.error as e:
        return errno.errorcode[e.args[0]]


def call(f, *args):
    r = f(*args)
    return "ok" if r == 0 else errno.errorcode[ctypes.get_errno()]


def polled(fd):
    p = select.poll()
    p.register(fd, select.POLLIN | select.POLLOUT)
    ev = p.poll(0)
    return " ".join(n for b, n in POLL.items() if ev and ev[0][1] & b) or "-"


def describe(what, fd):
    # 非阻塞：挂断了的读照样是 0，没挂断时不会卡住用例
    os.set_blocking(fd, False)
    print(what, "poll", polled(fd), "read", err(os.read, fd, 100), "write", err(os.write, fd, b"x"),
          "tcgetattr", err(lambda: termios.tcgetattr(fd) and None), "tcsetpgrp", err(os.tcsetpgrp, fd, os.getpgrp()),
          "isatty", os.isatty(fd))


def fork(f):
    """在子进程里运行 f，返回它的退出码；异常也结束子进程，不接着跑父进程的代码。"""
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


got = []
signal.signal(signal.SIGHUP, lambda s, f: got.append("HUP"))
signal.signal(signal.SIGCONT, lambda s, f: got.append("CONT"))

# 没有控制终端时 vhangup 什么也不做
print("no ctty", call(libc.vhangup), got)

master, slave = os.openpty()
name = os.ttyname(slave)
os.close(slave)


def session():
    os.setsid()
    fd = os.open(name, os.O_RDWR)  # 会话组长打开：成为控制终端
    print("ctty", os.tcgetpgrp(fd) == os.getpgrp())
    # 会话里另一个进程也开着它：挂断之后它的文件也读到 EOF，收不到信号
    r, w = os.pipe()
    other = os.fork()
    if other == 0:
        os.setpgid(0, 0)
        mine = os.open(name, os.O_RDWR | os.O_NOCTTY)
        mine_got = []
        signal.signal(signal.SIGHUP, lambda s, f: mine_got.append("HUP"))
        os.write(w, b"x")
        time.sleep(0.5)
        os.set_blocking(mine, False)
        print("other process", polled(mine), err(os.read, mine, 10), mine_got)
        os._exit(0)
    os.read(r, 1)
    os.write(master, b"typed but not read\n")
    time.sleep(0.1)
    print("vhangup", call(libc.vhangup), got)
    describe("old fd", fd)
    print("ioctl TIOCSCTTY", err(fcntl.ioctl, fd, TIOCSCTTY, 0), "fcntl", err(fcntl.fcntl, fd, fcntl.F_GETFL) != "EIO")
    print("/dev/tty", err(os.open, "/dev/tty", os.O_RDWR))
    os.waitpid(other, 0)
    # 再挂断一次：没有控制终端了，什么也不做
    print("again", call(libc.vhangup), got)
    # 节点还在，重新打开照常用：会话组长又得到控制终端，挂断之前的输入没了
    fd2 = os.open(name, os.O_RDWR)
    print("reopen", os.isatty(fd2), polled(fd2), "sid", os.getsid(0) == struct.unpack("i", fcntl.ioctl(fd2, TIOCGSID, b"\0" * 4))[0],
          "/dev/tty", err(lambda: os.close(os.open("/dev/tty", os.O_RDWR))))
    os.write(master, b"after\n")
    print("read", os.read(fd2, 100))
    os.write(fd2, b"out\n")
    time.sleep(0.1)
    describe("old fd still", fd)
    # 用 TIOCVHANGUP 挂断这个文件所在的终端
    print("TIOCVHANGUP", err(fcntl.ioctl, fd2, TIOCVHANGUP), got)
    describe("hung by ioctl", fd2)
    print("TIOCVHANGUP again", err(fcntl.ioctl, fd2, TIOCVHANGUP))


print("session", fork(session))
# 主设备不受影响：从设备挂断之前写的还读得到
print("master", polled(master), os.read(master, 100))
fd = os.open(name, os.O_RDWR | os.O_NOCTTY)
os.write(master, b"to new\n")
print("new open", os.read(fd, 100))

# 没有能力：vhangup 要 CAP_SYS_TTY_CONFIG，TIOCVHANGUP 要 CAP_SYS_ADMIN
def unprivileged():
    os.setsid()
    t = os.open(name, os.O_RDWR)
    os.setresuid(65534, 65534, 65534)
    print("unprivileged", call(libc.vhangup), err(fcntl.ioctl, t, TIOCVHANGUP), os.isatty(t))
print("unprivileged", fork(unprivileged))

# 主设备上的 TIOCVHANGUP：挂断的是主设备这一边
def master_side():
    m, s = os.openpty()
    os.write(s, b"from slave\n")
    time.sleep(0.1)
    print("master TIOCVHANGUP", err(fcntl.ioctl, m, TIOCVHANGUP))
    print("master after", polled(m), err(os.read, m, 100), err(os.write, m, b"y"))
    print("slave after", polled(s), err(os.write, s, b"z"), os.isatty(s))
    m2 = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY)
    print("new master", polled(m2))
print("master side", fork(master_side))

# 包模式下从设备被挂断：清空了从设备的输入、输出，主设备先读到这两个状态
def packet():
    m, s = os.openpty()
    fcntl.ioctl(m, 0x5420, struct.pack("i", 1))  # TIOCPKT
    os.set_blocking(m, False)
    print("packet TIOCVHANGUP", err(fcntl.ioctl, s, TIOCVHANGUP))
    print("packet master", polled(m), err(os.read, m, 100), err(os.read, m, 100))
print("packet", fork(packet))
PY
