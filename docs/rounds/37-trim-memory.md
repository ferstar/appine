# M9 之前：onTrimMemory（2026-10-26）

上一轮翻译池放到 16 个实例之后，池里最多留七八十 MB 翻译。内核层的 `Machine::trim_memory`（丢掉池里保留的翻译、共用的只读文件映射、dentry/inode 缓存，提交推迟的写事务）早就有了，这一轮把它接到 App 上（§11.2）。

- **能收到什么**：Android 14 起不再发 `RUNNING_MODERATE`、`RUNNING_LOW`、`RUNNING_CRITICAL`、`MODERATE`、`COMPLETE`，只发 `UI_HIDDEN` 和 `BACKGROUND`。第一版在 `UI_HIDDEN` 时释放，模拟器上按 HOME 之后一直没有收到：按 AOSP 的代码，系统只在进程状态降到 `IMPORTANT_BACKGROUND` 以下时才发它，有前台服务的进程停在前台服务；也不在后台的 LRU 列表里，收不到 `BACKGROUND`。所以有前台服务时，Android 14 起根本收不到 `onTrimMemory`。
- **什么时候释放**：自己看界面在不在。Activity 停下（切到别的 App、熄屏）时会把 `TerminalService.listener` 设成 null，这时开始计时，1 分钟之后还没回来就释放；回来时取消。只是切出去看一眼再回来时，不必把 node、python 这些重新翻译一遍。`onTrimMemory` 的 `RUNNING_LOW`、`RUNNING_CRITICAL`（Android 14 之前）以及 `BACKGROUND` 以上立即释放。正在运行的进程用着的翻译和映射不动，释放之后池子随着进程退出又会攒起来。
- **在哪个线程上做**：释放要取 BKL、提交仓库，不放在主线程上。它和停止内核放在同一个单线程的 executor 上按顺序做（原来停止在临时线程上），停止之后不会再有释放的调用用到已经释放的句柄。JNI 新增 `Native.trimMemory`。
- `Machine::trim_memory` 里内核层的部分包进 `host_call`：内核层在这里 panic 时那次调用失败，不穿过 JNI 让 App 进程 abort。

- **测试**：MachineTest 新增"释放缓存时正在运行的进程不受影响，之后照常起新进程"（一边跑循环一边释放 5 次）；4K（API 34）和 16K（API 37）的模拟器上 5 个测试都通过。4K 模拟器上装好 debug 版 App（内核层的日志打开）：`am send-trim-memory` 发 `RUNNING_LOW` 立即释放；按 HOME、20 秒后切回来，不释放；按 HOME 等 70 秒，释放；App 一直照常运行，日志里没有错误（看内核层日志里的 "trimming memory"）。cargo test（174 个）、clippy 和 Android clippy 通过。
