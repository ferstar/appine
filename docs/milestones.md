# appine 里程碑记录

每一轮（里程碑，以及 M8 之后、M9 之前的各轮补强）的结论、测量、实现要点和推迟项各记一篇，按完成顺序编号放在 [rounds/](rounds/) 里。这一页是索引，另记着各轮共用的[测量条件](#测量条件)、[已知的偶发失败](#已知的偶发失败)和 [R1 的测量记录](#r1-的测量记录)。里程碑的计划和排序依据见[设计文档](design.md) §14。

- 各篇是当时的记录。后来改动过的做法以设计文档第 1–13 节为准。
- 推迟项（以及正文里写着留到以后的事）后面用"→"注明后来在哪一轮补上、试过不做，或者只做了一部分；只做了一部分的写明还剩什么。没有"→"的就是还没做。M3b–M8 的"改动和推迟项"里标着"（修改 §x）"的是对设计的改动，不是推迟项。一轮做了以前推迟的事，要回到原来那一篇补上"→"。
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
| [M9 之前：跟踪停住的进程、跟踪者先取走结束](rounds/57-ptrace-stopped-reaped.md) | 2026-10-27 | 跟踪父进程已经取走停止事件的进程时，group-stop 报告停住它的信号，ATTACH 时先报告 group-stop、再报告 SIGSTOP；被跟踪的进程结束时，跟踪者不是父进程的话，先只交给跟踪者，它取走或者退出之后父进程才能回收、收到 SIGCHLD |
| [M9 之前：cgroup v2 的视图](rounds/58-cgroup.md) | 2026-10-27 | 只读的 cgroup v2 挂在 `/sys/fs/cgroup`，所有 guest 进程在同一个 cgroup 里，`memory.max` 是 OOM killer 的上限：JVM、Node.js 按它定堆，默认上限下 JVM 的默认最大堆 978 → 490 MiB，Node.js 的堆上限 2.09 → 1.05 GiB |
| [M9 之前：seccomp](rounds/59-seccomp.md) | 2026-10-28 | 严格模式和 cBPF 过滤器按 Linux 6.1 检查、执行，继承和 TSYNC，`PTRACE_EVENT_SECCOMP`，通知 fd（USER_NOTIF、ADDFD）；`strace --seccomp-bpf` 能用；线程各自退出时进程的退出状态由最后一个退出的线程定下；LTP 958 |
| [M9 之前：seccomp 的缓存和快速路径](rounds/60-seccomp-cache.md) | 2026-10-28 | 按 Linux 的 `seccomp_cache` 记下过滤器总是放行的编号，不再运行程序；这些编号和 vDSO 里读时钟照常不持 BKL：46 条的过滤器下 futex 唤醒 313 → 60 纳秒（没有过滤器时 60），读时钟 143 → 89 纳秒 |
| [M9 之前：进程结束的每一步分别兜住](rounds/61-finish-steps.md) | 2026-10-28 | 最后一个线程退出之后结束进程的九步（子进程交给 init、PDEATHSIG、SysV IPC、控制终端、跟踪、孤儿进程组等）各自兜住内核层的 bug，一步出错只丢掉这一步做的事，不再留下永远停着、等着的进程 |
| [M9 之前：负载普查](rounds/62-workload-survey.md) | 2026-10-28 | `scripts/survey` 在 appine 里跑 sshd、tmux、postgres、redis、mariadb、Rust、.NET 等 20 多种服务和工具：多数正常；找到 6 个问题：fork 之后父进程的页权限被改错（rustc 崩溃）、ssh 分配不了 PTY、redis/valkey 不肯启动、mariadb 卡住、.NET 看不到别的进程的内存、`/proc/meminfo` 的可用内存偏小 |
| [M9 之前：fork 之后还回的页按各自的权限恢复](rounds/63-returned-loans.md) | 2026-10-28 | 子进程还回借来的页时，父进程把宿主内存连着的一段按第一页所在映射的权限恢复，保护页的 PROT_NONE 扩散到旁边的页（rustc 退出时崩溃），可写的权限扩散到只读的页；改为不跨映射接段（[fork 写时复制](rounds/26-fork-cow.md)那一轮起的 bug） |
| [M9 之前：终端的属主和权限](rounds/64-pty-owner.md) | 2026-10-28 | `/dev/pts/N` 属于 tty 组，可以 chown、chmod：sshd 能把终端交给登录的用户，`ssh -t`、交互式登录和 mosh 能用了（原来 root 登录也被关掉） |
| [M9 之前：smaps 里共用的页](rounds/65-smaps-shared.md) | 2026-10-28 | fork 之后写时复制共用的页在 smaps 里算共享的脏页，Pss 按共用的个数分摊：redis、valkey 启动时的 `ARM64-COW-BUG` 检查通过，能启动了，后台保存、AOF 正常 |
| [M9 之前：io_destroy 叫醒等事件的线程](rounds/66-aio-destroy.md) | 2026-10-28 | 销毁 AIO 上下文时正在 `io_getevents` 里等它的线程返回（没有事件是 EINVAL），与 Linux 相同：mariadb 的 install-db 和关闭不再卡住 |
| [M9 之前：cgroup 的 memory.stat](rounds/67-memory-stat.md) | 2026-10-28 | 按 Linux 6.1 的格式补上 `memory.stat`：.NET 的 GC 改按整个 App 的用量（`memory.current` 减去 `inactive_file`）算内存负载，别的 guest 进程用的内存也算进去（原来只看自己的 RSS） |
| [M9 之前：/proc/meminfo 照搬宿主的](rounds/68-meminfo.md) | 2026-10-28 | `/proc/meminfo` 和 sysinfo 照搬宿主的各项，`MemAvailable` 算上页缓存：free、htop 不再显示内存几乎用完（可用 650 MB → 3.3 GB，与原生相同） |
| [M9 之前：glibc 发行版的探测](rounds/69-glibc-probe.md) | 2026-10-28 | Debian 13、Ubuntu 24.04 的 rootfs 直接当作仓库：apt、gcc、gdb、numpy/pandas 的 manylinux wheel、Node.js 的预编译模块、rustup、OpenJDK、code-server（浏览器连上去）都能用，内核层不用改；从 Alpine chroot 进去缺 `/proc` 等（没有 mount），mount 的子集先不做 |
| [M9 之前：第二轮普查，语言的测试集和大项目](rounds/70-survey-suites.md) | 2026-10-28 | CPython、PHP、Ruby、Node.js、Go 的测试集和 ripgrep、Laravel、Rails，与原生用同一个仓库对照：Laravel、Rails 正常；找到 6 个问题：线程频繁起停时宿主的堆被破坏（Node、Go、ripgrep 的测试都让 appine 崩溃）、目录的 mtime 和文件的 ctime 不更新（Python 新建的模块 import 不到）、FIFO 的 poll、PTY 的 TCFLSH 方向、每个进程多占约 12 MB、FICLONE 的错误码 |
| [M9 之前：线程退出时在 BKL 外丢掉弱引用](rounds/71-thread-exit-race.md) | 2026-10-28 | 线程退出时在持有 BKL 时丢掉线程局部变量里 `ThreadGroup` 的弱引用（原来放开之后才丢，非原子的 Rc 计数和同一进程里刚开始的线程一起改，计数丢失后提前释放，宿主的堆被破坏）：4 个进程不停地起停线程，原来 10 秒内崩溃，现在 60 秒正常；Ruby 的测试 `-j4` 跑完了（[signalfd 看调用的线程](rounds/52-signalfd-caller.md)那一轮起的 bug） |
| [M9 之前：翻译表里漏掉的墓碑](rounds/72-table-tombstones.md) | 2026-10-28 | Tier 1 恢复停放的块时原样恢复它留下的墓碑（原来插到新位置，旧墓碑没有记录、整体清空清不掉，计数却归零，反复之后翻译表被占满，插入找空位的循环一直转，进程连 SIGKILL 都结束不了）：Ruby 的测试 `-j4` 不再卡住，1781 → 479 秒；Node、Go、ripgrep 的测试重跑了，ripgrep、Go 与原生相同（[node 启动](rounds/34-node-startup.md)那一轮起的 bug） |
| [M9 之前：目录和文件的时间戳](rounds/73-timestamps.md) | 2026-10-28 | 目录里新建、删除、改名改目录的 mtime、ctime，普通文件的 ctime 取自宿主 object（chmod、改名等也让宿主改它），utimensat 的 `UTIME_NOW` 交给宿主：Python 能 import 后来新建的模块，PHP 的文件测试 590 个全过；rename 多约 2 微秒 |
| [M9 之前：FICLONE 返回 EOPNOTSUPP](rounds/74-ficlone.md) | 2026-10-28 | FICLONE、FICLONERANGE、FIDEDUPERANGE 按 Linux 的顺序检查之后返回 EOPNOTSUPP（原来 ENOTTY），和 ext4、f2fs 一样不支持 reflink：Python 3.14 的 `Path.copy()` 能用了，test_pathlib 全部通过 |
| [M9 之前：PTY 的 TCFLSH 和 TCXONC](rounds/75-pty-flow.md) | 2026-10-28 | 主设备上的 TCFLSH 按主设备的方向清；输出被 ^S、TCOOFF 停住时这一端的写阻塞（原来照样成功），已经写出的另一端照样能读，TCOOFF 要 TCOON 才恢复；TCIOFF、TCION 发 STOP、START 字符：CPython 的 test_termios、test_ioctl 不再各卡 20 分钟 |
| [M9 之前：FIFO 在写端来之前不报 POLLHUP](rounds/76-fifo-pollhup.md) | 2026-10-28 | 以 `O_NONBLOCK` 只读打开、当时没有写端的 FIFO，有写端打开过之前 poll 不报 POLLHUP（Linux 读端的 `f_version`）：Ruby 的 test_require 全部通过 |
| [M9 之前：删除的目录里不能新建，socket 可以 fchmod](rounds/77-dead-dir-socket-fchmod.md) | 2026-10-28 | 链接数已经是 0 的目录里新建、改名进去返回 ENOENT（原来成功了）；socket、匿名管道的 fd 可以 fchmod、fchown（原来 EACCES）：Node 的 test-fs-mkdir 和 `listen({ readableAll })` 的两个测试通过 |
| [M9 之前：网络信息的缺口](rounds/78-net-info-gaps.md) | 2026-10-28 | 只补文档：没有路由表（`ip route` 为空、没有 `/proc/net/route`）、`/proc/net/dev` 的流量计数全是 0、没有 `if_inet6` 等，Android 上 App 读不到宿主的这些信息；记下影响和可能的做法（`LinkProperties` 的路由、内核层自己计数） |
| [M9 之前：终端的访问时间和修改时间](rounds/79-pty-times.md) | 2026-10-28 | `/dev/pts/<n>` 记下自己的三个时间：从设备读到数据改访问时间、写出数据改修改时间（与 Linux 一样只改秒、相差 8 秒以上才改），utimensat 可以改（原来 EACCES），chmod、chown 改状态改变时间：w、who 的空闲时间有意义了 |
| [M9 之前：CPU 时间不倒退](rounds/80-cpu-times.md) | 2026-10-28 | 用户态、内核态的 CPU 时间按 Linux 的 `cputime_adjust` 各自不倒退（原来内核态时间按滴答取整，跨过一个滴答时用户态时间倒退，LTP 的 times03 偶发失败）；futex_wake05 不是偶发项，是[稳定性和内存](rounds/12-stability-memory.md)那一轮修好的 |
| [M9 之前：被叫醒的进程在 /proc 里是 R](rounds/81-woken-state.md) | 2026-10-28 | 被信号等叫醒、还没跑起来的线程在 `/proc` 里是 R（原来到它重新拿到 BKL 之前都是 S）：LTP 按状态回到 S 来同步父子进程，shmctl01 不再失败（原来记作偶发项，最近每次都失败） |
| [M9 之前：测试自己的竞争和丢掉的现场](rounds/82-test-races.md) | 2026-10-28 | 作业控制用例卡住是测试的竞争（ash 的 fg 先打印命令再交出终端，^C 可能落到 shell 上），原生 Linux 上用 strace 推迟 writev 的返回能稳定重现；改了测试，等 shell 退出加超时；差分测试失败时另存输出，LTP 保留上一次的日志 |
| [M9 之前：没有实现的 netlink 协议建不了](rounds/83-netlink-protocols.md) | 2026-10-28 | `NETLINK_ROUTE`、`NETLINK_KOBJECT_UEVENT` 以外的 netlink 协议是 EPROTONOSUPPORT（原来建得出、收到请求不回答）：Debian、Ubuntu 上走 PAM 的 sshd、su、login 不再因为等审计的 ACK 失败，`ss` 不再卡住；Debian 的 sshd 会话里 w 的空闲时间与原生相同 |
| [M9 之前：补测待查的几项](rounds/84-debt-checks.md) | 2026-10-28 | 只补测：CPython test_os 的失败是用例对负载敏感（原生满载时也超过 1 毫秒），Ruby 的 memory_leak 超时只是慢（其中一个原生也超时），VS Code 第一个终端丢字原生上一样；conda 能用，12 个普查负载在 Tier 0 下与 Tier 1 一致，Ubuntu 上 su、sshd 正常；普查不进 CI |
| [M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口](rounds/85-inet6-opath-exit.md) | 2026-10-28 | `/proc/net/if_inet6` 列出宿主的 IPv6 地址，JVM 不再关掉 IPv6；ioctl 和 FICLONE 的源不接受 `O_PATH` 的 fd（EBADF）；停在 fork 事件上再改用 PTRACE_SYSCALL 时停在这个系统调用的出口，有跟踪者时每个系统调用都记下编号；修了有 seccomp 的线程经由 vDSO 读时钟时多停一次出口（[seccomp](rounds/59-seccomp.md)那一轮起的 bug） |
| [M9 之前：排查在 BKL 外改 Rc 的计数](rounds/86-rc-outside-bkl.md) | 2026-10-28 | 逐个查了不持 BKL 运行内核代码的地方，正常执行时没有在锁外改 `Rc` 计数、`RefCell` 借用标志的；放开 BKL（等待、`Cx::unlocked`）期间 panic 时，展开经过的外层各帧在锁外丢 `Rc`（比如 execve 的 vfs），改为先重新拿到 BKL；`Cx::unlocked` 的闭包要求 `Send` |
| [M9 之前：不持 BKL 的系统调用兜住 panic，等待时不借着打开文件](rounds/87-fast-panic-held-borrows.md) | 2026-10-29 | 不持 BKL 的系统调用里的内核 bug 只结束那个进程（原来整个 App 终止）；查了持着 `RefCell` 的借用去等待的地方，修了 epoll_wait 和 seccomp 通知 fd 的 `NOTIF_RECV` 等待时借着打开文件，另一个线程这时 `fcntl(F_SETFL)`、`ioctl(FIONBIO)` 会让进程被结束 |
| [M9 之前：futex 等待者的清理，放开 BKL 时检查借用](rounds/88-futex-cleanup-borrow-check.md) | 2026-10-29 | futex 的等待者由守卫离开队列：中途 panic 也移除，超时、被信号打断时刚好被唤醒的算唤醒（原来持锁的路径和 `futex_waitv` 可能丢掉一次唤醒）；kernel crate 改用自己的 `RefCell`，调试构建里放开 BKL 时检查没有借着内核对象，新增 `waits` 用例让 19 种等待各走一遍 |
| [M9 之前：测试用的发布构建也检查借用](rounds/89-release-borrow-check.md) | 2026-10-29 | `appine-run` 和 debug 版 App 的发布构建也在放开 BKL 时检查借用；差分测试、LTP、普查认出内核层报告的内部错误（差分测试原来把 `appine-run` 的 stderr 丢掉）；`waits` 用例补上文件位置锁、租约的打破和 AIO 的完成；开销在误差范围内，带检查跑差分测试、LTP、普查和语言的测试集都没有报告内部错误 |
| [M9 之前：网络接口的流量计数](rounds/90-interface-counters.md) | 2026-10-29 | 内核层自己数 guest 的 socket 收发的载荷，接口按 socket 的本端地址认：`/proc/net/dev`（原来全是 0）、新增的 `/sys/class/net/<接口>/statistics/`（btop 读它）和 netlink 的 `IFLA_STATS` 报告同样的数；回环上的短连接 +4%、UDP 来回 +3% |
| [M9 之前：NETLINK_GENERIC 只有 nlctrl](rounds/91-generic-netlink.md) | 2026-10-29 | `NETLINK_GENERIC` 建得了（原来 EPROTONOSUPPORT），只有 Linux 总会有的 nlctrl 一个族，查 nl80211、ethtool、wireguard 等别的族是 ENOENT；回答、错误码、确认的格式与 Linux 6.8 逐字节相同；netlink socket 上的等待按 `SO_RCVTIMEO` 超时（原来一直等） |
| [M9 之前：调试器、valgrind 和 headless Chromium](rounds/92-debuggers-valgrind-chromium.md) | 2026-10-29 | 在 Debian 仓库里跑 delve、lldb、valgrind（memcheck、helgrind、drd、massif、callgrind）和 headless Chromium（命令行、Puppeteer、Playwright），与原生的 Debian 容器对照；找到内核层的 3 个问题（下面三轮），修好之后都与原生相同 |
| [M9 之前：`/proc/<pid>/fd` 和 maps 里的路径解析符号链接](rounds/93-fd-paths-resolve-symlinks.md) | 2026-10-29 | 打开的文件、mmap 的文件和解释器的映射与 Linux 的 `d_path` 一样显示解析完符号链接的路径（原来是词法规范化的）：valgrind 按它带着 `O_NOFOLLOW` 重新打开 Debian 的 ld.so，原来 ELOOP，memcheck 多报几千个错误；开销在误差范围内 |
| [M9 之前：`/proc/<pid>/task` 的链接数](rounds/94-task-dir-links.md) | 2026-10-29 | task 目录的链接数是 2 加线程数（原来总是 2）：Chromium 的 zygote fork 之前按它判断自己是不是单线程，原来一启动就崩溃 |
| [M9 之前：跟踪者退出时丢掉已经取走的信号](rounds/95-tracer-exit-drops-signal.md) | 2026-10-29 | 跟踪者不 detach 就退出时，已经被 wait 取走的信号投递停止里的信号丢掉，与 Linux 6.8 实测的相同（原来照 ptrace(2) 投递）：delve attach 之后读到 EOF 退出，进程原来被 SIGSTOP 停住 |
| [M9 之前：`/proc/<pid>/net`](rounds/96-proc-pid-net.md) | 2026-10-29 | 网络的那些文件与 Linux 一样放在 `/proc/<pid>/net` 下（task 子目录里也有），`/proc/net` 改成指向 `self/net` 的链接（原来是目录，经由 `/proc/self/net` 读不到） |
| [M9 之前：已经删除的文件带 ` (deleted)`](rounds/97-deleted-suffix.md) | 2026-10-29 | 删除之后还开着、映射着的文件，`/proc/<pid>/fd` 和 maps、smaps 里的路径后面加上 ` (deleted)`，与 Linux 相同（needrestart 按它找出还在用旧库的进程）；exe 不加 |
| [M9 之前：路由表](rounds/98-routes.md) | 2026-10-29 | 主表取自宿主（Android 上 App 从默认网络的 `LinkProperties` 交来，别处读宿主的 `/proc/net/route`），本地表按接口的地址合成：`ip route`、`ip route get`（原来 EOPNOTSUPP）、`ip rule`、`route -n`、`/proc/net/route`、`ipv6_route` 与 Linux 逐字节相同；`ip neigh` 是空表，新增 `/proc/net/sockstat`；NETLINK_ROUTE 的错误附上整条请求 |
| [M9 之前：App 里选 rootfs](rounds/99-choose-rootfs.md) | 2026-10-29 | 首次启动选 Alpine、Debian 13 或 Ubuntu 24.04，后两个选了才下载（版本、sha256 固定，按吞吐量测速选来源，断点续传），菜单里可以重置 rootfs；登录 shell 取 `/etc/passwd` 里 root 的（Debian、Ubuntu 是 bash）；JNI 的句柄改为登记表，修了重置、服务销毁时用到已经释放的 Machine |
| [M9 之前：文件能力](rounds/100-file-caps.md) | 2026-10-29 | execve 按 Linux 6.1 的规则算上程序文件的能力（`security.capability`）和环境能力，securebits 起作用；扩展属性按 `xattr_permission` 检查权限（原来不查）；写、截短、chown 时去掉文件能力和 setuid、setgid 位（原来写文件不去掉） |
| [M9 之前：接口和地址的 netlink 消息](rounds/101-link-addr.md) | 2026-10-29 | 地址消息补上广播地址、标志、`ifa_cacheinfo`，与 Linux 逐字节相同；链路消息补上状态、qdisc、发送队列长度等，`ip addr`、`ip link` 显示 `brd`、`state UP`；iproute2 的 `ip link show eth0` 发的单条 `RTM_GETLINK` 原来是 EOPNOTSUPP |
| [M9 之前：`/proc/net/snmp` 和输入结束时的 EOF](rounds/102-snmp-pty-eof.md) | 2026-10-29 | 新增 `/proc/net/snmp`、`netstat`（字段照 Linux 6.1，计数是内核层自己数的连接、段、数据报和字节，`ss -s` 原来报错）和只有表头的 `arp`；`appine-run --pty` 的 stdin 结束之后，等程序读完输入再送一个 VEOF（与 `script` 相同），原来程序一直等下去 |
| [M9 之前：AI 编程 CLI](rounds/103-ai-cli-survey.md) | 2026-10-29 | 模拟服务冒充三家的模型 API，Claude Code、Codex、Gemini CLI 在 appine 里调用工具（命令、读写改文件、grep、glob）与原生相同，交互界面也相同；两家用 bubblewrap 的沙箱与 docker 容器里一样建不了；补上 bubblewrap 先读的 `/proc/sys/kernel/overflowuid` 等 |
| [M9 之前：关掉主设备时挂断从设备](rounds/104-pty-hangup.md) | 2026-10-29 | 最后一个主设备关掉时与 Linux 的 `tty_vhangup` 一样：会话组长收到 SIGHUP、SIGCONT（它退出时再发给挂断时的前台进程组），会话失去控制终端，从设备读到 EOF、写和 ioctl 是 EIO，`/dev/pts/<n>` 删掉（原来 `tmux kill-server` 之后 opencode 留下来）；appine-run 输入结束时的 VEOF 落在规范模式里丢掉时再送一次 |
| [M9 之前：mount 的子集](rounds/105-mount-subset.md) | 2026-10-29 | procfs、sysfs、devpts、cgroup2 可以挂到任意目录上，它们里面的目录可以 bind（`--rbind` 带上挂在里面的）；合成节点带着经由的挂载，`..`、getcwd 经由挂载点回去，mountinfo 相对进程的根目录生成；卸下、EBUSY、只读挂载、新实例按 Linux 6.1：从 Alpine chroot 进 Debian 挂上之后 ps、top、free、findmnt 能用；仓库里的目录还不能 bind、没有 tmpfs |
| [M9 之前：代码缓存只算一次](rounds/106-code-cache-rss.md) | 2026-10-30 | 拆开量多进程负载多占的内存：Tier 1 代码缓存的写入、执行两个视图在宿主的 RSS 里各算一次，翻译多算了一倍；引擎记下写入视图里写过的页，定时器线程每秒拆一次页表项，OOM killer 和 cgroup 的用量扣掉还没拆的：20 个空闲的 python3 的 App 用量 321 → 218 MB，Codex 的界面 294 → 208 MB，计时不变（按写入量拆时 Maven 慢 3%，没有采用）；16K 页的宿主上清空实例时交还代码缓存 |
| [M9 之前：翻译代码的密度](rounds/107-code-density.md) | 2026-10-30 | 块尾的桩占翻译后代码的四成多：链接桩、退出桩、回填桩里的 guest pc 和退出原因改放在桩末尾的数据字里，由例程读出，回填失败时不再另带退出桩，`bl` 从落点读返回地址；每条 guest 指令翻译成的宿主字数 7.5–8.3 → 约 6，20 个 python3 的代码 100 → 78 MB，并行编译的峰值 185 → 155 MB，Maven、Gradle 快 3–5% |
| [M9 之前：同一个程序共用翻译](rounds/108-shared-translation.md) | 2026-10-30 | 同一个程序的进程在同样的地址映射着同样的可信文件时共用一个 Tier 1 实例：映射了 JIT、写过的代码或不一致的文件的成员离开组（shootdown 之后换空间），第二个成员加入时作废登记范围以外的块；20 个空闲 python3 的 App 用量 193 → 84 MB，`xargs -P4` 编译时代码缓存的峰值 155 → 53 MB、快 13%，shell 起进程快三成；第一个进程也记下程序文件 |
| [M9 之前：起进程和 mmap 的持锁时间](rounds/109-spawn-mmap-bkl.md) | 2026-10-30 | 重新量了 BKL：线程开始时持锁读宿主 `/proc` 取上下文切换次数的基数（改用 getrusage），私有匿名的宿主内存交还时丢弃内容留着复用（mmap 每次持锁 2.5 → 1.3 µs），挂上翻译挪到锁外；4 个同时 exec 414–474 → 376–402 µs，4 个同时跑管道少 14%，`xargs -P4` 快 2% |
| [M9 之前：openat 的开销](rounds/110-openat-cost.md) | 2026-10-30 | 分阶段量了 openat：`go build` 里合计只占墙钟的 1.3%，新建文件一次 55 µs（宿主上建 object 约 27 µs、SQLite 约 16 µs，缓存是冷的），打开已有文件 7.6 µs；没有便宜的改法，没改代码 |
| [M9 之前：检查点挪到后台](rounds/111-background-checkpoint.md) | 2026-10-30 | SQLite 的检查点（要 fsync）由仓库的后台线程做，不在持着 BKL 的提交里：写入停下之后补写回让 WAL 从头再用，一直在写时到 1 万页由提交的线程就地补上；解包时持锁的提交最长 8.4 → 0.6 ms |
| [M9 之前：宿主内存的接缝](rounds/112-host-seams.md) | 2026-10-30 | mremap 扩展、`mmap(MAP_FIXED)` 分块提交留下宿主地址不连续的接缝，访存组跨接缝失败就被拆掉（微基准里之后慢四成）；JVM、Go 九成以上的拆组来自接缝，但完全不拆组时只有 Maven 快 1.4%，没改代码 |
| [M9 之前：Go 换一组被占用的寄存器](rounds/113-go-registers.md) | 2026-10-30 | Go 的编译器固定用 Tier 1 占用的 x16、x26–x28（原生的 compile 里 15.5% 的执行时间），几乎不用 x19–x25：execve 时认出 Go 的 build ID 注记，Tier 1 改占 x21–x25，翻译模板、池里的实例按寄存器分配区分；`go build` 快 3–5%，计算型的 Go 程序快 2–3%，lockstep、差分测试、LTP 另用 Go 的一组跑所有程序 |
| [M9 之前：跳到未对齐的地址](rounds/114-unaligned-branch.md) | 2026-10-30 | guest 的间接跳转到地址 1 时 Tier 1 跳进 NULL（跳转缓存的空项）或者作废了的块（翻译表的墓碑），进程以内部错误结束；空项的 pc 改成各项不同、命中不了的值，查表时先排除未对齐的 pc，与 Linux 一样是 SIGBUS |
| [M9 之前：有 JIT 代码的进程也共用翻译](rounds/115-shared-with-jit.md) | 2026-10-30 | 组里的成员映射 JIT 等不可信的可执行内存时不再离开组：每个成员一个标签，登记过的文件映射里的块共用，别处的块按 pc 和标签私有，一次探测比两个键，共用块不链接到私有块；node 的翻译 89–99% 在程序文件里，4 个空闲的 node 的代码缓存 70 → 21 MB、App 用量 190 → 119 MB，计时不变 |
| [M9 之前：仓库里的目录 bind](rounds/116-store-binds.md) | 2026-10-30 | 为了让 bubblewrap（Codex、Claude Code 的沙箱）能用拆成几轮，这是第一轮：仓库节点带上挂载，仓库里的目录、文件可以 bind（`--rbind` 带上挂在里面的），只读、noexec、nosuid、nodev、nosymfollow 对仓库按经由的挂载生效（顺序照 Linux，`mkdir -p` 碰到已有的目录照常），跨挂载的 rename、link 是 EXDEV，`MS_MOVE`、pivot_root；卸下的挂载留着记录，`..` 不出挂载 |
| [M9 之前：tmpfs](rounds/117-tmpfs.md) | 2026-10-30 | 让 bubblewrap 能用的第二轮：tmpfs 是仓库里不挂在根目录下面的一棵树（上级是自己的目录，记在 `detached_roots` 里），每次挂上是新实例、有自己的设备号，挂载和 bind 都没了时连同里面的一起删掉、开机删上次留下的；选项和 mountinfo 照 Linux 6.1 的 shmem；差分测试里照 bubblewrap 的步骤换根（tmpfs 做底、两次 pivot_root、只读 bind、`--dev`、`--proc`）与原生相同 |
| [M9 之前：挂载命名空间](rounds/118-mount-namespaces.md) | 2026-10-30 | 让 bubblewrap 能用的第三轮：`clone`、`unshare` 的 `CLONE_NEWNS` 复制整棵挂载树（挂载命名空间放在 `FsContext` 里，挂载记着所属的命名空间，没人用了就卸下），mountinfo 和 mount、umount、pivot_root 只看自己的命名空间；`/proc/<pid>/ns/*` 和 nsfs 文件、setns(2)；`/proc/<pid>/root`、`cwd` 改成魔法链接 |
| [M9 之前：用户命名空间](rounds/119-user-namespaces.md) | 2026-10-30 | 让 bubblewrap 能用的第四轮：`CLONE_NEWUSER`（不要特权），凭据记着用户命名空间，对外的 ID 按 uid_map、gid_map 换算，`/proc/<pid>/uid_map`、`gid_map`、`setgroups` 照 Linux 6.1 读写；74 处能力检查分成 `capable()`、`capable_wrt_inode_uidgid`、`ns_capable` 三种；顺带照 Linux 补上 ptrace 访问检查的 commoncap 一步、setuid 之后不可 dump、execve 恢复可以 dump |
| [M9 之前：pid 命名空间](rounds/120-pid-namespaces.md) | 2026-10-30 | 让 bubblewrap 能用的第五轮：`CLONE_NEWPID`（clone、unshare、setns），内核里的 pid 仍是全局的，每个命名空间记一张本层编号的表，系统调用的参数、返回值、siginfo、wait、`SO_PEERCRED` 等在边界换算；1 号进程收养孤儿、不接收按默认方式处理的信号，它退出时结束里面的进程、等它们被回收；procfs 按挂上它的进程所在的命名空间编号，status 补上 `NSpid` 等四行 |
| [M9 之前：UTS 和 IPC 命名空间](rounds/121-uts-ipc-namespaces.md) | 2026-10-30 | 让 bubblewrap 能用的第六轮：`CLONE_NEWUTS`、`CLONE_NEWIPC`（clone、unshare、setns），按线程记；每个 UTS 命名空间有自己的主机名、域名，每个 IPC 命名空间有自己的 System V IPC 对象、POSIX 消息队列和限制，共享内存段的映射记着段所在的命名空间；sethostname 按命名空间的属主查能力 |
| [M9 之前：网络命名空间](rounds/122-net-namespaces.md) | 2026-10-30 | 让 bubblewrap 能用的第七轮：`CLONE_NEWNET`（clone、unshare、setns），新命名空间里只有 lo；socket 仍是宿主 socket，记着建立时的命名空间，地址在边界上换算（127.0.0.x ↔ 宿主上给它的 127.a.b.x，::1、:: ↔ IPv4 映射的 127.a.b.1），别的地址到不了；lo 经由 netlink（bwrap 的 `loopback_setup`）、接口 ioctl 配置，netlink、ioctl、`/proc/<pid>/net` 只看到它；AF_UNIX 的抽象地址按命名空间分开 |
| [M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱](rounds/123-bwrap.md) | 2026-10-30 | 让 bubblewrap 能用的最后一轮：端到端试 bwrap、Codex、Claude Code 的沙箱，补上 cgroup 命名空间、经由 `/proc/self/fd/N` 挂文件时 mountinfo 里的挂载点、remount 不往上找；Codex 的 `-s workspace-write`、`codex sandbox`、Claude Code 的 `sandbox.enabled` 在 appine 里都能用；新的差分测试组 `bwrap` |
| [M9 之前：V8 和 HotSpot 的寄存器](rounds/124-v8-hotspot-registers.md) | 2026-10-30 | 原生采样时记下 JIT 代码的字节（jitdump、JVMTI 代理）：V8 的 x26–x28、HotSpot 的 x27、x28 多是只读的基址，被 Tier 1 占用代价小，按"用到"挑的组合实测反而让 TurboFan 的循环慢 7–40%；"写被占用寄存器"的比例与实测一致，默认的一组对 HotSpot 已经最好，V8 换 Go 的一组只让 tsc 快 2–4%，不换 |
| [M9 之前：raw ICMP 的模拟](rounds/125-raw-icmp.md) | 2026-10-30 | guest 的 raw ICMP socket 在宿主上是 ping socket，收到的回答把 id 换回 guest 发的、IPv4 补上 IP 头（TTL、目的地址取自宿主的控制消息），`ICMP_FILTER`、`ICMPV6_FILTER`、`IPV6_CHECKSUM` 记在内核层：Alpine 默认的 busybox `ping` 原来一个回答也收不到，`ping6` 一开始就退出，现在与原生相同；建 raw socket 要 `CAP_NET_RAW`（原来不查） |
| [M9 之前：PR_SET_CHILD_SUBREAPER](rounds/126-child-subreaper.md) | 2026-10-31 | 进程可以设成 subreaper：孤儿先过继给同一个 pid 命名空间里最近的、还活着的 subreaper 祖先，没有才给 1 号进程（tini、s6 这类进程管理程序用它，原来 EINVAL） |
| [M9 之前：LTP 剩下的小差异](rounds/127-ltp-leftovers.md) | 2026-10-31 | 剩下的 LTP 失败逐项在原生上对照，多数是 musl 的行为；改了 6 处：readlink 的空路径、TCP 的 sendto 不把地址交给宿主（开发宿主的 AppArmor 拒绝 AF_UNSPEC）、`SHM_HUGETLB` 是 EINVAL、name_to_handle_at 和 open_by_handle_at 的错误顺序和 ESTALE、`O_APPEND` 文件上的 pwrite64 照 Linux 写到末尾；差分测试的脚本超过 128 KB 时报失败（原来两边都执行不了也算通过） |
| [M9 之前：traceroute 和 mtr](rounds/128-traceroute.md) | 2026-10-31 | 每个网络命名空间一个 ICMP 汇集点：raw socket 背后的 ping socket、设过 TTL 的 UDP socket 在宿主上打开 `IP_RECVERR`，到了的报文和错误拼回的 ICMP 错误报文分给所有 raw socket，替 guest 模拟 Linux 不开 `IP_RECVERR` 时的错误规矩；raw socket 不再把端口交给宿主，guest 的 `IP_RECVERR` 有了错误队列，非 ICMP 的 raw socket 与 Android 上一样是 EPERM：busybox 的 traceroute、Debian 的 `traceroute -I`、开发宿主上的 mtr 能用了；新的网络命名空间里 `ping6 ::1` 能用了 |
| [M9 之前：命名空间文件留住命名空间](rounds/129-namespace-files.md) | 2026-10-31 | 打开着的、bind 挂载出去的命名空间文件持有命名空间，里面的进程都退出之后照样能 setns 进去，`ip netns add`、`exec`、`list` 和 busybox 的 `unshare --net=<文件>`、`nsenter` 能用；挂载命名空间的文件不能经由挂载留住自己；`NS_GET_USERNS`、`NS_GET_PARENT`、`NS_GET_NSTYPE`、`NS_GET_OWNER_UID`；RTM_GETNSID；F_GETFL 的 `O_LARGEFILE` 只在 open 打开的文件上 |
| [M9 之前：vhangup](rounds/130-vhangup.md) | 2026-10-31 | `vhangup(2)`、`TIOCVHANGUP` 按 Linux 的 `tty_vhangup`：终端记着被挂断过几次，只挂断挂断时打开着的文件，会话组长收到 SIGHUP、SIGCONT，会话失去控制终端，没读的输入丢掉；节点还在，重新打开照常能用 |
| [M9 之前：tmpfs 的用量和限制](rounds/131-tmpfs-usage.md) | 2026-10-31 | tmpfs 与 Linux 一样按页数、inode 数记用量和限制：statfs 报告用量，新建、链接放不下和写满了是 ENOSPC（能写的写进去），fallocate 放不下是 ENOSPC；重新挂载可以改 `size=`、`nr_inodes=` |
| [M9 之前：setns 经由 pidfd](rounds/132-setns-pidfd.md) | 2026-10-31 | setns 接受 pidfd，按 Linux 的顺序逐个检查、全部通过才一起换成那个进程的几种命名空间，一起进用户命名空间时按那里的能力算：util-linux 2.41 起的 `nsenter -t`（Alpine 3.24、Debian 13）原来每次都失败，现在能用；多线程进 time 命名空间是 EUSERS |
| [M9 之前：点积指令 ASIMDDP](rounds/133-asimddp.md) | 2026-10-31 | 宿主有点积指令时宣告 ASIMDDP（HWCAP、ID 寄存器、cpuinfo），SDOT、UDOT 由原生指令桩执行、Tier 1 原样拷贝；libvpx 的 VP9 编码在 appine 里快 4.5%（原生 11%） |
| [M9 之前：task 目录的 children](rounds/134-task-children.md) | 2026-10-31 | `/proc/<pid>/task/<tid>/children` 列出线程建的子进程：子进程记着父线程，那个线程退出时转给进程里还活着的线程、排到最后，过继、`CLONE_PARENT`、非主线程 exec 都与 Linux 相同 |
| [M9 之前：SHA3、SHA512 和 I8MM](rounds/135-sha3-sha512-i8mm.md) | 2026-10-31 | 宿主有时宣告 SHA3、SHA512（两者一起）和 I8MM（`AT_HWCAP2` 不再总是 0），放行它们的编码；OpenSSL 的 SHA-512 在 appine 里快 4.1 倍，与原生持平 |
| [M9 之前：LRCPC](rounds/136-lrcpc.md) | 2026-10-31 | 宿主有时宣告 LRCPC：LDAPR、LDAPRH、LDAPRB 在 Tier 0 里照 LDAR 读，在 Tier 1 里检查过地址之后原样执行；§4.4 的 v1.x 特性都做完了 |
| [M9 之前：PDEATHSIG 和 __WNOTHREAD](rounds/137-pdeathsig-wnothread.md) | 2026-10-31 | `PR_SET_PDEATHSIG` 按线程记，父进程里建立它的线程一退出就发（`SI_USER`，发送者是父进程）；wait 的 `__WNOTHREAD` 只等调用的线程建的子进程 |
| [M9 之前：LRCPC2](rounds/138-lrcpc2.md) | 2026-10-31 | 宿主有时宣告 LRCPC2（`ilrcpc`）：带 9 位偏移的 LDAPUR、LDAPURS、STLUR 在 Tier 0 里照 LDAR、STLR 读写，在 Tier 1 里检查基址加偏移之后去掉偏移原样执行 |
| [M9 之前：未实现的系统调用](rounds/139-unimplemented-syscalls.md) | 2026-10-31 | 只有文档：对照 Linux 6.1 的系统调用表补记返回 ENOSYS 的编号；openat2、adjtimex、ioprio、syslog、process_madvise、process_mrelease 与 6.1 不同，其余与没配对应功能的 Linux 一样 |
| [M9 之前：ioprio、syslog 和 adjtimex](rounds/140-ioprio-syslog-adjtimex.md) | 2026-10-31 | ioprio_set、ioprio_get 按线程记、交给宿主线程（`ionice` 原来设不上就不执行命令）；syslog(2) 回答空的日志，权限、控制台级别按 6.1，新增 `/proc/sys/kernel/printk`、`dmesg_restrict`；adjtimex、clock_adjtime 回答查询（Linux 宿主上取宿主的 NTP 状态，Android 上按未同步合成），要改东西的是 EPERM |
| [M9 之前：FP16 和 JSCVT](rounds/141-fp16-jscvt.md) | 2026-10-31 | 宿主有时宣告半精度浮点运算（FPHP、ASIMDHP，一起宣告）和 FJCVTZS：只放行编码，FPCR 的 FZ16 可写；llama.cpp 的 ggml 要 FP16 才会用 armv8.2_1 以上的后端；llama.cpp 的 F16 模型提示处理快 82%、生成快 44%（Qwen2.5-0.5B，Q4_0 不变） |
| [M9 之前：RDM、FCMA、FHM 和 BF16](rounds/142-rdm-fcma-fhm-bf16.md) | 2026-10-31 | 宿主有时宣告的另外四个 SIMD 扩展：只放行编码，FHM 只和 FP16 一起宣告；在 appine 里 `gcc -mcpu=native` 也用上它们；BF16 点积用 BFDOT 原生快 2.4 倍、appine 里快 1.5 倍，另外三种在 M2 上差别在 5% 以内 |
| [M9 之前：/dev/kmsg 和内核日志](rounds/143-kmsg.md) | 2026-10-31 | 内核层有了自己的日志（6.1 的 printk 环形缓冲区）：`/dev/kmsg` 的读写、lseek、poll 和限速，syslog(2) 的各个动作、`/proc/kmsg`、`printk_devkmsg`；OOM killer 和内部错误结束进程记进去，guest 里 `dmesg` 看得到；差分测试的镜像不再带上仓库 /tmp 里的东西 |
| [M9 之前：openat2](rounds/144-openat2.md) | 2026-10-31 | openat2 按 6.1 实现：`open_how` 和标志的检查，路径解析里的 `RESOLVE_BENEATH`、`IN_ROOT`、`NO_SYMLINKS`、`NO_MAGICLINKS`、`NO_XDEV`（含 `nd_jump_root` 在 nd->root 还没取过时的 EXDEV）；GNU tar 的新版本配 glibc ≥ 2.43 离不开它，bubblewrap 0.12 原来走退路 |
| [M9 之前：termios2](rounds/145-termios2.md) | 2026-10-31 | `TCGETS2`、`TCSETS2`、`TCSETSW2`、`TCSETSF2` 和 termios 里的速率（按 `CBAUD`、`CIBAUD` 换算，`BOTHER`）：glibc 2.42 起 `tcgetattr`、`isatty`、`tcsetattr` 只用它们，Ubuntu 26.04 里原来每个终端都"不是终端"；PTY 总是 `CS8`、`CREAD`，地址位改不了 |
| [M9 之前：PTY 的包模式](rounds/146-pty-packet-mode.md) | 2026-10-31 | 主设备的 `TIOCPKT`、`TIOCGPKT`：每次读先给一个状态字节（从设备这一边的清空、^S/^Q 停住和恢复、流控的开关、EXTPROC），有状态时 poll 带 POLLPRI；顺带补上 `TIOCGPTLCK`、`TIOCSIG`。GNU screen 原来一个窗口也开不了 |
| [M9 之前：/proc/<pid>/exe 的跟踪权限](rounds/147-proc-exe-access.md) | 2026-10-31 | 读 `/proc/<pid>/exe` 和经由它打开、执行，与 `root`、`cwd` 一样要能以 `PTRACE_MODE_READ_FSCREDS` 看那个进程（别的用户的、不可 dump 的是 EACCES） |
| [M9 之前：用户命名空间里的 ID 换算](rounds/148-userns-ids.md) | 2026-10-31 | `SO_PEERCRED`、`SCM_CREDENTIALS`（声明的凭据按 `scm_check_creds` 检查，没声明时是真实 ID）、信号和 waitid 的 si_uid（SIGCHLD 原来总是 0）、SysV IPC 的属主、auxv 的 `AT_UID`、core dump、`/proc/net/tcp` 的 uid 按命名空间换算 |
| [M9 之前：Ubuntu 26.04 的普查](rounds/149-ubuntu26-survey.md) | 2026-10-31 | glibc 2.43、Rust 写的 coreutils 的 Ubuntu 26.04 当仓库，apt 装了 238 个包，常用命令与原生相同；tar 已经离不开 openat2；找到三处：从设备没打开过时主设备就 EIO（screen 时好时坏）、短的符号链接报了数据块、新分配的 PTY 的窗口大小 |
| [M9 之前：新分配的 PTY](rounds/150-new-pty-state.md) | 2026-11-01 | 主设备只在从设备打开过、又全部关掉之后才 EIO、POLLHUP（Linux 的 `TTY_OTHER_CLOSED`），原来没打开过时就是，GNU screen 的窗口时好时坏；新终端的窗口大小从 0×0 开始 |
| [M9 之前：短的符号链接不占块](rounds/151-short-symlink-blocks.md) | 2026-11-01 | 目标短于 60 字节（仓库，对应 ext4）、128 字节（tmpfs）的符号链接 `st_blocks` 是 0，`ls -l` 的 total 与原生相同 |
| [M9 之前：nmap 以 root 运行](rounds/152-nmap-unprivileged.md) | 2026-11-01 | nmap 以 root 运行时默认要 raw socket、打不开就退出；App 给会话设 `NMAP_UNPRIVILEGED=1`，改用 connect 扫描，与没有 CAP_NET_RAW 的原生 root 相同 |
| [M9 之前：/proc/<pid>/exe 指向程序文件本身](rounds/153-exe-file.md) | 2026-11-01 | exe 照 Linux 的 `mm->exe_file` 指向程序文件本身：删除之后带 ` (deleted)`，改了名、被删除、被 rename 换掉之后经由它 stat、open、execve 到的仍是原来那个（删除了的经由记着的宿主文件） |
| [M9 之前：FRINTTS、FlagM、SB 和 DPB](rounds/154-frintts-flagm-sb-dpb.md) | 2026-11-01 | 宿主有时宣告 FRINTTS（FRINT32Z 等）、FlagM（CFINV、RMIF、SETF8/16）、FlagM2（XAFLAG、AXFLAG）、SB、DPB/DPB2（DC CVAP、CVADP）；`/proc/cpuinfo` 的名字表补到 `sb`，`gcc -mcpu=native` 与原生相同 |
| [M9 之前：DIT](rounds/155-dit.md) | 2026-11-01 | 宿主有时宣告 DIT：MSR、MRS DIT；信号处理函数带着它、sigreturn 和 ptrace 照 pstate 第 24 位恢复，fork、线程继承，execve 清零；宿主线程的 DIT 跟着 guest 的 |
| [M9 之前：低端口映射和监听通知](rounds/156-low-ports.md) | 2026-11-01 | guest 绑 1024 以下的端口时宿主上换成 +10000（被占依次 +20000、+30000、宿主分配），guest 一侧看不出来；App 第一次看到非回环地址上的监听时发通知，告诉用户别的设备连哪个端口 |
| [M9 之前：podman 和 docker 的普查](rounds/157-container-survey.md) | 2026-11-01 | vfs 存储、host 网络、不用 cgroup 时 podman 的常用功能与原生相同（改掉 capget 等四处小问题、临时绕过 mqueue 和可写挂载 cgroup2 之后）；docker 停在 runc 经由 fd 执行 memfd；找到 12 个问题，按顺序排好 |
| [M9 之前：容器运行时碰到的四处小问题](rounds/158-container-small-fixes.md) | 2026-11-01 | capget 空数据指针探测版本返回 0；开机时的命名空间里什么也不改的 `RTM_NEWLINK`、`RTM_SETLINK` 成功（dockerd 拉起 lo）；合成文件系统根目录的 inode 是 1（runc 检查 /proc）；线程目录里有 fd、fdinfo |
| [M9 之前：可写地挂 cgroup2](rounds/159-cgroup2-rw-mount.md) | 2026-11-01 | 不带只读也挂得上 cgroup2（crun 给容器这样挂），里面照旧改不了 |
| [M9 之前：mqueue 文件系统](rounds/160-mqueue-fs.md) | 2026-11-01 | `mount -t mqueue` 列出挂上它的进程所在的 IPC 命名空间里的 POSIX 消息队列（每个命名空间一个实例）：建、删、chmod、chown，打开得到 mq 描述符；podman 不再需要临时绕过就能起容器 |
| [M9 之前：经由 fd 执行 memfd 和已经删除的文件](rounds/161-exec-open-file.md) | 2026-11-01 | fexecve、`/proc/<pid>/fd/<n>` 用打开文件的宿主文件执行；memfd 的 fd 与 Linux 一样不算写者；runc 起容器、`podman build --isolation chroot` 要用 |
| [M9 之前：time 命名空间](rounds/162-time-namespaces.md) | 2026-11-01 | unshare（clone3）的 `CLONE_NEWTIME`、`/proc/<pid>/timens_offsets`、setns；单调时钟和 boottime 加偏移（含快速路径），绝对时刻的等待减偏移，uptime、sysinfo、启动时间也按它；6.2 起的放宽（线程、vfork 留在原来的，execve 再进）；dockerd、runc 要用 |
| [M9 之前：cgroup 树：建子 cgroup、迁移进程](rounds/163-cgroup-tree.md) | 2026-11-01 | `/sys/fs/cgroup` 可写：mkdir、rmdir 子 cgroup，经由 `cgroup.procs`、`cgroup.threads`、clone3 的 `CLONE_INTO_CGROUP` 迁移进程，`subtree_control` 的规则，`cgroup.kill`，cgroup 命名空间以所在的 cgroup 为根；限制只按格式记下（下一步起作用）；`oom_score_adj` 照 Linux 解析（runc 写的带 NUL）；dockerd 建得了容器的 cgroup |
| [M9 之前：cgroup 的 pids.max 和 memory.max 起作用](rounds/164-cgroup-limits.md) | 2026-11-01 | fork、建线程按各层的 `pids.max` 检查（EAGAIN，僵尸进程也算）；OOM killer 也量设了 `memory.max` 的 cgroup，超出时在里面挑一个结束（`memory.events` 往上累加、`memory.oom.group` 整组结束、调低上限时马上结束）；量的开销压在 BKL 时间的 5% 以内 |
| [M9 之前：cgroup.freeze](rounds/165-cgroup-freeze.md) | 2026-11-01 | 冻结的进程在返回 guest 之前停下（与停止信号同一处等待，父进程、wait 看不到），迁移、fork 跟着 cgroup，`cgroup.events` 的 frozen；被打断的系统调用照 Linux 重新执行：epoll_wait 改成 EINTR，ppoll、pselect6 写回剩下的时间，有超时的 futex 等待经由 restart_syscall 保持到期时刻 |
| [M9 之前：cgroup 的设备过滤（eBPF）](rounds/166-bpf-devices.md) | 2026-11-01 | bpf(2) 的子集：`BPF_PROG_TYPE_CGROUP_DEVICE` 的加载（校验器的错误码与 Linux 相同）、挂到 cgroup、查询、按编号取、取信息，解释执行；打开设备、mknod 时按 cgroup 的有效程序放行；伪文件从偏移 0 再读时重新生成（seq_file）；docker、podman 不用 `--cgroups disabled` 也能跑容器，限制、OOM、pause 都起作用 |
| [M9 之前：devpts 的实例](rounds/167-devpts-instances.md) | 2026-11-01 | 每次挂 devpts 是一个新的实例：自己的一套终端（编号从 0 开始）、设备号和选项（uid、gid、mode、ptmxmode、max），ptmx 设备节点按同目录的 `pts` 找实例，fd 链接、ttyname 按经由的挂载；TIOCGPTPEER；开着终端时卸不下；容器里只看得到自己的终端 |
| [M9 之前：open_tree、move_mount 和 mount_setattr](rounds/168-open-tree.md) | 2026-11-01 | open_tree（`OPEN_TREE_CLONE` 复制出不挂在任何地方的树，fd 关掉就拆）、move_mount 接上或者挪、mount_setattr 改挂载的标志；删除挂载点只看自己的命名空间，别处的随之卸下；用户命名空间里的 root 能装 seccomp 过滤器；podman `--uidmap` 的容器起得来了 |
| [M9 之前：容器网络的普查](rounds/169-container-network-survey.md) | 2026-11-01 | docker、podman 的 bridge 网络依赖什么：netlink 建 bridge 和 veth、按接口的 sysctl；docker 关掉 iptables 也在容器里写内置 DNS 的 nat 规则，`-p` 用 docker-proxy；podman 按名字解析靠 aardvark-dns，`-p` 只靠 nftables；appine 里两家都卡在建 bridge；地址映射方案的四步 |
| [M9 之前：有状态的网卡](rounds/170-netdev.md) | 2026-11-01 | 地址映射方案的第一步：bridge、veth、dummy 的建删改照 Linux 6.1 记在内核层，veth 的对端可以在别的命名空间里，载波和状态、地址、按地址加的和经由 netlink 加的路由、IPv6 的链路本地地址、extack、命名空间的编号；`/sys/class/net` 按读者的命名空间、bridge 的 `brif`；新的 `/proc/sys/net`（接口的配置、`ip_forward`、`ip_unprivileged_port_start` 等）；docker、podman 的 bridge 网络建得起来，容器里的地址和路由与原生相同 |
| [M9 之前：经由虚拟网卡的连接](rounds/171-netmap.md) | 2026-11-01 | 地址映射方案的第二步：虚拟地址在 socket 层换成宿主上的回环地址（新命名空间的都是它的 127.a.b.1，开机时的命名空间的各分一个 127.255.x.y），低端口映射按目的地的命名空间，经由默认路由出去的交给宿主网络，反过来按"谁在看"换回；绑通配地址的数据报 socket 宿主上也绑通配地址、发往映射的目的地时指定源地址；`SO_ATTACH_FILTER` 的指针；容器之间、外层到容器、docker 的 `-p`、podman 的按名字解析都通了 |
| [M9 之前：nftables 的子集和 socket 层的 DNAT](rounds/172-nftables.md) | 2026-11-01 | 地址映射方案的第三步：`NETLINK_NETFILTER` 上 nftables 的表、链、规则、集合照 6.8 的内核记在每个网络命名空间里（批、生成号、句柄、转储、extack 的偏移），iptables-nft 的 `nft_compat`；connect、sendto 时执行 OUTPUT 钩子上的 nat 链，DNAT 在 socket 层换目的地址、收到的来源换回；docker 自定义网络按名字解析、`"iptables": true` 的 dockerd、podman 的 nftables 驱动（含经由 OUTPUT 的 `-p`）都能用了；修了新命名空间里发给自己虚拟地址的 UDP 收不到回答 |
| [M9 之前：发布的端口：PREROUTING 的 DNAT 和宿主上的转发](rounds/173-port-forwarding.md) | 2026-11-01 | 地址映射方案的第四步：进入别的命名空间（虚拟地址、容器连外层地址）的连接、数据报执行那里的 PREROUTING nat（入口接口、redirect 到入口接口的地址）；开机时的命名空间里被 PREROUTING DNAT 的监听交给宿主上的转发线程，从 appine 外面（手机上的浏览器、别的设备）连进来的转给容器；MPTCP 的 socket 列进 `/proc/net/tcp`、低端口映射按 TCP；podman 的 `-p` 从外面也连得通，普查与原生一致 |
| [M9 之前：各个网络命名空间的端口互不相干](rounds/174-namespace-ports.md) | 2026-11-01 | 新命名空间里显式绑的端口在宿主上另选、登记在映射表里：外层（podman、docker-proxy）占着同一个端口时容器照样绑得上，`-p 8080:8080` 这类内外同端口的发布原来起不来；新命名空间里的 TCP 监听登记在命名空间里，连过去没有对得上的监听就是 ECONNREFUSED（不再被外层的通配监听接走），只绑 127.0.0.1 的服务从别处连不到 |
| [M9 之前：过滤链](rounds/175-filter.md) | 2026-11-01 | connect、sendto 时执行连接沿途经过的过滤链（发送方的 OUTPUT，经由开机时的命名空间的 FORWARD、INPUT，目的地的 INPUT，各自的 PREROUTING 上的 filter 链）；reject 带错误码，xt 的 REJECT、conntrack、state；UDP 回给收到过的对方是 ESTABLISHED；TCP 被丢弃的连到接受队列满了的宿主监听上、照 Linux 的重传超时；docker 网络之间的隔离与原生一致 |
| [M9 之前：从外面转发进来的连接带上真实的客户端地址](rounds/176-real-client-address.md) | 2026-11-01 | 转发线程接受的外面设备的 TCP 连接按连接分一个 127.254.x.y、绑客户端的端口再连容器，登记下来；容器里 accept、getpeername、`/proc/net` 报告客户端真实的地址（原来是网关），回环地址来的照旧 |
| [M9 之前：网络命名空间的 sysctl 补齐、UDP 也查有没有人绑](rounds/177-net-sysctls.md) | 2026-11-02 | `/proc/sys/net` 的 `core/`、`ipv4/`、`ipv6/`、`unix/` 下照 Linux 补齐（140 项加开机时的命名空间才有的 5 项，值按命名空间记、只记不起作用），docker 的 `--sysctl`、compose 的 `sysctls:` 能用了；新命名空间里绑了的 UDP socket 也登记，发往没人绑的端口的悄悄丢掉；修了先绑通配地址的 UDP socket connect 时端口变了；podman 的 iptables 驱动普查与原生一致 |
| [M9 之前：测试提速，以及顺带找到的几个问题](rounds/178-test-speed.md) | 2026-11-02 | 全套从半个多小时降到约 9 分钟：差分测试并发跑（每个通道一份 APFS 克隆的仓库，长的组先开始），两个用例里白等的 100 秒和 10 秒去掉，colima 换成 8 CPU、8 GiB、40 GB 盘，LTP 的仓库放到虚拟机本地的卷上（virtiofs 上八路同时写 SQLite 等锁超时是 EIO）；修了端口映射释放之后对方才读到的回答换不回 guest 端口、`tcp_ehash_entries` 等按内存定的 sysctl 写死；fork 照 Linux 按合起来的 VMA 估算提交量，LTP 的 fork14 不再跑到 150 秒超时 |
| [M9 之前：页表按需建立，AddressSanitizer 能用了](rounds/179-demand-page-tables.md) | 2026-11-02 | soft-MMU 的页表原来按页建，ASan 一启动映射约 14 TB 的影子内存，一次 mmap 要建约 28 GB 页表，持着 BKL、OOM killer 插不进来，宿主杀掉整个 App；现在 1 MB 以上的映射等访问时一次补一张叶子表，fork 借页、量驻留内存只看有表项的几段（`PROT_NONE` 留着表项、权限清零），ASan、LSan 能用，LTP 的 fork14 通过；顺带修了 fork、mremap 申请宿主内存时漏看 `MAP_NORESERVE`（有大的不计提交量映射的进程 fork 是 ENOMEM） |
| [M9 之前：透明代理：SO_ORIGINAL_DST、SO_MARK 和 owner 匹配](rounds/180-original-dst.md) | 2026-11-02 | 实测 ShadowSocks、proxychains：SOCKS5、HTTP、UDP、proxychains 都能用，透明代理（`sslocal --protocol redir` 配 iptables 的 REDIRECT）拿不到原目的地址；现在 connect 时按宿主上的四元组记下被 nat 的连接，accept 时领走，`SO_ORIGINAL_DST` 照原生的语义回答（连接跟踪开不开按规则集判断，ip/ip6/inet 分开，跨命名空间各看各的）；`SO_MARK` 只记在 socket 上、带进 nftables，xt 的 owner、mark 匹配也认，按 uid、标记排除代理自己的连接都能用；测试时发现共用翻译的一个偶发 SIGILL（178 轮就有，下一轮查 → 修了） |
| [M9 之前：共用翻译的组里最后一个成员离开时作废别人留下的块](rounds/181-last-member-leaves.md) | 2026-11-02 | 上一轮发现的偶发 SIGILL：同一个程序共用翻译的组里，别的成员退出、离开之后，它们登记的映射和翻译的块还在；最后一个成员因为在登记过的地址上映射了别的文件而离开时留着原来的翻译，执行到了别人翻译的代码（几个 python3 加载的扩展模块不同，客户端执行了服务端的 select 模块的块）；现在离开时作废与它的映射对不上的块，原来的探测脚本 8 次都不再崩 |
| [M9 之前：容器连外网也经过外层的 PREROUTING](rounds/182-outer-prerouting.md) | 2026-11-02 | 透明代理的推迟项：容器经由外层的网关连外网时也执行外层的 PREROUTING nat，在外层对从 bridge 来的做 REDIRECT 就能透明代理所有容器（sslocal redir 实测），`udp dport 53 redirect` 能劫持容器的 DNS；docker、podman 的端口发布规则按地址类型限定，不受影响；顺带修了外层绑通配地址的 UDP 服务端回答容器时来源是 127.0.0.1（连接了的 socket 收不到）、外层 DNAT 到同一网段的容器时不过外层的 FORWARD、nft 的 `oif` 不认、DNAT 到外面的地址时绑了回环地址出不去 |
| [M9 之前：新命名空间里没绑定的 UDP socket 先发命名空间里、再发外面](rounds/183-udp-wide-first.md) | 2026-11-02 | 上一轮的推迟项：没绑定的 UDP socket 第一个数据报发往命名空间里的地址时绑了回环地址，之后再发往外面是 EINVAL；现在与先发往外面时一样在宿主上绑通配地址、按包指定源地址，顺序无关（发给自己的虚拟地址的不变）；实测 musl 的解析器、Node、Go、Java 本来就先绑了通配地址，碰到的是不先 bind 就 sendto 的程序 |

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
| futex_wait05、epoll_pwait03、epoll_wait02、clock_nanosleep02、nanosleep01、select02、pselect01、pselect01_64、poll02、prctl09 | 检查睡眠的超时精度，睡 1 毫秒量出 1.3–1.6 毫秒就失败，取决于虚拟机当时的计时延迟（M7 起）；[独占的唤醒](rounds/48-exclusive-wakeup.md)那一轮遇到过虚拟机里原生的 nanosleep 1 毫秒也要 1.5 毫秒，这几项连续失败，旧的二进制也一样；[线程退出时在 BKL 外丢掉弱引用](rounds/71-thread-exit-race.md)那一轮虚拟机空闲时原生的平均 1.58 毫秒，其中 7 项新旧二进制都失败；[路由表](rounds/98-routes.md)那一轮 select02 单独跑也失败，原生 1 毫秒睡眠的中位数 1.59 毫秒 |
| getrusage03 | 要求可用内存至少 512 MB，全量并发时不够就跳过，单独运行通过 |
| nice05 | 比较两个 nice 值不同的线程用掉的 CPU 时间，appine 不按 nice 分配 CPU 时差值的正负是随机的（M8 起）；[推迟项](rounds/19-deferred.md)那一轮把 nice 交给宿主线程之后通过 |
| exit_group01 | [终端的属主和权限](rounds/64-pty-owner.md)那一轮全量并发运行时以退出码 134（SIGABRT）失败一次，日志在重跑时被覆盖了（[测试自己的竞争和丢掉的现场](rounds/82-test-races.md)那一轮起 `scripts/ltp` 保留上一次的日志）；单独跑 8 次、在另外 7 路 LTP 测试的负载下顺序跑 30 次都通过，原因没查。多半是[线程退出时在 BKL 外丢掉弱引用](rounds/71-thread-exit-race.md)那一轮修的竞争：宿主的堆被破坏时 glibc 以 SIGABRT 结束 appine-run |
| mmap21_01、mmap21_02 | [文件操作的宿主开销](rounds/33-file-ops-host-cost.md)那一轮清理时 unlink `/dev/shm` 下的文件得到 EIO；单独重跑 3 次、与 `/dev/shm`、mmap、文件操作有关的 162 个测试 16 路并发重跑 3 次都没有再出现，10 月 24 日的基线里也有过一次（LTP 的仓库放在 virtiofs 上）。[ptrace 的第二步](rounds/41-ptrace-gdb.md)那一轮的日志是 `unlink(/dev/shm/ltp_mmap21_2)` 得到 ENOENT：LTP 的库把结果放在 `/dev/shm/ltp_<程序名>_<pid>`，mmap21_01 和 mmap21_02 是同一个程序，每个 appine-run 里的第一个进程都是 pid 2，并发运行时两个用例共用仓库的 `/dev/shm`、用同一个文件，一个删掉了另一个的。这是并发跑 LTP 的方式造成的，不是内核的问题；[`/proc/net/snmp` 和输入结束时的 EOF](rounds/102-snmp-pty-eof.md)那一轮是 `open` 这个文件得到 EEXIST，同一个原因；[低端口映射](rounds/156-low-ports.md)那一轮是 mmap21_01 失败（unlink 得到 ENOENT），同一个原因 |


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
| [同一个程序共用翻译](rounds/108-shared-translation.md) | shell 起进程 | 1.13×（原来 1.6×） |
| [Go 换一组被占用的寄存器](rounds/113-go-registers.md) | `go build` 冷编译 | 2.6×（默认的一组 2.7×） |
| [点积指令 ASIMDDP](rounds/133-asimddp.md) | `vpxenc` VP9 编码（单线程，360p 60 帧） | 2.2×（不宣告点积时 2.3×） |
| [SHA3、SHA512 和 I8MM](rounds/135-sha3-sha512-i8mm.md) | Python `hashlib.sha512`（32 MiB） | 1.0×（不宣告 SHA512 时 4.1×） |
| [FP16 和 JSCVT](rounds/141-fp16-jscvt.md) | llama.cpp `llama-bench`（4 线程）：Qwen2.5-0.5B F16 的提示 128、生成 32；Q4_0 的同样两项 | 1.64×、1.22×（不宣告 FP16 时 2.98×、1.74×）；1.38×、1.21× |
