# CPython：哈希、浮点格式化、十进制、unicode、sqlite3、线程
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
