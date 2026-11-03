# M9：进程被结束、服务没起来时提示（2026-11-02）

M9 计划里的第六轮。内核层结束 guest 进程（内核层的 bug、引擎让宿主出错、OOM killer）时，报告原来只写进 logcat 和内核日志（`dmesg`），标签页里只看到 shell 打印的 "Killed"、"Segmentation fault"，看不出是内存不够还是 appine 的 bug（[第 20 轮](20-robustness.md)、[第 21 轮](21-robustness-finish.md)、[第 143 轮](143-kmsg.md)的推迟项）。开机时 OpenRC 有服务没起来，输出只在 `/var/log/boot.log` 里，没人看（[第 187 轮](187-app-init.md)的推迟项）。

## 做法

### 内核层：结束进程的报告（`Machine::take_kills`）

内核层每结束一个进程记一份报告（`KillReport`）：pid、进程名、控制台的编号、原因。原因是两种（`KillReason`）：

- **bug**：内核层的 bug、引擎的宿主故障（`bug::kill`、`bug::kill_process`），带着报告的那一行（与写进 stderr、内核日志的相同）。
- **内存不够**：OOM killer 结束的进程（`oom::out_of_memory`），连同 `memory.oom.group` 时一起结束的（Linux 整组结束时把挑中的那个再算一次，报告不重复）。带着是哪个 cgroup 的 `memory.max`（App 的内存上限时没有 cgroup）、用量和上限。

进程的控制终端是 App 的某个控制台（`Machine::open_console` 建的）时记下它的编号，App 据此找到标签页。撞上 bug 时线程组可能正被借着，借不到就当作没有控制终端，报告照样记下。最多攒 32 份，App 来不及取时丢掉最早的；`appine-run` 不取，报告只在 stderr。

`Native.takeKills` 把报告转成一行一项、制表符分隔的文本。App 在原来每秒取监听端口和关机请求的那次轮询里一起取。

### App：标签页里说明原因，或者发通知

控制终端是某个标签页时，在那个标签页里用黄色显示一行，比如：

```
[appine] sort (pid 12) was killed: cgroup /t reached its memory.max (16.1 MiB of 16 MiB).
```

App 的内存上限时说"App 的内存用到了上限，OOM killer 挑中了它"；bug 时说是 appine 的内部错误，`dmesg` 里有报告。大小按 1024 进制显示：第一版用 Android 的 `Formatter`（1000 进制），16M 的 `memory.max` 显示成 "17 MB of 17 MB"，看不出超了。

**插在哪里**：提示到的时候，shell 多半已经打印了 "Killed" 和新的提示符（每秒才取一次）。第一版直接追加在后面，提示落在提示符之后，光标停在下一行的空行上，看起来像提示符丢了，用户接着输入时 shell 的行编辑和屏幕也对不上。改成插在光标所在的行上面（vendored 的 `TerminalSession.showMessage`）：

1. 先换行再回到原来的行（只用 LF，列不变）：光标在最后一行时屏幕上卷，腾出地方；
2. 记下光标（DECSC），回到行首，在这一行插入提示要占的行数（IL）：提示符和输入了一半的命令跟着下移；
3. 写提示，恢复光标（DECRC）再下移同样的行数，回到提示符后面原来的位置。

占几行按终端的列数和每个字的宽度（`WcWidth`，中文占两列）算。shell 不知道屏幕变了，但它的光标、行内容和屏幕上的仍然一致，接着输入、回车都正常（手动在屏幕最后一行、输入了一半的命令时试过）。

不在标签页里显示、改发通知的情况：

- 正在全屏程序里（终端在备用屏幕上，vim 这些）：写进去会弄乱它的画面。
- 没有控制终端：OpenRC 起的服务、`setsid` 出去的进程。
- 界面不在（App 在后台）：在标签页里显示了，另外发通知。

通知在新的"进程和服务"渠道里，按 pid 编号，点开回到 App。标签页还没显示过（恢复出来的、终端模拟器还没建）时提示先攒着，建好之后先显示。

### 开机时没起来的服务

