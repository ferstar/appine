# docker-run-options: --cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined --security-opt systempaths=unconfined
# bubblewrap：几种常见的用法，包括 Codex 的沙箱实际传的参数（docs/rounds/123-bwrap.md）。
# 原生一侧是有 CAP_SYS_ADMIN 的 docker 容器，不盖住 /proc 下的路径（盖住了 Linux 就不让在
# 用户命名空间里挂新的 procfs）。附加组、不在新 pid 命名空间里时的 pid 两边不同，不比；
# 挂载只比 bwrap 自己建的几个挂载点和是否只读（文件系统类型、外面带进来的挂载两边不同）
cd /tmp
rm -rf ws
mkdir -p ws

echo "== plain"
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp sh -c '
    echo "uid $(id -u) gid $(id -g)"
    touch /x 2>&1; touch /tmp/x && echo "tmp writable"
    ls /dev | tr "\n" " "; echo
' 2>&1

echo "== mounts"
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --ro-bind /etc/passwd /tmp/passwd \
    --bind /tmp/ws /tmp/ws awk '{print $5, ($6 ~ /^ro/) ? "ro" : "rw"}' /proc/self/mountinfo 2>&1 |
    grep -E "^/(tmp|tmp/passwd|tmp/ws|dev/null|dev/pts|proc) " | sort

echo "== unshare-all"
bwrap --unshare-all --hostname box --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp sh -c '
    echo "uid $(id -u) gid $(id -g) pid $$"; hostname
    ls /proc | grep -c "^[0-9]"
    ip -o addr | cut -d" " -f2-4
    wget -q -T 2 -O- http://10.0.0.1/ 2>&1 | head -1
' 2>&1

echo "== codex"
bwrap --as-pid-1 --new-session --die-with-parent --tmpfs / --dev /dev --ro-bind /bin /bin \
    --ro-bind /etc /etc --ro-bind /lib /lib --ro-bind /sbin /sbin --ro-bind /usr /usr \
    --bind /tmp/ws /tmp/ws --chdir /tmp/ws \
    --unshare-user --unshare-pid --unshare-ipc --unshare-net --proc /proc --cap-drop ALL \
    -- /bin/sh -c '
    echo "pid $$ in $(pwd), uid $(id -u) gid $(id -g)"
    grep -E "^Cap(Eff|Prm|Bnd)" /proc/self/status
    ls / | tr "\n" " "; echo
    touch /etc/x 2>&1; echo work > /tmp/ws/out && echo "workspace writable"
    ip -o link | cut -d: -f2
' 2>&1
echo "outside sees: $(cat ws/out)"

echo "== uid"
bwrap --unshare-user --uid 1000 --gid 1000 --ro-bind / / --proc /proc sh -c 'echo "uid $(id -u) gid $(id -g)"; grep -E "^Cap(Eff|Prm)" /proc/self/status' 2>&1

echo "== errors"
bwrap --ro-bind /nonexistent /x true 2>&1
bwrap --ro-bind / / --chdir /nonexistent true 2>&1
echo "exit $?"
