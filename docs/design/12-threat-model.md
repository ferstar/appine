# 12. 威胁模型

**对外声明**：和 iSH、Termux 一样，appine **不是安全沙箱**。我们不承诺在 guest 内部不同 uid 之间，以及 guest 和宿主 App 之间，提供安全边界。

| # | 要求 | 做法 |
|---|---|---|
| | **硬性工程不变量** | |
| 1 | guest 只能通过自己的页表访问内存。DBT 只原样拷贝白名单内的指令，所有访存指令都必须改写 | [engine 的 map](../../.claude/maps/engine.md) |
| 2 | 内核层不把 guest 指针当作宿主指针 | syscall 参数里的地址一律是 `u64` 或 `UserSlice`（`core/kernel/src/uaccess.rs`），只能经由 `Mm` 读写 |
| 3 | 宿主路径永远不由 guest 输入拼出来，宿主侧只出现 `objects/<数字>` 形式的路径，symlink 只存在于数据库里。因此"恶意 tar 包借 symlink 逃逸"这类问题在结构上不可能发生 | §8.1 |
| | **稳健性** | |
| 4 | **guest 自己的 OOM killer**：给所有 guest 内存设一个总上限，超出上限时由我们挑选一个 guest 进程杀掉，不让系统 LMK 杀掉整个 App。同时为 `RLIMIT_NPROC` 设置默认值，防止 fork 炸弹 | `core/kernel/src/oom.rs`。上限量的是整个 App 进程的匿名内存（RssAnon + RssShmem + VmSwap，LMK 看的就是它；Tier 1 代码缓存在写入视图里还没拆掉的页扣掉一次，见 [engine 的 map](../../.claude/maps/engine.md) 的 W^X），默认是物理内存的一半，`MachineOptions::memory_limit` 可以改。guest 碰新的页由宿主内核处理，所以是定时检查：有 guest 线程在运行时每 20 毫秒量一次，超过上限的 3/4 之后每 5 毫秒。超出时按 Linux 的 oom_badness 挑一个进程用 SIGKILL 结束，记进内核日志（[kernel/signal 的 map](../../.claude/maps/kernel/signal.md)），等它退完（最多 1 秒）再看下一个。上限和用量也以 cgroup 的 `memory.max`、`memory.current` 告诉 guest（§8.2）；guest 给子 cgroup 设的 `memory.max` 一起检查。fork 炸弹由 `threads-max` 和 `RLIMIT_NPROC` 挡住（[kernel/process 的 map](../../.claude/maps/kernel/process.md)） |
| 5 | 内核层以 `panic=unwind` 编译，在 syscall 入口用 `catch_unwind` 兜住 panic，只杀掉当前 guest 进程并生成报告。debug 构建下直接 abort | `core/kernel/src/bug.rs`。运行循环在每次进入内核的地方（包括不持 BKL 的系统调用）兜住 panic，获取 BKL 时忽略中毒标记；引擎的 bug 走 [engine 的 map](../../.claude/maps/engine.md) 的 `APPINE_EXIT_HOST_FAULT`。两种情况都像 Linux 的 oops 一样用 SIGSEGV 结束整个进程，往 stderr（Android 上是 logcat）写报告（带 panic 的消息和位置），同一份报告也记进内核日志，App 说明原因（§11.2）。运行循环之外的内核代码也兜住：线程开始时、退出的每一步、定时器线程处理一个进程的定时器时，结束那个进程；进程结束的九步各自兜住，一步出错只丢掉这一步做的事；内存上限的检查、信号驱动的 I/O 出错时关掉这项功能；宿主调用 `Machine` 的方法（JNI）时只让那次调用失败。debug 构建默认直接终止宿主进程，`APPINE_ON_BUG=kill`/`abort` 可以覆盖 |
| | **对 Android 暴露的接口** | |
| 6 | 只导出启动 Activity 和 DocumentsProvider，不提供供其他 App 执行命令的 intent | DocumentsProvider 由 `MANAGE_DOCUMENTS` 保护，只有系统的"文件"App 能直接调用，别的 App 经由用户授予的 URI 权限 |
| 7 | guest 第一次在非回环地址上 `listen` 某个端口时，发出一条通知 | 内核攒着事件（`Machine::take_listens`），App 每 2 秒取一次，在"网络服务"渠道里发通知：进程名、端口，低端口换到了宿主上的哪个端口（§10） |
| 8 | JIT 代码页使用 W^X 双映射 | [engine 的 map](../../.claude/maps/engine.md) |

**信任链**：`apk` 用 rootfs 中自带的 Alpine 公钥验证每个软件包的签名，所以更换镜像不会削弱信任。调试日志只保存在本地；如果以后加入遥测，必须由用户主动开启，并同步更新 Play 的数据安全表单。
