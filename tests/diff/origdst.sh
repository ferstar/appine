# docker-run-options: --privileged
# 透明代理要的几样（docs/rounds/180-original-dst.md）：
# - SO_ORIGINAL_DST、IP6T_SO_ORIGINAL_DST：每种情形在新的网络命名空间里（lo 起来）建规则、
#   连接，看接受的一端、发起的一端、监听的 socket 回答什么（optlen、地址、错误码）。没有
#   规则、只有空的 nat 链（不开连接跟踪）、只有 ct 的 filter 规则（开）、nft 的 redirect 和
#   dnat、iptables 的 REDIRECT、ip6 和 inet 的表，双栈的监听，IPv4 映射的地址，客户端在
#   accept 之前就关掉了；
# - 跨命名空间：开机时的命名空间里一个 bridge，A、B 两个"容器"，B 的 PREROUTING 做
#   REDIRECT、A 的 OUTPUT 做 DNAT、外层的 PREROUTING 对从 bridge 来的做 REDIRECT；A 的 socket
#   带着 SO_MARK 经由 veth 进入 B 时标记清掉；A 连外网的地址也经过外层的 PREROUTING：被
#   REDIRECT 到外层的代理（只认本机地址的 `fib daddr type local` 规则不算）、DNAT 到 B（外层的
#   FORWARD 拒绝的连不上）、UDP 的 53 端口劫持到外层（回答来自原来的目的地）；外层绑通配
#   地址的 UDP 服务端回答 A 时，来源是外层面向 A 的地址；
# - SO_MARK：读写、长度、没有权限、UDP、accept 出来的沿用监听的；
# - iptables 的 owner、mark 匹配在 nat 和 filter 的 OUTPUT 上，nft 的 meta skuid、meta mark；
# - 一个 Python 写的透明代理：问 SO_ORIGINAL_DST，带着 SO_MARK 连原来的目的地（规则放过有
#   标记的），转一次请求。
# 原生一侧是特权容器，规则真的在内核里执行。各个角色的 Python 都先 import 同一组模块。
exec 2>&1
cd /tmp
cat > od.py <<'PY'
import errno, os, select, socket, struct, subprocess, sys, time

SO_ORIGINAL_DST = 80
SO_MARK = 36
V4, V6 = socket.AF_INET, socket.AF_INET6
socket.setdefaulttimeout(15)


def nft(script):
    script = script.replace("{ ", "{\n").replace("; ", "\n").replace(" }", "\n}")
    subprocess.run(["nft", "-f", "-"], input=script.encode(), check=True)


def sh(cmd):
    subprocess.run(cmd, shell=True, check=True)


def err(e):
    return errno.errorcode.get(e.errno, e.errno)


def fmt(b):
    fam = struct.unpack("<H", b[:2])[0] if len(b) >= 2 else None
    port = struct.unpack(">H", b[2:4])[0] if len(b) >= 4 else None
    if fam == 2 and len(b) >= 8:
        return f"{socket.inet_ntoa(b[4:8])}:{port} rest={b[8:16].hex()}"
    if fam == 10 and len(b) >= 28:
        flow, scope = struct.unpack(">I", b[4:8])[0], struct.unpack("<I", b[24:28])[0]
        return f"[{socket.inet_ntop(V6, b[8:24])}]:{port} flow={flow} scope={scope}"
    return b.hex()


def q(s, level=socket.SOL_IP, ln=16):
    """SO_ORIGINAL_DST：optlen（Linux 原样写回）和写进来的 sockaddr。"""
    try:
        b = s.getsockopt(level, SO_ORIGINAL_DST, ln)
        return f"optlen={len(b)} " + fmt(b[: 28 if level == socket.IPPROTO_IPV6 else 16])
    except OSError as e:
        return err(e)


def orig(s):
    try:
        b = s.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
        return f"{socket.inet_ntoa(b[4:8])}:{struct.unpack('>H', b[2:4])[0]}"
    except OSError as e:
        return err(e)


def listener(fam, addr, port, mark=None):
    l = socket.socket(fam, socket.SOCK_STREAM)
    l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if mark is not None:
        l.setsockopt(socket.SOL_SOCKET, SO_MARK, mark)
    l.bind((addr, port))
    l.listen()
    return l


def pair(lfam, laddr, lport, cfam, caddr, cport):
    l = listener(lfam, laddr, lport)
    c = socket.socket(cfam, socket.SOCK_STREAM)
    c.connect((caddr, cport))
    a, _ = l.accept()
    return l, c, a


