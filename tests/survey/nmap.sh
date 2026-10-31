# nmap：以 root 运行时默认走 raw socket（SYN 扫描、ICMP/ARP 主机发现），Android 上的 App
# 建不了这种 socket；NMAP_UNPRIVILEGED 让它改用 connect 扫描（docs/rounds/152-nmap-unprivileged.md）。
# 原生对照：`scripts/survey -n alpine:3.24 nmap`，再加 SURVEY_DOCKER_OPTS=--cap-drop=NET_RAW
# 就是没有 CAP_NET_RAW 的 root（与 Android 上的 App 一样）
[ -f /usr/share/nmap/nse_main.lua ] || apk add -q nmap nmap-nselibs nmap-scripts
command -v python3 >/dev/null || apk add -q python3
nmap --version | head -1
python3 -m http.server 2280 --bind 127.0.0.1 >/dev/null 2>&1 &
srv=$!
sleep 1
quiet() { grep -v '^Starting\|^Nmap done\|latency\|^Service detection\|^SF'; }
echo "== root, default"; nmap -n -p 2280,2281 127.0.0.1 2>&1 | quiet
echo "== NMAP_UNPRIVILEGED=1"; NMAP_UNPRIVILEGED=1 nmap -n -p 2280,2281 127.0.0.1 2>&1 | quiet
echo "== host discovery"; NMAP_UNPRIVILEGED=1 nmap -n -sn 127.0.0.1 2>&1 | quiet
echo "== service version"; NMAP_UNPRIVILEGED=1 nmap -n -sV -p 2280 127.0.0.1 2>&1 | quiet
echo "== login shell"; printf 'export NMAP_UNPRIVILEGED=1\n' > /tmp/nmap-profile.sh
ENV=/tmp/nmap-profile.sh sh -ic 'nmap -n -p 2280 127.0.0.1' 2>&1 | quiet
kill $srv
