# M9 之前：有状态的网卡：bridge、veth、地址和路由（2026-11-01）

← [M9 之前：容器网络的普查（2026-11-01）](169-container-network-survey.md) · [索引](../milestones.md) · [M9 之前：经由虚拟网卡的连接（2026-11-01）](171-netmap.md) →

[容器网络的普查](169-container-network-survey.md)定下的方案 1 的第一步：docker、podman 搭 bridge 网络用的 netlink 请求原来都是 EOPNOTSUPP（dockerd 报 `Failed to create bridge docker0 via netlink: operation not supported` 退出，netavark 报 `create bridge: Netlink error: Not supported`），新的网络命名空间里也只有 lo。这一轮照 Linux 6.1 记下虚拟网卡的状态：bridge、veth、dummy 的建删改，地址，路由，接口的 sysctl。做完之后两家的 bridge 网络建得起来，容器起得来，容器里 `ip addr`、`ip route` 看到的与原生相同。数据包仍然不经过这些设备：容器之间、容器与外面的流量要下一步在 socket 层按地址映射。

## 做法

动手之前在原生的特权容器里用 strace 记下 dockerd、netavark 发的每条 netlink 请求（容器命名空间里的那些 strace 不解码，另写了个小脚本按 ifinfomsg、ifaddrmsg、rtmsg 和属性解出来），差分测试的用例也先在原生上跑出样子。

- **设备表**（新的 `core/kernel/src/netdev.rs`）：每个网络命名空间（开机时的那个也是）有一张表，记着虚拟设备、经由它们的路由和 sysctl。
  - 开机时的命名空间里虚拟设备叠在宿主的接口之上：编号从宿主最大的编号往后分配（docker 的容器里 lo、eth0 之后是 3，与原生相同），名字不能与宿主的重复；宿主的接口照旧改不了（[第 158 轮](158-container-small-fixes.md)的规矩）。新的命名空间里与 lo 并列，编号从 2 起。
  - veth 的两端各在自己的命名空间里，经由弱引用找到对端；删掉一端另一端随之删除。命名空间没人用了，它的设备表随之释放，别的命名空间里对端在那边的 veth 在下一次查表时清掉（Linux 的 `default_device_exit_batch` 也删 veth 连同对端）。
- **netlink 的修改请求**（`netdev::request`，原来 `RTM_NEWLINK` 之外的都直接 EOPNOTSUPP）：
  - `RTM_NEWLINK` 新建（`IFLA_LINKINFO` 的种类 bridge、veth、dummy，别的是 EOPNOTSUPP "Unknown device type"）：名字可以含 `%d`（没给时是 `bridge%d` 这样），编号可以指定。veth 的对端照 `veth_newlink` 先登记：`VETH_INFO_PEER` 里是对端的 ifinfomsg 和属性，带 `IFLA_NET_NS_FD`、`IFLA_NET_NS_PID` 时直接建在那个命名空间里（netavark、docker 29 都这样建容器里的 eth0），没给时在发请求的这个里。之后按 ifinfomsg 改标志、挂到 `IFLA_MASTER` 上，挂不上就删掉新建的。
  - 改已有的（`RTM_SETLINK`，或者 `RTM_NEWLINK` 找到了）照 `do_setlink` 的顺序：移到别的命名空间（先停用、摘下，地址和路由都去掉，sysctl 按那边的 `default` 重来，编号在那边被占了就重新分配，名字被占了用 `IFLA_IFNAME` 改名、没给是 EEXIST；bridge 不能移，EINVAL）、MAC（要合法的单播地址，EADDRNOTAVAIL）、MTU（种类的上下限，"mtu less than device minimum" 等）、组、改名（按编号找到时；6.1 里启用着的不能改，EBUSY）、别名、标志（`rtnl_dev_combine_flags`）、master、发送队列长度。前面改了的在后面出错时也留着，与 Linux 一样。
  - 挂到 bridge 上（`do_set_master`、`br_add_if`）：bridge 不能挂到 bridge 上（ELOOP），lo 是 EINVAL，挂到不是 bridge 的接口上是 EOPNOTSUPP。bridge 的 MTU、MAC 没有由用户设过时跟着端口（端口里最小的；去掉最后一个端口时 MAC 变成全 0，与 `br_stp_recalculate_bridge_id` 一样，差分测试里原生也是）。
  - `RTM_DELLINK`：lo、宿主的接口是 EOPNOTSUPP。