def show(tag, l, c, a, v6=False):
    print(f"{tag}: accepted SOL_IP {q(a)}")
    print(f"{tag}: accepted SOL_IP optlen 64 {q(a, ln=64)}")
    print(f"{tag}: client SOL_IP {q(c)}")
    print(f"{tag}: listener SOL_IP {q(l)}")
    if v6:
        print(f"{tag}: accepted SOL_IPV6 {q(a, socket.IPPROTO_IPV6, 28)}")
        print(f"{tag}: accepted SOL_IPV6 optlen 64 {q(a, socket.IPPROTO_IPV6, 64)}")
        print(f"{tag}: client SOL_IPV6 {q(c, socket.IPPROTO_IPV6, 28)}")
        print(f"{tag}: accepted SOL_IPV6 optlen 27 {q(a, socket.IPPROTO_IPV6, 27)}")
    for x in (l, c, a):
        x.close()


def as_user(uid, fn):
    """在子进程里以 uid（gid 同号）运行 fn，打印它的结果。"""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            os.setgid(uid)
            os.setuid(uid)
            out = fn()
        except OSError as e:
            out = err(e)
        os.write(w, str(out).encode())
        os._exit(0)
    os.close(w)
    out = b""
    while chunk := os.read(r, 1000):
        out += chunk
    os.waitpid(pid, 0)
    os.close(r)
    return out.decode()


def where(port, uid=0, mark=None, ls=()):
    """连 127.0.0.2:port，返回哪个监听接受了它（被 REDIRECT 到 1081 的报原目的地址）。"""

    def connect():
        c = socket.socket()
        if mark is not None:
            c.setsockopt(socket.SOL_SOCKET, SO_MARK, mark)
        c.settimeout(5)
        try:
            c.connect(("127.0.0.2", port))
        except OSError as e:
            return err(e)
        c.close()
        return "connected"

    res = as_user(uid, connect) if uid else connect()
    if res != "connected":
        return res
    r, _, _ = select.select(ls, [], [], 5)
    if not r:
        return "nobody accepted"
    a, _ = r[0].accept()
    me = a.getsockname()[1]
    out = f"redirected (orig {orig(a)})" if me == 1081 else f"direct to {me}"
    a.close()
    return out


