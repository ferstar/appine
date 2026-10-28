# CPython：哈希、浮点格式化、十进制、unicode、sqlite3、线程；文件和目录的时间戳；FICLONE
python3 - <<'PY'
import base64, decimal, fractions, hashlib, json, math, re, sqlite3, struct, threading, unicodedata, zlib
print(hashlib.sha256(b"x" * 10**6).hexdigest(), hashlib.md5(b"appine").hexdigest())
print([round(math.sin(i), 12) for i in range(8)])
print(repr(2 ** 0.5), 1e300 * 1e10, -0.0, math.fsum([0.1] * 10), 0.1 + 0.2)
print(struct.pack("<d", 0.1).hex(), float.fromhex("0x1.fffffffffffffp+1023"), math.ldexp(1, -1074))
decimal.getcontext().prec = 50
print(decimal.Decimal(2).sqrt())
print(sum(fractions.Fraction(1, n) for n in range(1, 100)))
d = {"a": list(range(50)), "b": "ß→İ"}
print(base64.b64encode(zlib.compress(json.dumps(d).encode(), 9)).decode())
print("ß".upper(), "İ".lower(), unicodedata.name("€"), sorted("Zäa"))
db = sqlite3.connect(":memory:")
db.execute("create table t(x integer, y text)")
db.executemany("insert into t values (?, ?)", [(i, str(i * i)) for i in range(1000)])
print(db.execute("select count(*), sum(x), max(length(y)) from t where x % 7 = 3").fetchone())
total, lock = [0], threading.Lock()
def work(n):
    s = sum(i * i for i in range(n))
    with lock:
        total[0] += s
threads = [threading.Thread(target=work, args=(10000 + i,)) for i in range(8)]
for t in threads:
    t.start()
for t in threads:
    t.join()
print(total[0])
print(re.sub(r"(\w+)@(\w+)", r"\2 at \1", "alice@example bob@test"))
print(int("9" * 400) % 1000000007, pow(3, 10**5, 10**9 + 7))
PY

# 时间戳：目录里新建、删除、改名改目录的 mtime 和 ctime；写、截断、chmod、utimensat 改
# 文件的 ctime；import 找得到同一个目录里后来新建的模块（它按目录的 mtime 重新列目录）
rm -rf /tmp/ts && mkdir /tmp/ts && cd /tmp/ts && python3 - <<'PY'
import os, sys, time
def times(p):
    st = os.stat(p, follow_symlinks=False)
    return st.st_mtime_ns, st.st_ctime_ns
def check(name, p, op):
    before = times(p)
    time.sleep(0.05)
    op()
    after = times(p)
    print(name, after[0] > before[0], after[1] > before[1])
def append(p):
    with open(p, "a") as f:
        f.write("data")
os.mkdir("a")
os.mkdir("b")
check("create", "a", lambda: open("a/f", "w").close())
check("rename-from", "a", lambda: os.rename("a/f", "b/f"))
check("rename-to", "a", lambda: os.rename("b/f", "a/f"))
check("link", "a", lambda: os.link("a/f", "a/g"))
check("unlink", "a", lambda: os.unlink("a/g"))
check("mkdir", "a", lambda: os.mkdir("a/d"))
check("rmdir", "a", lambda: os.rmdir("a/d"))
check("symlink", "a", lambda: os.symlink("f", "a/l"))
check("write", "a/f", lambda: append("a/f"))
check("truncate", "a/f", lambda: os.truncate("a/f", 1))
check("chmod", "a/f", lambda: os.chmod("a/f", 0o600))
for p in ["a/f", "a", "a/l"]:
    before = times(p)
    time.sleep(0.05)
    os.utime(p, ns=(10**9, 2 * 10**9), follow_symlinks=False)
    print("utime", p, times(p)[0], times(p)[1] > before[1])
os.mkdir("m")
sys.path.insert(0, "/tmp/ts/m")
with open("m/one.py", "w") as f:
    f.write("x = 1\n")
import one
time.sleep(0.05)
with open("m/two.py", "w") as f:
    f.write("x = 2\n")
import two
print("import", one.x, two.x)
PY

# FICLONE：检查的顺序和错误码；文件系统不支持 reflink 时 Path.copy 退回普通的复制
python3 - <<'PY'
import errno, fcntl, os, pathlib, struct
FICLONE, FICLONERANGE = 0x40049409, 0x4020940d
def err(f):
    try:
        f()
        return "ok"
    except OSError as e:
        return errno.errorcode[e.errno]
with open("/tmp/ts/src", "w") as f:
    f.write("cloned data\n")
src = os.open("/tmp/ts/src", os.O_RDONLY)
dst = os.open("/tmp/ts/dst", os.O_WRONLY | os.O_CREAT, 0o644)
rdonly = os.open("/tmp/ts/dst", os.O_RDONLY)
append = os.open("/tmp/ts/dst", os.O_WRONLY | os.O_APPEND)
directory = os.open("/tmp/ts", os.O_RDONLY)
pipe = os.pipe()[0]
# O_PATH 打开的 fd 不能做 ioctl 的对象，也不能做源（fdget 不接受，EBADF）
path_src = os.open("/tmp/ts/src", os.O_PATH)
path_dst = os.open("/tmp/ts/dst", os.O_PATH)
for name, fd, arg in [("file", dst, src), ("rdonly", rdonly, src), ("append", append, src),
                      ("dir", dst, directory), ("pipe", dst, pipe), ("badfd", dst, 999),
                      ("path-src", dst, path_src), ("path-dst", path_dst, src)]:
    print("ficlone", name, err(lambda: fcntl.ioctl(fd, FICLONE, arg)))
print("ficlonerange", err(lambda: fcntl.ioctl(dst, FICLONERANGE, struct.pack("qQQQ", src, 0, 0, 0))))
print("ficlonerange path-src", err(lambda: fcntl.ioctl(dst, FICLONERANGE, struct.pack("qQQQ", path_src, 0, 0, 0))))
print("fioclex path", err(lambda: fcntl.ioctl(path_src, 0x5451)))
if hasattr(pathlib.Path, "copy"):
    pathlib.Path("/tmp/ts/src").copy("/tmp/ts/copy")
    print("path.copy", open("/tmp/ts/copy").read().strip())
PY
