# M9 之前：App 里选 rootfs（2026-10-29）

← [M9 之前：路由表（2026-10-29）](98-routes.md) · [索引](../milestones.md) · [M9 之前：文件能力（2026-10-29）](100-file-caps.md) →

[glibc 发行版的探测](69-glibc-probe.md)那一轮的结论是：Debian、Ubuntu 的 rootfs 直接当作仓库就能完整地用，内核层不用改，给用户 glibc 环境（pip 的 manylinux wheel、各种预编译的二进制）更便宜的路是 App 能用它们建仓库，mount 的子集先不做。原来 App 只能建 Alpine 的仓库。这一轮只改 App 和 JNI 层。

- **选发行版**：第一次运行时弹出选择框：Alpine Linux 3.24、Debian 13（需下载 47 MB）、Ubuntu 24.04（需下载 28 MB）。标签栏新增一个菜单（⋮），里面是"重置 rootfs…"，确认之后结束所有会话和 guest 进程，删掉仓库，再弹出选择框。下载、导入失败时选择框重新弹出，把原因写在标题下面（对话框挡住了中间的状态文字）。
- **tarball**（`Rootfs.kt`）：
  - Alpine 的照旧在 APK 的 assets 里。
  - Debian 是 debuerreotype 构建的 `debian:trixie`（Docker 官方镜像用的同一份，Debian 13.7，2026-10-27），固定在 `docker-debian-artifacts` 的 `dist-arm64v8` 分支的提交 e36653f 上：这个分支的历史是线性的，旧提交一直在。
  - Ubuntu 是官方的 ubuntu-base 24.04.5，清华、中科大的 `ubuntu-cdimage` 镜像上有同一份。
  - 版本、长度和 sha256 固定写在代码里，与 `scripts/fetch-rootfs` 一样只认摘要对得上的，所以从哪个来源下载都一样可信。有几个来源时先并发测速，从快的开始试，一个失败了换下一个。
  - 中途超时、连接断开时带 `Range` 接着下（这一次下到了数据才算，最多 5 次），服务器不支持断点续传时从头再来。校验过的 tarball 留在缓存目录里，导入成功才删掉，导入失败重来时不必再下载。
- **测速改为按吞吐量**：原来选 Alpine 镜像时比的是下完前 128 KB 用了多久。第一次在模拟器上选 Ubuntu 时下得极慢（2 分钟 2 MB），当时没有记下挑了哪个来源；在 Mac 上量了一下（下表），经由代理时清华只有 14 KB/s，只看开头一小段的用时分不出这种情况。现在各下最多 512 KB、5 秒，按字节每毫秒比，并把各来源的速度和最后用的地址记进日志；Alpine、Debian、Ubuntu 的软件源镜像和下载 tarball 用同一个测速。之后再选 Ubuntu，经由代理时清华测不出速度，用了中科大，15 秒左右下完。

  | 来源 | 经由代理 | 直连 |
  |---|---|---|
  | cdimage.ubuntu.com | 3.6 MB/s | 0.69 MB/s |
  | 清华 | 14 KB/s | 1.4 MB/s |
  | 中科大 | 0.86 MB/s | 6.8 MB/s |
  | raw.githubusercontent.com（Debian） | 模拟器上两个一共 80 MB 约 90 秒 | 18 KB/s |

- **配置**：
  - 包管理器的源：apt 的 deb822 源用 http，rootfs 里没有 ca-certificates，软件包有签名。镜像列表是官方的（deb.debian.org、ports.ubuntu.com）、清华、中科大、阿里云，首次启动时与 Alpine 一样在后台测速，选最快的写进去。
  - Debian 的 resolv.conf 指向 1.1.1.1、没有 hosts、hostname 是 `debuerreotype`，Ubuntu 的 resolv.conf 和 hosts 是空文件：都改写好，resolv.conf 指向 DNS 存根（系统的私人 DNS、VPN 才生效）。
  - motd 和 `/etc/profile.d/appine.sh` 三个发行版一样，motd 里按发行版写 apk 或 apt 的例子。
