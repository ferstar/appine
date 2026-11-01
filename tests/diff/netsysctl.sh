# docker-run-options: --privileged
# 网络命名空间的 sysctl（docs/rounds/177-net-sysctls.md）：新的网络命名空间里 /proc/sys/net 下
# 常用的几项的权限和默认值，写入（整数写几个改前几个、个数多了和不是数是 EINVAL、拥塞控制
# 算法要是有的、只读的写不了），以及每个命名空间各有一份。docker、compose 的 `sysctls:` 在
# 容器里写这些（原来 appine 里没有这些文件，容器起不来）。不比整个目录的列表：Linux 6.1 和
# 6.8 的项不一样。tcp_rmem 的默认上限也不比：6.1、6.8 最多 6 MB，新内核（CI 的）最多 32 MB
# （都按内存大小定）。
exec 2>&1
unshare -n sh -c '
cd /proc/sys/net
for f in core/somaxconn core/optmem_max ipv4/tcp_keepalive_time ipv4/tcp_keepalive_intvl \
    ipv4/tcp_keepalive_probes ipv4/tcp_fin_timeout ipv4/tcp_syncookies ipv4/tcp_rmem ipv4/tcp_wmem \
    ipv4/tcp_congestion_control ipv4/tcp_available_congestion_control ipv4/tcp_tw_reuse \
    ipv4/tcp_max_syn_backlog ipv4/tcp_ehash_entries ipv4/tcp_fastopen_key ipv4/ip_default_ttl \
    ipv4/ip_nonlocal_bind ipv4/ip_local_reserved_ports ipv6/bindv6only unix/max_dgram_qlen; do
    printf "%s %s: %s\n" "$f" "$(stat -c %a $f)" "$(cat $f | tr "\t" " ")"
done
w() {
    printf "%s" "$2" > "$1" 2>/tmp/err
    echo "write $1 <- $2: $? $(sed "s/.*: //" /tmp/err) -> $(cat $1 | tr "\t" " ")"
}
w ipv4/tcp_keepalive_time 600
w ipv4/tcp_rmem 8192
w ipv4/tcp_rmem "1 2 3"
w ipv4/tcp_rmem "1 2 3 4"
w ipv4/tcp_fin_timeout abc
w ipv4/tcp_congestion_control reno
w ipv4/tcp_congestion_control nosuchcc
w ipv4/ip_local_reserved_ports "8080,9000-9010"
w ipv4/tcp_ehash_entries 1
w ipv6/bindv6only 1
w unix/max_dgram_qlen 512
echo "== another namespace has its own"
unshare -n sh -c "cat ipv4/tcp_keepalive_time ipv4/tcp_congestion_control unix/max_dgram_qlen"
' | sed -E 's/ 131072 [0-9]+$/ 131072 MAX/'
