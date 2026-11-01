# docker-run-options: --privileged
# nftables 的子集（docs/rounds/172-nftables.md）：在新的网络命名空间里
# - 用 iptables-nft 建 docker 写进容器的内置 DNS 规则（-N、-C、-I、DNAT、SNAT），看 -S、
#   nft list ruleset，以及发往 127.0.0.11:53 的 UDP、TCP 被 DNAT 到服务真正监听的端口：
#   客户端看到的对端、回答的来源，服务端看到的来源和本端；
# - 用 nft -f 装一份 netavark 那样的规则（inet 表、匿名集合、具名集合、fib、dnat、
#   redirect），看带句柄的列表、端口转发、删规则之后；
# - 出错的情形：跳到没有的链、环、删被引用的链、已有的链、没有特权。
# 原生一侧是特权容器，规则真的在内核里执行。计数器（appine 总是 0）都去掉。
exec 2>&1
cd /tmp
cat > srv.py <<'PY'
import socket, sys
# srv.py tcp|udp <地址> <端口> <次数> <就绪文件>
socket.setdefaulttimeout(15)
kind, addr, port, n, ready = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
if kind == "tcp":
    s = socket.socket()
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
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    for _ in range(n):
        data, peer = s.recvfrom(100)
        s.sendto(f"{data.decode()}: server sees {peer[0]}".encode(), peer)
PY
cat > cli.py <<'PY'
import errno, os, socket, sys, time
# cli.py tcp|udp|udpconn <地址> <端口> <消息> [<文件>]：给了文件的，UDP 发出之后等它出现
# （服务端已经退出）才读回答
kind, addr, port, msg = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
late = sys.argv[5] if len(sys.argv) > 5 else None
try:
    if kind == "tcp":
        c = socket.socket()
        c.settimeout(10)
        c.connect((addr, port))
        p = c.getpeername()
        print(f"{msg}: connected to {p[0]}:{p[1]}")
        c.sendall(msg.encode())
        print(c.recv(200).decode())
    else:
        c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        c.settimeout(5)
        if kind == "udpconn":
            c.connect((addr, port))
            p = c.getpeername()
            print(f"{msg}: connected to {p[0]}:{p[1]}")
            c.send(msg.encode())
        else:
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
# 两个 unshare -n 的 shell 共用的函数
cat > /tmp/fns.sh <<'SH'
wait_for() { for i in $(seq 100); do [ -f "$1" ] && return; sleep 0.1; done; echo "timeout: $1"; }
servers=""
serve() {
    rm -f "/tmp/ready-$3"
    python3 /tmp/srv.py "$@" "/tmp/ready-$3" &
    servers="$servers $!"
    wait_for "/tmp/ready-$3"
}
served() { wait $servers; servers=""; }
# 表的句柄原生上不总是从 1 起（同一个命名空间里是计数器，见 docs/rounds/172-nftables.md），不比
nocount() { sed -E 's/counter packets [0-9]+ bytes [0-9]+/counter/; s/^(table .*) # handle [0-9]+$/\1/'; }
SH

unshare -n sh -c '
. /tmp/fns.sh
ip link set lo up
echo "== docker embedded DNS rules (iptables-nft)"
T() { "$@" >/tmp/out 2>&1; c=$?; sed "s/^/  /" /tmp/out; echo "[$c] $*" | sed "s/ --wait//"; }
T iptables --wait -t nat -N DOCKER_OUTPUT
T iptables --wait -t nat -C OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -I OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -I DOCKER_OUTPUT -d 127.0.0.11 -p udp --dport 53 -j DNAT --to-destination 127.0.0.11:40001
T iptables --wait -t nat -I DOCKER_OUTPUT -d 127.0.0.11 -p tcp --dport 53 -j DNAT --to-destination 127.0.0.11:40002
T iptables --wait -t nat -N DOCKER_POSTROUTING
T iptables --wait -t nat -I POSTROUTING -d 127.0.0.11 -j DOCKER_POSTROUTING
T iptables --wait -t nat -I DOCKER_POSTROUTING -s 127.0.0.11 -p udp --sport 40001 -j SNAT --to-source :53
T iptables --wait -t nat -C OUTPUT -d 127.0.0.11 -j DOCKER_OUTPUT
T iptables --wait -t nat -N DOCKER_OUTPUT
T iptables --wait -t nat -A NOPE -j ACCEPT
T iptables --wait -t nat -X DOCKER_OUTPUT
iptables -t nat -S
nft list ruleset | nocount

