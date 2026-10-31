# PTY 的模式：struct termios2 的 TCGETS2、TCSETS2、TCSETSW2、TCSETSF2（glibc 2.42 起
# tcgetattr、tcsetattr、isatty 只用它们），速率字段按 CBAUD、CIBAUD 换算（Linux 的
# set_termios），PTY 总是 8 位、不校验、允许接收（pty_set_termios），地址位不变
python3 - <<'PY'
import errno, fcntl, functools, os, select, struct, termios

print = functools.partial(print, flush=True)
TCGETS2, TCSETS2, TCSETSW2, TCSETSF2 = 0x802c542a, 0x402c542b, 0x402c542c, 0x402c542d
FMT2 = "<4IB19s2I"
BOTHER, CBAUD, CBAUDEX, IBSHIFT, ADDRB = 0o10000, 0o10017, 0o10000, 16, 0o4000000000


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def get2(fd):
    b = fcntl.ioctl(fd, TCGETS2, bytes(44))
    return list(struct.unpack(FMT2, b))


def set2(fd, t, req=TCSETS2):
    return fcntl.ioctl(fd, req, struct.pack(FMT2, *t))


def show(label, fd):
    i, o, c, l, line, cc, isp, osp = get2(fd)
    print(label, "iflag=%o oflag=%o cflag=%o lflag=%o line=%d" % (i, o, c, l, line),
          "cc=" + cc.hex(), "ispeed=%d ospeed=%d" % (isp, osp))


m, s = os.openpty()
show("slave", s)
show("master", m)
# TCGETS 的前 36 字节与 TCGETS2 相同
old = fcntl.ioctl(s, termios.TCGETS, bytes(36))
print("tcgets prefix", old == fcntl.ioctl(s, TCGETS2, bytes(44))[:36])

# 不是终端的：ENOTTY；坏的指针：EFAULT
r, w = os.pipe()
nul = os.open("/dev/null", os.O_RDWR)
f = os.open("/tmp", os.O_RDONLY)
for name, fd in [("pipe", r), ("null", nul), ("dir", f)]:
    print(name, err(fcntl.ioctl, fd, TCGETS2, bytes(44)), err(fcntl.ioctl, fd, TCSETS2, bytes(44)))
import ctypes
libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
for req in (TCGETS2, TCSETS2):
    rc = libc.ioctl(s, req, ctypes.c_void_p(8))
    print("fault", hex(req), rc, errno.errorcode.get(ctypes.get_errno()))


