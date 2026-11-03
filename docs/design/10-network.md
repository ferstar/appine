# 10. 网络

| 子项 | 做法 |
|---|---|
| TCP/UDP | 透传给宿主：guest 的 file 对象内部持有一个宿主非阻塞 socket，地址、选项和控制消息按字节原样传递。阻塞在内核层的等待机制里模拟 |
| ICMP | guest 的 raw ICMP socket 在宿主上是同一协议的 ping socket（不需要特权），`SO_TYPE` 照样报告 `SOCK_RAW`（`core/kernel/src/rawicmp.rs`）。每个网络命名空间一个汇集点：ping socket 和设过 TTL 的 UDP socket（traceroute）在宿主上打开 `IP_RECVERR`，到了的报文和 ICMP 错误拼成 raw socket 看到的样子分给每个 raw socket。只能发 echo 请求；别的协议的 raw socket 与 Android 上一样是 EPERM |
| 透明代理 | `SO_ORIGINAL_DST` 照 Linux 的 `getorigdst` 回答（`core/kernel/src/origdst.rs`）：nat 在 connect 时由 socket 层做，所以 connect 把被 nat 换过目的地址的连接按宿主上的四元组记下原目的地址，accept 时领走。`SO_MARK` 只记在 socket 上、带进 nftables，不设到宿主 socket（App 没有这个权限）。没有 TPROXY |
| `NETLINK_ROUTE` | 在进程内模拟，接口和地址取自宿主的 `getifaddrs`（targetSdk 37 下 bionic 仍能列出，只是没有 MAC），消息与 Linux 逐字节相同。邻居表总是空的。改不了宿主的接口和路由（什么也不改的请求成功，比如 dockerd 拉起已经起着的 lo；要改的是 `EOPNOTSUPP`）。虚拟网卡见下面的"网络命名空间" |
| 路由表 | `core/kernel/src/route.rs` 按 Linux 没有自定义规则时的样子拼出主表和本地表。主表是宿主的路由：Android 上 App 读不了宿主的路由表，`TerminalService` 用 `NetworkCallback` 把默认网络的 `LinkProperties.getRoutes()` 经由 `Machine::set_routes` 交进来；别的宿主上读宿主的 `/proc/net/route`。`ip route get` 的源地址由宿主选（对目标 connect 一个 UDP socket） |
| 别的 netlink 协议 | `NETLINK_GENERIC` 只有 `nlctrl`；`NETLINK_NETFILTER` 见下面的 nftables；`NETLINK_KOBJECT_UEVENT` 建得了、什么也收不到。别的（审计、`SOCK_DIAG`……）与没有编进内核的子系统一样是 `EPROTONOSUPPORT` |
| DNS | `resolv.conf` 写 `nameserver 127.0.0.53`。socket 层截下发往这个地址 53 端口的 UDP 数据报，交给 `android_res_nsend`，这样系统的私人 DNS、VPN 和按网络分流设置都能生效；Linux 开发宿主上转发给宿主 `/etc/resolv.conf` 里的服务器。回答放在 socket 自己的队列里 |
| 1024 以下的端口 | App 没有 `CAP_NET_BIND_SERVICE`，Google Play 系统更新只放开了 22、80、443 等少数几个（`core/kernel/src/portmap.rs`）。guest 绑低端口时宿主 socket 先试原来的端口，不让或者被手机上别的 App 占着时依次试 `+ 10000`、`+ 20000`、`+ 30000`，再不行让宿主分配；guest 一侧看不出来（getsockname、accept、recvfrom、`/proc/net` 都是原来的端口，连本机上 guest 绑着的这些端口时换成宿主上的）。换了端口时写一条内核日志，App 第一次看到非回环地址上的监听时发通知（§12 第 7 条）。`MachineOptions::low_port_offset` 为 0 时不映射，`always_map_low_ports` 让宿主绑得了时也换（开发宿主上测映射用） |
| 回环地址 | 透传给宿主，Android 上的浏览器能直接访问 guest 里起的服务。代价是设备上的其他 App 同样能访问，需要写进文档（真机待确认） |
| AF_UNIX | 进程内模拟（`core/kernel/src/unix.rs`）。`SCM_RIGHTS` 传递的是 guest 的打开文件，`SO_PEERCRED` 返回 guest 的 pid 和 uid；只经由在途消息互相引用的 socket 由垃圾回收释放 |
| `/proc/net/*` | 遍历所有进程的 fd 表合成，地址和状态向宿主 socket 查询，不读宿主的 `/proc/net`（Android 10 起 App 读不了）。内核层看不到的计数（错误、重传）是 0 |
| 流量计数 | 内核层自己数 guest 的 socket 收发的载荷（`core/kernel/src/traffic.rs`），`/proc/net/dev`、sysfs 的 `statistics/`、netlink 和 `/proc/net/snmp` 报告同样的数：Android 上 App 读不到宿主的计数，宿主的计数里也有别的 App 的流量。接口按 socket 的本端地址认 |
| tun/VpnService | v1 不做 |