echo "== DNAT of 127.0.0.11:53"
serve udp 127.0.0.11 40001 2
python3 /tmp/cli.py udp 127.0.0.11 53 udp-query
python3 /tmp/cli.py udpconn 127.0.0.11 53 udp-connected
serve tcp 127.0.0.11 40002 1
python3 /tmp/cli.py tcp 127.0.0.11 53 tcp-query
served
echo "== after deleting the DNAT rules"
iptables -t nat -D DOCKER_OUTPUT -d 127.0.0.11 -p tcp --dport 53 -j DNAT --to-destination 127.0.0.11:40002; echo "[$?] -D"
iptables -t nat -F DOCKER_OUTPUT; echo "[$?] -F"
python3 /tmp/cli.py tcp 127.0.0.11 53 tcp-no-dnat
iptables -t nat -S DOCKER_OUTPUT
'

cat > /tmp/nv.nft <<'NFT'
table inet nv {
	set protos {
		type inet_proto
		elements = { tcp, udp }
	}

	chain OUTPUT {
		type nat hook output priority dstnat; policy accept;
		fib daddr type local jump HOSTPORT
	}

	chain HOSTPORT {
		meta l4proto { tcp, udp } th dport 8080 jump DNAT8080
		meta l4proto @protos th dport 8081 redirect to :9001
		tcp dport 8082 counter accept
	}

	chain DNAT8080 {
		ip daddr 10.70.0.1 meta l4proto tcp dnat ip to 10.70.0.1:9000
		ip daddr 10.70.0.1 meta l4proto udp dnat ip to 10.70.0.1:9002
	}
}
NFT

unshare -n sh -c '
. /tmp/fns.sh
ip link set lo up
ip link add dummy0 type dummy
ip addr add 10.70.0.1/24 dev dummy0
ip link set dummy0 up
echo "== nft -f (netavark style)"
nft -f /tmp/nv.nft; echo "load: $?"
nft -a list ruleset | nocount
echo "== port forwarding"
serve tcp 10.70.0.1 9000 1
python3 /tmp/cli.py tcp 10.70.0.1 8080 tcp-8080
serve udp 10.70.0.1 9002 1
python3 /tmp/cli.py udp 10.70.0.1 8080 udp-8080
serve tcp 127.0.0.1 9001 1
python3 /tmp/cli.py tcp 127.0.0.1 8081 redirect-8081
served
# 服务端回完就退出，客户端之后才读到回答：来源照样是 8080
rm -f /tmp/exited
serve udp 10.70.0.1 9002 1
python3 /tmp/cli.py udp 10.70.0.1 8080 udp-8080-late /tmp/exited &
late=$!
served; touch /tmp/exited; wait $late
python3 /tmp/cli.py tcp 10.70.0.1 8082 accepted-8082
python3 /tmp/cli.py tcp 10.1.2.3 8080 not-local
echo "== errors"
nft add rule inet nv HOSTPORT jump NOPE; echo "[$?]"
nft add rule inet nv DNAT8080 jump HOSTPORT; echo "[$?]"
nft add rule inet nv HOSTPORT jump OUTPUT; echo "[$?]"
nft delete chain inet nv DNAT8080; echo "[$?]"
nft add chain inet nv OUTPUT { type filter hook input priority 0 \; }; echo "[$?]"
nft delete set inet nv protos; echo "[$?]"
nft add table inet nv; echo "[$?] add an existing table"
nft create table inet nv; echo "[$?] create an existing table"
echo "== delete a rule, flush, delete"
h=$(nft -a list chain inet nv HOSTPORT | sed -n "s/.*tcp dport 8082 .*# handle //p")
nft delete rule inet nv HOSTPORT handle "$h"; echo "[$?] delete rule $h"
nft flush chain inet nv DNAT8080; echo "[$?] flush"
python3 /tmp/cli.py tcp 10.70.0.1 8080 after-flush
nft -a list table inet nv | nocount
nft delete table inet nv; echo "[$?] delete table"
nft list ruleset; echo "[$?] empty"
nft -f /tmp/nv.nft; echo "reload: $?"
nft -a list chain inet nv OUTPUT | nocount
echo "== without CAP_NET_ADMIN"
python3 -c "import os; os.setgid(65534); os.setuid(65534); os.execvp(\"nft\", [\"nft\", \"list\", \"ruleset\"])"; echo "[$?]"
python3 -c "import os; os.setgid(65534); os.setuid(65534); os.execvp(\"iptables\", [\"iptables\", \"-t\", \"nat\", \"-S\"])"; echo "[$?]"
'
