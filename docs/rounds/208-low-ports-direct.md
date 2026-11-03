# M9：宿主让绑的低端口就绑原来的（2026-11-03）

← [M9：内核 bug 的报告带上出错之前的内核日志（2026-11-03）](207-report-kernel-log.md) · [索引](../milestones.md)

[设置页](190-settings-default-alpine.md)那一轮的推迟项：Android 17（API 37）的 16K 模拟器上 App 自己就能绑 22，`lowPortsMoveToHighPortsOnTheHost` 的前提断言失败，原因没查。第 207 轮之后筛推迟项时查到了原因：Google 从 2026 年 5 月的 Google Play 系统更新起放开了一批 1024 以下的端口，App 没有 `CAP_NET_BIND_SERVICE` 也能绑（[heise](https://heise.de/-11314477)、[Android Authority](https://www.androidauthority.com/p-3672981/) 的报道，来源是 Issue Tracker 里 Google 工程师的回复）：

- TCP：20、21、22、23、80、443、445、515、631（FTP、SSH、Telnet、HTTP、HTTPS、SMB、打印）；
- UDP：319、320（PTP）、443（QUIC）；
- 要 Android 13 以上、内核 5.15 以上。做在 Connectivity 的 Mainline 模块里（eBPF 的 bind 钩子），随 Play 系统更新到达，不用升级系统；Android 17 的预览版先放开了 UDP 319、320。

在两个模拟器上用 `adb shell` 的 `toybox nc -l` 逐个试（shell 用户同样没有 `CAP_NET_BIND_SERVICE`，两边的 `ip_unprivileged_port_start` 都是 1024）：

| | API 34（Pixel 4a，内核 6.1，Connectivity 模块 340818080） | API 37（Qenix 16K，内核 6.12，370399999） |
|---|---|---|
| TCP 20、21、22、23、80、443、445、515、631 | 都绑不了 | 都绑得了 |
| UDP 319、320、443 | 都绑不了 | 都绑得了 |
| TCP 25、53、79、319，UDP 22、53、123 | 绑不了 | 绑不了 |

与那份名单一个不差，190 篇的现象就是它。名单按协议分：22 只放开了 TCP。

[第 156 轮](156-low-ports.md)定的是总是映射：开发虚拟机和手机走同一条路，测得到。有了这个更新的手机上，guest 的 sshd 本来可以就用 22，却被换到 10022：别的设备要连 10022，程序自己打印、配置文件里写的却是 22，用户得看通知。改成宿主让绑原来的端口就绑原来的。

## 做法

- **先试原来的端口**（`core/kernel/src/portmap.rs` 的 `bind`）：宿主回 EACCES（不在放开的名单里、没有这个更新的系统）或者 EADDRINUSE（手机上别的 App 占着）时，再照原来的顺序换：同一个端口另一个协议换到的宿主端口，`+ 10000`、`+ 20000`、`+ 30000`，宿主分配。换过的端口还是 EACCES、别的错误照旧直接返回。
- **名单不写进 App**：每次直接试，系统以后再放开、厂商改了都跟着变，试不成只多一次 bind。
- **绑上原来的端口也登记**，宿主端口就是它自己：同一个端口还有 socket 绑着、刚释放不到 2 分钟时仍先用它，guest 自己的冲突、TIME_WAIT 照 Linux 是 EADDRINUSE，不会把宿主的 EADDRINUSE 当作别的 App 占着、换到 10022 去（第 156 轮的第一版就因为这个被差分测试抓到过）。getsockname、`/proc/net`、连本机的换算原样照用，换出来还是原来的端口。
- **只放开了一个协议的端口**：TCP 22 绑原来的之后，guest 的 UDP 22 先试 22（EACCES），TCP 那边的宿主端口也是 22、已经试过，接着就是 10022。
- **内核日志和通知**：没换端口时不写 "port 22/tcp is 22 on the host"。监听事件照常报告，宿主端口与 guest 端口相同，App 原来就有这种情况的说法（"同一网络里的其他设备连得上它"）；换了端口时的说法原来是"App 用不了 1024 以下的端口"，现在不对了，改成"这台手机上的 App 用不了端口 79"（被占着也是"用不了"）。
- **开发用的宿主**：以 root 运行、在 docker 容器里（`ip_unprivileged_port_start` 是 0）时同样绑原来的端口，`appine-run` 里 guest 绑 79 就是宿主的 79。要在这样的宿主上测映射，加了 `MachineOptions::always_map_low_ports`（`appine-run --always-map-low-ports`），照旧总是换。差分测试的 `lowports` 组靠它测换算：`scripts/difftest` 认用例里 `# appine-options: …` 这一行，加给 appine-run，与 `# docker-run-options:` 一样。

## 测试

- `portmap` 的单元测试：新的 `the_real_port_when_the_host_allows_it`：宿主让绑时宿主端口就是 22、照样登记；同一个端口的第二个 socket 冲突是 EADDRINUSE、不冲突时用同一个；全部关掉之后宿主说 EADDRINUSE（TIME_WAIT）照样返回，不换；只放开了 TCP 时 UDP 22 依次试 22、10022；原来的端口被别的程序占着换到 10080；不让绑的 53 上 UDP 先试 53、再试 TCP 换到的 20053；总是映射时宿主让绑也换。`tries_the_offsets_then_lets_the_host_choose` 改成宿主对低端口回 EACCES，试的顺序是 22、10022、20022、30022；换过的端口也是 EACCES、别的错误（EADDRNOTAVAIL）直接返回。变异检验：去掉先试原来的端口、去掉 EACCES 时换下一个，两种改法下这两个用例都失败。
- 集成测试 `low_ports_bind_the_real_port_or_elsewhere`（原来的 `low_ports_bind_elsewhere_on_the_host`）：两个执行层各跑两次。总是映射的照旧占住宿主的 10079，guest 换到 20079，有内核日志；默认的先在宿主上试绑那个端口，绑得了（开发容器里）就要求 guest 也在原来的端口上、没有内核日志，绑不了（CI 的 runner 不是 root）就要求 10000 + 端口。第一次跑时没换端口的那次 `dmesg | grep` 找不到、脚本退出码是 1，改成 `|| true`。
- 差分测试：`lowports` 组带 `--always-map-low-ports`，两个执行层照旧与原生逐字相同；临时去掉那一行（容器里直接绑原来的端口）也相同。手动：开发容器里 `nc -l -p 79` 默认没有内核日志，带选项时换到 10079；以 uid 1000、`ip_unprivileged_port_start` 为 1024 的容器里（与 Android 一样绑不了）guest 的 22、79 换到 10022、10079，guest 的 netstat 里仍是 22、79。
- 仪器测试：`lowPortsMoveToHighPortsOnTheHost` 拆成两个。`lowPortsTheHostRefusesMoveToHighPorts` 用 79（不在名单里，哪个版本都绑不了，先断言 App 自己绑不了）：换到 10079，App 进程连得上，有内核日志。`lowPortsTheHostAllowsAreBoundAsTheyAre` 用 22，App 自己绑不了就跳过：guest 的 nc 就在宿主的 22 上，App 进程连 22 读到数据，没有内核日志。Pixel 4a（API 34）上 26 个里 24 个通过，跳过 `downloadedDistributionsRun`（照例）和绑 22 的那个；Qenix 16K（API 37）上 25 个通过，只跳过 `downloadedDistributionsRun`。190 篇里在 API 37 上失败的就是这个用例。lint 通过。通知的文字没有在界面上手动看：没换端口的那句原来就有，换了端口的那句只多了一个参数。
- `cargo test --workspace` 351 个通过，clippy（整个工作区和 Android）、格式通过；差分测试两层各 52 组通过（Tier 0 的 `nsfs` 第一次失败：它用 `hostname | grep -c bb` 看外面的主机名有没有被 unshare 里的改掉，原生一侧的主机名是容器的编号，随机的十六进制里正好有 "bb"，改成整行比较 `grep -cx` 之后两层都通过）；LTP 1014 通过，与第 207 轮逐项比只多了 select02（[偶发项](../milestones.md#已知的偶发失败)），没有 BUG。

**推迟项**：

- 只在 API 37 的模拟器上看到放开的名单；真机上 Play 系统更新到了没有、厂商有没有改，要真机（R1）。
- 同一台手机更新前后不一样：更新之前在 10022 上的 sshd，重启之后就在 22 上了，别的设备上写死了 10022 的（`~/.ssh/config`）连不上，要看新的通知。
- 绑原来的端口时 guest 和手机上别的 App 抢同一个端口：别的 App 先占着时 guest 换到 10022（与原来一样）；guest 先占着时别的 App 绑不上，原来两边不冲突。
