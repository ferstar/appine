#!/bin/sh
# 断电（宿主崩溃）之后仓库的完整性（第 198 轮）。在 Linux 虚拟机里以 root 运行，要
# dm-flakey 和 dmsetup，docker 里有 appine-dev、alpine:3.24.3 两个镜像，仓库的路径在虚拟机
# 里与宿主相同（colima 的 virtiofs 就是这样）：
#
#   colima ssh -- sudo sh tests/crash/crash.sh <appine|native> <power|kill> [次数] [二进制]
#
# 在 dm-flakey 上的 ext4 里放一个刚导入的 Alpine 仓库，跑 workload.sh；到随机的时刻（3–15
# 秒）power 让设备丢掉之后所有的写（相当于断电）、结束负载、卸下、恢复、重新挂上；kill
# 只结束负载（App 被杀），宿主的页缓存都在。然后在 appine 里（开机的清理照常做）跑
# check.sh。native 在 alpine 容器里直接跑同一个负载，当对照。二进制是容器里的路径
# （仓库挂在 /work）。STOP=1 时第一次失败就停下，仓库留在 /mnt/appine-crash/s。
set -eu
mode=$1 how=$2 runs=${3:-10}
repo=$(cd "$(dirname "$0")/../.." && pwd)
bin=${4:-/work/target/linux-arm64/release/appine-run}
img=/var/tmp/appine-crash.img
dev=/dev/mapper/appine-crash
mnt=/mnt/appine-crash
base=/var/tmp/appine-crash-base
out=/var/tmp/appine-crash-out
mkdir -p "$mnt" "$out"

if ! dmsetup info appine-crash >/dev/null 2>&1; then
    truncate -s 4G "$img"
    loop=$(losetup -f --show "$img")
    size=$(blockdev --getsz "$loop")
    dmsetup create appine-crash --table "0 $size flakey $loop 0 180 0"
    mkfs.ext4 -q "$dev"
fi
loop=$(dmsetup table appine-crash | awk '{print $4}')
size=$(dmsetup table appine-crash | awk '{print $2}')
table() {
    dmsetup load appine-crash --table "$1"
    dmsetup suspend --nolockfs appine-crash
    dmsetup resume appine-crash
}
allow="0 $size flakey $loop 0 180 0"
drop="0 $size flakey $loop 0 0 180 1 drop_writes"

if [ ! -d "$base" ]; then
    docker run --rm -v "$repo:/work" -v /var/tmp:/var/tmp -w /work appine-dev \
        "$bin" --rootfs "$base" --import target/rootfs/alpine-minirootfs-3.24.3-aarch64.tar.gz
fi

# run "<docker 的选项>" 命令…：appine 里用仓库的 /bin/sh，原生用 alpine 的
run() {
    opts=$1
    shift
    if [ "$mode" = appine ]; then
        # shellcheck disable=SC2086
        docker run $opts -v "$repo:/work" -v "$mnt:/c" -w /work appine-dev \
            "$bin" --tier 1 --rootfs /c/s --no-pty --env PATH=/usr/sbin:/usr/bin:/sbin:/bin "$@"
    else
        # shellcheck disable=SC2086
        docker run $opts -v "$repo:/work" -v "$mnt:/c" -w /work alpine:3.24.3 "$@"
    fi
}

fails=0
n=1
while [ "$n" -le "$runs" ]; do
    mountpoint -q "$mnt" || mount "$dev" "$mnt"
    rm -rf "$mnt/s" "$mnt/native"
    cp -a "$base" "$mnt/s"
    sync
    if [ "$mode" = appine ]; then data=/data; else data=/c/native; fi
    docker rm -f appine-crash-w >/dev/null 2>&1 || true
    run "-d --name appine-crash-w" /bin/sh -c "$(cat "$repo/tests/crash/workload.sh")" sh "$data" >/dev/null
    delay=$(shuf -i 3-15 -n 1)
    sleep "$delay"
    # 断电之前打印出来的才算：切换之后 fsync 里的写被丢掉，guest 看来照样成功
    docker logs appine-crash-w > "$out/log-$n" 2>&1
    [ "$how" = power ] && table "$drop"
    docker kill appine-crash-w >/dev/null
    docker rm appine-crash-w >/dev/null
    umount "$mnt"
    if [ "$how" = power ]; then
        table "$allow"
    fi
    blockdev --flushbufs "$dev"
    echo 3 > /proc/sys/vm/drop_caches
    mount "$dev" "$mnt"
    synced=$(grep -c synced "$out/log-$n" || true)
    grep synced "$out/log-$n" | run "--rm -i" /bin/sh -c "$(cat "$repo/tests/crash/check.sh")" sh "$data" > "$out/check-$n" 2>&1 || true
    if grep -qx ok "$out/check-$n"; then
        r=ok
    else
        r=FAIL
        fails=$((fails + 1))
    fi
    echo "run $n: ${delay}s, $synced synced, $r $(grep -v '^ok$' "$out/check-$n" | head -3 | tr '\n' ';')"
    if [ "$r" = FAIL ] && [ -n "${STOP:-}" ]; then
        echo "stopped; the store is at $mnt/s"
        exit 1
    fi
    n=$((n + 1))
done
umount "$mnt"
echo "$mode $how: $fails of $runs failed"