- **登录 shell**（`ConsolePty.LOGIN`）：原来固定是 `/bin/sh`（argv[0] 是 `-sh`），在 Debian、Ubuntu 上是 dash。改为以 `/etc/passwd` 里 root 的 shell 登录（`-l`），Alpine 是 ash，Debian、Ubuntu 是 bash，chsh 改过之后照样，找不到时用 `/bin/sh`。passwd 从 fd 3 读、exec 时关掉：第一版用 `while read ...; done < /etc/passwd`，循环的重定向也作用于里面 exec 的 shell，登录 shell 把 passwd 剩下的行当命令读了（仪器测试里看到 `bin:x:1:1:...: not found`）。
- **JNI 的句柄**（`hosts/android/src/lib.rs`）：原来是 `Arc<Machine>` 的裸指针，要求 Kotlin 在 `stop` 之后不再用它。第一次在模拟器上试重置时 App 无响应：会话结束之后 TerminalSession 在主线程上才清理控制台（`ConsolePty.close` → `closeConsole`），这时 worker 已经 `stop` 释放了 Machine，主线程卡在已经释放的锁上。服务销毁（`onDestroy`）原来就是同样的顺序。改为把 Machine 登记在一张表里，交给 Kotlin 的是编号；`stop` 从表里删掉，之后的调用查不到它：读写返回 -1，`waitFor` 返回 -9，新开控制台、起进程抛出异常，别的什么也不做。
- **测试**：
  - 先在 `appine-run` 里验证这两个固定版本的 tarball：导入（Debian 4407 个文件，Ubuntu 2562 个），照 App 的配置改好 `/etc` 之后，按 passwd 登录是 bash，`getent hosts` 经由存根解析，`apt-get update`、`apt-get install file` 都正常。
  - `MachineTest` 新增五个：
    - `loginShellComesFromPasswd`：登录 shell 读了 /etc/profile，改了 passwd 里的 shell 之后照样；
    - `downloadsAreVerified`：用 `file:` 地址代替网络，摘要、长度不对时删掉并报错；
    - `downloadsResumeWhereTheyBrokeOff`：测试里起一个本地的 HTTP 服务，第一次只发三分之一就断开，第二次按 `Range` 回 206，或者不认 `Range`、回 200。Android 不让 App 走明文 HTTP，debug 版加了一份只允许 127.0.0.1 的网络安全配置（`src/debug/`）。
    - `callsAfterStopAreHarmless`：`stop` 之后的各个调用。
    - `downloadedDistributionsRun`：真的下载 Debian、Ubuntu，在登录 shell 里看 os-release、`$SHELL`，运行 `apt-get update`。要联网，默认跳过，`-Pandroid.testInstrumentationRunnerArguments.network=true` 时运行。
  - Android 14（4K 页）的模拟器上 11 个都通过。联网的那个是模拟器开着 `-http-proxy`（经由 Mac 上的代理）单独跑的，那样 127.0.0.1 也被转给代理，本地 HTTP 服务的用例不能一起跑。联网的那个第一次在从 GitHub 下载 Debian 时读超时失败，于是加了断点续传；再跑时下到 26.8 MB 断了一次，接着下完，通过。Android 17（16K 页）的模拟器上不联网的 10 个通过。AGP 的结果 XML 把按假设跳过的用例记成 failure，Gradle 的退出码是 0。
  - 变异检验：续传次数改成 0，`downloadsResumeWhereTheyBrokeOff` 以 `unexpected end of stream` 失败。
  - 在模拟器上点了一遍界面：清掉数据后启动弹出选择框；选 Ubuntu，显示测速、下载进度，进入 bash，os-release、resolv.conf 都对；从菜单重置之后再弹出选择框；断网选 Debian，选择框重新弹出并写着无法解析主机名。
  - lint 没有新的提示，`cargo ndk` 的 clippy 通过。内核层没有改动，没有重跑差分测试和 LTP。

**推迟项**：

- Debian 的 tarball 只有 GitHub 一个来源，国内直连只有 18 KB/s，47 MB 要下四十多分钟；没有镜像站带 debuerreotype 的构建。可以自己放一份，或者改用清华、中科大的 LXC 镜像（每天构建、旧的几天后删掉，没法固定摘要，而且是 xz，导入只认 gzip）。→ 定了暂时只用 GitHub 这个官方来源，不自己托管（M9：设置页，默认导入 Alpine）；换成 Debian 改为先在后台下载，下载期间原来的系统照常能用，慢的问题仍在
- 版本固定在 App 里：Debian、Ubuntu 出了新的小版本要发新版 App 才换，已经装好的由用户自己 `apt upgrade`。→ Ubuntu 按决定去掉了（M9：设置页，默认导入 Alpine）；Debian 仍是这样
- 一次只有一个 rootfs，换发行版就丢掉原来的文件；没有导出、导入（§11.4 放到 v1.x），也不能两个发行版同时用、共用文件（那时再回来做 mount）。
- 下载中途不能取消、回去换一个，只能等它失败或者结束 App。→ 换发行版改为先在后台下载，设置页和通知里都能取消，取消、失败时原来的系统不动（M9：设置页，默认导入 Alpine）
- 以后换软件源镜像仍要手改（没有设置界面）。→ 设置里可以从内置的镜像里选，或者选回自动测速（M9：设置页，默认导入 Alpine）；列表以外的镜像仍要手改
- 两个 rootfs 都很小：ip、less、vi、curl 都没有（Ubuntu 有 ps），ubuntu-base 的 dpkg 还不装 man 和 doc。→ Ubuntu 去掉了（M9：设置页，默认导入 Alpine）；Debian 的仍然很小
- 只有这三个发行版，没有 Fedora、Arch 等。→ 按决定去掉了 Ubuntu，只剩 Alpine 和 Debian（M9：设置页，默认导入 Alpine）；仍没有 Fedora、Arch 等
- 测速仍是估计：5 秒、512 KB 的吞吐量看不出之后会不会被限速，慢但没断的来源不会换。
