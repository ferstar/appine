# Debian（glibc）：apt 装后面几个负载要的软件包（联网），其余 debian-* 之前先运行。
# 仓库由 docker 镜像导出（docs/rounds/69-glibc-probe.md），Ubuntu 的换成 ubuntu:24.04：
#
#   c=$(docker create --platform linux/arm64 debian:trixie)
#   docker export $c | gzip -1 > target/glibc/debian-trixie.tar.gz; docker rm $c
#   scripts/dev target/linux-arm64/release/appine-run --rootfs target/glibc/debian-store \
#       --import target/glibc/debian-trixie.tar.gz
#   scripts/survey -s target/glibc/debian-store glibc/debian-install glibc/debian-c ...
export DEBIAN_FRONTEND=noninteractive
# docker export 出来的 rootfs 里 resolv.conf、hosts 是空的（docker 运行时才挂上），内核层只在
# resolv.conf 不存在时写 DNS 存根
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
[ -s /etc/hosts ] || printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n' > /etc/hosts
apt-get update -q 2>&1 | tail -2
apt-get install -y -q --no-install-recommends procps ca-certificates curl xz-utils git file locales \
    python3 python3-venv python3-pip gcc g++ make libc6-dev nodejs npm strace gdb \
    openjdk-21-jdk-headless >/tmp/apt.log 2>&1
echo "apt-get install: $?"; tail -3 /tmp/apt.log
dpkg -l | grep -c '^ii'
