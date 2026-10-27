# appine 里程碑记录

每一轮（里程碑，以及 M8 之后、M9 之前的各轮补强）的结论、测量、实现要点和推迟项各记一篇，按完成顺序编号放在 [rounds/](rounds/) 里。这一页是索引，另记着各轮共用的[测量条件](#测量条件)、[已知的偶发失败](#已知的偶发失败)和 [R1 的测量记录](#r1-的测量记录)。里程碑的计划和排序依据见[设计文档](design.md) §14。

- 各篇是当时的记录。后来改动过的做法以设计文档第 1–13 节为准。
- 推迟项（以及正文里写着留到以后的事）后面用"→"注明后来在哪一轮补上、试过不做，或者只做了一部分；只做了一部分的写明还剩什么。没有"→"的就是还没做。一轮做了以前推迟的事，要回到原来那一篇补上"→"。
- 文中的 §x 指设计文档的章节；R1–R6、A1–A3 是设计文档 §16 的风险编号。
- 新的一轮：在 `rounds/` 里接着编号新建一篇（开头的导航行指向前后两篇），在下表加一行；有新的倍数时在 R1 的测量记录里加一行。

| 一轮 | 日期 | 要点 |
|---|---|---|
| [M2 结果](rounds/01-m2.md) | 2026-10-24 | 原型 Tier 1：整数基准几何平均 1.85×，Neoverse N2 上 2.37× |
| [M1a 结果](rounds/02-m1a.md) | 2026-10-24 | SIMD&FP、LL/SC、LSE；lockstep 扩展成三方比对 |
| [M1b 结果](rounds/03-m1b.md) | 2026-10-24 | busybox 经由 `ld-musl` 跑通；`gzip` 4.75× |
| [M3a 结果](rounds/04-m3a.md) | 2026-10-24 | 多进程、多线程、BKL、信号帧、vDSO |
| [M3b 结果](rounds/05-m3b.md) | 2026-10-24 | 进程内 PTY、作业控制、procfs |
| [M4 结果](rounds/06-m4.md) | 2026-10-24 | 网络、DNS 存根、AF_UNIX；`apk add`、`git clone`；R5 关闭 |
| [M5 结果](rounds/07-m5.md) | 2026-10-24 | Android App；4K、16K 页模拟器通过 |
| [M6 结果](rounds/08-m6.md) | 2026-10-25 | 在 appine 里编译 Lua、SQLite（约 10×）；凭据、记录锁、IPC、定时器；LTP 849 |
| [M7 结果](rounds/09-m7.md) | 2026-10-25 | Tier 1 正式版：SIMD 翻译、4096 项 TLB、翻译模板、W^X；编译 4.7–6.1× |
| [M8 结果](rounds/10-m8.md) | 2026-10-25 | Go、Node.js 行为正确；`EPOLLET` 修正；LTP 856 |
| [M8 补测：JVM](rounds/11-m8-jvm.md) | 2026-10-25 | OpenJDK 21/25、Maven、Gradle；`/proc/self/exe` 解析符号链接；inotify |
| [M9 之前：稳定性和内存](rounds/12-stability-memory.md) | 2026-10-25 | 截短已映射的文件、`PROT_NONE` 不进页表、`MADV_DONTNEED`、`restart_syscall`、robust futex |
| [M9 之前：性能剖析](rounds/13-profiling.md) | 2026-10-25 | 元数据缓存、fork 只看碰过的页；`git status` 19× → 6× |
| [M9 之前：起进程的翻译开销](rounds/14-spawn-translation.md) | 2026-10-25 | 翻译跨 exec 保留；shell 起进程 9× → 6.3× |
| [M9 之前：栈指针常驻寄存器和返回预测](rounds/15-sp-return-prediction.md) | 2026-10-25 | SP 常驻 X26、影子返回栈；返回预测让 gcc 快 10% |
| [M9 之前：futex 定向唤醒](rounds/16-futex-targeted-wake.md) | 2026-10-25 | FUTEX_WAKE 只通知被唤醒的线程；Gradle 热构建 6.1 → 4.8 秒 |
| [M9 之前：兼容性补漏](rounds/17-compat.md) | 2026-10-25 | maps 命名、堆单 VMA、实时信号排队、pidfd、`execveat`、`/proc/net`；LTP 889 |
| [M9 之前：剩下的小缺口](rounds/18-small-gaps.md) | 2026-10-25 | `/dev/shm` 开机清空、smaps/pagemap、`ru_maxrss`、unshare、`max_map_count`；LTP 905 |
| [M9 之前：推迟项](rounds/19-deferred.md) | 2026-10-25 | `O_ASYNC`、租约、POSIX 消息队列、AIO、AF_UNIX GC；LTP 944；io_uring 不做 |
| [M9 之前：稳健性](rounds/20-robustness.md) | 2026-10-25 | 内核层 panic、引擎的宿主故障只结束一个进程；App 内存上限和 OOM killer；threads-max、`RLIMIT_NPROC` |
| [M9 之前：稳健性的收尾](rounds/21-robustness-finish.md) | 2026-10-25 | 线程开始和退出、定时器线程、宿主调用（JNI）里的 panic；App 进程里经过 libsigchain 的恢复 |
| [M9 之前：栈的大小上限](rounds/22-stack-limit.md) | 2026-10-26 | 主线程栈按需增长，受 `RLIMIT_STACK` 的当前值限制；syscall 和信号帧也让它增长；mmap 区按上限让出间隔；LTP 944 |
| [M9 之前：访存检查和翻译表](rounds/23-tlb-checks.md) | 2026-10-26 | 跨页访问不退出、同一基址的访存共用一次 TLB 检查、翻译表复用墓碑；gcc 3.6×、python 4.9×，Gradle 热构建 4.8 → 2.9 秒 |
| [M9 之前：多线程共用翻译和定向唤醒](rounds/24-shared-translation.md) | 2026-10-26 | 测量改用本地卷上的仓库；同一地址空间的线程共用 Tier 1 的翻译、池子按程序保留翻译；唤醒分通道；普通文件读写不持 BKL；`go build` 5.4× → 3.2×，Maven 8.5× → 4.4×，`node -e 0` 5.1× → 2.9× |
| [M9 之前：不持 BKL 的 stat](rounds/25-stat-without-bkl.md) | 2026-10-26 | 缓存全命中的 newfstatat、statx 不取 BKL；并行 lstat 的 `git status` 24 → 13 毫秒，4 线程并行 lstat 每次 4.1 → 0.9 微秒 |
| [M9 之前：fork 写时复制](rounds/26-fork-cow.md) | 2026-10-26 | 私有匿名内存写时复制、宿主线程池、唤醒按子进程/线程结束/管道细分；写过 100 MB 的进程 fork 21 → 2.3 毫秒，空进程 118 → 90 微秒 |
| [M9 之前：交错访存共用检查](rounds/27-interleaved-checks.md) | 2026-10-26 | 同一基址的访存中间夹着别的访存也共用一次 TLB 检查；量清楚了 TLB 检查贵在哪，按访存点缓存页等两个实验没有收益；python 循环 0.82 → 0.75 秒 |
| [M9 之前：起进程的宿主开销](rounds/28-spawn-host-cost.md) | 2026-10-26 | 放开 BKL 之后再叫醒、再交还内存；驻留内存取样合并、缓存；同一版本的程序文件里的块不再核对；只读文件映射共用宿主映射；execve 放开 BKL 建地址空间；fork 后 exec 221 → 163 微秒（原生 154），管道 0.85 → 0.54 毫秒，shell 起进程 0.275 → 0.21 秒 |
| [M9 之前：BKL 的等待和持锁](rounds/29-bkl-wait-hold.md) | 2026-10-26 | 等 BKL 时只让一个线程自旋最多 30 微秒；fork 的宿主映射预先建好；execve 记着程序文件；4 个进程同时起进程的总吞吐提高 4 成，管道 0.54 → 0.45 毫秒 |
| [M9 之前：翻译后代码里的访存](rounds/30-translated-access.md) | 2026-10-26 | 按用途量了翻译后代码的开销（TLB 检查 gcc 32%、python 56%）；末字节地址从基址算、地址已知的访存表项位置是常数；python 循环 −4% |
| [M9 之前：超级块的实验](rounds/31-superblocks.md) | 2026-10-26 | 调用之后、前向跳转之后接着翻译：python 最多快 2%，gcc 第二次编译反而慢一成，没有采用 |
| [M9 之前：别的负载和文件操作](rounds/32-workloads-file-ops.md) | 2026-10-26 | 各类负载的倍数；链接点不等同步完成，java 启动 −15%；仓库推迟最多 5 毫秒合并提交，新建、删除文件快 2–4 倍，`cp -r` 再 `rm -rf` 2.8× → 1.6× |
| [M9 之前：文件操作的宿主开销](rounds/33-file-ops-host-cost.md) | 2026-10-26 | 后台线程删 object、操作里读缓存并填进新建的条目、object 相对目录操作、普通文件间的 sendfile 交给宿主、管道少叫醒；同一个卷上 `cp -r` 再 `rm -rf` 2.0× → 1.6×，`tar x` 2.3× → 1.7× |
| [M9 之前：node 启动](rounds/34-node-startup.md) | 2026-10-26 | outline atomics 的 STXR 借一个寄存器在 Tier 1 里做（原子操作的循环 43 → 8.3 纳秒）；核对时页还没映射的块停放、执行到时整页恢复；`node -e 0` 热启动 −12%，python 启动 −6% |
| [M9 之前：JIT 负载](rounds/35-jit.md) | 2026-10-26 | 代码作废按内容、dsb 时由分发器就地作废；大实例不挤掉池里别的翻译；Maven 6.7 → 6.2 秒，Gradle 热构建 −6% |
| [M9 之前：多进程并行](rounds/36-parallel-processes.md) | 2026-10-26 | 翻译池放 16 个实例；起进程时少做 mincore；4 个同时 exec −18%、4 个同时跑管道 −23%，池里多留约 56 MB 翻译 |
| [M9 之前：onTrimMemory](rounds/37-trim-memory.md) | 2026-10-26 | 界面离开 1 分钟之后、内存紧张时释放池里的翻译等可以重建的缓存；有前台服务时 Android 14 起收不到 `onTrimMemory`，自己看界面在不在 |
| [M9 之前：拆 BKL](rounds/38-split-bkl.md) | 2026-10-26 | 按负载量 BKL 的持锁和等锁；读时钟不取 BKL（4 个进程同时读每次 800 → 88 纳秒）；futex 的等待和唤醒、nanosleep 不取 BKL（4 对线程同时 futex 乒乓 10.8 → 4.9 微秒） |
| [M9 之前：平均负载、sysfs 和 limits](rounds/39-loadavg-sysfs.md) | 2026-10-27 | 平均负载按 Linux 的算法每 5 秒取样，所有线程都在等待时停下；只有 CPU 拓扑的 sysfs（`nproc --all`）；`/proc/<pid>/limits`；删除之后仍然打开着的文件可以 futimens |
| [M9 之前：ptrace 的第一步，strace](rounds/40-ptrace-strace.md) | 2026-10-27 | 系统调用、信号、group-stop 和 `PTRACE_EVENT_*` 的停止，寄存器经由 regset 读写，wait 报告被跟踪的线程；process_vm_readv/writev；strace 能用 |
| [M9 之前：ptrace 的第二步，gdb](rounds/41-ptrace-gdb.md) | 2026-10-27 | 解释器单步、强制写入被跟踪进程的内存并作废翻译、`/proc/<pid>/mem`、`/proc/<pid>/auxv`；gdb 的断点、单步、软件观察点、`catch syscall` 与原生一致 |
| [M9 之前：epoll 的边沿](rounds/42-epoll-edges.md) | 2026-10-27 | `EPOLLET` 按 Linux 的唤醒时机：已经就绪时又来了数据也再触发（进程内的对象记数据代数，宿主 socket 用一个边沿触发的宿主 epoll 记新到的数据），tokio 程序（uv）不再卡住；水平触发的 `EPOLLEXCLUSIVE` 一个边沿只让一个 epoll 返回 |
| [M9 之前：core dump](rounds/43-coredump.md) | 2026-10-27 | 默认动作生成 core 的信号结束进程时按 `core_pattern` 写 ELF core 文件，格式与 arm64 Linux 相同，gdb 可以事后分析；`WCOREDUMP`、`CLD_DUMPED`、`coredump_filter`、`MADV_DONTDUMP`；被结束的线程停在 svc 上；LTP 955 |
| [M9 之前：硬件断点和观察点](rounds/44-hw-watchpoints.md) | 2026-10-27 | `NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 报告 6 个断点、4 个观察点，由解释器检查，有它们的线程只用 Tier 0；gdb 的 `watch`、`rwatch`、`awatch`、`hbreak` 与原生一致 |
| [M9 之前：ptrace 和 procfs 的细节](rounds/45-ptrace-procfs.md) | 2026-10-27 | 单步进信号处理函数先停在第一条指令上；sigsuspend 里 GETSIGMASK 读到原来的屏蔽字、SETSIGMASK 之后不再恢复；`/proc/<pid>/mem` 拿打开时的地址空间；auxv、environ、maps、smaps、pagemap 打开时要能跟踪 |
| [M9 之前：mremap 和 sysfs](rounds/46-mremap-sysfs.md) | 2026-10-27 | mremap 可以扩展私有文件映射（接着映射文件后面的内容）；sysfs 的 `cache/`、`cpufreq/` 照搬宿主对应的 CPU 的 |
| [M9 之前：嵌套 epoll 的边沿](rounds/47-nested-epoll.md) | 2026-10-27 | 外层 epoll 以边沿触发关注内层时，内层关注的文件每来一次事件外层都再触发，两层嵌套和外层阻塞等待时也一样 |
| [M9 之前：独占的唤醒](rounds/48-exclusive-wakeup.md) | 2026-10-27 | `EPOLLEXCLUSIVE` 的文件上来了新的边沿只叫醒一个等待者（定时器线程替它们看），没轮到的进程不醒；`/proc/<pid>/status` 的上下文切换次数取宿主线程的 |
| [M9 之前：按对象唤醒](rounds/49-targeted-wakeups.md) | 2026-10-27 | poll、epoll 和阻塞的读写只被它们等的管道、PTY、eventfd、timerfd、inotify、epoll、AF_UNIX 连接上的修改叫醒（原来系统里每次读写都叫醒所有 poll、epoll 的等待者），关闭文件、进程退出不再广播；修了 `timerfd_settime` 不叫醒等它的线程 |
| [M9 之前：Tier 1 里的观察点](rounds/50-watchpoints-tier1.md) | 2026-10-27 | 有硬件观察点的线程照常用 Tier 1，只有被观察的页上的访问交给解释器（被观察的页不进 TLB）；gdb 里用 `watch` 等变量被写从 10.8 秒降到 0.45 秒 |
| [M9 之前：起进程和设定时器不再叫醒所有人](rounds/51-spawn-timer-wakeups.md) | 2026-10-27 | vfork（`posix_spawn`）的子进程 execve 或退出时只叫醒父进程，setitimer、timer_settime 只叫醒定时器线程，signalfd 改掩码只叫醒等它的线程；原来都叫醒所有等待者 |
| [M9 之前：signalfd 看调用的线程](rounds/52-signalfd-caller.md) | 2026-10-27 | signalfd 的 poll、epoll 与读一样看调用的线程和它的进程挂起的信号（原来看创建它的线程）：发给别的线程的信号不再让它报告可读却读不到，fork 出来的子进程看自己的 |
| [M9 之前：锁和宿主 socket 只叫醒等它们的线程](rounds/53-locks-host-sockets.md) | 2026-10-27 | 文件锁、租约、f_pos 锁改走自己的通道，关闭宿主 socket、DNS 存根和 netlink 的回答只叫醒等这个 socket 的线程，阻塞在宿主 socket 上的收发只登记自己：有 8 个空等的进程时 flock 一次 10 → 0.2 微秒、TCP 建立连接再关闭 45–53 → 14–16 微秒，有阻塞在 recv 里的空等进程时管道读写 6.8 → 0.3 微秒；修了 flock 降级不叫醒等锁的一方 |
| [M9 之前：消息队列和信号驱动的 I/O 不再叫醒所有人](rounds/54-mqueue-fasync.md) | 2026-10-27 | POSIX 消息队列按对象唤醒（收发原来叫醒所有等待者），关闭它不再广播；设了 `O_ASYNC` 的文件上读数据只叫醒定时器线程；有 8 个空等的进程时消息队列收发一次 19–21 → 0.3 微秒；没有修改再整个请求文件类的通道了 |
| [M9 之前：宿主 stdio 上的阻塞读只等宿主 fd](rounds/55-host-stdio.md) | 2026-10-27 | `appine-run` 交给 guest 的宿主 stdin 上阻塞的读不再被别的进程的管道读写叫醒（原来每 2000 次管道来回叫醒它约 2600 次）；只影响 `appine-run`，App 里没有这种文件 |
| [M9 之前：被屏蔽的信号只叫醒等它的线程](rounds/56-blocked-signals.md) | 2026-10-27 | 被屏蔽的信号挂起只叫醒 sigtimedwait、signalfd 的等待者（原来叫醒所有等待者；musl 的 fork 在 clone 前后屏蔽所有信号，这时退出的子进程的 SIGCHLD 就会触发）：屏蔽着 SIGCHLD 起子进程时 8 个空等的进程不再每次都被叫醒，屏蔽着的信号发出再取走 9–16 → 0.4 微秒 |

## 测量条件

各篇的测量没有另外注明时，条件如下：

- **宿主**：Apple Silicon 上 colima 起的 arm64 Linux 虚拟机（4 核）。appine 是 release 构建，用 Tier 1。
- **原生对照**：同一台虚拟机上的 Alpine 3.24 容器。M8 起是差分测试的仓库导出的 docker 镜像，两边的文件完全相同。[性能剖析](rounds/13-profiling.md)那一轮发现，对照的文件和状态必须完全相同：对照镜像起初没有打包 `/var`，原生一侧的 apk 没有仓库索引要读。
- **热缓存**：从[性能剖析](rounds/13-profiling.md)起，每个负载在同一个 appine 会话里连跑两次，取第二次（原生一侧同样）。手机上 App 是长期运行的进程，第二次更接近实际。
- **仓库位置**：从[多线程共用翻译和定向唤醒](rounds/24-shared-translation.md)起，基准仓库放在虚拟机本地的 docker 卷 `appine-bench` 上。之前在 `target/` 里，经由 virtiofs 共享进虚拟机，宿主的每次 open、stat 都跨虚拟机，文件系统、起进程类负载的倍数偏高。从[文件操作的宿主开销](rounds/33-file-ops-host-cost.md)起，原生一侧的文件操作也在同一个卷上（之前在容器自己的 overlay 上）。
- **A/B**："之前"或"HEAD"一列是这一轮开始时的二进制，与改动之后的二进制在同一个会话里交替运行几轮（`target/mem/ab.sh` 等脚本），给出区间。
- **Neoverse N2**：GitHub 的 `ubuntu-24.04-arm` runner（Azure Cobalt 100，2 vCPU），CI 的性能探针在那里运行。它的核心比 Apple Silicon 窄，更接近手机的大核和中核。

## 已知的偶发失败

LTP 的 syscall 测试全量并发运行（`scripts/ltp -j 8`）时，下面几项时好时坏，与当时的改动无关：每次都用上一轮的二进制在同一时刻重跑过，或者单独重跑过。各篇里只记它们这次通过还是失败。

| 测试 | 原因 |
|---|---|
| shmctl01 | 全量并发时子进程还没 `shmdt` 完就超时，单独运行多数通过；[多进程并行](rounds/36-parallel-processes.md)那一轮用 HEAD 的二进制单独跑 8 次也失败 3 次，写时复制之前的二进制并发跑 shm 子集也会失败 |
| futex_wait05、epoll_pwait03、epoll_wait02、clock_nanosleep02、nanosleep01、select02 | 检查睡眠的超时精度，睡 1 毫秒量出 1.3–1.6 毫秒就失败，取决于虚拟机当时的计时延迟（M7 起）；[独占的唤醒](rounds/48-exclusive-wakeup.md)那一轮遇到过虚拟机里原生的 nanosleep 1 毫秒也要 1.5 毫秒，这几项连续失败，旧的二进制也一样 |
| getrusage03 | 要求可用内存至少 512 MB，全量并发时不够就跳过，单独运行通过 |
| nice05 | 比较两个 nice 值不同的线程用掉的 CPU 时间，appine 不按 nice 分配 CPU 时差值的正负是随机的（M8 起）；[推迟项](rounds/19-deferred.md)那一轮把 nice 交给宿主线程之后通过 |
| futex_wake05 | [稳定性和内存](rounds/12-stability-memory.md)那一轮记为以前就时好时坏，原因没查 |
| times03 | [翻译后代码里的访存](rounds/30-translated-access.md)那一轮看到一次进程的用户态 CPU 时间倒退了一个滴答（111 → 110），单独重跑 3 次都通过，之后没再出现，原因没查 |
| mmap21_02 | [文件操作的宿主开销](rounds/33-file-ops-host-cost.md)那一轮清理时 unlink `/dev/shm` 下的文件得到 EIO；单独重跑 3 次、与 `/dev/shm`、mmap、文件操作有关的 162 个测试 16 路并发重跑 3 次都没有再出现，10 月 24 日的基线里也有过一次（LTP 的仓库放在 virtiofs 上）。[ptrace 的第二步](rounds/41-ptrace-gdb.md)那一轮的日志是 `unlink(/dev/shm/ltp_mmap21_2)` 得到 ENOENT：LTP 的库把结果放在 `/dev/shm/ltp_<程序名>_<pid>`，mmap21_01 和 mmap21_02 是同一个程序，每个 appine-run 里的第一个进程都是 pid 2，并发运行时两个用例共用仓库的 `/dev/shm`、用同一个文件，一个删掉了另一个的。这是并发跑 LTP 的方式造成的，不是内核的问题 |

cargo test 里 `ctrl_c_and_ctrl_z_control_the_foreground_job`（Tier 1）卡住过一次：作业停着、shell 在等输入，测试在等它退出（[起进程的宿主开销](rounds/28-spawn-host-cost.md)）。之后重放几十次都没再出现，原因没查明。

## R1 的测量记录

相对原生的倍数，条件见[测量条件](#测量条件)，详见对应的一篇。

| 时间点 | 负载 | 倍数 |
|---|---|---|
| [M2](rounds/01-m2.md) | 整数基准程序的几何平均 | 1.85×；Neoverse N2 上 2.37× |
| [M1b](rounds/03-m1b.md) | busybox `sha256sum`、`gzip -c` | 1.51×、4.75× |
| [M6](rounds/08-m6.md) | 编译 Lua、SQLite | 约 10×，主要是 Tier 1 还不翻译的 SIMD 退回 Tier 0 |
| [M7](rounds/09-m7.md) | 编译 `lvm.c`、SQLite `make`、Lua `make linux`；Neoverse N2 上的性能探针 | 4.8×、4.7×、6.1×；2.25× |
| [M8](rounds/10-m8.md) | node 里 JIT 生成的代码；`go build` 冷编译 | 1.9×；8.7×（单线程 5.6×，多线程扩展性差） |
| [M8 补测：JVM](rounds/11-m8-jvm.md) | 测试程序；Maven 热构建；Gradle 热构建 | 4–5×；11×；14×（跳转缓存未命中、JIT 代码打补丁） |
| [性能剖析](rounds/13-profiling.md)、[翻译保留](rounds/14-spawn-translation.md) | `git status`；shell 起进程 | 6×；6.3× |
| [SP 常驻寄存器和返回预测](rounds/15-sp-return-prediction.md) | gcc 编译 `lvm.c`；python | 3.9×；6.2× |
| [futex 定向唤醒](rounds/16-futex-targeted-wake.md) | Gradle 热构建 | 9× |
| [访存检查和翻译表](rounds/23-tlb-checks.md) | gcc 编译 `lvm.c`；python；Maven、Gradle 热构建；`go build` 冷编译 | 3.6×；4.9×；9.4×、5.6×；6.5× |
| [多线程共用翻译和定向唤醒](rounds/24-shared-translation.md)（仓库改放虚拟机本地卷，以上各行都在 virtiofs 上测，文件系统、起进程类的倍数偏高） | gcc；python；Maven、Gradle 热构建；`go build` 冷编译；shell 起进程；`git status`；`node -e 0` | 3.4×；4.3×；4.4×、4.0×；3.2×；2.2×；1.1×（并行 lstat 2.3×）；2.9× |
| [不持 BKL 的 stat](rounds/25-stat-without-bkl.md) | 并行 lstat 的 `git status` | 1.3–1.4× |
| [fork 写时复制](rounds/26-fork-cow.md) | fork、子进程退出、等待（进程写过 0、10、100 MB） | 1.25×、2.2×、3.8×（原来 1.6×、18×、35×） |
| [起进程的宿主开销](rounds/28-spawn-host-cost.md) | fork 后 exec、vfork 后 exec、`posix_spawn`；`sh -c 'cat \| wc'`；shell 起进程 | 1.05×、1.15×、1.2×；1.4×；1.4×（原来 1.45×、1.6×、1.6×；2.2×；1.8×） |
| [BKL 的等待和持锁](rounds/29-bkl-wait-hold.md) | 4 个进程同时 fork 后 exec（总吞吐）；`sh -c 'cat \| wc'`；shell 起进程 | 2.7×（原来 3.7×）；1.15×；1.3× |
| [翻译后代码里的访存](rounds/30-translated-access.md) | gcc 编译 `lvm.c`；python 循环 | 3.2×；4.6× |
| [别的负载和文件操作](rounds/32-workloads-file-ops.md) | 这一轮开始时：`find`、`du`；`tar` 只读；`grep -r`；`gzip`；sqlite 内存库 / 文件库；`awk`、lua 循环；node 启动 / 计算；java 启动 / 计算。这一轮之后：`cp -r` 再 `rm -rf` | 约 1×；1.4×；3.0×；2.0×；3.8× / 2.7×；2.8×、2.0×；3.1× / 2.6×；4.0× / 2.8×。1.6×（原来 2.4×） |
| [文件操作的宿主开销](rounds/33-file-ops-host-cost.md)（原生对照改在同一个卷上） | `cp -r` 再 `rm -rf`；`tar x` 再 `rm -rf` | 1.6×；1.7×（原来 2.0×、2.3×） |
| [node 启动](rounds/34-node-startup.md) | `node -e 0` 热启动；outline atomics 的原子操作循环 | 2.2×；1.2×（原来 6×） |
| [JIT 负载](rounds/35-jit.md) | Maven、Gradle 热构建 | 3.6×、3.4× |
| [多进程并行](rounds/36-parallel-processes.md) | 4 个进程同时 exec；4 个同时跑管道 | 2.5×；3.1×（原来 3.1×、4×） |
| [拆 BKL](rounds/38-split-bkl.md) | `go build` 冷编译 | 2.8× |
