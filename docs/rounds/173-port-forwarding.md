# M9 之前：发布的端口：PREROUTING 的 DNAT 和宿主上的转发（2026-11-01）

← [M9 之前：nftables 的子集和 socket 层的 DNAT（2026-11-01）](172-nftables.md) · [索引](../milestones.md)

[容器网络的普查](169-container-network-survey.md)定下的方案 1 的第四步，也是最后一步。[上一轮](172-nftables.md)之后，podman 用 nftables 驱动时，`-p 8080:80` 只在 appine 里面经由 127.0.0.1、外层地址连得上：那是 OUTPUT 钩子上的 DNAT。别的情形还不行：

- 从 appine 外面连（手机上的浏览器开 localhost:8080、别的设备），连接到的是 podman 占着端口、从不 accept 的那个监听 socket，没有回应。
- 容器经由网关、外层地址连发布的端口，原生经过外层命名空间的 PREROUTING，appine 不执行它。
- 普查里 podman 占着端口的那个监听 socket 不在 `netstat` 里。

## 做法

- **MPTCP 的 socket**：podman 是 Go 写的。Go 1.24 起监听默认用 MPTCP（`socket(AF_INET, SOCK_STREAM, IPPROTO_MPTCP)`），宿主支持就用。podman 占端口的 socket 其实在 conmon 手上（fd 5），只是 appine 的 `/proc/net/tcp` 只列协议号是 TCP 的。现在 MPTCP 的也列：原生内核也列，它的第一个子流就是 TCP socket。低端口映射里 MPTCP 的端口也按 TCP 登记：原来按协议号 262 登记，Go 写的服务监听低端口时，别人连不上它。
- **从别的命名空间进来的**（`route`，`core/kernel/src/syscall/net.rs`）：connect、sendto 先过自己命名空间的 OUTPUT nat，再换成宿主上的地址。目的地在别的命名空间时，还要过那里的 PREROUTING nat（`netns::prerouting_nat`），DNAT 之后按那个命名空间换成宿主上的地址。"目的地在别的命名空间"有两种：
  - 目的地是那个命名空间的虚拟地址，比如容器连网关 10.88.0.1。
  - 新命名空间里经由宿主网络出去，目的地却是开机时的命名空间的本机地址，比如容器连外层地址（宿主的地址），这时进入的是开机时的命名空间。
  - 合成的包：源地址是发送方面向目的地选的地址，入口接口是目的地命名空间里面向这个源地址的接口（`iifname "br0"` 这类匹配用得上）。
  - redirect：在 PREROUTING 上换成入口接口的地址，在 OUTPUT 上换成回环地址（`nf_nat_redirect_ipv4`）。
  - meta：PREROUTING 上没有出口接口，`skuid`、`skgid` 取不到。
  - 收到的数据报的来源照上一轮换回 guest 给的地址：发送方看到的实际目的地是按发送方换算的目的地命名空间里的那个地址。
- **从 appine 外面进来的**（`core/kernel/src/forward.rs`）：
  - 哪些 socket 要转发：开机时的命名空间里监听着的 TCP socket、绑了端口的 UDP socket。对每个这样的 socket，假设一个从外面来的包，在 PREROUTING 上跑一遍：源地址 192.0.2.1，目的地是它绑的地址（通配地址换成宿主上第一个不是回环的 IPv4 地址），入口接口是那个地址所在的接口。被 DNAT 了的，宿主 fd 复制一份交给转发线程。
  - 转发线程：只碰宿主 fd，不拿 BKL。它接受连接，先绑开机时的命名空间面向目的地的那个地址（容器看到的来源是网关，与 docker-proxy 一样），再连到 DNAT 的目的地在宿主上的地址（低端口换成映射的），在两边之间双向转发，一边读完了就对另一边半关。UDP 按来源各开一个宿主 socket 转发，回答从原来那个 socket 发回来源，闲置两分钟就关掉。
  - 什么时候重新判断：guest listen、绑定 UDP 端口时；开机时的命名空间里的 nftables 规则集提交了改动时；别的命名空间新映射了低端口时（容器里的服务后起，探测时它的端口还没有映射）。socket 关掉、规则不再 DNAT 了，就不再转发；内核没了，线程关掉手上的 fd 退出。
  - podman 的端口这样就从外面连得通了。docker 开着 iptables 时，docker-proxy 的监听也会被转发（docker 在 PREROUTING 里同样有 DNAT），效果相同；关着时没有规则，仍由 docker-proxy 转发。