- **载波和状态**照 6.1：veth 两端都启用了才有载波；bridge 有一个转发状态的端口（端口启用、有载波，STP 关着马上转发）才有，但在按端口算过状态之前（端口增删、改 MAC 时算，带着 MAC 建的 bridge 建好就算一次）载波是建出来时的"有"、状态一直是 UNKNOWN；dummy 总有载波，状态总是 UNKNOWN。启用了、没有载波的 veth 是 LOWERLAYERDOWN（`default_operstate` 看的是 iflink 是不是自己）。qdisc 在启用过之后是 noqueue，没启用过是 noop。
- **地址**：IPv4 照 `__inet_insert_ifa`：同一网段、同一地址是 EEXIST（`NLM_F_REPLACE` 时改），同一网段的第二个是 secondary，primary 排在 secondary 前面；删掉 primary 时同一网段的 secondary 一起删掉。IPv6 的地址按范围排（global 在 link 前面，同一范围新加的在前），接口关了 IPv6 时 EACCES。
- **IPv6 的自动配置**：接口启用并有载波、没有关掉 IPv6 时照 addrconf 配置：按 MAC 生成链路本地地址（EUI-64，`addr_gen_mode` 为 1 时不生成，`IFA_PROTO` 是 kernel_ll），加 fe80::/64 和 ff00::/8 的路由。没有 DAD，地址马上可用（原生上要等一会儿才不是 tentative）。接口停用时 IPv6 的地址和路由都去掉（`keep_addr_on_down` 为 0）。
- **路由**：按地址加的照 `fib_add_ifaddr`：本地路由总加（secondary 的首选源地址是 primary），显式的广播地址、网段、网段的广播在接口启用时加；接口停用时去掉经由它的路由，本地路由留着（`fib_sync_down_dev` 不动 scope 为 nowhere 的下一跳），再启用只补回按地址加的。`RTM_NEWROUTE`、`RTM_DELROUTE`：只有单播路由，下一跳要是虚拟设备、启用着（"Device for nexthop is not up"），网关要在这个命名空间里某条范围不小于 link 的路由的网段里（"Nexthop has invalid gateway"），目标的主机位不是 0 是 EINVAL，首选源地址要是本机地址；`NLM_F_EXCL`、`NLM_F_REPLACE`、`NLM_F_APPEND` 照 `fib_table_insert`。没有载波的接口上的路由带 linkdown（本地路由不带）。转储时与宿主的（或者 lo 的）路由合在一起，按表的编号和 Linux 转储的顺序排，同样的按加入的先后。
- **`ip route get`**：主表里最长前缀匹配到虚拟设备上的路由，或者不在开机时的命名空间里时，按这个命名空间的表选出口和源地址（首选源地址，没有时取出口接口上同一网段的地址，`inet_select_addr`）；开机时的命名空间里匹配到宿主的路由时照旧由宿主选。本机地址的源地址改成本地路由的首选源地址（secondary 是 primary 的）。
- **转储**：`RTM_GETLINK` 的回答里虚拟设备带 `IFLA_LINKINFO`（种类；bridge 的参数是默认值；端口带 `IFLA_INFO_SLAVE_KIND` bridge 和端口状态）、`IFLA_LINK`（veth 的对端）、`IFLA_MASTER`、`IFLA_MIN_MTU`、`IFLA_MAX_MTU`、`IFLA_CARRIER_CHANGES` 等；对端在别的命名空间里时带 `IFLA_LINK_NETNSID`，这个编号与 Linux 的 `peernet2id_alloc` 一样转储时分配，`RTM_GETNSID` 回答它（原来总是 -1，[第 129 轮](129-namespace-files.md)）。按 `IFLA_MASTER`、种类过滤的转储（netavark 用它看 bridge 上还有没有端口）带 `NLM_F_DUMP_FILTERED`。接口多时转储分成几个数据报（每个不超过 8 KiB）。
- **extack**：socket 设了 `NETLINK_EXT_ACK` 时出错的确认附上消息（iproute2 打印成 `Error: ….`），只给 6.1 里就有的那几条；`NETLINK_CAP_ACK` 时出错的确认只附上消息头。
- **ioctl**：虚拟设备的 `SIOCGIFINDEX`、`SIOCGIFNAME`、`SIOCGIFFLAGS`、`SIOCSIFFLAGS`、`SIOCGIFMTU`、`SIOCSIFMTU`、`SIOCSIFNAME`、`SIOCGIFHWADDR`、`SIOCSIFHWADDR`、`SIOCGIFTXQLEN`、`SIOCSIFTXQLEN`、`SIOCGIFADDR` 等（busybox 的 `ip link set` 用的就是这些），开机时的命名空间里按名字认出来的不交给宿主；`SIOCGIFCONF` 接在宿主的后面。
- **`/proc/<pid>/net`**、**`/sys/class/net`**：接口、路由、`if_inet6` 都含虚拟设备。`/sys/class/net` 改成按读者所在的网络命名空间（Linux 按挂载 sysfs 的进程的；容器里 runc 在新的命名空间里挂 sysfs，看到的一样）。虚拟设备的 `iflink` 是 veth 的对端，`operstate` 可以是 lowerlayerdown；bridge 有 `bridge/`（`stp_state` 等）和 `brif/<端口>`（指向 `../../<端口>/brport` 的链接），端口有 `brport/state`。dockerd 起容器时读 `bridge/stp_state`、端口的 `state`，等端口进入转发状态；原来读不到，每个容器打两条警告、多等一会儿。
- **`/proc/sys/net`**（新的 `core/kernel/src/netconf.rs`，原来整个目录都没有）：按读者所在的网络命名空间（与 Linux 一样）。
  - `ipv4/conf/`、`ipv6/conf/` 下是 `all`、`default` 和每个接口（虚拟设备的、lo 的、宿主接口的），项和默认值照 6.1 的 `devinet_sysctl_table`（33 项）、`addrconf_sysctl`（58 项）。新接口照抄 `default`；新的命名空间里 IPv4 照抄开机时的那个现在的值、IPv6 是默认值（`devconf_inherit_init_net` 为 0）。
  - 起作用的：`disable_ipv6`（去掉、恢复接口的 IPv6；`all` 上写改到每个接口）、`forwarding`（`all` 上写改到每个接口，`ip_forward` 是它的别名）、IPv4 的 `default`（改到还没单独设过这一项的接口）、IPv6 的 `mtu`（不能超过接口的 MTU）、`stable_secret`（没设过时读是 EIO）。别的只记下：appine 不转发、不收发 ARP 和路由通告。
  - `ipv4/ip_unprivileged_port_start`：bind 低于它的端口要 `CAP_NET_BIND_SERVICE`（原来固定是 1024；docker 给容器设成 0）。`ipv4/ip_local_port_range`、`ipv4/ping_group_range`（podman 默认给容器设，crun 打不开就起不了容器）、`core/somaxconn` 只记下：端口由宿主分配，ping socket 交给宿主（Android 允许所有组）。
  - 写入照 `proc_dointvec`、各项的范围（EINVAL）。

