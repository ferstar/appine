# M9 之前：App 按 inittab 开机、关机，没有标签页时留在后台（2026-11-02）

← [M9 之前：内置的 init 按 inittab 开机、关机（2026-11-02）](186-inittab.md) · [索引](../milestones.md)

支持 OpenRC 的第四轮，也是最后一轮：把上一轮的 `Machine::boot`、`shutdown`、`take_power` 接到 App 上。原来 App 这边有三个问题：

- 冷启动时没有地方起服务。
- 关掉最后一个标签页时整个 App 退出（`MainActivity.onSessionsChanged` 停止服务），后台的服务一起没了。设计文档 §11.2 写的是"还有终端会话或 guest 进程就保持前台服务"，两者不一致。
- 退出时直接 SIGKILL 所有进程。

## 做法

### JNI（`hosts/android/src/lib.rs`、`Native.kt`）

| 函数 | 作用 |
|---|---|
| `boot` | `Machine::boot` |
| `shutdown` | `Machine::shutdown(PowerOff, 30 秒)`，然后从表里删掉句柄 |
| `takePower` | 取走关机请求：0 没有，1 重启，2 停机，3 关机 |
| `processes` | 还在运行的 guest 进程数 |

`stop`（直接结束所有进程）留给重置 rootfs 和测试用。

### TerminalService

- **开机**：内核启动之后，在单独的线程（`appine-boot`）上 `Native.boot`。OpenRC 起服务要一会儿，不占 `worker`，也不挡标签页。第 184 轮之后 `/run`、`/dev` 已经挂好，OpenRC 不会盖住标签页正在用的东西，标签页不必等开机。
- **OpenRC 的配置**：启动内核之前，`/etc/rc.conf.d/appine.conf` 不存在时写一个，内容是 `rc_provide="loopback net"`（`Rootfs.ensureOpenRcConfig`）。
  - 调研时发现，只 `rc-update add nginx` 时，nginx 要 `net`，OpenRC 去拉起 networking；没有 `/etc/network/interfaces` 时它失败，nginx 也起不来。
  - 网络是宿主的，总是通的，由 appine 提供 `net`、`loopback`。
  - 已经装好的 rootfs 也会补上，用户改过的不动；没装 OpenRC 时没人读它。
- **轮询**：原来每 2 秒取一次监听事件，现在同时取关机请求和进程数（都要取 BKL，在 `worker` 上）：
  - `reboot` → `restart`：结束所有会话，关机，启动一个新的内核。Activity 先显示"正在重启"，新内核起来之后开一个新标签页。
  - `poweroff`、`halt` → `exitAll`。
  - 没有会话、界面不在（`listener` 是 null）、也没有 guest 进程了 → `exitAll`。
- **`exitAll`**（通知里的"全部退出"、上面两种情况）：
  1. 告诉 Activity 退出，结束会话，通知改成"正在停止服务"。
  2. 在 `worker` 上 `Native.shutdown`。
  3. 完了才 `stopSelf`。
  - 关机期间前台服务还在，App 进程不会被当作缓存进程杀掉。
  - 关机期间用户又打开了 App（又有了 `listener`）时，不停止服务，重新启动内核。
  - 被系统直接销毁（`onDestroy` 时内核还在）的照样在 `worker` 上关机，尽力而为。
- **通知**：有会话时"N 个会话运行中"，没有会话时"在后台运行"，退出时"正在停止服务"。

### MainActivity

- 关掉最后一个标签页只 `finish()`，不再停止服务。服务在 2 秒内看一次：没有 guest 进程了就自己退出，还有（OpenRC 起的服务、`nohup` 的后台任务）就留在后台。
- 再打开 App 时，服务里没有会话，照旧开一个新标签页。
- 新增 `onRestarting`：显示"正在重启"，直到新内核起来。

### 写的时候碰到的问题

- `exitAll` 先让 Activity `finish()`，Activity 要到 `onStop` 才放开 `listener`。没有慢服务时关机不到 1 秒就完成了，这时 `listener` 还在，被当作"用户又打开了 App"，重新启动了内核，还开了一个会话。
- 表现是 `poweroff` 之后界面退出，服务和通知（"1 个会话运行中"）却一直在。
- 现在 `exitAll` 告诉 Activity 之后立即清掉 `listener`。之后再有 `listener`，就一定是用户回来了。

## 测试

- 仪器测试新增 `bootsAndShutsDownByInittab`（Pixel 4a、API 34 的模拟器上通过）：
  - 写一个 inittab：sysinit 写文件，once 是 `sleep 30`，shutdown 再写一行。
  - `boot` 之后 `processes` 至少 1，sysinit 运行了。
  - guest 的 `poweroff` 之后 `takePower` 是 `POWER_OFF`，再取是 `POWER_NONE`。
  - `shutdown` 不到 10 秒，之后 `processes` 是 0；下一个内核看到 shutdown 那一行。
  - `ensureOpenRcConfig` 写的配置在。
  - 最后恢复原来的 inittab。
- 手动（同一个模拟器上的 App，用 `adb shell input` 输入）：在 inittab 里加了 `::once:/bin/sleep 3600` 和 `::shutdown:/bin/touch /root/down`。
  - guest 里 `reboot`：标签页换成新内核上的新会话，`/root/down` 有了（关机的命令运行了），`sleep 3600` 在新内核里运行。
  - 用 `exit` 关掉唯一的标签页：界面退出，前台服务还在，通知是 "Running in the background"。
  - 重新打开 App，执行 `poweroff`：3 秒内服务退出，`/root/down` 又有了。修上面那个问题之前，22 秒后服务还在，通知是"1 session running"。
  - 杀掉 `sleep`、关掉标签页：5 秒内服务退出（没有 guest 进程了）。
  - 最后恢复了模拟器里的 inittab。
- 两个 clippy、格式通过；App 的 Kotlin 编译通过。

**推迟项**：

- 熄屏之后继续运行要的 `PARTIAL_WAKE_LOCK`（设计文档 §11.2）还没做：在后台跑着的服务熄屏后可能被暂停。
- 开机的输出只在 `/var/log/boot.log` 里，App 不提示有服务启动失败。
