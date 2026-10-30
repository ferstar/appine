# M9 之前：网络命名空间（2026-10-30）

← [M9 之前：UTS 和 IPC 命名空间（2026-10-30）](121-uts-ipc-namespaces.md) · [索引](../milestones.md) · [M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱（2026-10-30）](123-bwrap.md) →

让 bubblewrap 能用的第七轮。Codex 默认的沙箱带着 `--unshare-net`：命令在只有 lo 的网络命名空间里运行，bwrap 先用 netlink 给 lo 配上 127.0.0.1/8、把它打开（`loopback_setup`，要收到确认）。原来 `CLONE_NEWNET` 是 EPERM。

appine 的 TCP、UDP、ping socket 都是宿主 socket，宿主上没有可用的网络命名空间，所以这里不另造协议栈，而是在系统调用的边界上换算地址（`core/kernel/src/netns.rs`）：

- **网络命名空间**（`crate::nsproxy::NetNs`）：与 UTS、IPC 命名空间一样按线程记。开机时的那个就是宿主的网络，什么都不换。新建的分到宿主回环网段里的一段 127.a.b.0/24（a、b 是 1 到 65535 里没有被还在用的命名空间占着的编号），里面有一个 lo：一开始关着、没有地址（Linux 的 `loopback_net_init`），打开时没有 IPv4 地址就配上 127.0.0.1/8、并且有 ::1（`inetdev_event`、addrconf）。
- **socket 记下建立时所在的命名空间**（之后 setns 到别处也还在那个命名空间里，与 Linux 的 `sock_net` 一样），新命名空间里的 socket 的地址这样换：
  - guest 的 127.0.0.x 是宿主的 127.a.b.x；绑定通配地址时宿主上绑 127.a.b.1，getsockname 照旧报告通配地址，连接之后报告实际的。
  - AF_INET6 的 ::1 和 :: 换成 IPv4 映射的 ::ffff:127.a.b.1，`IPV6_V6ONLY` 只记在 guest 一侧、不设到宿主 socket 上；反过来 ::ffff:127.a.b.1 报告成 ::1（IPv4 的客户端连到 IPv6 的通配地址时 Linux 报告 ::ffff:127.0.0.1，这里分不出来）。
  - 127.0.0.0/8 总能绑定（与 Linux 上看到的一样，lo 关着、没有地址时也行）；别的地址绑定是 EADDRNOTAVAIL。connect、sendto 要 lo 打开、目的地所在的网段配在 lo 上，否则 ENETUNREACH；连 ::1 时没有 ::1 是 EADDRNOTAVAIL（选不出源地址）。
  - 没有绑定的 socket 在 connect、sendto、listen 之前先在宿主上绑到 127.a.b.1（Linux 自动绑定的是命名空间里的通配地址，宿主上绑通配地址就到了宿主的所有接口上）。
  - recvfrom、recvmsg、accept、getpeername、`/proc/net/tcp` 这些出口的地址都换回来。DNS 存根在新命名空间里不用（127.0.0.53 在里面也是 lo 上的地址，没有人监听），流量不计入宿主接口的计数，TCP 的 `/proc/net/snmp` 计数也不数。
  - 1024 以下的端口改成看网络命名空间的属主里的 `CAP_NET_BIND_SERVICE`（原来是 `capable()`），检查在地址换算之后（Linux 的 `inet_bind` 先报 EADDRNOTAVAIL）。