## 测试

- 新的差分测试组 `portfwd`。原生一侧是特权容器，数据包真的经过 bridge 和 nat。
  - 拓扑：开机时的命名空间里两个 bridge（10.60.0.1/24、10.61.0.1/24），A、B 两个命名空间各挂在一个上面，网段不同：B 的回答经由外层命名空间，NAT 才换得回来。外层命名空间里有 PREROUTING 的规则，还像 podman 那样占着 8080、从不 accept。
  - A 连网关地址和外层地址的 8080，转到 B 的 80：比 A 看到的对端，B 看到的来源和本端。
  - UDP 5353 转到 B 的 53：未连接的和连接的都比，回答的来源是 A 发往的那个。
  - `iifname != "br0"` 的规则对从 br0 进来的不 DNAT，连到外层命名空间自己的服务。
  - redirect：从网关地址、外层地址进来的都换成入口接口的地址（10.60.0.1）。
  - 外层命名空间自己连：只过 OUTPUT，不过 PREROUTING。
  - 删掉规则之后：ECONNREFUSED。
  - 写测试时发现的原生行为：A、B 在同一个网段时，B 的回答直接经 bridge 回给 A，不经过网关，NAT 换不回来，原生连不上（netavark 为此对同网段的流量做 masquerade）。所以测试用两个网段。
  - 两个执行层下都与原生逐字相同；`nftables`、`netmap` 两组照旧通过。
- 单元测试新增 2 个（`forward`）：TCP 的转发（数据双向、目的地关掉之后客户端读到结尾、丢掉 Handle 之后不再转发），UDP 的转发（回答的来源是客户端发往的那个端口）。
- 变异检查：
  - `portfwd` 组在下面每处都与原生不同：
    - 不执行 PREROUTING：B 看到的来源变成网关。连接落到外层占着 8080 的监听上，又被转发线程转给了 B。
    - 容器连外层地址时不当作进入开机时的命名空间：同上。
    - 没有入口接口：iifname 的规则判断错了。
    - PREROUTING 上 redirect 到回环地址：redirect 的那几项连不上。
    - 跨命名空间时收到的来源不换回：UDP 回答的来源不对。
  - 转发器的单元测试抓到：不半关（客户端读不到结尾）、UDP 的回答不发回。
- 从 appine 外面连：在开发容器里后台起 appine，里面用 nftables 驱动的 podman 起 `-p 8080:80` 的容器，再从容器外（开发容器里的 bash `/dev/tcp`）连 127.0.0.1:8080，拿到了容器里的页面。这个没法做成差分测试：原生一侧没有"appine 外面"。
- 普查（与原生对照）：
  - podman：防火墙 none 和 nftables 两种驱动下，`-p` 时占着 8080 的监听都与原生一样有一个，各项都与原生一致。不同的只剩容器里 ping 外层的网关（推迟项）、写 sysctl 的次数（上一轮就是这样）和随机的链名。
  - docker：与上一轮相同，两种配置下与原生一致（不同的同上）。
  - 两个普查都没有内核层报告的内部错误。
- cargo test（313 个）、clippy（含 Android）、格式、两层的全部差分测试（47 组）通过。LTP 的 syscall 测试 1012 个通过，每一项都与上一轮相同。

**推迟项**：

- 只执行 PREROUTING 和 OUTPUT 上的 nat，POSTROUTING 不执行。所以：
  - 容器经由网关连回同一个网段里发布的端口（hairpin），原生靠 masquerade，容器看到的来源是网关；这里看到的是对方容器的地址。
  - 同一个网段里原生连不上的情形（没有 masquerade 时回答不经过网关），这里连得上。
- 从外面来的连接，转不转发、转到哪里，按探测的包定，不按每个连接真实的来源：只看来源地址、入口接口的规则可能判断得不一样。转发线程接管了监听 socket 之后，guest 自己 accept 时与它抢连接：podman 的那个从不 accept；docker-proxy 抢到的同样转给容器。只转发 IPv4 的监听。
- 从外面转发进来的连接，容器看到的来源是网关（与 docker-proxy 一样），原生 DNAT 时是外面客户端的真实地址。
- 开机时的命名空间里监听通配地址的 socket，在宿主上也盖住了别的命名空间的回环地址：别的命名空间里这个端口没有监听时，连它的地址会连到这个 socket 上，原生是 ECONNREFUSED。
- 从外面来的 UDP，每个来源一个转发 socket，闲置两分钟才关；没有连接跟踪那样的超时和计数。