## 测试

- 新的差分测试组 `netdev`（原生一侧是特权容器，要写 `/proc/sys`）：
  - 新的命名空间里：建 bridge、veth（指定、不指定名字）、dummy，`ip link`、种类、`/proc/net/dev`；挂到 bridge 上、端口状态、`ip link show master`；按顺序启用 bridge、一端、另一端、dummy，停用一端，看各自的标志和状态。
  - 地址：同一网段的 secondary、重复的（busybox 的 ip 看错误码）、`brd +`、两个 IPv6 地址；`ip route show table all`（链路本地地址的本地路由按随机的地址排，挪到最后排好再比）。
  - 路由：经由网关、指定度量、网关不在网段里、接口停用着、重复、主机位不是 0、`append`、首选源地址、IPv6 的；`ip route get` 经由网关的、直连的、按首选源地址的、本机 secondary 地址的、IPv6 的；删除、删不存在的；停用接口之后路由的变化、再启用。
  - 改名（停用之后）、MTU 的上下限、MAC 不合法、bridge 的 MTU 跟着端口和用户设的；各种错误（重名、不认识的种类、lo 挂到 bridge、bridge 挂到 bridge、删 lo、删不存在的、移走 bridge、lo 移到它所在的命名空间、删不存在的地址、名字太长）。
  - sysctl：两族的目录、几项默认值、写入和错误、`ip_forward`、`disable_ipv6` 去掉和恢复链路本地地址、关了 IPv6 时加地址、IPv6 的 `mtu` 和 `addr_gen_mode` 的范围、`stable_secret`、`ping_group_range`、`ip_local_port_range` 与 `ip_unprivileged_port_start` 的互相约束，以及普通用户 bind 80、1500 随后者变化。
  - 停用接口时 IPv6 的地址和路由去掉、再启用又配置。
  - 另一个命名空间：veth 的对端直接建在那边（`@if<n>`、`link-netnsid 0`）、两边各自启用、在那边配地址和默认路由；把 veth 移过去；名字冲突时 EEXIST、用新名字移过去；那个命名空间的进程退出之后，对端在那边的 veth 都没了。
  - 删除 veth（连同对端）、对应的 sysctl 目录没了、bridge 上按地址加的路由带 linkdown、bridge 没有端口之后 MAC 是全 0。
  - 开机时的命名空间（docker 容器的）：建 bridge、配地址、启用，`ip link`、地址、路由（宿主的和它的）、`ip route get`、sysfs 的 `operstate` 和 `type`、`if_nameindex`、`/proc/net/dev`、sysctl 目录；挂上 veth 之后 `bridge/stp_state`、`brport/state`、`brif/<端口>` 的链接和经由它读的状态、`iflink`、`operstate`；删掉之后都没了。
  - 写的时候碰到的原生上的时序：IPv6 地址在 DAD 完成前是 tentative，即使 `accept_dad` 是 0 也要等一个随机的延迟（最多 1 秒）；bridge 经由 linkwatch（最多晚 1 秒）才知道端口的载波变了。这两处等一下再看。
  - 两个执行层下都与原生逐字相同。