def probe(sc):
    if sc == "none":
        show("no rules", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        u = socket.socket(V4, socket.SOCK_DGRAM)
        u.bind(("127.0.0.1", 0))
        u.connect(("127.0.0.1", 9))
        print("udp", q(u), q(u, ln=4))
        print("tcp optlen 8", q(socket.socket(), ln=8))
        print("AF_INET SOL_IPV6", q(socket.socket(), socket.IPPROTO_IPV6, 28))
    elif sc == "chain":
        nft("table ip t { chain out { type nat hook output priority -100; } }")
        show("empty nat chain", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "filterct":
        nft("table ip t { chain out { type filter hook output priority 0; ct state new accept } }")
        show("ip filter ct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        show("ip filter ct, IPv6", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
    elif sc == "inetct":
        nft("table inet t { chain out { type filter hook output priority 0; ct state new accept } }")
        show("inet filter ct, IPv6", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
    elif sc == "redirect":
        nft(
            "table ip t { chain out { type nat hook output priority -100; "
            "ip daddr 127.0.0.2 tcp dport 2290 redirect to :1081; "
            "ip daddr 127.0.0.3 tcp dport 80 dnat to 127.0.0.1:1081; "
            "ip daddr 127.0.0.2 tcp dport 2291 redirect to :1082 } }"
        )
        show("redirected", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.2", 2290))
        show("dnat", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.3", 80))
        show("direct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
        show("dual-stack listener", *pair(V6, "::", 1082, V4, "127.0.0.2", 2291), v6=True)
        show("v6 client, mapped", *pair(V4, "127.0.0.1", 1081, V6, "::ffff:127.0.0.2", 2290), v6=True)
        show("v6 without an ip6 table", *pair(V6, "::1", 1083, V6, "::1", 1083), v6=True)
        l = listener(V4, "127.0.0.1", 1081)
        c = socket.create_connection(("127.0.0.2", 2290))
        c.sendall(b"x")
        c.close()
        time.sleep(0.2)
        a, _ = l.accept()
        print("client closed before accept:", q(a), a.recv(10))
        a.close()
        c = socket.create_connection(("127.0.0.2", 2290))
        a, _ = l.accept()
        c.close()
        time.sleep(0.2)
        print("client closed after accept:", q(a))
        a.close()
        l.close()
    elif sc == "iptables":
        sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2290 -j REDIRECT --to-ports 1081")
        show("iptables REDIRECT", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.2", 2290))
        show("iptables, direct", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "ip6":
        nft("table ip6 t { chain out { type nat hook output priority -100; ip6 daddr ::1 tcp dport 2290 redirect to :1083 } }")
        show("ip6 redirected", *pair(V6, "::1", 1083, V6, "::1", 2290), v6=True)
        show("IPv4 without an ip table", *pair(V4, "127.0.0.1", 1081, V4, "127.0.0.1", 1081))
    elif sc == "inet":
        nft("table inet t { chain out { type nat hook output priority -100; tcp dport 2290 redirect to :1084 } }")
        show("inet, IPv4", *pair(V6, "::", 1084, V4, "127.0.0.1", 2290), v6=True)
        show("inet, IPv6", *pair(V6, "::", 1084, V6, "::1", 2290), v6=True)


def marks():
    s = socket.socket()
    print("default", s.getsockopt(socket.SOL_SOCKET, SO_MARK))
    s.setsockopt(socket.SOL_SOCKET, SO_MARK, 5)
    print("set 5", s.getsockopt(socket.SOL_SOCKET, SO_MARK))
    print("optlen 2", s.getsockopt(socket.SOL_SOCKET, SO_MARK, 2).hex())
    s.setsockopt(socket.SOL_SOCKET, SO_MARK, -1)
    print("set -1", hex(s.getsockopt(socket.SOL_SOCKET, SO_MARK) & 0xFFFFFFFF))
    try:
        s.setsockopt(socket.SOL_SOCKET, SO_MARK, b"\x01\x00")
    except OSError as e:
        print("optlen 2 set", err(e))
    u = socket.socket(V4, socket.SOCK_DGRAM)
    u.setsockopt(socket.SOL_SOCKET, SO_MARK, 6)
    print("udp", u.getsockopt(socket.SOL_SOCKET, SO_MARK))

    def unprivileged():
        x = socket.socket()
        seen = x.getsockopt(socket.SOL_SOCKET, SO_MARK)
        try:
            x.setsockopt(socket.SOL_SOCKET, SO_MARK, 1)
            return f"read {seen}, set ok"
        except OSError as e:
            return f"read {seen}, set {err(e)}"

    print("nobody:", as_user(65534, unprivileged))
    l = listener(V4, "127.0.0.1", 1081, mark=9)
    c = socket.create_connection(("127.0.0.1", 1081))
    a, _ = l.accept()
    print("accepted from a listener with mark 9:", a.getsockopt(socket.SOL_SOCKET, SO_MARK),
          "client:", c.getsockopt(socket.SOL_SOCKET, SO_MARK))


def owner():
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2290 -m owner --uid-owner 65534 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2291 -m owner ! --uid-owner 0 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2292 -m owner --gid-owner 65534 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2293 -m mark --mark 7 -j RETURN")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2293 -j REDIRECT --to-ports 1081")
    sh("iptables -t nat -A OUTPUT -p tcp -d 127.0.0.2 --dport 2294 -m mark --mark 0x10/0xf0 -j REDIRECT --to-ports 1081")
    sh("iptables -A OUTPUT -p tcp -d 127.0.0.2 --dport 2295 -m owner --uid-owner 65534 -j REJECT --reject-with tcp-reset")
    sh("iptables -A OUTPUT -p tcp -d 127.0.0.2 --dport 2296 -m mark ! --mark 3 -j REJECT --reject-with tcp-reset")
    nft("table ip n { chain out { type nat hook output priority -50; "
        "meta skuid 65534 ip daddr 127.0.0.2 tcp dport 2297 redirect to :1081; "
        "meta mark 0x100 return; ip daddr 127.0.0.2 tcp dport 2298 redirect to :1081 } }")
    ls = [listener(V4, "127.0.0.1", 1081)] + [listener(V4, "127.0.0.2", p) for p in range(2290, 2299)]
    for port, cases in [
        (2290, [(0, None), (65534, None)]),
        (2291, [(0, None), (65534, None)]),
        (2292, [(0, None), (65534, None)]),
        (2293, [(0, None), (0, 7), (0, 8)]),
        (2294, [(0, 0x13), (0, 0x23), (0, None)]),
        (2295, [(0, None), (65534, None)]),
        (2296, [(0, 3), (0, None)]),
        (2297, [(0, None), (65534, None)]),
        (2298, [(0, None), (0, 0x100), (0, 0x101)]),
    ]:
        for uid, mark in cases:
            m = "" if mark is None else f" mark {mark:#x}"
            print(f"{port} uid {uid}{m}: {where(port, uid, mark, ls)}")


def proxy():
    """一个透明代理：接受被 REDIRECT 来的连接，问 SO_ORIGINAL_DST，带着标记 0x100（规则放过
    它）连原来的目的地，转一次请求和回答。"""
    nft("table ip p { chain out { type nat hook output priority -100; "
        "meta mark 0x100 return; ip daddr 127.0.0.0/8 tcp dport { 2290, 2291 } redirect to :1081 } }")
    origin = [listener(V4, "127.0.0.2", 2290), listener(V4, "127.0.0.3", 2291)]
    px = listener(V4, "127.0.0.1", 1081)
    pid = os.fork()
    if pid == 0:
        for _ in range(2):
            a, _ = px.accept()
            dst = a.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
            ip, port = socket.inet_ntoa(dst[4:8]), struct.unpack(">H", dst[2:4])[0]
            up = socket.socket()
            up.setsockopt(socket.SOL_SOCKET, SO_MARK, 0x100)
            up.connect((ip, port))
            up.sendall(a.recv(100) + f" (via proxy to {ip}:{port})".encode())
            a.sendall(up.recv(200))
            a.close()
            up.close()
        os._exit(0)
    for ip, port in [("127.0.0.2", 2290), ("127.0.0.3", 2291)]:
        c = socket.create_connection((ip, port))
        c.sendall(f"hello {ip}:{port}".encode())
        r, _, _ = select.select(origin, [], [], 5)
        s, peer = r[0].accept()
        got = s.recv(200).decode()
        s.sendall(f"origin {s.getsockname()[0]}:{s.getsockname()[1]} got '{got}'".encode())
        s.close()
        print(c.recv(300).decode())
        c.close()
    os.waitpid(pid, 0)


def srv(ports, ready, n):
    ls = [listener(V4, "0.0.0.0", int(p)) for p in ports]
    open(ready, "w").close()
    for _ in range(int(n)):
        r, _, _ = select.select(ls, [], [], 15)
        if not r:
            print("server: nothing accepted")
            return
        a, _ = r[0].accept()
        name = a.recv(100).decode()
        me = a.getsockname()
        a.sendall(f"{name}: server {me[0]}:{me[1]} sees orig {orig(a)}".encode())
        a.close()


def cli(addr, port, name, mark=None):
    c = socket.socket()
    if mark is not None:
        c.setsockopt(socket.SOL_SOCKET, SO_MARK, int(mark))
    try:
        c.connect((addr, int(port)))
    except OSError as e:
        print(f"{name}: {err(e)}")
        return
    c.sendall(name.encode())
    print(c.recv(200).decode(), f"| client orig {orig(c)}")


def usrv(port, ready, n):
    s = socket.socket(V4, socket.SOCK_DGRAM)
    s.bind(("0.0.0.0", int(port)))
    open(ready, "w").close()
    for _ in range(int(n)):
        data, peer = s.recvfrom(200)
        s.sendto(f"{data.decode()}: udp server {port} sees {peer[0]}".encode(), peer)


def ucli(addr, port, name):
    s = socket.socket(V4, socket.SOCK_DGRAM)
    s.sendto(name.encode(), (addr, int(port)))
    data, peer = s.recvfrom(200)
    print(f"{data.decode()} | reply from {peer[0]}:{peer[1]}")
    c = socket.socket(V4, socket.SOCK_DGRAM)
    c.connect((addr, int(port)))
    c.send(f"{name}, connected".encode())
    data = c.recv(200).decode()
    print(f"{data} | peer {c.getpeername()[0]}:{c.getpeername()[1]}")


cmd = sys.argv[1]
if cmd == "probe":
    probe(sys.argv[2])
elif cmd == "marks":
    marks()
elif cmd == "owner":
    owner()
elif cmd == "proxy":
    proxy()
elif cmd == "srv":
    srv(sys.argv[2:-2], sys.argv[-2], sys.argv[-1])
elif cmd == "cli":
    cli(*sys.argv[2:])
elif cmd == "usrv":
    usrv(*sys.argv[2:])
elif cmd == "ucli":
    ucli(*sys.argv[2:])
PY
fresh() { unshare -n sh -c "ip link set lo up; python3 /tmp/od.py $*"; }

for sc in none chain filterct inetct redirect iptables ip6 inet; do
    echo "== SO_ORIGINAL_DST: $sc"
    fresh probe "$sc"
done
echo "== SO_MARK"
fresh marks
echo "== owner and mark matches"
fresh owner
echo "== a transparent proxy"
fresh proxy

echo "== across namespaces"
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
# 外层把 DNAT 到 B 的转发过去
echo 1 > /proc/sys/net/ipv4/ip_forward
ip link add br0 address 02:00:00:00:50:01 type bridge
ip addr add 10.50.0.1/24 dev br0
ip link set br0 up
container() {
    unshare -n sleep 600 &
    eval "$1=$!"
    sleep 0.3
    ip link add "veth$1" type veth peer name eth0 netns "$(eval echo \$$1)"
    ip link set "veth$1" master br0
    ip link set "veth$1" up
    nsenter -t "$(eval echo \$$1)" -n sh -c "ip link set lo up; ip link set eth0 up; ip addr add $2/24 dev eth0; ip route add default via 10.50.0.1"
}
container A 10.50.0.2
container B 10.50.0.3
sleep 2
# B 的 PREROUTING：标记 7 的放过（经由 veth 进来时标记已经清掉了）
nsenter -t "$B" -n nft -f - <<'N'
table ip t {
  chain pre {
    type nat hook prerouting priority -100
    meta mark 7 return
    tcp dport 2290 redirect to :1081
  }
}
N
nsenter -t "$A" -n nft -f - <<'N'
table ip t {
  chain out {
    type nat hook output priority -100
    ip daddr 10.50.0.99 tcp dport 80 dnat to 10.50.0.3:1082
  }
}
N
# 外层：从 bridge 来的 2290 REDIRECT 到代理 1083；2292 只有发往本机地址的才到 1083（docker、
# podman 发布端口的规则这样限定），别的到 1084；2293、2294 DNAT 到 B，FORWARD 拒绝 B 的 1085（B 在那里监听着）；
# UDP 的 53 劫持到 1053。A 和 B 在同一个网段，DNAT 到 B 的要 masquerade，B 的回答才经过外层
# 换回来（appine 不执行 POSTROUTING，连接本来就在 socket 层）
nft -f - <<'N'
table ip t {
  chain pre {
    type nat hook prerouting priority -100
    iif br0 tcp dport 2290 redirect to :1083
    fib daddr type local tcp dport 2292 redirect to :1083
    iif br0 tcp dport 2292 redirect to :1084
    iif br0 tcp dport 2293 dnat to 10.50.0.3:1082
    iif br0 tcp dport 2294 dnat to 10.50.0.3:1085
    iif br0 udp dport 53 redirect to :1053
  }
  chain post {
    type nat hook postrouting priority 100
    ip saddr 10.50.0.2 ip daddr 10.50.0.3 masquerade
  }
  chain fw {
    type filter hook forward priority 0
    iif br0 oif br0 tcp dport 1085 reject with tcp reset
  }
}
N
rm -f /tmp/ready-b /tmp/ready-b2 /tmp/ready-outer /tmp/ready-udp
nsenter -t "$B" -n python3 /tmp/od.py srv 1081 1082 /tmp/ready-b 5 &
b_srv=$!
# 连到这里的都应该被外层的 FORWARD 拒绝：用完就停掉
nsenter -t "$B" -n python3 /tmp/od.py srv 1085 /tmp/ready-b2 2 &
b_rejected=$!
python3 /tmp/od.py srv 1083 1084 /tmp/ready-outer 4 &
outer_srv=$!
wait_for /tmp/ready-b
wait_for /tmp/ready-b2
wait_for /tmp/ready-outer
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.3 2290 "A to B, PREROUTING redirect"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.3 2290 "A to B with mark 7" 7
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.99 80 "A OUTPUT dnat to B"
python3 /tmp/od.py cli 10.50.0.3 2290 "outer to B"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2290 "A to the outer gateway"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2290 "A to an outside address"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2292 "A to the gateway, local-only rule"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2292 "A to an outside address, local-only rule"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2293 "A to an outside address, dnat to B"
nsenter -t "$A" -n python3 /tmp/od.py cli 192.0.2.10 2294 "A to an outside address, dnat to B, outer FORWARD rejects"
nsenter -t "$A" -n python3 /tmp/od.py cli 10.50.0.1 2294 "A to the gateway, dnat to B, outer FORWARD rejects"
kill "$b_rejected"
wait "$b_rejected" 2>/dev/null
wait "$b_srv" "$outer_srv"
python3 /tmp/od.py usrv 1053 /tmp/ready-udp 4 &
udp_srv=$!
wait_for /tmp/ready-udp
nsenter -t "$A" -n python3 /tmp/od.py ucli 10.50.0.1 1053 "A udp to the gateway"
nsenter -t "$A" -n python3 /tmp/od.py ucli 192.0.2.10 53 "A udp to an outside address"
wait "$udp_srv"
kill "$A" "$B"
wait "$A" "$B" 2>/dev/null
nft delete table ip t
ip link del br0