- **lo 的配置**：netlink 的 `RTM_NEWADDR`、`RTM_DELADDR`（IPv4 地址、::1；别的 IPv6 地址是 EOPNOTSUPP）、`RTM_NEWLINK`/`RTM_SETLINK` 的 `IFF_UP`，接口 ioctl 的 `SIOCSIFFLAGS`、`SIOCSIFMTU`，都要在命名空间的属主里有 `CAP_NET_ADMIN`（EPERM）；`NLM_F_EXCL` 时已有的地址是 EEXIST，别的接口是 ENODEV。开机时的命名空间里这些请求照旧不支持（宿主的接口改不了）。
- **只看得到 lo**：netlink 的 `RTM_GETLINK`、`RTM_GETADDR`、`RTM_GETROUTE`（local 表按 lo 的地址合成）、接口 ioctl（`SIOCGIFINDEX`、`SIOCGIFNAME`、`SIOCGIFFLAGS`、`SIOCGIFADDR`、`SIOCGIFNETMASK`、`SIOCGIFMTU`、`SIOCGIFHWADDR`、`SIOCGIFTXQLEN`、`SIOCGIFCONF`）都按 socket 所在的命名空间，if_nameindex、getifaddrs、`ip` 因此只看到 lo。关着的 lo 的 qdisc 是 noop、状态是 DOWN（原来回环接口总报告 noqueue、UNKNOWN）。`/proc/<pid>/net/*` 按那个进程所在的命名空间：dev、if_inet6、route 只有 lo，tcp、udp、unix 这些只列建在里面的 socket，计数都是 0。
- **AF_UNIX**：抽象地址按网络命名空间分开（Linux 的 `unix_find_abstract`），自动绑定的也是；路径地址不分。socketpair 和连接时新建的服务端一端在连接方的命名空间里（`unix_stream_connect`）。
- **新建和进入**：clone、unshare 的 `CLONE_NEWNET` 要在（可能是新建的）用户命名空间里有 `CAP_SYS_ADMIN`，新命名空间属于它，所以 root 建的用户命名空间里新建的网络命名空间可以配 lo；setns（`netns_install`）要在它的属主和自己的用户命名空间里都有 `CAP_SYS_ADMIN`。`/proc/<pid>/ns/net` 按线程报告。
- 顺带：`/proc/sys/user/max_*_namespaces` 可以写了（0 到 INT_MAX，只记下，仍不实际限制）：LTP 的网络命名空间测试一开始要改写它。
- **测试**：
  - 单元测试 `loopback_addresses_map_to_the_namespace_range`（lo 关着、打开之后的绑定和连接，IPv4、IPv6 两个方向的换算，外面的地址，开机时的命名空间不换）、`netlink_configures_lo`（bwrap 的两条请求、EEXIST、别的接口、没有能力）。
  - 差分测试 `namespaces` 组加上（docker 一侧另加 `NET_ADMIN`，appine 里 root 本来就有）：busybox 的 `unshare -n` 里 `ip link show`、`ip -o addr` 在 lo 打开前后，`ip route show table local`，`/proc/net/dev`，ping 外面的地址是 Network unreachable；Python 里新命名空间的编号、if_nameindex、if_nametoindex，lo 关着时的绑定和连接，普通用户 netlink 配 lo 是 EPERM，bwrap 的 `loopback_setup` 两条请求都确认、再加一次是 EEXIST，回环上的 TCP 两端的地址和数据、通配地址、IPv6 的 :: 和 ::1、UDP 和自动绑定，外面的 TCP、UDP 都到不了、绑不上，外面监听的回环端口和抽象地址在里面连不到、同名的抽象地址在里面能绑，`/proc/self/net/dev` 只有 lo、tcp 里有自己的 socket 没有外面的，setns 回原来的命名空间之后新建的 socket 连得上外面的端口、原来的 socket 照常能用，普通用户 unshare 是 EPERM，root 建的用户命名空间里的网络命名空间（exec 之后的 `ip` 没有能力，与原生一样配不了 lo）。两个执行层下都与原生相同。
  - 变异检验（第一层的 `namespaces` 组）：出口的地址不换回来，看到的是宿主上的 127.a.b.x；抽象地址不分命名空间，里面连上了外面的、绑不上同名的；netlink 配 lo 不查能力，普通用户也配得上。替没有绑定的 socket 先绑到 127.a.b.1 这一步从里面看不出来（宿主替它绑的通配地址报告出来也是通配地址），没有用例能查。
  - cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试全量 989 个通过（上次 987，那次 mmap21_02 偶发失败）：`scripts/ltp` 给 LTP 的内核配置加上 `CONFIG_NET_NS`，setsockopt05 由跳过变成通过（它给新命名空间里的 lo 设 MTU 再经由回环发 UDP）；bind06、sendto03、setsockopt06–09 仍跳过，原因由缺 `CONFIG_NET_NS` 变成没有 AF_PACKET；sendmsg03 由跳过变成失败（见推迟项）。

**推迟项**：

- 127.0.0.0/8 里只有 127.0.0.0/24 换得了（宿主上每个命名空间只有一段 /24），别的回环地址绑不上、连不到；lo 上配的别的 IPv4 地址只记下、到不了；IPv6 只有 ::1。
- 隔离只在 guest 一侧：宿主上别的进程（包括开机时的命名空间里的 guest 进程）连 127.a.b.x 连得到新命名空间里的服务；新命名空间里绑定的端口和宿主上绑通配地址的端口冲突。
- IPv4 的客户端连到 IPv6 的通配地址时，服务端看到的对端是 ::1，Linux 是 ::ffff:127.0.0.1；`IP_PKTINFO` 这些控制消息里的地址不换。 → raw ICMP socket 的 `IP_PKTINFO` 换了（M9 之前：raw ICMP 的模拟）；别的 socket 的控制消息、IPv4 客户端的对端地址仍不换
- 没有 AF_PACKET、veth、tun 这些，新命名空间里除了 lo 建不出别的接口；sysfs 里的 `/sys/class/net` 仍是宿主的接口（Linux 按挂载 sysfs 的进程的命名空间）。
- raw ICMP socket 是用 ping socket 模拟的，`IP_HDRINCL` 设不上，busybox 的 ping 收不到回答（原来就这样，开机时的命名空间里也是）；LTP 的 sendmsg03 因此由跳过变成失败。 → busybox 的 ping 收不到回答已修好（M9 之前：raw ICMP 的模拟）；`IP_HDRINCL` 仍设不上，sendmsg03 仍失败
- 新命名空间里 lo 上的流量不计数，`/proc/net/snmp`、`netstat` 都是 0。
- 命名空间文件不让网络命名空间活着（与[挂载命名空间](118-mount-namespaces.md)的推迟项相同）。