开机完成之后（`Native.boot` 返回，OpenRC 的 sysinit、boot、default 都跑完了）读 `/var/log/boot.log`，找 OpenRC 写的 `ERROR: <服务> failed to start`，去掉重复的。有的话发一条通知："2 个服务没有起来"，正文列出服务名，说明开机的输出在 `/var/log/boot.log`。

手动测试时第一次打开 App 就报了两个服务没起来，是仪器测试里写进 `boot.log` 的假服务：minirootfs 没装 OpenRC，inittab 里的程序都不存在、一个命令也不运行，`boot.log` 原来只在第一个命令运行时清空，就一直留着上次开机的内容。现在一个命令也没运行时也清空它（有内容才写）。

### 顺带修的

- **合成文件系统里已有的名字，mkdir 是 EACCES**：写 OOM 的测试时发现 `mkdir -p /sys/fs/cgroup/t` 失败。busybox 的 `mkdir -p` 从头逐级 mkdir，`mkdir /sys/fs` 在 Linux 上是 EEXIST，它接着往下；appine 先检查父目录 `/sys` 能不能修改，在合成文件系统里是 EACCES，就停下了。Linux 的 `filename_create` 先查名字，已经存在就是 EEXIST，然后才轮到只读、没有权限。新的 `create_parent_at` 照这个顺序，mkdirat、mknodat、symlinkat、linkat 的新名字和 bind 一个 unix socket 的路径（EADDRINUSE）都用它。原来在 cgroup 的根下面直接 `mkdir /sys/fs/cgroup/x` 是好的，只是 `-p` 不行。
- **/dev/zero、/dev/urandom 一次只读 64 KB**：`dd if=/dev/zero bs=256M count=1` 在 appine 里读到的是 `0+1 records`（64 KB），Linux 读满 256 MB。内核层与 guest 之间按 64 KB 分块拷贝，普通文件、procfs 读满所有块，设备文件只读一块就返回了。/dev/zero、/dev/full、/dev/urandom（/dev/random 相同）现在也读满。/dev/kmsg 一次读一条，照旧。
- **宿主线程不持 BKL 用仓库**：`Machine::inittab`（开机、关机时读 `/etc/inittab`）和开机时检查 inittab 的程序在不在，都把 `Rc<Vfs>` 克隆出来、放开 BKL 再用。关机时 guest 进程还在运行，同一个 SQLite 连接、同一组缓存就可能被两个线程同时用，`Rc` 的计数也可能被同时改。现在都持着 BKL 做。[第 193 轮](193-login-account.md)记过 `init_runs_the_inittab` 偶发一次失败：关机时 guest 线程在 rusqlite 的语句缓存里撞上 `RefCell` 已被借走，正是两个线程同时用一个连接的样子，可能就是这个原因。没能复现来确认：修之前的写法用 6 个不停 `ls -lR`、`stat` 的进程压着关机，跑了 40 次都没出现（窗口只有读 inittab 的几微秒）。

## 测试

- 内核层（`cargo test`）：
  - `host_calls_survive_kernel_bugs` 加了一段：在控制台上运行 `guest-bugs`（各种 bug 各在一个子进程里触发），之后 `take_kills` 至少 6 份，都是 bug、带着这个控制台的编号、报告里是同一个 pid；有 "kernel panic in syscall 43409" 那一份；取一次就空了。
  - 新增 `oom_kills_are_reported_to_the_app`：cgroup 的 `memory.max` 是 16M，里面的 awk 拼出 64 MB 的字符串之后接着运行一会儿，报告是 cgroup `/oomt`、上限 16 MB、用量超过上限；App 的上限是 96 MB，awk 拼 128 MB，报告没有 cgroup、上限是 96 MB；`setsid` 出去、没有控制终端的进程，报告里没有控制台；`memory.oom.group` 的 cgroup 里 awk 被挑中、同组的 sleep 一起结束，正好两份报告（看代码时发现第一版里挑中的那个有两份，组里又算了一次；去掉修正时这一段失败）。
  - `init_runs_the_inittab` 加了一段：inittab 里只有没装的程序时，开机之后上次的 `boot.log` 被清空。去掉清空的代码时这一段失败（读到 32 字节的旧内容）。