def setcflag(fd, cbaud, ibaud=0, isp=0, osp=0, req=TCSETS2, keep=None):
    t = get2(fd)
    t[2] = t[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | cbaud | (ibaud << IBSHIFT)
    if keep is None:
        t[6], t[7] = isp, osp
    set2(fd, t, req)
    i, o, c, l, line, cc, isp2, osp2 = get2(fd)
    return "cflag=%o ispeed=%d ospeed=%d" % (c, isp2, osp2)


# 老的 Bxxx 写法：速率按表换算，字段里的值不算
print("B9600", setcflag(s, 0o15, isp=1, osp=2))
print("B115200/B1200", setcflag(s, 0o10002, 0o11))
print("B4000000", setcflag(s, 0o10017))
print("B0", setcflag(s, 0))
# BOTHER：用字段里的速率，输入、输出可以不同
print("BOTHER", setcflag(s, BOTHER, BOTHER, 12345, 54321))
print("BOTHER out only", setcflag(s, BOTHER, 0, 777, 31250))
print("BOTHER in only", setcflag(s, 0o15, BOTHER, 4242, 0))
# 主设备上设置的是从设备的
print("via master", setcflag(m, BOTHER, 0, 0, 2400))
show("slave after master", s)

# 老的 TCSETS 带着 BOTHER：速率字段沿用原来的
t = get2(s)
t[2] = t[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | BOTHER
t[6], t[7] = 1111, 2222
set2(s, t)
t2 = list(struct.unpack("<4IB19s", fcntl.ioctl(s, termios.TCGETS, bytes(36))))
t2[2] = t2[2] & ~CBAUD | BOTHER | (0o15 << IBSHIFT)
fcntl.ioctl(s, termios.TCSETS, struct.pack("<4IB19s", *t2))
show("tcsets bother", s)
t2[2] = t2[2] & ~CBAUD & ~(CBAUD << IBSHIFT) | 0o17
fcntl.ioctl(s, termios.TCSETS, struct.pack("<4IB19s", *t2))
show("tcsets b38400", s)

# TCSETA（termio）也重新换算速率
ta = bytearray(fcntl.ioctl(s, termios.TCGETA, bytes(18)))
c = struct.unpack_from("<H", ta, 4)[0]
struct.pack_into("<H", ta, 4, c & ~0o17 | 0o13)
fcntl.ioctl(s, termios.TCSETA, bytes(ta))
show("tcseta b2400", s)

# PTY 总是 CS8、CREAD、不校验，地址位改不了
t = get2(s)
t[2] = t[2] & ~0o60 & ~0o200 | 0o20 | 0o400 | 0o1000 | ADDRB
set2(s, t)
show("cs6 parenb parodd addrb", s)

# 别的标志、控制字符、行规程编号原样保存；TCSETSF2 清掉没读的输入，TCSETSW2 照常
t = get2(s)
t[0] |= termios.IXANY
t[3] &= ~termios.ECHO
t[4] = 0
cc = bytearray(t[5])
cc[termios.VINTR] = 1
t[5] = bytes(cc)
set2(s, t, TCSETSW2)
show("tcsetsw2", s)
os.write(m, b"abc\n")
select.select([s], [], [], 5)  # 原生上数据经由 flip buffer 异步交给行规程
print("before flush", struct.unpack("<i", fcntl.ioctl(s, termios.FIONREAD, bytes(4)))[0])
set2(s, get2(s), TCSETSF2)
print("after tcsetsf2", struct.unpack("<i", fcntl.ioctl(s, termios.FIONREAD, bytes(4)))[0])

# 规范模式开关时已有的输入保留（与 TCSETS 一样）
os.write(m, b"xy")
t = get2(s)
t[3] &= ~termios.ICANON
set2(s, t)
print("raw read", os.read(s, 10))

# Python 的 termios 模块（经由 libc 的 tcgetattr/tcsetattr）与 ioctl 一致
a = termios.tcgetattr(s)
a[4] = a[5] = termios.B57600
termios.tcsetattr(s, termios.TCSANOW, a)
show("tcsetattr b57600", s)
print("isatty", os.isatty(s), os.isatty(m), os.isatty(r))
PY

# 包模式（TIOCPKT，Linux 的 pty_set_pktmode、n_tty_read、n_tty_poll）：主设备每次读先给
# 一个状态字节。有状态（从设备的清空、^S/^Q 停住和恢复、流控的开关、EXTPROC 时 termios
# 变了）时只读到它，否则是 TIOCPKT_DATA（0）加数据；有状态时 poll 带 POLLPRI。GNU screen
# 打开每个窗口都要它，失败就不开窗口
python3 - <<'PY'
import errno, fcntl, functools, os, select, signal, struct, termios, time

print = functools.partial(print, flush=True)
TIOCPKT, TIOCGPKT, TIOCGPTLCK, TIOCSIG = 0x5420, 0x80045438, 0x80045439, 0x40045436
EXTPROC = 0o200000
NAMES = {select.POLLIN: "IN", select.POLLPRI: "PRI", select.POLLOUT: "OUT", select.POLLERR: "ERR",
         select.POLLHUP: "HUP", select.POLLRDNORM: "RDNORM", select.POLLWRNORM: "WRNORM"}


def err(f, *args):
    try:
        r = f(*args)
        return "ok" if r is None else r
    except OSError as e:
        return errno.errorcode[e.errno]


def geti(fd, req):
    return struct.unpack("i", fcntl.ioctl(fd, req, bytes(4)))[0]


def pkt(fd, on):
    return fcntl.ioctl(fd, TIOCPKT, struct.pack("i", on))


def events(fd):
    p = select.poll()
    p.register(fd, select.POLLIN | select.POLLPRI | select.POLLOUT)
    r = p.poll(0)
    return "|".join(n for b, n in sorted(NAMES.items()) if r and r[0][1] & b) or "-"


def wait(fd, pri=False):
    # 原生上写进主设备的数据经由 flip buffer 异步交给行规程，等到它产生结果
    select.select([] if pri else [fd], [], [fd] if pri else [], 5)


def rd(fd, n=100):
    try:
        return os.read(fd, n)
    except OSError as e:
        return errno.errorcode[e.errno]


def change(fd, f):
    a = termios.tcgetattr(fd)
    f(a)
    termios.tcsetattr(fd, termios.TCSANOW, a)


m, s = os.openpty()
os.set_blocking(m, False)
print("slave TIOCPKT", err(pkt, s, 1), "slave TIOCGPKT", err(geti, s, TIOCGPKT),
      "slave TIOCGPTLCK", err(geti, s, TIOCGPTLCK), "slave TIOCSIG", err(fcntl.ioctl, s, TIOCSIG, 2))
print("TIOCGPKT", geti(m, TIOCGPKT), "TIOCGPTLCK", geti(m, TIOCGPTLCK))
print("bad pointer", err(fcntl.ioctl, m, TIOCPKT, 0))
os.write(s, b"before")
wait(m)
print("plain read", rd(m))
pkt(m, 1)
print("TIOCGPKT on", geti(m, TIOCGPKT), "events", events(m), "empty read", rd(m), "zero read", rd(m, 0))
os.write(s, b"hello\n")
wait(m)
print("events with data", events(m))
print("one byte", rd(m, 1), "then", rd(m))
print("select exc", select.select([], [], [m], 0)[2] == [])

# 从设备这一边的清空：输入是 FLUSHREAD，输出是 FLUSHWRITE；主设备那边的不报告
for name, q in [("TCIFLUSH", termios.TCIFLUSH), ("TCOFLUSH", termios.TCOFLUSH), ("TCIOFLUSH", termios.TCIOFLUSH)]:
    termios.tcflush(s, q)
    print("slave", name, events(m), select.select([], [], [m], 0)[2] == [m], rd(m), rd(m))
termios.tcflush(m, termios.TCIOFLUSH)
print("master TCIOFLUSH", events(m), rd(m))
# TCSETSF 从哪一边都是清空从设备的输入
termios.tcsetattr(m, termios.TCSAFLUSH, termios.tcgetattr(m))
print("TCSAFLUSH via master", rd(m))
termios.tcsetattr(s, termios.TCSADRAIN, termios.tcgetattr(s))
print("TCSADRAIN", rd(m))

# 停住、恢复：TCOOFF/TCOON 和 ^S/^Q；两个都发生过时只剩后一个
termios.tcflow(s, termios.TCOOFF)
print("TCOOFF", rd(m))
termios.tcflow(s, termios.TCOOFF)
print("TCOOFF again", rd(m))
termios.tcflow(s, termios.TCOON)
print("TCOON", rd(m))
termios.tcflow(s, termios.TCOOFF)
termios.tcflow(s, termios.TCOON)
print("off and on", rd(m))
os.write(m, b"\x13")
wait(m, True)
print("^S", rd(m))
os.write(m, b"\x11")
wait(m, True)
print("^Q", rd(m), rd(m))
termios.tcflow(m, termios.TCOOFF)
termios.tcflow(m, termios.TCOON)
print("master TCOOFF/TCOON", rd(m))

# ^S/^Q 流控的开关：IXON、VSTOP、VSTART 都要是默认的才算
change(s, lambda a: a.__setitem__(0, a[0] & ~termios.IXON))
print("IXON off", rd(m))
change(s, lambda a: a.__setitem__(0, a[0] & ~termios.IXON))
print("again", rd(m))
change(m, lambda a: a.__setitem__(0, a[0] | termios.IXON))
print("IXON on via master", rd(m))


def vstop(c):
    def f(a):
        a[6][termios.VSTOP] = c
    return f


change(s, vstop(b"\x01"))
print("VSTOP changed", rd(m))
change(s, vstop(b"\x13"))
print("VSTOP back", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] | EXTPROC))
print("EXTPROC on", rd(m))
change(s, lambda a: a.__setitem__(1, a[1]))
print("EXTPROC kept", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] & ~EXTPROC))
print("EXTPROC off", rd(m), rd(m))