**网络命名空间**（`crate::netns`，[kernel/process 的 map](../../.claude/maps/kernel/process.md)）：开机时的那个就是宿主的网络。数据包不经过任何虚拟设备，一切在 socket 层换算：

- 新建的里面一开始只有 lo。socket 仍是宿主 socket，地址在边界上换算：guest 的 127.0.0.x 是宿主上给这个命名空间的 127.a.b.x，没有绑定的 socket 在 connect、sendto、listen 之前先绑到 127.a.b.1。
- **虚拟网卡**（`core/kernel/src/netdev.rs`）：bridge、veth、dummy 的建删改、地址、路由、接口的 sysctl 照 Linux 6.1 按命名空间记，netlink、ioctl、`/proc`、`/sys` 都看得到。虚拟地址在 socket 层换成宿主上的回环地址；connect、sendto 按目的地址找到它所在的命名空间，经由默认路由出去的交给宿主网络，反过来按"谁在看"换回。
- **nftables**（`core/kernel/src/nft.rs`）：表、链、规则、集合照 Linux 记在每个网络命名空间里，格式照 6.8，iptables-nft 能用。connect、sendto 时执行发送方 OUTPUT 上的 nat 链，目的地在别的命名空间时再执行那里 PREROUTING 上的 nat（外层的 REDIRECT、DNAT 能透明代理、劫持容器的连接和 DNS）；同时执行沿途经过的过滤链（OUTPUT、FORWARD、INPUT），拒绝的马上返回错误码，丢弃的照 Linux 的重传超时。POSTROUTING 只记下。没有 conntrack，计数器总是 0。
- **端口**：新命名空间里显式绑的端口在宿主上另选（外层占着同一个端口时也绑得上），监听登记在命名空间里，连过去没有对得上的就是 ECONNREFUSED。
- **从外面连进来**：开机时的命名空间里被 PREROUTING DNAT 的监听交给宿主上的转发线程（`core/kernel/src/forward.rs`），从 appine 外面连进来的转给 DNAT 的目的地（podman、docker 的 `-p`），容器里看到的是客户端真实的地址。

**I/O 多路复用**：每个 guest file 对象实现一个 `poll` 操作，宿主 fd 和进程内对象统一在 [kernel/process 的 map](../../.claude/maps/kernel/process.md) 的等待机制里处理。

- **epoll** 在进程内实现，等待时把关注的宿主 fd 和线程自己的唤醒事件放进同一次宿主 `poll`，关注的进程内文件登记它们的唤醒对象（[kernel/process 的 map](../../.claude/maps/kernel/process.md)）。没有用宿主 epoll 实现 guest 的 epoll，fd 很多时再换。
- **`EPOLLET`**：与 Linux 一样，某一位从未就绪变成就绪，或者那个方向又来了事件时触发，即使本来就已经就绪（tokio、Go 的 netpoller 靠这一条）。
  - 进程内的对象每来一次事件记一次"数据代数"，与 Linux 唤醒等待队列的时机对应。
  - 宿主 socket 上的数据不经过内核层：有边沿触发的关注时，给它建一个只关注它的、边沿触发的宿主 epoll，宿主每来一次数据它就可读一次。
  - 程序在一个方向上用完数据之后，这个方向上次报告过的位作废；等待时不等报告过、仍然就绪的位，免得宿主 poll 立刻返回、变成忙等。
  - 嵌套的 epoll：内层的数据代数是各个关注来过事件的次数之和，外层边沿触发时按它再触发。
- **`EPOLLEXCLUSIVE`**：一个边沿只报告给一个 epoll。等待者不自己等独占关注的文件，登记在这个文件的独占门上，由定时器线程替它们看，只叫醒排在最前面的一个。
- **信号驱动的 I/O**（`O_ASYNC`、`F_SETOWN`、`F_SETSIG`）：由定时器线程检查（[kernel/process 的 map](../../.claude/maps/kernel/process.md)），与 `EPOLLET` 用同一个数据代数。
