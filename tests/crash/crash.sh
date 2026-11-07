#!/bin/sh
# Integrity of the store after a power loss (host crash) (round 198). Run as root in the Linux VM;
# needs dm-flakey and dmsetup, the two images appine-dev and alpine:3.24.3 in docker, and the
# repository at the same path in the VM as on the host (as colima's virtiofs has it):
#
#   colima ssh -- sudo sh tests/crash/crash.sh <appine|native> <power|kill> [runs] [binary]
#
# A freshly imported Alpine store is put in an ext4 on dm-flakey and workload.sh runs; at a random
# moment (3–15 seconds), power makes the device drop all later writes (like a power loss), ends the
# workload, unmounts, restores and remounts; kill only ends the workload (the app being killed),
# with the host's page cache intact. Then check.sh runs inside Appine (with the boot cleanup done as
# usual). native runs the same workload directly in an alpine container, for comparison. The binary
# is a path in the container (the repository is mounted at /work). With STOP=1 it stops at the first
# failure, leaving the store in /mnt/appine-crash/s.
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

# run "<docker options>" command…: appine uses the store's /bin/sh, native uses alpine's
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
    # Only what was printed before the power loss counts: after the switch, writes in fsync are
    # dropped, yet succeed as far as the guest can tell
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
