# docker-run-options: --privileged
# 各个网络命名空间的端口互不相干（docs/rounds/174-namespace-ports.md）：开机时的命名空间里一个
# bridge（10.70.0.1/24），两个新的网络命名空间 A、B 挂在上面（10.70.0.2、10.70.0.3）。外层占着
# 一个端口（监听通配地址、从不 accept，像 podman、docker-proxy 那样）时 A 照样绑得上同一个
# 端口，反过来也一样，TCP、UDP；连到 A 上没有监听的端口是 ECONNREFUSED（外层的监听不会接走）；
# 只绑 127.0.0.1、只绑 eth0 地址的服务从别的地址连不上；A 自己的冲突照常是 EADDRINUSE，
# SO_REUSEPORT 照常可以；/proc/net/tcp 里是 guest 的端口；服务回完就退出、之后才读到的
# 回答，来源照样是 guest 的端口。原生一侧是特权容器。
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import socket, sys
# srv.py tcp|udp <地址> <端口> <次数> <就绪文件>；没等到连接时 15 秒后退出
socket.setdefaulttimeout(15)
kind, addr, port, n, ready = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
fam = socket.AF_INET6 if ":" in addr else socket.AF_INET
if kind == "tcp":
    s = socket.socket(fam, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen()
    open(ready, "w").close()
    for _ in range(n):
        c, peer = s.accept()
        data = c.recv(100).decode()
        me = c.getsockname()
        c.sendall(f"{data}: server sees {peer[0]} -> {me[0]}:{me[1]}".encode())
        c.close()
else:
    s = socket.socket(fam, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    for _ in range(n):
        data, peer = s.recvfrom(100)
        s.sendto(f"{data.decode()}: server sees {peer[0]}".encode(), peer)
PY
cat > cli.py <<'PY'
import errno, os, socket, sys, time
# cli.py tcp|udp <地址> <端口> <消息> [<文件>]：给了文件的，UDP 发出之后等它出现（服务端
# 已经退出）才读回答
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
late = sys.argv[5] if len(sys.argv) > 5 else None
fam = socket.AF_INET6 if ":" in addr else socket.AF_INET
try:
    if kind == "tcp":
        c = socket.socket(fam, socket.SOCK_STREAM)
        c.settimeout(5)
        c.connect((addr, port))
        c.sendall(msg.encode())
        print(c.recv(200).decode())
    else:
        c = socket.socket(fam, socket.SOCK_DGRAM)
        c.settimeout(5)
        c.sendto(msg.encode(), (addr, port))
        for _ in range(100 if late else 0):
            if os.path.exists(late):
                break
            time.sleep(0.1)
        data, peer = c.recvfrom(200)
        print(f"{data.decode()}; reply from {peer[0]}:{peer[1]}")
except OSError as e:
    print(f"{msg}: {errno.errorcode.get(e.errno, e.errno)}")
PY
# hold.py tcp|udp <端口> <就绪文件>：外层占着端口（TCP 监听、从不 accept），30 秒
cat > hold.py <<'PY'
import socket, sys, time
kind, port, ready = sys.argv[1], int(sys.argv[2]), sys.argv[3]
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM if kind == "tcp" else socket.SOCK_DGRAM)
try:
    s.bind(("0.0.0.0", port))
    if kind == "tcp":
        s.listen()
    print(f"outer binds {kind} {port}: ok", flush=True)
except OSError as e:
    print(f"outer binds {kind} {port}: {e.strerror}", flush=True)
open(ready, "w").close()
time.sleep(30)
PY
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
servers=""
serve() {
    pid=$1; shift; rm -f "/tmp/ready-$1-$3"
    nsenter -t "$pid" -n python3 /tmp/srv.py "$@" "/tmp/ready-$1-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$1-$3"
}
served() { wait $servers; servers=""; }
holders=""
hold() {
    rm -f "/tmp/held-$1-$2"
    python3 /tmp/hold.py "$1" "$2" "/tmp/held-$1-$2" &
    holders="$holders $!"
    wait_for "/tmp/held-$1-$2"
}

ip link add br0 address 02:00:00:00:70:01 type bridge
ip addr add 10.70.0.1/24 dev br0
ip link set br0 up
container() {
    unshare -n sleep 600 &
    eval "$1=$!"
    sleep 0.3
    ip link add "veth$1" type veth peer name eth0 netns "$(eval echo \$$1)"
    ip link set "veth$1" master br0
    ip link set "veth$1" up
    nsenter -t "$(eval echo \$$1)" -n sh -c "ip link set lo up; ip link set eth0 up; ip addr add $2/24 dev eth0; ip route add default via 10.70.0.1"
}
container A 10.70.0.2
container B 10.70.0.3
sleep 2

echo "== the outer namespace holds the port first"
hold tcp 7000
serve "$A" tcp 0.0.0.0 7000 2
nsenter -t "$B" -n python3 cli.py tcp 10.70.0.2 7000 b-to-a
python3 cli.py tcp 10.70.0.2 7000 outer-to-a
served
echo "== A first, then the outer namespace"
serve "$A" tcp 0.0.0.0 7001 1
hold tcp 7001
nsenter -t "$B" -n python3 cli.py tcp 10.70.0.2 7001 b-to-a
served
echo "== UDP"
hold udp 7002
serve "$A" udp 0.0.0.0 7002 1
nsenter -t "$B" -n python3 cli.py udp 10.70.0.2 7002 b-to-a-udp
served
echo "== UDP: A's server has exited before B reads the reply"
rm -f /tmp/exited
serve "$A" udp 0.0.0.0 7012 1
nsenter -t "$B" -n python3 cli.py udp 10.70.0.2 7012 b-to-a-late /tmp/exited &
late=$!
served; touch /tmp/exited; wait $late
echo "== nothing listening in A"
hold tcp 7003
nsenter -t "$B" -n python3 cli.py tcp 10.70.0.2 7003 b-to-a-7003
python3 cli.py tcp 10.70.0.2 7003 outer-to-a-7003
nsenter -t "$A" -n python3 cli.py tcp 127.0.0.1 7003 a-to-itself-7003
echo "== UDP to A where nothing is bound (the outer namespace answers on that port)"
serve $$ udp 0.0.0.0 7009 1
nsenter -t "$B" -n python3 cli.py udp 10.70.0.2 7009 b-to-a-udp-unbound
python3 cli.py udp 127.0.0.1 7009 outer-to-itself
served
serve "$A" udp 127.0.0.1 7010 1
nsenter -t "$B" -n python3 cli.py udp 10.70.0.2 7010 b-to-a-udp-lo
nsenter -t "$A" -n python3 cli.py udp 127.0.0.1 7010 a-to-lo-udp
served
echo "== bound to 127.0.0.1 in A"
serve "$A" tcp 127.0.0.1 7004 1
nsenter -t "$B" -n python3 cli.py tcp 10.70.0.2 7004 b-to-a-lo
nsenter -t "$A" -n python3 cli.py tcp 10.70.0.2 7004 a-to-own-eth0
nsenter -t "$A" -n python3 cli.py tcp 127.0.0.1 7004 a-to-lo
served
echo "== bound to the eth0 address in A"
serve "$A" tcp 10.70.0.2 7005 1
nsenter -t "$A" -n python3 cli.py tcp 127.0.0.1 7005 a-to-lo-7005
nsenter -t "$B" -n python3 cli.py tcp 10.70.0.2 7005 b-to-a-eth0
served
echo "== conflicts inside A"
nsenter -t "$A" -n python3 -c '
import socket, errno
def try_bind(name, *opts):
    s = socket.socket()
    for o in opts:
        s.setsockopt(socket.SOL_SOCKET, o, 1)
    try:
        s.bind(("0.0.0.0", 7006 if not opts else 7007)); s.listen(); print(name, "ok")
    except OSError as e:
        print(name, errno.errorcode[e.errno])
    return s
a = try_bind("first 7006")
b = try_bind("second 7006")
c = try_bind("first 7007 reuseport", socket.SO_REUSEPORT)
d = try_bind("second 7007 reuseport", socket.SO_REUSEPORT)
# 行序不比（Linux 按哈希桶，appine 按 fd 的先后）
rows = []
for l in open("/proc/net/tcp").read().splitlines()[1:]:
    f = l.split()
    if f[3] == "0A" and f[1].split(":")[1] in ("1B5E", "1B5F"):
        rows.append("listening " + f[1])
print("\n".join(sorted(rows)))
print("getsockname", a.getsockname(), c.getsockname())
'
kill $holders "$A" "$B" 2>/dev/null; wait 2>/dev/null
ip link del br0