- 单元测试 4 个（`netdev`）：对端在别的命名空间的 veth、命名空间释放之后清掉；bridge 的载波、MTU、MAC 跟着端口；地址和按地址加的路由、路由的错误、停用和启用；sysctl 的 `default`、`all` 和 `disable_ipv6`。`netconf` 1 个。
- 变异检查：veth 的载波不看对端、接口停用时不去掉经由它的路由、secondary 的本地路由的首选源地址用它自己，`netdev` 组在状态、路由表、`ip route get` 那几处与原生不同。
- 普查（[第 169 轮](169-container-network-survey.md)的两个负载，与原生对照）：
  - docker（`"iptables": false`）：docker0 的地址、状态（没有容器时 NO-CARRIER）、路由，默认网络的容器里 eth0 的地址、默认路由、resolv.conf，ping 外层的网关（原生关着 iptables 时也不通）都与原生逐行相同。容器之间按 IP 互访是 "Network unreachable"、外层访问容器的 IP 超时（下一步）；`-p 8080:80` 的容器起不来：dockerd 给监听 socket 装丢包的 BPF 过滤器（`SO_ATTACH_FILTER`），结构里的指针没有换成宿主的，EFAULT（下一步的 socket 层一起做）；自定义网络按名字解析不了：内置 DNS 要在容器里写 nat 规则，iptables-nft 用的 `NETLINK_NETFILTER` 没有（第 3 步）。iptables 开着时 dockerd 初始化 nftables 失败、起不来（同上）。 → 都通了（M9 之前：nftables 的子集和 socket 层的 DNAT）dockerd 的警告里还有发免费 ARP 要 AF_PACKET、conntrack 和 IPsec 的模块（`NETLINK_NETFILTER`、`NETLINK_XFRM`）。
  - podman（`firewall_driver = "none"`）：默认网络的容器里 eth0 的地址、带度量 100 的默认路由、resolv.conf、ping 外层网关的结果与原生逐行相同；起初 crun 打不开 `/proc/sys/net/ipv4/ping_group_range`（podman 默认给有自己网络命名空间的容器设它），补上之后才起得来。容器之间、外层到容器同 docker；自定义网络的容器起不来：aardvark-dns 要在开机时的命名空间里绑网关地址 10.89.0.1:53，宿主上没有这个地址（EADDRNOTAVAIL，下一步的地址映射）；`-p` 时原生由 podman 占着端口（防火墙关着，连上去没有回应），appine 里没有那个监听 socket（第 4 步再看）。nftables 驱动时 netavark 调 nft 失败（第 3 步）。
  - 两个普查都没有内核层报告的内部错误。
