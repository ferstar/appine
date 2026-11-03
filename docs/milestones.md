# appine 里程碑记录

每一轮（里程碑，以及 M8 之后、M9 之前的各轮补强）的结论、测量、实现要点和推迟项各记一篇，按完成顺序编号放在 [rounds/](rounds/) 里。这一页是索引，另记着各轮共用的[测量条件](#测量条件)、[已知的偶发失败](#已知的偶发失败)和 [R1 的测量记录](#r1-的测量记录)。里程碑的计划和排序依据见[设计文档](design/14-milestones.md) §14。

- 各篇是当时的记录。后来改动过的做法以设计文档第 1–13 节和 `.claude/maps/` 里各模块的 map 为准（已迁过去的节从设计文档删掉了：§4 CPU 引擎 → [engine](../.claude/maps/engine.md)，§5 内存 → [kernel/mm](../.claude/maps/kernel/mm.md)，§6 进程 → [kernel/process](../.claude/maps/kernel/process.md)，§7 信号与 ABI → [kernel/signal](../.claude/maps/kernel/signal.md)，§8 文件系统 → [kernel/store](../.claude/maps/kernel/store.md)、[kernel/vfs](../.claude/maps/kernel/vfs.md)、[kernel/procfs](../.claude/maps/kernel/procfs.md)）。设计文档只写现在的做法：和 Linux 一样的地方一句带过，只写 appine 自己的机制和与 Linux 不同的地方；改动的经过、测量和出处写在各篇里，不写进设计文档。
- 推迟项（以及正文里写着留到以后的事）后面用"→"注明后来在哪一轮补上、试过不做，或者只做了一部分；只做了一部分的写明还剩什么。没有"→"的就是还没做。M3b–M8 的"改动和推迟项"里标着"（修改 §x）"的是对设计的改动，不是推迟项。一轮做了以前推迟的事，要回到原来那一篇补上"→"。
- 文中的 §x 指写那一轮时设计文档的章节（迁到 map 的节以对应的 map 为准）；R1–R6、A1–A3 是设计文档 §16 的风险编号。
- 新的一轮：在 `rounds/` 里接着编号新建一篇，在下表加一行（要点一句话，写标题没说的结果或数字）；有新的倍数时在 R1 的测量记录里加一行。
- 各篇的结构：开头是导语，不加标题，写起因和结论；之后依次是「做法」「结果」「测试」「推迟项」四节（`##`），没有的省掉（M3b–M8 的最后一节叫「改动和推迟项」）。调查、实验的小节照内容起名（如「原因」「时间花在哪里」「试过没有采用：……」），放在「做法」前面。节里的子主题用 `- **标签**：` 列表项，大块的用 `###`。
- 各篇的测试只写测了哪些方面、测试中发现的问题和以后用得上的坑；逐条的用例、变异检验、cargo test、clippy、LTP 的例行结果不写。

| 一轮 | 日期 | 要点 |
|---|---|---|
| [M2 结果](rounds/01-m2.md) | 2026-10-24 | 原型 Tier 1 的整数基准几何平均 1.85×，Neoverse N2 上 2.37× |
| [M1a 结果](rounds/02-m1a.md) | 2026-10-24 | SIMD&FP、LL/SC、LSE；lockstep 扩展成三方比对 |
| [M1b 结果](rounds/03-m1b.md) | 2026-10-24 | busybox 经由 `ld-musl` 跑通；`gzip` 4.75× |
| [M3a 结果](rounds/04-m3a.md) | 2026-10-24 | 多进程、多线程、BKL、信号帧、vDSO |
| [M3b 结果](rounds/05-m3b.md) | 2026-10-24 | 进程内 PTY、作业控制、procfs |
| [M4 结果](rounds/06-m4.md) | 2026-10-24 | 网络、DNS 存根、AF_UNIX；`apk add`、`git clone` 能用；R5 关闭 |
| [M5 结果](rounds/07-m5.md) | 2026-10-24 | Android App；4K、16K 页模拟器通过 |
| [M6 结果](rounds/08-m6.md) | 2026-10-25 | 编译 Lua、SQLite 约 10×；凭据、记录锁、IPC、定时器；LTP 849 |
| [M7 结果](rounds/09-m7.md) | 2026-10-25 | Tier 1 正式版（SIMD、4096 项 TLB、W^X）；编译 4.7–6.1× |
| [M8 结果](rounds/10-m8.md) | 2026-10-25 | Go、Node.js 行为正确；LTP 856 |
| [M8 补测：JVM](rounds/11-m8-jvm.md) | 2026-10-25 | OpenJDK 21/25、Maven、Gradle 能用；补上 inotify |
| [M9 之前：稳定性和内存](rounds/12-stability-memory.md) | 2026-10-25 | 截短已映射的文件、`PROT_NONE` 不进页表、robust futex |
| [M9 之前：性能剖析](rounds/13-profiling.md) | 2026-10-25 | 元数据缓存、fork 只看碰过的页；`git status` 19× → 6× |
| [M9 之前：起进程的翻译开销](rounds/14-spawn-translation.md) | 2026-10-25 | 翻译跨 exec 保留；shell 起进程 9× → 6.3× |
| [M9 之前：栈指针常驻寄存器和返回预测](rounds/15-sp-return-prediction.md) | 2026-10-25 | SP 常驻 X26、影子返回栈；gcc 快 10% |
| [M9 之前：futex 定向唤醒](rounds/16-futex-targeted-wake.md) | 2026-10-25 | FUTEX_WAKE 只通知被唤醒的线程；Gradle 热构建 6.1 → 4.8 秒 |
| [M9 之前：兼容性补漏](rounds/17-compat.md) | 2026-10-25 | maps 命名、实时信号排队、pidfd、`execveat`；LTP 889 |
| [M9 之前：剩下的小缺口](rounds/18-small-gaps.md) | 2026-10-25 | `/dev/shm` 开机清空、smaps/pagemap、unshare；LTP 905 |
| [M9 之前：推迟项](rounds/19-deferred.md) | 2026-10-25 | `O_ASYNC`、租约、POSIX 消息队列、AIO；io_uring 不做 |
| [M9 之前：稳健性](rounds/20-robustness.md) | 2026-10-25 | 内核 bug 只结束一个进程；App 内存上限和 OOM killer |
| [M9 之前：稳健性的收尾](rounds/21-robustness-finish.md) | 2026-10-25 | 线程起停、定时器线程、JNI 里的 panic；libsigchain 下的恢复 |
| [M9 之前：栈的大小上限](rounds/22-stack-limit.md) | 2026-10-26 | 主线程栈按需增长，受 `RLIMIT_STACK` 限制 |
| [M9 之前：访存检查和翻译表](rounds/23-tlb-checks.md) | 2026-10-26 | 同一基址共用 TLB 检查、跨页不退出；gcc 3.6× |
| [M9 之前：多线程共用翻译和定向唤醒](rounds/24-shared-translation.md) | 2026-10-26 | 测量改用本地卷；`go build` 5.4× → 3.2×，Maven 8.5× → 4.4× |
| [M9 之前：不持 BKL 的 stat](rounds/25-stat-without-bkl.md) | 2026-10-26 | 并行 lstat 的 `git status` 24 → 13 毫秒 |
| [M9 之前：fork 写时复制](rounds/26-fork-cow.md) | 2026-10-26 | 写过 100 MB 的进程 fork 21 → 2.3 毫秒 |
| [M9 之前：交错访存共用检查](rounds/27-interleaved-checks.md) | 2026-10-26 | python 循环 0.82 → 0.75 秒；另两个实验没有收益 |
| [M9 之前：起进程的宿主开销](rounds/28-spawn-host-cost.md) | 2026-10-26 | fork 后 exec 221 → 163 微秒（原生 154） |
| [M9 之前：BKL 的等待和持锁](rounds/29-bkl-wait-hold.md) | 2026-10-26 | 等锁时先自旋；4 个进程同时起进程的吞吐高 4 成 |
| [M9 之前：翻译后代码里的访存](rounds/30-translated-access.md) | 2026-10-26 | TLB 检查占 gcc 32%、python 56%；python 循环 −4% |
| [M9 之前：超级块的实验](rounds/31-superblocks.md) | 2026-10-26 | 调用、前向跳转之后接着翻译，gcc 反而慢，没有采用 |
| [M9 之前：别的负载和文件操作](rounds/32-workloads-file-ops.md) | 2026-10-26 | 各类负载的倍数；仓库合并提交，`cp -r` 再 `rm -rf` 2.8× → 1.6× |
| [M9 之前：文件操作的宿主开销](rounds/33-file-ops-host-cost.md) | 2026-10-26 | 后台删 object、sendfile 交给宿主；`tar x` 2.3× → 1.7× |
| [M9 之前：node 启动](rounds/34-node-startup.md) | 2026-10-26 | outline atomics 在 Tier 1 里做；`node -e 0` 快 12% |
| [M9 之前：JIT 负载](rounds/35-jit.md) | 2026-10-26 | 代码按内容作废；Maven 6.7 → 6.2 秒 |
| [M9 之前：多进程并行](rounds/36-parallel-processes.md) | 2026-10-26 | 翻译池放 16 个实例；并行 exec、管道快约两成 |
| [M9 之前：onTrimMemory](rounds/37-trim-memory.md) | 2026-10-26 | 界面离开或内存紧张时释放可以重建的缓存 |
| [M9 之前：拆 BKL](rounds/38-split-bkl.md) | 2026-10-26 | 读时钟、futex、nanosleep 不取 BKL |
| [M9 之前：平均负载、sysfs 和 limits](rounds/39-loadavg-sysfs.md) | 2026-10-27 | 平均负载取样、CPU 拓扑的 sysfs、`/proc/<pid>/limits` |
| [M9 之前：ptrace 的第一步，strace](rounds/40-ptrace-strace.md) | 2026-10-27 | 系统调用、信号、事件的停止和 regset；strace 能用 |
| [M9 之前：ptrace 的第二步，gdb](rounds/41-ptrace-gdb.md) | 2026-10-27 | 断点、单步、软件观察点、`catch syscall` 与原生一致 |
| [M9 之前：epoll 的边沿](rounds/42-epoll-edges.md) | 2026-10-27 | `EPOLLET` 照 Linux 的时机再触发，tokio 程序不再卡住 |
| [M9 之前：core dump](rounds/43-coredump.md) | 2026-10-27 | 按 `core_pattern` 写 ELF core，gdb 能事后分析 |
| [M9 之前：硬件断点和观察点](rounds/44-hw-watchpoints.md) | 2026-10-27 | 6 个断点、4 个观察点，由解释器检查 |
| [M9 之前：ptrace 和 procfs 的细节](rounds/45-ptrace-procfs.md) | 2026-10-27 | 单步进信号处理函数、sigsuspend 里的屏蔽字、`/proc/<pid>/mem` |
| [M9 之前：mremap 和 sysfs](rounds/46-mremap-sysfs.md) | 2026-10-27 | mremap 扩展私有文件映射；sysfs 的 `cache/`、`cpufreq/` |
| [M9 之前：嵌套 epoll 的边沿](rounds/47-nested-epoll.md) | 2026-10-27 | 内层每来一次事件，边沿触发的外层都再触发 |
| [M9 之前：独占的唤醒](rounds/48-exclusive-wakeup.md) | 2026-10-27 | 一个边沿只叫醒一个 `EPOLLEXCLUSIVE` 的等待者 |
| [M9 之前：按对象唤醒](rounds/49-targeted-wakeups.md) | 2026-10-27 | poll、epoll 和阻塞的读写只被它们等的对象叫醒 |
| [M9 之前：Tier 1 里的观察点](rounds/50-watchpoints-tier1.md) | 2026-10-27 | 只有被观察的页交给解释器；gdb 的 `watch` 10.8 → 0.45 秒 |
| [M9 之前：起进程和设定时器不再叫醒所有人](rounds/51-spawn-timer-wakeups.md) | 2026-10-27 | vfork、定时器、signalfd 只叫醒相关的线程 |
| [M9 之前：signalfd 看调用的线程](rounds/52-signalfd-caller.md) | 2026-10-27 | poll、epoll 与读一样看调用的线程，不看创建它的 |
| [M9 之前：锁和宿主 socket 只叫醒等它们的线程](rounds/53-locks-host-sockets.md) | 2026-10-27 | 有空等的进程时 flock 一次 10 → 0.2 微秒 |
| [M9 之前：消息队列和信号驱动的 I/O 不再叫醒所有人](rounds/54-mqueue-fasync.md) | 2026-10-27 | 消息队列按对象唤醒；`O_ASYNC` 只叫醒定时器线程 |
| [M9 之前：宿主 stdio 上的阻塞读只等宿主 fd](rounds/55-host-stdio.md) | 2026-10-27 | 只影响 `appine-run`，App 里没有这种文件 |
| [M9 之前：被屏蔽的信号只叫醒等它的线程](rounds/56-blocked-signals.md) | 2026-10-27 | 只叫醒 sigtimedwait、signalfd 的等待者 |
| [M9 之前：跟踪停住的进程、跟踪者先取走结束](rounds/57-ptrace-stopped-reaped.md) | 2026-10-27 | group-stop、ATTACH 的报告顺序；跟踪者先取走结束 |
| [M9 之前：cgroup v2 的视图](rounds/58-cgroup.md) | 2026-10-27 | 只读的视图；JVM、Node.js 按 `memory.max` 定堆 |
| [M9 之前：seccomp](rounds/59-seccomp.md) | 2026-10-28 | 严格模式、cBPF 过滤器、通知 fd；`strace --seccomp-bpf` 能用 |
| [M9 之前：seccomp 的缓存和快速路径](rounds/60-seccomp-cache.md) | 2026-10-28 | 总是放行的编号不运行程序；futex 唤醒 313 → 60 纳秒 |
| [M9 之前：进程结束的每一步分别兜住](rounds/61-finish-steps.md) | 2026-10-28 | 九步各自兜住内核 bug，不再留下永远停着的进程 |
| [M9 之前：负载普查](rounds/62-workload-survey.md) | 2026-10-28 | 20 多种服务和工具多数正常，找到 6 个问题 |
| [M9 之前：fork 之后还回的页按各自的权限恢复](rounds/63-returned-loans.md) | 2026-10-28 | 恢复权限不跨映射，rustc 退出时不再崩溃 |
| [M9 之前：终端的属主和权限](rounds/64-pty-owner.md) | 2026-10-28 | sshd 能把终端交给用户，`ssh -t`、mosh 能用 |
| [M9 之前：smaps 里共用的页](rounds/65-smaps-shared.md) | 2026-10-28 | redis、valkey 的写时复制检查通过，能启动 |
| [M9 之前：io_destroy 叫醒等事件的线程](rounds/66-aio-destroy.md) | 2026-10-28 | mariadb 的 install-db 和关闭不再卡住 |
| [M9 之前：cgroup 的 memory.stat](rounds/67-memory-stat.md) | 2026-10-28 | .NET 的 GC 按整个 App 的用量算内存负载 |
| [M9 之前：/proc/meminfo 照搬宿主的](rounds/68-meminfo.md) | 2026-10-28 | free、htop 显示的可用内存与原生相同 |
| [M9 之前：glibc 发行版的探测](rounds/69-glibc-probe.md) | 2026-10-28 | Debian 13、Ubuntu 24.04 的 rootfs 不改内核层就能用 |
| [M9 之前：第二轮普查，语言的测试集和大项目](rounds/70-survey-suites.md) | 2026-10-28 | 语言的测试集和大项目对照原生，找到 6 个问题 |
| [M9 之前：线程退出时在 BKL 外丢掉弱引用](rounds/71-thread-exit-race.md) | 2026-10-28 | 修了线程频繁起停时宿主的堆被破坏 |
| [M9 之前：翻译表里漏掉的墓碑](rounds/72-table-tombstones.md) | 2026-10-28 | 修了翻译表被占满、进程连 SIGKILL 都结束不了 |
| [M9 之前：目录和文件的时间戳](rounds/73-timestamps.md) | 2026-10-28 | Python 能 import 后来新建的模块 |
| [M9 之前：FICLONE 返回 EOPNOTSUPP](rounds/74-ficlone.md) | 2026-10-28 | Python 3.14 的 `Path.copy()` 能用 |
| [M9 之前：PTY 的 TCFLSH 和 TCXONC](rounds/75-pty-flow.md) | 2026-10-28 | CPython 的 test_termios、test_ioctl 不再卡住 |
| [M9 之前：FIFO 在写端来之前不报 POLLHUP](rounds/76-fifo-pollhup.md) | 2026-10-28 | Ruby 的 test_require 全部通过 |
| [M9 之前：删除的目录里不能新建，socket 可以 fchmod](rounds/77-dead-dir-socket-fchmod.md) | 2026-10-28 | Node 的 test-fs-mkdir 等两个测试通过 |
| [M9 之前：网络信息的缺口](rounds/78-net-info-gaps.md) | 2026-10-28 | 只补文档：路由表、流量计数、`if_inet6` 的缺口 |
| [M9 之前：终端的访问时间和修改时间](rounds/79-pty-times.md) | 2026-10-28 | w、who 的空闲时间有意义了 |
| [M9 之前：CPU 时间不倒退](rounds/80-cpu-times.md) | 2026-10-28 | 照 `cputime_adjust`，times03 不再偶发失败 |
| [M9 之前：被叫醒的进程在 /proc 里是 R](rounds/81-woken-state.md) | 2026-10-28 | shmctl01 不再失败 |
| [M9 之前：测试自己的竞争和丢掉的现场](rounds/82-test-races.md) | 2026-10-28 | 作业控制用例的竞争；测试失败时保留现场 |
| [M9 之前：没有实现的 netlink 协议建不了](rounds/83-netlink-protocols.md) | 2026-10-28 | 走 PAM 的 sshd、su、login 不再失败 |
| [M9 之前：补测待查的几项](rounds/84-debt-checks.md) | 2026-10-28 | 只补测：几项待查的失败与原生一致 |
| [M9 之前：if_inet6、O_PATH 的 ioctl 和中途停住之后的出口](rounds/85-inet6-opath-exit.md) | 2026-10-28 | JVM 不再关掉 IPv6；fork 事件之后停在系统调用出口 |
| [M9 之前：排查在 BKL 外改 Rc 的计数](rounds/86-rc-outside-bkl.md) | 2026-10-28 | 放开 BKL 期间 panic 时先拿回锁再展开 |
| [M9 之前：不持 BKL 的系统调用兜住 panic，等待时不借着打开文件](rounds/87-fast-panic-held-borrows.md) | 2026-10-29 | 不持锁的系统调用里的 bug 只结束那个进程 |
| [M9 之前：futex 等待者的清理，放开 BKL 时检查借用](rounds/88-futex-cleanup-borrow-check.md) | 2026-10-29 | 等待者由守卫离开队列，不再丢唤醒 |
| [M9 之前：测试用的发布构建也检查借用](rounds/89-release-borrow-check.md) | 2026-10-29 | 差分测试、LTP、普查认出内核层报告的内部错误 |
| [M9 之前：网络接口的流量计数](rounds/90-interface-counters.md) | 2026-10-29 | `/proc/net/dev`、sysfs、netlink 报告同样的数 |
| [M9 之前：NETLINK_GENERIC 只有 nlctrl](rounds/91-generic-netlink.md) | 2026-10-29 | 回答与 Linux 6.8 逐字节相同 |
| [M9 之前：调试器、valgrind 和 headless Chromium](rounds/92-debuggers-valgrind-chromium.md) | 2026-10-29 | 找到内核层的 3 个问题，修好之后与原生相同 |
| [M9 之前：`/proc/<pid>/fd` 和 maps 里的路径解析符号链接](rounds/93-fd-paths-resolve-symlinks.md) | 2026-10-29 | valgrind 能重新打开 Debian 的 ld.so |
| [M9 之前：`/proc/<pid>/task` 的链接数](rounds/94-task-dir-links.md) | 2026-10-29 | Chromium 的 zygote 不再一启动就崩溃 |
| [M9 之前：跟踪者退出时丢掉已经取走的信号](rounds/95-tracer-exit-drops-signal.md) | 2026-10-29 | delve attach 之后读到 EOF 退出，进程不再停住 |
| [M9 之前：`/proc/<pid>/net`](rounds/96-proc-pid-net.md) | 2026-10-29 | `/proc/net` 改成指向 `self/net` 的链接 |
| [M9 之前：已经删除的文件带 ` (deleted)`](rounds/97-deleted-suffix.md) | 2026-10-29 | needrestart 按它找出还在用旧库的进程 |
| [M9 之前：路由表](rounds/98-routes.md) | 2026-10-29 | 主表取自宿主；`ip route` 等与 Linux 逐字节相同 |
| [M9 之前：App 里选 rootfs](rounds/99-choose-rootfs.md) | 2026-10-29 | Alpine、Debian、Ubuntu 可选，可以重置 |
| [M9 之前：文件能力](rounds/100-file-caps.md) | 2026-10-29 | 照 Linux 6.1 算文件能力、环境能力；扩展属性检查权限 |
| [M9 之前：接口和地址的 netlink 消息](rounds/101-link-addr.md) | 2026-10-29 | `ip addr`、`ip link` 显示 `brd`、`state UP` |
| [M9 之前：`/proc/net/snmp` 和输入结束时的 EOF](rounds/102-snmp-pty-eof.md) | 2026-10-29 | `ss -s` 能用；`appine-run --pty` 输入结束时送 VEOF |
| [M9 之前：AI 编程 CLI](rounds/103-ai-cli-survey.md) | 2026-10-29 | Claude Code、Codex、Gemini CLI 与原生相同，沙箱建不了 |
| [M9 之前：关掉主设备时挂断从设备](rounds/104-pty-hangup.md) | 2026-10-29 | 照 `tty_vhangup` 挂断，`tmux kill-server` 之后不留进程 |
| [M9 之前：mount 的子集](rounds/105-mount-subset.md) | 2026-10-29 | procfs 等挂到任意目录、bind；chroot 进 Debian 之后 ps 能用 |
| [M9 之前：代码缓存只算一次](rounds/106-code-cache-rss.md) | 2026-10-30 | 20 个空闲 python3 的 App 用量 321 → 218 MB |
| [M9 之前：翻译代码的密度](rounds/107-code-density.md) | 2026-10-30 | 每条 guest 指令约 8 → 6 个宿主字；Maven、Gradle 快 3–5% |
| [M9 之前：同一个程序共用翻译](rounds/108-shared-translation.md) | 2026-10-30 | 20 个空闲 python3 193 → 84 MB；shell 起进程快三成 |
| [M9 之前：起进程和 mmap 的持锁时间](rounds/109-spawn-mmap-bkl.md) | 2026-10-30 | 4 个同时 exec 快约一成 |
| [M9 之前：openat 的开销](rounds/110-openat-cost.md) | 2026-10-30 | 没有便宜的改法，没改代码 |
| [M9 之前：检查点挪到后台](rounds/111-background-checkpoint.md) | 2026-10-30 | 解包时持锁的提交最长 8.4 → 0.6 ms |
| [M9 之前：宿主内存的接缝](rounds/112-host-seams.md) | 2026-10-30 | 完全不拆组也只快 1.4%，没改代码 |
| [M9 之前：Go 换一组被占用的寄存器](rounds/113-go-registers.md) | 2026-10-30 | `go build` 快 3–5% |
| [M9 之前：跳到未对齐的地址](rounds/114-unaligned-branch.md) | 2026-10-30 | 照 Linux 是 SIGBUS，原来以内部错误结束 |
| [M9 之前：有 JIT 代码的进程也共用翻译](rounds/115-shared-with-jit.md) | 2026-10-30 | 4 个空闲 node 的 App 用量 190 → 119 MB |
| [M9 之前：仓库里的目录 bind](rounds/116-store-binds.md) | 2026-10-30 | bubblewrap 的第一轮：仓库里的目录、文件可以 bind |
| [M9 之前：tmpfs](rounds/117-tmpfs.md) | 2026-10-30 | bubblewrap 的第二轮 |
| [M9 之前：挂载命名空间](rounds/118-mount-namespaces.md) | 2026-10-30 | bubblewrap 的第三轮 |
| [M9 之前：用户命名空间](rounds/119-user-namespaces.md) | 2026-10-30 | bubblewrap 的第四轮 |
| [M9 之前：pid 命名空间](rounds/120-pid-namespaces.md) | 2026-10-30 | bubblewrap 的第五轮 |
| [M9 之前：UTS 和 IPC 命名空间](rounds/121-uts-ipc-namespaces.md) | 2026-10-30 | bubblewrap 的第六轮 |
| [M9 之前：网络命名空间](rounds/122-net-namespaces.md) | 2026-10-30 | bubblewrap 的第七轮 |
| [M9 之前：bubblewrap 和 AI 编程 CLI 的沙箱](rounds/123-bwrap.md) | 2026-10-30 | Codex、Claude Code 的沙箱能用了 |
| [M9 之前：V8 和 HotSpot 的寄存器](rounds/124-v8-hotspot-registers.md) | 2026-10-30 | 只有文档：量过之后不换寄存器 |
| [M9 之前：raw ICMP 的模拟](rounds/125-raw-icmp.md) | 2026-10-30 | busybox 的 `ping`、`ping6` 能用 |
| [M9 之前：PR_SET_CHILD_SUBREAPER](rounds/126-child-subreaper.md) | 2026-10-31 | tini、s6 这类进程管理程序用它 |
| [M9 之前：LTP 剩下的小差异](rounds/127-ltp-leftovers.md) | 2026-10-31 | 多数是 musl 的行为，改了 6 处 |
| [M9 之前：traceroute 和 mtr](rounds/128-traceroute.md) | 2026-10-31 | 每个网络命名空间一个 ICMP 汇集点；traceroute、mtr 能用 |
| [M9 之前：命名空间文件留住命名空间](rounds/129-namespace-files.md) | 2026-10-31 | `ip netns`、`nsenter` 能用 |
| [M9 之前：vhangup](rounds/130-vhangup.md) | 2026-10-31 | 照 `tty_vhangup` 挂断，节点留着、重新打开照常能用 |
| [M9 之前：tmpfs 的用量和限制](rounds/131-tmpfs-usage.md) | 2026-10-31 | statfs 报告用量，放不下、写满了是 ENOSPC |
| [M9 之前：setns 经由 pidfd](rounds/132-setns-pidfd.md) | 2026-10-31 | `nsenter -t` 原来每次都失败，现在能用 |
| [M9 之前：点积指令 ASIMDDP](rounds/133-asimddp.md) | 2026-10-31 | VP9 编码快 4.5% |
| [M9 之前：task 目录的 children](rounds/134-task-children.md) | 2026-10-31 | 子进程记着父线程，线程退出时转给别的线程 |
| [M9 之前：SHA3、SHA512 和 I8MM](rounds/135-sha3-sha512-i8mm.md) | 2026-10-31 | OpenSSL 的 SHA-512 快 4.1 倍 |
| [M9 之前：LRCPC](rounds/136-lrcpc.md) | 2026-10-31 | §4.4 的 v1.x 特性都做完了 |
| [M9 之前：PDEATHSIG 和 __WNOTHREAD](rounds/137-pdeathsig-wnothread.md) | 2026-10-31 | 建立它的线程一退出就发；`__WNOTHREAD` 只等自己建的 |
| [M9 之前：LRCPC2](rounds/138-lrcpc2.md) | 2026-10-31 | 带偏移的 LDAPUR、STLUR 等 |
| [M9 之前：未实现的系统调用](rounds/139-unimplemented-syscalls.md) | 2026-10-31 | 只有文档：补记返回 ENOSYS 的编号 |
| [M9 之前：ioprio、syslog 和 adjtimex](rounds/140-ioprio-syslog-adjtimex.md) | 2026-10-31 | `ionice` 能用；syslog(2)、adjtimex 回答查询 |
| [M9 之前：FP16 和 JSCVT](rounds/141-fp16-jscvt.md) | 2026-10-31 | llama.cpp 的 F16 模型快 44–82% |
| [M9 之前：RDM、FCMA、FHM 和 BF16](rounds/142-rdm-fcma-fhm-bf16.md) | 2026-10-31 | `gcc -mcpu=native` 用上它们；BF16 点积快 1.5 倍 |
| [M9 之前：/dev/kmsg 和内核日志](rounds/143-kmsg.md) | 2026-10-31 | guest 里 `dmesg` 看得到 OOM 和内部错误 |
| [M9 之前：openat2](rounds/144-openat2.md) | 2026-10-31 | 新版 GNU tar 离不开它 |
| [M9 之前：termios2](rounds/145-termios2.md) | 2026-10-31 | Ubuntu 26.04 里的终端认得出了 |
| [M9 之前：PTY 的包模式](rounds/146-pty-packet-mode.md) | 2026-10-31 | GNU screen 能用 |
| [M9 之前：/proc/<pid>/exe 的跟踪权限](rounds/147-proc-exe-access.md) | 2026-10-31 | 别的用户的、不可 dump 的进程是 EACCES |
| [M9 之前：用户命名空间里的 ID 换算](rounds/148-userns-ids.md) | 2026-10-31 | `SO_PEERCRED`、si_uid、IPC 的属主等按命名空间换算 |
| [M9 之前：Ubuntu 26.04 的普查](rounds/149-ubuntu26-survey.md) | 2026-10-31 | apt 装了 238 个包，找到三处问题 |
| [M9 之前：新分配的 PTY](rounds/150-new-pty-state.md) | 2026-11-01 | 从设备没打开过时主设备不 EIO；窗口大小从 0×0 开始 |
| [M9 之前：短的符号链接不占块](rounds/151-short-symlink-blocks.md) | 2026-11-01 | `ls -l` 的 total 与原生相同 |
| [M9 之前：nmap 以 root 运行](rounds/152-nmap-unprivileged.md) | 2026-11-01 | App 设 `NMAP_UNPRIVILEGED=1`，改用 connect 扫描 |
| [M9 之前：/proc/<pid>/exe 指向程序文件本身](rounds/153-exe-file.md) | 2026-11-01 | 删了、换了之后经由它打开的仍是原来的文件 |
| [M9 之前：FRINTTS、FlagM、SB 和 DPB](rounds/154-frintts-flagm-sb-dpb.md) | 2026-11-01 | `gcc -mcpu=native` 与原生相同 |
| [M9 之前：DIT](rounds/155-dit.md) | 2026-11-01 | 信号、ptrace、fork 都带着，execve 清零 |
| [M9 之前：低端口映射和监听通知](rounds/156-low-ports.md) | 2026-11-01 | 宿主上换成 +10000，看到监听时发通知 |
| [M9 之前：podman 和 docker 的普查](rounds/157-container-survey.md) | 2026-11-01 | podman 的常用功能与原生相同；找到 12 个问题 |
| [M9 之前：容器运行时碰到的四处小问题](rounds/158-container-small-fixes.md) | 2026-11-01 | capget、`RTM_NEWLINK`、合成根目录的 inode、线程目录的 fd |
| [M9 之前：可写地挂 cgroup2](rounds/159-cgroup2-rw-mount.md) | 2026-11-01 | crun 给容器这样挂，里面照旧改不了 |
| [M9 之前：mqueue 文件系统](rounds/160-mqueue-fs.md) | 2026-11-01 | podman 不再需要临时绕过就能起容器 |
| [M9 之前：经由 fd 执行 memfd 和已经删除的文件](rounds/161-exec-open-file.md) | 2026-11-01 | runc 起容器、`podman build --isolation chroot` 要用 |
| [M9 之前：time 命名空间](rounds/162-time-namespaces.md) | 2026-11-01 | dockerd、runc 要用 |
| [M9 之前：cgroup 树：建子 cgroup、迁移进程](rounds/163-cgroup-tree.md) | 2026-11-01 | dockerd 建得了容器的 cgroup |
| [M9 之前：cgroup 的 pids.max 和 memory.max 起作用](rounds/164-cgroup-limits.md) | 2026-11-01 | fork 按 `pids.max` 检查，OOM killer 按 cgroup 挑 |
| [M9 之前：cgroup.freeze](rounds/165-cgroup-freeze.md) | 2026-11-01 | 冻结的进程停在返回 guest 之前 |
| [M9 之前：cgroup 的设备过滤（eBPF）](rounds/166-bpf-devices.md) | 2026-11-01 | docker、podman 不用 `--cgroups disabled` 也能跑容器 |
| [M9 之前：devpts 的实例](rounds/167-devpts-instances.md) | 2026-11-01 | 容器里只看得到自己的终端 |
| [M9 之前：open_tree、move_mount 和 mount_setattr](rounds/168-open-tree.md) | 2026-11-01 | podman `--uidmap` 的容器起得来了 |
| [M9 之前：容器网络的普查](rounds/169-container-network-survey.md) | 2026-11-01 | 两家都卡在建 bridge；定下地址映射方案的四步 |
| [M9 之前：有状态的网卡](rounds/170-netdev.md) | 2026-11-01 | 地址映射的第一步：docker、podman 的 bridge 网络建得起来 |
| [M9 之前：经由虚拟网卡的连接](rounds/171-netmap.md) | 2026-11-01 | 地址映射的第二步：容器之间、docker 的 `-p` 都通了 |
| [M9 之前：nftables 的子集和 socket 层的 DNAT](rounds/172-nftables.md) | 2026-11-01 | 地址映射的第三步：docker 自定义网络、podman 的 nftables 驱动能用 |
| [M9 之前：发布的端口：PREROUTING 的 DNAT 和宿主上的转发](rounds/173-port-forwarding.md) | 2026-11-01 | 地址映射的第四步：从外面也连得到发布的端口 |
| [M9 之前：各个网络命名空间的端口互不相干](rounds/174-namespace-ports.md) | 2026-11-01 | 内外同端口的 `-p 8080:8080` 起得来了 |
| [M9 之前：过滤链](rounds/175-filter.md) | 2026-11-01 | docker 网络之间的隔离与原生一致 |
| [M9 之前：从外面转发进来的连接带上真实的客户端地址](rounds/176-real-client-address.md) | 2026-11-01 | 容器里看到外面设备的真实地址，原来是网关 |
| [M9 之前：网络命名空间的 sysctl 补齐、UDP 也查有没有人绑](rounds/177-net-sysctls.md) | 2026-11-02 | docker 的 `--sysctl` 能用；新命名空间里的 UDP 也登记 |
| [M9 之前：测试提速，以及顺带找到的几个问题](rounds/178-test-speed.md) | 2026-11-02 | 全套从半个多小时降到约 9 分钟 |
| [M9 之前：页表按需建立，AddressSanitizer 能用了](rounds/179-demand-page-tables.md) | 2026-11-02 | 大映射等访问时才建页表；ASan、LSan 能用 |
| [M9 之前：透明代理：SO_ORIGINAL_DST、SO_MARK 和 owner 匹配](rounds/180-original-dst.md) | 2026-11-02 | 透明代理（`sslocal --protocol redir`）能用 |
| [M9 之前：共用翻译的组里最后一个成员离开时作废别人留下的块](rounds/181-last-member-leaves.md) | 2026-11-02 | 修了共用翻译的偶发 SIGILL |
| [M9 之前：容器连外网也经过外层的 PREROUTING](rounds/182-outer-prerouting.md) | 2026-11-02 | 在外层 REDIRECT 就能透明代理所有容器 |
| [M9 之前：新命名空间里没绑定的 UDP socket 先发命名空间里、再发外面](rounds/183-udp-wide-first.md) | 2026-11-02 | 不先 bind 就 sendto 的程序发往哪里都行 |
| [M9 之前：开机时挂好 /dev、/dev/shm、/dev/mqueue 和 /run](rounds/184-boot-tmpfs.md) | 2026-11-02 | 支持 OpenRC 的第一轮 |
| [M9 之前：Alpine 开机时写的 sysctl、protected_symlinks/hardlinks 和随机数的 ioctl](rounds/185-protected-links-random.md) | 2026-11-02 | 支持 OpenRC 的第二轮 |
| [M9 之前：内置的 init 按 inittab 开机、关机](rounds/186-inittab.md) | 2026-11-02 | 支持 OpenRC 的第三轮 |
| [M9 之前：App 按 inittab 开机、关机，没有标签页时留在后台](rounds/187-app-init.md) | 2026-11-02 | 支持 OpenRC 的最后一轮 |
| [M9 之前：8 核上的扩展性和 BKL 的交接](rounds/188-bkl-spinners.md) | 2026-11-02 | 只有同时起进程不扩展；放宽到 4 个自旋者快 7–13% |
| [M9 之前：rt_sigprocmask 不取 BKL 的尝试和进出分发器的共用计数](rounds/189-sigmask-dbt-running.md) | 2026-11-02 | rt_sigprocmask 免锁没有采用；8 个线程读时钟 844 → 155 ns |
| [M9：设置页，默认导入 Alpine](rounds/190-settings-default-alpine.md) | 2026-11-02 | 换发行版、重置、选镜像、换执行层 |
| [M9：熄屏时保持运行](rounds/191-keep-awake.md) | 2026-11-02 | 持 `PARTIAL_WAKE_LOCK`，不申请电池优化的豁免 |
| [M9：许可证改为 AGPL-3.0-or-later，开源许可页](rounds/192-license.md) | 2026-11-02 | Termux 终端库按 GPLv3-only 对待，R3 消失 |
| [M9：以指定用户登录](rounds/193-login-account.md) | 2026-11-02 | 默认以最先建的普通用户登录；修了 DNS 存根的 `FIONREAD` |
| [M9：被杀之后恢复标签页](rounds/194-restore-tabs.md) | 2026-11-02 | 以原来的账号、目录起新的 shell，显示原来的回看记录 |
| [M9：进程被结束、服务没起来时提示](rounds/195-kill-reports.md) | 2026-11-02 | 在标签页或通知里说明原因 |
| [M9：在"文件"App 里浏览（只读）](rounds/196-documents-read.md) | 2026-11-02 | DocumentsProvider，不经过内核直接读仓库 |
| [M9：在"文件"App 里改家目录](rounds/197-documents-write.md) | 2026-11-02 | 新建、写、删、改名，限在家目录里 |
| [M9：断电之后仓库不坏](rounds/198-power-loss.md) | 2026-11-03 | 照 ext4 补上刷盘的顺序；dm-flakey 断电 75 次不坏 |
| [M9：39 位的宿主地址空间](rounds/199-reserved-mappings.md) | 2026-11-03 | `PROT_NONE` 的预留不占宿主地址空间；Chromium 4.83 TB → 12.4 GB |
| [M9：发布时放出的源码](rounds/200-release-sources.md) | 2026-11-03 | `scripts/release-sources` 生成三份源码 |
| [M9："文件"App 与 guest 双向的通知](rounds/201-documents-notify.md) | 2026-11-03 | guest 收得到 inotify 事件，"文件"App 会刷新 |
| [M9：内核的仓库只在持着它的 BKL 时用](rounds/202-store-bkl-check.md) | 2026-11-03 | 第 193 轮那次偶发 panic 的原因 |
| [M9：换镜像不丢用户加的源](rounds/203-mirror-keeps-lines.md) | 2026-11-03 | 只换内置镜像的地址，别的行都留着 |
| [M9："文件"App 按内容认出文本文件](rounds/204-text-by-content.md) | 2026-11-03 | 没有 NUL、是合法 UTF-8 的给 `text/plain` |
| [M9：内核 bug 的报告能复制，带着 panic 的位置](rounds/205-copy-bug-report.md) | 2026-11-03 | panic 的消息和代码位置接进报告 |
| [M9：Debian 上装 sudo 显示进度、能跳过，普通用户的 shell 有作业控制](rounds/206-debian-sudo-progress.md) | 2026-11-03 | Ctrl-C 只跳过这一次；修了 `su -c` 另起会话 |
| [M9：内核 bug 的报告带上出错之前的内核日志](rounds/207-report-kernel-log.md) | 2026-11-03 | 报告带上出错之前的 10 条日志 |
| [M9：宿主让绑的低端口就绑原来的](rounds/208-low-ports-direct.md) | 2026-11-03 | Play 系统更新放开了 22、80、443 等端口 |

## 测量条件

各篇的测量没有另外注明时，条件如下：

- **宿主**：Apple Silicon 上 colima 起的 arm64 Linux 虚拟机（4 核；[测试提速](rounds/178-test-speed.md)起 8 核）。appine 是 release 构建，用 Tier 1。
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
| [8 核上的扩展性和 BKL 的交接](rounds/188-bkl-spinners.md)（虚拟机从[测试提速](rounds/178-test-speed.md)起是 8 核） | Lua 编译 `xargs -P1`、`-P4`、`-P8`；`git grep` 1、4、8 个线程；4、8 个进程同时 fork 后 exec；4、8 个同时跑管道 | 3.3×、3.0–3.3×、3.5×；2.3×、2.0×、1.7×；2.2–2.3×、4.1–4.3×（原来 2.5×、4.4×）；2.5–2.6×、4.0–4.3×（原来 3.0×、4.8×） |