- 差分测试新增：`cgroup-tree` 里 `mkdir /sys`、`/sys/fs`、`/sys/fs/cgroup`、`/proc/sys`、`/proc/self`、`/dev/pts` 都是 "File exists"，`mkdir -p /sys/fs/cgroup/t/p/q` 成功；`coreutils` 里 `dd` 从 /dev/zero 读 2 块 4M、从 /dev/urandom 读 1 块 1M，都是整块。两种执行层都与原生一致。全套差分测试两层各 52 组通过。
- 仪器测试（Pixel 4a、API 34 的模拟器）：
  - 新增 `killsAreReportedToTheApp`：测试钩子模拟的内核 panic 报告是 bug、进程名对、有控制台编号，报告里是同一个 pid；cgroup 里 awk 被 OOM 结束的报告是 `oom`、`/oomt`、上限 16777216、用量更大；取一次就空了。第一版用 `dd bs=64M` 吃内存，模拟器上 dd 17 毫秒就做完退出了，OOM killer 每 20 毫秒才量一次，有时发现不了（设计如此，§12 第 4 条），改成吃完内存之后接着运行一会儿的 awk，连跑 3 次都通过。
  - `bootsAndShutsDownByInittab` 的 inittab 加了一项像 OpenRC 那样打印两个服务（一个重复）没起来的命令，`Rootfs.failedServices` 读出这两个。
  - 22 个全部通过（`downloadedDistributionsRun` 照例默认跳过）。
- 手动（模拟器上的 App）：
  - 把 shell 放进 16M 的 cgroup，`sort /dev/zero`：标签页里先是 shell 的 "Killed" 和新的提示符，然后提示插在提示符上面。在屏幕已满、光标在最后一行、提示符后面输入了一半的 `echo typed half` 时也一样，回车照常执行。
  - `setsid sort /dev/zero`：没有控制终端，发了通知（"sort was killed"）。在 vi 里时后台的 sort 被结束：vi 的画面没动，发了通知；退出 vi 之后标签页里也没有插入。在 busybox 的 less 里时（它不用备用屏幕）提示插在它的状态行上面，见推迟项。
  - 开机失败的通知：上面说的旧 `boot.log` 让通知第一次就出来了（"2 services did not start: fake, other…"），修了清空之后重开 App 没有这条通知。真装 OpenRC、加一个起不来的服务的试验没做成：这时宿主的网络很差（`ping` 丢一半、170 毫秒），模拟器里 `apk add openrc` 下载一直停着；开发容器里修改前后的 `appine-run` 用 HTTPS 下载同样都在十几 KB 处停住，与这一轮的改动无关。
- `cargo test --workspace`、clippy（整个工作区和 Android）、格式通过；lint 只有原来那类 UseKtx。LTP 的 syscall 测试 1014 个通过，与第 193 轮存的结果逐项比，只有[偶发项](../milestones.md#已知的偶发失败)里检查睡眠精度的 futex_wait05、pselect01_64 这次过了，没有 BUG。

**推迟项**：

- 报告每秒取一次，提示总在 shell 的 "Killed" 和新提示符之后才到，靠插在提示符上面弥补。
- 不用备用屏幕、自己整屏重画的程序（busybox 的 less、top）在前台时，提示照样插在光标那一行上面：手动试了 busybox 的 less，提示出现在它的状态行上面，上面的内容被顶上去两行，直到它下次重画时被盖掉；这种情况不发通知。
- OOM killer 定时量用量：一下子分配完、在下一次测量之前就退出的进程不会被结束，也就没有提示（§12 第 4 条原有的限制）。
- 内核层的 bug 只说"内部错误"，报告的原文要用 `dmesg` 看；没有"复制报告"或者"反馈问题"的入口。→ 通知上有"复制报告"了（版本、设备、发行版、执行层和报告），报告里也有了 panic 的消息和位置（[M9：内核 bug 的报告能复制，带着 panic 的位置](205-copy-bug-report.md)）；"反馈问题"仍没有，等定了仓库公开与否
- 开机失败的通知只认 OpenRC 的 `ERROR: <服务> failed to start`；sysvinit（Debian 的 `/etc/init.d/rc`）的失败格式不统一，没有提示。之后运行中的服务崩溃（`rc-service` 里是 crashed）也不提示。