- cargo test（307 个）、clippy（含 Android）、格式、两层的全部差分测试通过。LTP 的 syscall 测试 1012 个通过（与上次相同）：clone09 这次通过，futex_wait05 这次因为睡得太久失败（计时的用例，宿主有负载时时过时不过）。

**推迟项**：

- 数据包不经过这些设备：容器之间、外层到容器、容器到网关的连接都还到不了（"Network unreachable"），容器里的程序只能用 lo 和宿主的网络（下一步：socket 层的地址映射）。 → 在 socket 层按地址映射连通了（M9 之前：经由虚拟网卡的连接）；数据包仍不经过它们
- 没有 DAD，IPv6 地址加上就可用；没有路由通告、`autoconf`、临时地址。
- 只有 bridge、veth、dummy 三种；vlan、macvlan、vxlan、tun 等是 EOPNOTSUPP "Unknown device type"。没有 AF_PACKET（dockerd 发免费 ARP 时只打一条警告）。
- 只有单播路由，表的编号不大于 255；没有多路径、`onlink`、blackhole 这些；没有自定义规则（`ip rule`）。开机时的命名空间里只能经由虚拟设备加路由，删宿主的路由仍是 EOPNOTSUPP。
- 新命名空间里的 lo 不能改名（Linux 停用着时可以）；6.1 里启用着的接口不能改名（6.2 起可以）；`promote_secondaries`、`keep_addr_on_down` 只记下（前者不提升，后者关接口时仍去掉地址）。
- bridge 的参数（STP、VLAN 过滤、组播）只有默认值，`IFLA_INFO_DATA` 接受但不起作用；bridge 的 FDB、端口的参数没有。
- `/sys/class/net` 按读者的网络命名空间，不是挂载 sysfs 的进程的；`/proc/sys/net` 下只有上面这些，`ipv4/` 下别的（`tcp_*` 等）、`netfilter/`、`unix/` 都没有。 → `core/`、`ipv4/`、`ipv6/`、`unix/` 下的补齐了（只记下值，M9 之前：网络命名空间的 sysctl 补齐）；`netfilter/` 仍没有；net 的 sysctl 文件属于 root，不是命名空间属主里的 root（用户命名空间里的 root 写不了）。
- `ping_group_range` 只记下，不限制建 ping socket。
- `RTM_NEWNSID` 仍是 EOPNOTSUPP。
