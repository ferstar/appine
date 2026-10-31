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
