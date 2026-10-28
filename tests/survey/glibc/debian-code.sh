# Debian（glibc）：code-server（VS Code 的服务端，自带 glibc 的 node）启动、回应页面
cd /tmp; s=$(date +%s); cs=$HOME/.local/bin/code-server
[ -x "$cs" ] || { curl -fsSL https://code-server.dev/install.sh | sh -s -- --method standalone >/tmp/cs-install.log 2>&1; echo "install: $? ($(( $(date +%s) - s ))s)"; tail -1 /tmp/cs-install.log; }
$cs --version | head -1
$cs --bind-addr 127.0.0.1:8080 --auth none --disable-telemetry /tmp >/tmp/cs.log 2>&1 &
pid=$!  # dash 的 kill %1 发给进程组，这里后台任务不是自己一个进程组
s=$(date +%s); for i in $(seq 60); do curl -fs -m 5 -o /dev/null http://127.0.0.1:8080/healthz && break; sleep 1; done
echo "ready: $(( $(date +%s) - s ))s $(curl -s -m 5 http://127.0.0.1:8080/healthz)"
curl -sL -m 60 -o /tmp/cs.html -w 'page: %{http_code} %{size_download} bytes\n' 'http://127.0.0.1:8080/?folder=/tmp'
grep -o '<title>[^<]*\|workbench[a-z./-]*\.js' /tmp/cs.html | sort -u | head -3
js=$(grep -o 'static/[^"]*workbench\.js' /tmp/cs.html | head -1)
[ -n "$js" ] && curl -s -m 60 -o /dev/null -w "workbench.js: %{http_code} %{size_download} bytes\n" "http://127.0.0.1:8080/$js"
kill $pid; wait $pid; echo "exit: $?"; tail -3 /tmp/cs.log