# ^C：没有前台进程组也清空两边（没设 NOFLSH）。关掉回显：Linux 清空输出只清还在 flip
# buffer 里的，回显什么时候到主设备那边说不准
change(s, lambda a: a.__setitem__(3, a[3] & ~termios.ECHO))
os.write(m, b"abc\x03")
wait(m, True)
print("^C", rd(m), "input left", fcntl.ioctl(s, termios.FIONREAD, bytes(4)) == bytes(4))
change(s, lambda a: a.__setitem__(3, a[3] | termios.NOFLSH))
os.write(m, b"x\x03")
time.sleep(0.2)
print("^C with NOFLSH", rd(m))
change(s, lambda a: a.__setitem__(3, a[3] & ~termios.NOFLSH | termios.ECHO))
termios.tcflush(s, termios.TCIFLUSH)
rd(m)

# 关掉包模式：没有头，状态不记；再打开时以前的状态也没有了
termios.tcflush(s, termios.TCIFLUSH)
pkt(m, 0)
print("off", geti(m, TIOCGPKT), events(m))
os.write(s, b"plain")
wait(m)
print("off read", rd(m))
termios.tcflush(s, termios.TCOFLUSH)
pkt(m, 1)
print("on again", events(m), rd(m))
termios.tcflush(s, termios.TCIFLUSH)
pkt(m, 1)
print("on while on", rd(m))

