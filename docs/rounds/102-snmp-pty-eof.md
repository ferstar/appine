# M9 之前：`/proc/net/snmp` 和输入结束时的 EOF（2026-10-29）

← [M9 之前：接口和地址的 netlink 消息（2026-10-29）](101-link-addr.md) · [索引](../milestones.md)

审推迟项时挑出来的两个便宜的兼容性缺口：

- **`/proc/net/snmp`、`netstat`、`arp`**（[网络信息的缺口](78-net-info-gaps.md)、[路由表](98-routes.md)的推迟项）原来没有。iproute2 的 `ss -s` 读 `snmp` 里的 CurrEstab，打印 `ss: get_snmpstat: No such file or directory`；net-tools 的 `netstat -s`、iproute2 的 `nstat`、sysstat 的 `sar -n TCP,UDP` 也读这两个文件。
- **`appine-run --pty` 的 stdin 结束**（[M3b](05-m3b.md) 的推迟项）：桥接线程不再读，程序却收不到 EOF。`printf 'make\n' | appine-run --pty sh -i` 跑完命令之后 shell 一直等输入，不退出。

- **改动**：
  - `/proc/net/snmp`（`core/kernel/src/net.rs`）：各组的字段和顺序照 Linux 6.1 的 `snmp_seq_show`（6.2 起 Icmp 在 OutErrors 后面插了两个限速的计数，6.3 起 Ip 末尾多了 OutTransmits，这里都没有）。计数取自 `core/kernel/src/traffic.rs` 新增的协议计数，与接口的计数在同一个地方数：
    - IP：IPv4 收发的包数（与接口的计数一样按载荷估计）；Forwarding 是 2（不转发），DefaultTTL 是 64。
    - ICMP：ping socket 的回显请求和回答；IcmpMsg 与 Linux 一样只列出不为 0 的类型。
    - TCP：RTO 的几项是 Linux 的初值。ActiveOpens 在宿主发出 SYN 时数（connect 成功、正在连接、马上被拒绝）；AttemptFails 在得知这次连接失败时数（阻塞的 connect 的结果、非阻塞时再 connect 一次的结果、SO_ERROR 读到 ECONNREFUSED），socket 上记着发起的连接还没有数过失败，不会数两次；PassiveOpens 数 accept 到的连接；CurrEstab 是现在 ESTABLISHED 的 guest TCP socket 数（与 Linux 一样不含 CLOSE_WAIT）；InSegs、OutSegs 是估计的包数。
    - UDP：IPv4 收发的数据报，IPv6 socket 上 IPv4 映射的地址也算（Linux 的 `udpv6_sendmsg` 把它交给 IPv4），偷看的不算。
    - 错误、重传、复位、分片这些内核层看不到，是 0。
  - `/proc/net/netstat`：TcpExt、IpExt、MPTcpExt 三组，字段照 6.1。只有 IpExt 的 InOctets、OutOctets（IPv4 的载荷字节数）和 InNoECTPkts（收到的包都当作不带 ECN 标记）不是 0。
  - `/proc/net/arp`：只有表头，与 `ip neigh` 的空表一致。
  - `InetSocket` 记下协议号：建 socket 时 guest 给的，给的是 0 时按类型取 TCP、UDP；accept 到的跟着监听的。
  - 桥接线程（`core/kernel/src/terminal.rs`）：stdin 读到结束（或者出错）之后不再读，等程序把已经送进去的输入读完（规范模式下没有完整的行，非规范模式下队列空了），往控制台送一个 termios 里的 VEOF（被禁用时不送），只送一次。与 util-linux 2.35 起的 `script` 相同：它在 stdin 结束时等从设备读空了再写一个 ^D。不当作挂断：发 SIGHUP 会把还在跑的命令杀掉，`printf 'sleep 3; echo done\n'` 就看不到 done。
- **测试**：
  - cargo test：`tests/guest/traffic.c` 加上 snmp、netstat 的检查：格式（每组名字一行、数值一行，个数相同）和常数项；回环上的一个连接让 ActiveOpens、PassiveOpens 各加 1、CurrEstab 加 2，关掉之后回到原来的数；IP 的包数和字节数与 lo 的计数相同，TCP 的段加上 UDP 的数据报等于 lo 的包数；阻塞、非阻塞的连接被拒绝各算一次 ActiveOpens 和 AttemptFails；ping 的回显。rootfs 测试新增：`--pty` 下经由管道给 `sh -i`、`cat` 输入，输入结束之后它们自己退出。
  - 差分测试 compat 组：两个文件的组名、各组名字和数值的个数、各版本都有的几个常数项；回环上 IPv4、IPv6 各一个连接、关掉它们、阻塞和非阻塞地连一个没人监听的端口、IPv4 和 IPv4 映射的数据报（IPv6 的不算），前后 TCP、UDP 几项的变化。字段列表随内核版本变化，不比；IP 的包数、TCP 的段 Linux 还算握手和 ACK，不比。与原生相同。
  - Debian 的仓库里：`ss -s` 显示 `estab 2`，`nstat -a` 列出连接、段、数据报和字节数。
  - 变异检验：去掉送 VEOF（rootfs 用例 30 秒超时）、去掉 PassiveOpens 的计数、去掉 SO_ERROR 那里的失败计数，cargo test 都失败。变异检验最后一次 `cargo test --release` 留下的是变异过的 release 二进制，之后只跑了 clippy，第一次差分测试用的就是它，compat 组的 AttemptFails 少一次；重编之后通过。
  - cargo test 232 个、clippy（含 Android 目标）、两层的差分测试都通过。LTP 958，与[上一轮](101-link-addr.md)相同：futex_wait05 由失败变成通过，mmap21_02 由通过变成失败（`open(/dev/shm/ltp_mmap21_2)` 得到 EEXIST），都是[已知的偶发失败](../milestones.md#已知的偶发失败)。

**推迟项**：

- IP 的包数、TCP 的段与接口的计数一样是估计的，握手、ACK、重传都不算；Ip 的 InReceives 总是等于 InDelivers。
- 非阻塞的连接因为超时、不可达而失败，guest 只从 poll 和 SO_ERROR 得知时，AttemptFails 不数（SO_ERROR 的 ETIMEDOUT 也可能来自已经建立的连接，分不出来）；EstabResets、OutRsts、RetransSegs、InErrs 都是 0。
- PassiveOpens 在 accept 时数，Linux 在握手完成时数：还在监听队列里、没有被 accept 的连接看不到。
- ICMP 只有 ping socket 自己的回显：ping 回环地址时 Linux 还数本机收到请求、发出回答（InEchos、OutEchoReps），这里答复的是宿主，不算。IPv6 的 ICMP、UDP 在 `/proc/net/snmp6` 里，没有这个文件。
- DNS 存根的查询不经过宿主 socket，不算 UDP 的数据报（与接口的计数一样）。
- `/proc/net` 仍没有 `snmp6`、`protocols`、`dev_mcast`、`igmp` 等（原生的容器里 57 个文件，这里 18 个）。
- VEOF 只送一次：`printf 'cat\necho after\n' | appine-run --pty sh -i` 里 EOF 被 cat 读走，shell 之后仍然等输入（`script` 也这样）。