# epoll 的 EPOLLPRI
ep = select.epoll()
ep.register(m, select.EPOLLIN | select.EPOLLPRI)
termios.tcflow(s, termios.TCOOFF)
print("epoll", ep.poll(0), rd(m))
termios.tcflow(s, termios.TCOON)
rd(m)

# 从设备都关掉之后：先读完状态和数据，再是 EIO
os.write(s, b"last")
wait(m)
termios.tcflush(s, termios.TCIFLUSH)
os.close(s)
print("closed", events(m), rd(m), rd(m), rd(m))
os.close(m)

# TIOCSIG：给从设备的前台进程组发信号，只能是 SIGINT、SIGQUIT、SIGTSTP
m, s = os.openpty()
name = os.ttyname(s)
print("TIOCSIG without fg", err(fcntl.ioctl, m, TIOCSIG, signal.SIGINT), err(fcntl.ioctl, m, TIOCSIG, signal.SIGKILL),
      err(fcntl.ioctl, m, TIOCSIG, 0))
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    os.setsid()
    fd = os.open(name, os.O_RDWR)
    os.write(w, b"x")
    signal.pause()
    os._exit(0)
os.read(r, 1)
print("TIOCSIG SIGINT", err(fcntl.ioctl, m, TIOCSIG, signal.SIGINT))
_, st = os.waitpid(pid, 0)
print("child", os.WIFSIGNALED(st) and signal.Signals(os.WTERMSIG(st)).name)
PY

# 新分配的终端：窗口大小全是 0；主设备的读只在从设备打开过、又全部关掉之后才是 EIO（poll
# 带 POLLHUP，Linux 的 TTY_OTHER_CLOSED），从来没打开过时照常等。GNU screen 的子进程打开从
# 设备之前，主进程就在读主设备
python3 - <<'PY'
import errno, fcntl, functools, os, select, struct, time

print = functools.partial(print, flush=True)
TIOCGPTN, TIOCSPTLCK = 0x80045430, 0x40045431


def st(m):
    p = select.poll()
    p.register(m, select.POLLIN | select.POLLOUT)
    ev = p.poll(0)
    try:
        r = os.read(m, 10)
    except OSError as e:
        r = errno.errorcode[e.errno]
    return (ev[0][1] if ev else 0), r


def lock(m, on):
    fcntl.ioctl(m, TIOCSPTLCK, struct.pack("i", on))


m = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
name = "/dev/pts/%d" % struct.unpack("I", fcntl.ioctl(m, TIOCGPTN, bytes(4)))[0]
print("winsize", struct.unpack("4H", fcntl.ioctl(m, 0x5413, bytes(8))))  # TIOCGWINSZ
print("locked", st(m))
lock(m, 0)
print("unlocked, never opened", st(m))
s = os.open(name, os.O_RDWR | os.O_NOCTTY)
print("slave open", st(m))
s2 = os.dup(s)
os.close(s)
print("one of two closed", st(m))
os.close(s2)
print("all closed", st(m))
lock(m, 1)
print("locked again", st(m))
lock(m, 0)
s = os.open(name, os.O_RDWR | os.O_NOCTTY)
print("reopened", st(m))
os.close(s)
os.close(m)

# 阻塞的读：子进程过一会儿才打开从设备、写一行再关掉
m = os.open("/dev/ptmx", os.O_RDWR | os.O_NOCTTY)
name = "/dev/pts/%d" % struct.unpack("I", fcntl.ioctl(m, TIOCGPTN, bytes(4)))[0]
lock(m, 0)
pid = os.fork()
if pid == 0:
    time.sleep(0.3)
    s = os.open(name, os.O_RDWR | os.O_NOCTTY)
    os.write(s, b"late\n")
    time.sleep(0.2)
    os._exit(0)
print("blocking read", os.read(m, 100))
os.waitpid(pid, 0)
print("after the child", st(m))
PY
