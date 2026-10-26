# appine 里程碑记录

按完成顺序记录每一轮的结论、测量、实现要点和推迟项。里程碑的计划和排序依据见[设计文档](design.md) §14。

- 这里是当时的记录。后来改动过的做法以设计文档第 1–13 节为准。
- 推迟项后面用"→"注明了后来在哪一轮补上。
- 文中的 §x 指设计文档的章节；R1–R6、A1–A3 是设计文档 §16 的风险编号。

| 一轮 | 日期 | 要点 |
|---|---|---|
| [M2 结果](#m2-结果2026-10-24) | 2026-10-24 | 原型 Tier 1：整数基准几何平均 1.85×，Neoverse N2 上 2.37× |
| [M1a 结果](#m1a-结果2026-10-24) | 2026-10-24 | SIMD&FP、LL/SC、LSE；lockstep 扩展成三方比对 |
| [M1b 结果](#m1b-结果2026-10-24) | 2026-10-24 | busybox 经由 `ld-musl` 跑通；`gzip` 4.75× |
| [M3a 结果](#m3a-结果2026-10-24) | 2026-10-24 | 多进程、多线程、BKL、信号帧、vDSO |
| [M3b 结果](#m3b-结果2026-10-24) | 2026-10-24 | 进程内 PTY、作业控制、procfs |
| [M4 结果](#m4-结果2026-10-24) | 2026-10-24 | 网络、DNS 存根、AF_UNIX；`apk add`、`git clone`；R5 关闭 |
| [M5 结果](#m5-结果2026-10-24) | 2026-10-24 | Android App；4K、16K 页模拟器通过 |
| [M6 结果](#m6-结果2026-10-25) | 2026-10-25 | 在 appine 里编译 Lua、SQLite（约 10×）；凭据、记录锁、IPC、定时器；LTP 849 |
| [M7 结果](#m7-结果2026-10-25) | 2026-10-25 | Tier 1 正式版：SIMD 翻译、4096 项 TLB、翻译模板、W^X；编译 4.7–6.1× |
| [M8 结果](#m8-结果2026-10-25) | 2026-10-25 | Go、Node.js 行为正确；`EPOLLET` 修正；LTP 856 |
| [M8 补测：JVM](#m8-补测jvm2026-10-25) | 2026-10-25 | OpenJDK 21/25、Maven、Gradle；`/proc/self/exe` 解析符号链接；inotify |
| [M9 之前：稳定性和内存](#m9-之前稳定性和内存2026-10-25) | 2026-10-25 | 截短已映射的文件、`PROT_NONE` 不进页表、`MADV_DONTNEED`、`restart_syscall`、robust futex |
| [M9 之前：性能剖析](#m9-之前性能剖析2026-10-25) | 2026-10-25 | 元数据缓存、fork 只看碰过的页；`git status` 19× → 6× |
| [M9 之前：起进程的翻译开销](#m9-之前起进程的翻译开销2026-10-25) | 2026-10-25 | 翻译跨 exec 保留；shell 起进程 9× → 6.3× |
| [M9 之前：栈指针常驻寄存器和返回预测](#m9-之前栈指针常驻寄存器和返回预测2026-10-25) | 2026-10-25 | SP 常驻 X26、影子返回栈；返回预测让 gcc 快 10% |
| [M9 之前：futex 定向唤醒](#m9-之前futex-定向唤醒2026-10-25) | 2026-10-25 | FUTEX_WAKE 只通知被唤醒的线程；Gradle 热构建 6.1 → 4.8 秒 |
| [M9 之前：兼容性补漏](#m9-之前兼容性补漏2026-10-25) | 2026-10-25 | maps 命名、堆单 VMA、实时信号排队、pidfd、`execveat`、`/proc/net`；LTP 889 |
| [M9 之前：剩下的小缺口](#m9-之前剩下的小缺口2026-10-25) | 2026-10-25 | `/dev/shm` 开机清空、smaps/pagemap、`ru_maxrss`、unshare、`max_map_count`；LTP 905 |
| [M9 之前：推迟项](#m9-之前推迟项2026-10-25) | 2026-10-25 | `O_ASYNC`、租约、POSIX 消息队列、AIO、AF_UNIX GC；LTP 944；io_uring 不做 |
| [M9 之前：稳健性](#m9-之前稳健性2026-10-25) | 2026-10-25 | 内核层 panic、引擎的宿主故障只结束一个进程；App 内存上限和 OOM killer；threads-max、`RLIMIT_NPROC` |
| [M9 之前：稳健性的收尾](#m9-之前稳健性的收尾2026-10-25) | 2026-10-25 | 线程开始和退出、定时器线程、宿主调用（JNI）里的 panic；App 进程里经过 libsigchain 的恢复 |
| [M9 之前：栈的大小上限](#m9-之前栈的大小上限2026-10-26) | 2026-10-26 | 主线程栈按需增长，受 `RLIMIT_STACK` 的当前值限制；syscall 和信号帧也让它增长；mmap 区按上限让出间隔；LTP 944 |
| [M9 之前：访存检查和翻译表](#m9-之前访存检查和翻译表2026-10-26) | 2026-10-26 | 跨页访问不退出、同一基址的访存共用一次 TLB 检查、翻译表复用墓碑；gcc 3.6×、python 4.9×，Gradle 热构建 4.8 → 2.9 秒 |
| [M9 之前：多线程共用翻译和定向唤醒](#m9-之前多线程共用翻译和定向唤醒2026-10-26) | 2026-10-26 | 测量改用本地卷上的仓库；同一地址空间的线程共用 Tier 1 的翻译、池子按程序保留翻译；唤醒分通道；普通文件读写不持 BKL；`go build` 5.4× → 3.2×，Maven 8.5× → 4.4×，`node -e 0` 5.1× → 2.9× |
| [M9 之前：不持 BKL 的 stat](#m9-之前不持-bkl-的-stat2026-10-26) | 2026-10-26 | 缓存全命中的 newfstatat、statx 不取 BKL；并行 lstat 的 `git status` 24 → 13 毫秒，4 线程并行 lstat 每次 4.1 → 0.9 微秒 |
| [M9 之前：fork 写时复制](#m9-之前fork-写时复制2026-10-26) | 2026-10-26 | 私有匿名内存写时复制、宿主线程池、唤醒按子进程/线程结束/管道细分；写过 100 MB 的进程 fork 21 → 2.3 毫秒，空进程 118 → 90 微秒 |
| [M9 之前：交错访存共用检查](#m9-之前交错访存共用检查2026-10-26) | 2026-10-26 | 同一基址的访存中间夹着别的访存也共用一次 TLB 检查；量清楚了 TLB 检查贵在哪，按访存点缓存页等两个实验没有收益；python 循环 0.82 → 0.75 秒 |
| [M9 之前：起进程的宿主开销](#m9-之前起进程的宿主开销2026-10-26) | 2026-10-26 | 放开 BKL 之后再叫醒、再交还内存；驻留内存取样合并、缓存；同一版本的程序文件里的块不再核对；只读文件映射共用宿主映射；execve 放开 BKL 建地址空间；fork 后 exec 221 → 163 微秒（原生 154），管道 0.85 → 0.54 毫秒，shell 起进程 0.275 → 0.21 秒 |
| [M9 之前：BKL 的等待和持锁](#m9-之前bkl-的等待和持锁2026-10-26) | 2026-10-26 | 等 BKL 时只让一个线程自旋最多 30 微秒；fork 的宿主映射预先建好；execve 记着程序文件；4 个进程同时起进程的总吞吐提高 4 成，管道 0.54 → 0.45 毫秒 |

---

## M2 结果（2026-10-24）

**结论**：Tier 1 相对原生的几何平均，在 Apple Silicon 上是 1.85 倍，在 Neoverse N2 上是 2.37 倍，都落在"≤ 3 倍，按原计划继续"一档。决策 4（soft-MMU）和决策 5（分层引擎）不需要改动，下一步做 M1a。

**测量条件**：

- **Apple Silicon**：colima 起的 arm64 Linux 虚拟机（4 核）。原生和 Tier 1 各运行 5 次取最短耗时，Tier 0 运行 1 次。
- **Neoverse N2**：GitHub 的 `ubuntu-24.04-arm` runner，据 Arm 的文档跑在 Azure Cobalt 100 上。CI 日志里 `appine-bench` 打印的是 `ARM 0xd49 Neoverse-N2 x2`，也就是私有仓库拿到的 2 vCPU 规格。性能探针步骤运行 3 次取最短，不测 Tier 0；两次 CI 运行都测得 2.37×。
- 基准程序用 gcc `-O2 -mgeneral-regs-only` 静态编译，不依赖 libc，见 `tests/bench/programs/`；三种方式的输出逐字相同。

| 程序 | 负载 | 原生（Apple） | Tier 0（Apple） | Tier 1（Apple） | Tier 1（Neoverse N2） |
|---|---|---|---|---|---|
| sha256 | 32 位 ALU | 0.287s | 65.7× | 1.36× | 2.06× |
| lz | 访存、分支 | 0.239s | 49.7× | 1.97× | 2.33× |
| sort | 访存、难预测的分支 | 0.318s | 23.3× | 1.64× | 1.98× |
| crc32 | 查表 | 0.311s | 18.5× | 2.17× | 2.32× |
| vm | 间接分支 | 0.290s | 73.6× | 2.29× | **3.37×** |
| **几何平均** | | | **40.1×** | **1.85×** | **2.37×** |

翻译统计（`appine-run --stats`）：约 55% 的 guest 指令原样拷贝，5–13% 因为用到被占用的寄存器或 SP 而改写，13–22% 是改写成快速路径的访存指令，没有指令退回 Tier 0。开销基本按访存比例增长：ALU 为主的 sha256 最低，以访存和间接分支为主的 crc32、vm 最高。

这个数字偏乐观，原因有四：

1. **宿主核心偏宽**。Apple Silicon 能把插入的整数指令大量并行吸收；换到较窄的 Neoverse N2 上，几何平均从 1.85× 升到 2.37×，印证了这一点。据公开资料，Neoverse N2 和手机上的 Cortex-A710 同源，所以 2.37× 更接近手机大核、中核的情况。手机小核（A510/A520 这类顺序执行核心）会更差，M5 要上真机测。
2. **只有整数负载**。没有 SIMD、没有 libc，热循环里也没有 syscall。M1 完成后要用 busybox `gzip`/`sha256sum` 补测（结果见下面的 M1b 结果）。
3. **原型缺少一些功能**。自修改代码的失效检查、LL/SC、SIMD 访存都还没做；加上之后，访存路径的成本应该大致相同，但还要实测确认。
4. **单线程**，也没有信号投递。

最薄弱的是间接分支：`vm` 在 Neoverse N2 上已经到了 3.37×。如果补测结果变差，手上的优化方向按优先级排列如下：

1. **`ret` 改用宿主的返回地址预测**，不走跳转缓存。这是针对 `vm` 这类间接分支负载的，排第一。→ 已补上（M9 之前：栈指针常驻寄存器和返回预测）
2. 同一基本块内，对同一页的 TLB 检查外提。→ 已补上，做法是同一基址的访存共用一次检查（M9 之前：访存检查和翻译表）
3. 把 guest SP 常驻在一个宿主寄存器里，省掉每次栈访问前的一条 `ldr`。→ 已补上（同上）
4. 基址对齐已知时，省掉跨页检查。→ M7 把跨页检查并进了 TLB 比较，不再需要

Tier 0 慢了 40 倍，这在意料之中：它只负责冷代码和语义参考，而且还没用上设计里的尾调用分发（后来也没有做，见 §4.2）。

## M1a 结果（2026-10-24）

- **SIMD&FP**：基础 FP 和 Advanced SIMD 全部可用，AES/PMULL/SHA1/SHA2 按宿主特性开放；数据处理指令走原生桩，访存用 C 实现。
- **原子类**：LDXR/STXR、LDXP/STXP、LDAR/STLR、CLREX，以及宿主支持时的 LSE（LD<op>、SWP、CAS、CASP）。CRC32 按宿主特性开放。
- **特性宣告**：`AT_HWCAP` 和 ID 寄存器按 §4.4 从同一份集合生成。
- **lockstep** 扩展成三方比对（硬件、Tier 0、Tier 1）：用例最多 4 条指令，状态包括 V0–V31、FPCR、FPSR 和两页数据区，新增整数与 SIMD 访存、独占访问序列、LSE、CRC32、FP 系统寄存器，以及覆盖整个 SIMD&FP 数据处理编码空间的模糊测试。每类 5 万次、共约 160 万个用例全部一致；人为注入的三个 bug（SIMD 多结构访存的寄存器错位、FPSR 不写回、STXR 状态反转）都在几次迭代内被抓到。
- **白名单**：五个编码空间各抽样 30 万条，与 `llvm-mc` 完全一致；其中 SIMD&FP 数据处理空间里约 10% 的样本是可执行编码。
- 过程中修掉一个 M0 遗留的解码 bug：整数 LDNP 的 opc = 01（不存在的 LDNPSW）原来会被当成 LDPSW 执行。

## M1b 结果（2026-10-24）

**结论**：`appine-run --rootfs <仓库> /bin/busybox ls /` 经由 `ld-musl` 跑通，Tier 0 和 Tier 1 的输出相同，达到退出标准。

- **加载器**：支持 `ET_EXEC` 和 `ET_DYN`。位置无关的主程序加载在 `0x5555_5555_0000`（47 位地址空间的 2/3，与 arm64 Linux 的 `ELF_ET_DYN_BASE` 取法相同），解释器放在 mmap 区的顶端；program break 紧跟在主程序之后。auxv 按 §7.5 提供，`AT_MINSIGSTKSZ` 暂报 5120；`AT_SYSINFO_EHDR` 随 vDSO 在 M3a 加入。
- **soft-MMU v1**：VMA 表加页表。支持匿名映射和文件映射（私有、共享）、`MAP_FIXED`/`MAP_FIXED_NOREPLACE`、按页切分 VMA 的 munmap/mprotect、brk；地址自顶向下分配，栈下方留 1 GiB 间隔。凡是拆除映射或修改权限的操作，都会让运行循环刷新 TLB 和 Tier 1 的翻译缓存。只读打开的文件建立共享映射后，不能再用 mprotect 加写权限（`EACCES`），与宿主映射的权限保持一致。
- **inode 仓库 v1**：表结构按 §8.1。tar.gz 在一个事务里导入，路径只在仓库的命名空间里解析，含 `..` 的条目直接拒绝。Alpine 3.24.3 minirootfs 导入后有 83 个文件、96 个目录、335 个符号链接。`appine-run --rootfs <目录> --import <tar.gz>` 建库，`scripts/fetch-rootfs` 下载并按 sha256 校验 tarball。
- **syscall**（约 60 个）：
  - 文件：`openat`（含 `O_CREAT`/`O_EXCL`/`O_TRUNC`/`O_PATH`）、读写四件套、`lseek`、`getdents64`、`newfstatat`/`fstat`、`readlinkat`、`faccessat(2)`、`mkdirat`、`chdir`/`fchdir`/`getcwd`、`dup`/`dup3`/`fcntl`、`sendfile`、`ppoll`；`ioctl` 只把终端相关的几个请求（`TCGETS`、`TCSETS*`、`TIOCGWINSZ`、`TIOCSWINSZ`）透传给宿主 fd。
  - 内存：`brk`、`mmap`、`munmap`、`mprotect`、`madvise`（`MADV_DONTNEED` 把匿名内存清零）；`mremap` 返回 `ENOMEM`，musl 会退回到复制。
  - 进程、时间、杂项：`exit(_group)`、`set_tid_address`、`futex`（单线程语义）、各种 `get*id`、`prctl`（只支持进程名）、`prlimit64`/`getrlimit`/`setrlimit`、`umask`、`uname`、`sysinfo`、`getrandom`、`clock_*`、`nanosleep`、`gettimeofday`。
  - 信号：`rt_sigaction`、`rt_sigprocmask`、`kill`/`tkill`/`tgkill`。信号有挂起和屏蔽，但还没有信号帧：发给自己的信号要么被忽略，要么按默认动作终止进程（装了处理函数的也按默认动作处理，并记一条日志）。musl 的 `abort()` 因此能正确终止。
- **指令**：musl 的 `memset` 会读 `DCZID_EL0` 并使用 `DC ZVA`，Tier 0 补上了这两条（块大小 64 字节）。
- **测试**：`hosts/cli/tests/rootfs_programs.rs` 的 12 个端到端用例，每个都在两个执行层下运行：`ls`、经符号链接调用 applet、`cat` 跟随相对符号链接、`uname`、两个执行层的 `sha256sum` 结果一致、shell 重定向与追加、`mkdir -p`/`cp`/`find`、stdin、环境变量、退出码，以及找不到程序（127）和不能执行（126）。CI 先运行 `scripts/fetch-rootfs`，并设置 `APPINE_REQUIRE_ROOTFS=1`，找不到 rootfs 时测试直接失败而不是跳过。

**推迟项**：

- §5.2：程序段是拷贝进匿名内存的，代码页还没有经由宿主 page cache 在进程间共享。→ 已补上（M6）
- §5.2：匿名内存是一次映射对应一次宿主 mmap，还没有用 2MB arena。→ 一直没有做，改为限制 VMA 数（§5.2）
- §8.1：没有内存里的 dentry/inode 缓存，每个路径组件都查一次 SQLite。→ 已补上（M9 之前：性能剖析）
- devfs、procfs、vDSO 移到 M3；`unlinkat`、`renameat2`、`symlinkat`、`linkat`、`fchmodat`、`fchownat`、`utimensat` 等修改命名空间的 syscall 也在 M3 补齐。→ 修改命名空间的 syscall 和 vDSO 在 M3a 补上，procfs 在 M3b；devfs 改为留在仓库里（M3b）
- 目前只以 root 运行，VFS 不做权限检查（§11.1）。→ 已补上（M6）

**busybox 补测**（M2 遗留项）。测量条件：Apple Silicon 上的 colima 虚拟机，输入是 16 MB 的数据（busybox 和 `ld-musl` 两个文件重复拼接），从宿主 stdin 读入；原生是在同一台虚拟机上直接用 Alpine 的 `ld-musl` 启动同一个 busybox。每种方式运行 2 次取较短的一次，三种方式的输出逐字相同。

| 命令 | 原生 | Tier 0 | Tier 1 |
|---|---|---|---|
| `sha256sum` | 0.089s | 51.3× | 1.51× |
| `gzip -c` | 0.712s | 34.7× | **4.75×** |

`gzip` 落在"3–5 倍，继续，同时记下优化方向"一档。原因已经查明：`--stats` 显示 Tier 1 退回 Tier 0 达 9800 万次，几乎全部来自 `fill_window` 滑动哈希表的循环。编译器把这段循环生成了 `ldr h`、`uqsub h`、`str h` 这类 SIMD 标量指令，而 Tier 1 还不翻译 SIMD&FP，每执行一条都要退出一次。所以 M2 结果里列出的优化方向前面要再加一条，并排在第一位：

0. **Tier 1 翻译 SIMD&FP**：数据处理指令原样拷贝（guest 的 V 寄存器在块内常驻宿主 V 寄存器），SIMD 访存和整数访存一样走内联 TLB。→ 已补上（M7）

## M3a 结果（2026-10-24）

**结论**：`sh -c` 里的管道、命令替换、子 shell、trap、`find -exec`/`xargs`（vfork）、后台任务和 `wait`/`kill`、SIGPIPE、`#!` 脚本都与原生的 Alpine busybox 行为一致；多线程测试程序在两个执行层下都通过，各连续运行 20 次没有失败。达到退出标准。

- **内核层结构**（§6）：全局状态是一个 `Kernel`，由一把 `Mutex`（BKL）保护。内核对象之间仍然用 `Rc`/`RefCell` 共享，规则是只在持有 BKL 时访问它们；唯一的例外是线程自己的私有状态（`TaskState`），只有它所属的线程会访问，等待期间也一直借用着。每个 guest 线程一个宿主线程，运行循环不持有 BKL 执行 guest 代码，遇到 syscall、同步异常或 kick 时获取 BKL。内核层 panic 时直接终止宿主进程，因为 BKL 保护的状态可能已经不一致。
- **等待**（§6）：只用了"宿主 `poll()` + 每线程 eventfd"这一种途径。任何可能让别人的等待条件成立的修改都请求一次唤醒，释放 BKL 时唤醒所有正在等待的线程，由它们各自重新检查条件（相当于只有一个 wait queue，以后按 profiling 拆分）。宿主 fd 的读先 poll（可以被 guest 信号打断）再读，写在释放 BKL 之后进行，阻塞的宿主 I/O 不会卡住其他 guest 线程。
- **进程与线程**：
  - `clone` 支持 fork、vfork（父进程等到子进程 execve 或退出）、线程，以及 `CLONE_PARENT`、`CLONE_SETTLS`、`CLONE_{PARENT,CHILD}_SETTID`、`CLONE_CHILD_CLEARTID`；`CLONE_NEW*` 返回 `EPERM`，`clone3` 返回 `ENOSYS`（glibc 和 musl 会退回到 `clone`；M9 之前的兼容性补漏实现了 `clone3`）。
  - `execve` 支持 `#!`（最多 4 层），先结束同一线程组的其他线程（非主线程调用时接过主线程的 tid），然后关闭 close-on-exec 的 fd，把信号处理函数恢复默认。
  - `wait4`/`waitid`、`exit`/`exit_group`、进程组和会话（`setpgid`/`setsid` 等）。init 收养孤儿进程并立即回收；父进程忽略 SIGCHLD 或设置了 `SA_NOCLDWAIT` 时子进程不留僵尸。第一个进程结束时会话结束，其余进程收到 SIGKILL。
- **内存**：fork 按 §5.3 v1 整份复制，`MAP_SHARED|MAP_ANONYMOUS` 在 fork 之后真正共享。TLB shootdown 按 §5.1。C 引擎里两处原本不能多线程运行的地方也改了：页表表项改用原子操作读写；原生指令桩的缓存写满会整体清空，现在查找和执行都在读锁内，避免覆盖别的线程正在执行的桩。
- **信号**（§7）：线程和线程组各有挂起集合；信号在 syscall 之后、被 kick 打断之后和同步异常之后投递，同步异常产生的信号优先，一次可以叠放多个帧。帧的布局与 Linux arm64 逐字节一致（有单元测试核对偏移），同步异常时带上合成的 `esr_context`。`rt_sigreturn` 校验 pstate 和每个记录，帧无效时以 SIGSEGV 终止。`sigaltstack` 支持 `SS_AUTODISARM`。syscall 重启实现了 `ERESTARTSYS`/`ERESTARTNOINTR`/`ERESTARTNOHAND`；`rt_sigsuspend` 和 `ppoll` 临时换上的屏蔽字由信号投递恢复。`AT_MINSIGSTKSZ` 改为 4720，与没有 SVE/SME 的 arm64 Linux 算出来的值相同。
- **vDSO**（§7.2）：在运行时按字节拼出 ELF 镜像，导出 `LINUX_2.6.39` 版本的 `__kernel_rt_sigreturn`（前面有一条 nop，与 Linux 相同）和三个时间函数的 svc 存根；有一个单元测试按 musl `__vdsosym` 的算法查找这些符号。
- **其他 syscall**：futex（`WAIT`/`WAKE`/`*_BITSET`/`REQUEUE`/`CMP_REQUEUE`，键是宿主地址）、`pipe2`、`ppoll` 支持管道和宿主 fd 混合等待、`unlinkat`、`renameat`/`renameat2`（含 `RENAME_NOREPLACE`/`RENAME_EXCHANGE`）、`symlinkat`、`linkat`、`mknodat`、`fchmod(at)`、`fchown(at)`、`utimensat`、`(f)truncate`、`(f)statfs`、`close_range`、`sched_getaffinity`、`times`、`getrusage`，以及一些常用的 `fcntl`/`ioctl` 请求。
- **设备**：`/dev/null`、`zero`、`full`、`random`、`urandom` 按设备号在进程内实现。这些节点作为普通的设备节点存在仓库的 `/dev` 里（导入 rootfs 和每次启动时补齐），guest 自己用 mknod 建的同号节点行为相同（M3b 决定保留这种做法，见 M3b 结果）。
- **测试**：`hosts/cli/tests/rootfs_programs.rs` 增加到 21 个端到端用例；`tests/guest/threads.c` 用 musl-gcc 静态编译，检查互斥锁、条件变量（含超时）、原子操作、线程局部存储、发给指定线程的信号、备用信号栈、访存出错后修复返回和 `siglongjmp` 跳出、`SA_RESTART`、多线程进程里的 fork、分离线程的退出。开发容器和 CI 安装了 musl-tools，CI 设置 `APPINE_REQUIRE_MUSL=1`。

**性能**：BKL 没有带来可见的开销。M1b 的 busybox 测量重做一遍（同样的条件），Tier 1 的 `sha256sum` 是 1.50×、`gzip -c` 是 4.84×，与 M1b 的 1.51×、4.75× 在误差范围内。新增一项进程创建的测量：`sh` 循环 200 次 fork + exec `/bin/true`，原生 0.25s，Tier 0 0.68s，Tier 1 0.73s，即每次 3.5ms 左右，代价主要是整份复制和每个线程新建的翻译缓存。

**推迟项**：

- 作业控制在 M3b：停止信号的默认动作暂时按忽略处理（记一条日志），还没有 `SIGCONT`、`WUNTRACED`。→ 已补上（M3b）
- `restart_syscall`（`ERESTART_RESTARTBLOCK`）没有实现：没有处理函数的信号打断 `nanosleep` 时，按原来的时长重新睡。→ 已补上（M9 之前：稳定性和内存）
- 还没有：
  - 间隔定时器（`setitimer`/`alarm`）、`pselect6`、CPU 时间统计（`times`/`getrusage` 返回 0）。→ 已补上（M4）
  - 具名 FIFO、文件记录锁（加锁总是成功）、`chroot`。→ 已补上（M6）
  - robust futex 列表。→ 已补上（M9 之前：稳定性和内存）
  - 实时信号排队（同一个信号挂起多次只算一次）。→ 已补上（M9 之前：兼容性补漏）
- 每个线程一份 Tier 1 翻译缓存，跨进程共享在 M7。→ 已补上（M7，翻译模板）

## M3b 结果（2026-10-24）

**结论**：在进程内 PTY 上运行的交互式 `ash` 里，管道、Ctrl-C、Ctrl-Z/`jobs`/`fg`、`vi` 编辑文件都正常，Tier 0 和 Tier 1 都是。达到退出标准。

- **PTY 与行规程**（§9）：`core/kernel/src/tty.rs` 是纯数据结构，覆盖 `ICANON` 行编辑（ERASE、WERASE、KILL、LNEXT、REPRINT、EOF，`IUTF8` 时按字符退格）、`ECHO`/`ECHOE`/`ECHOK`/`ECHOKE`/`ECHOCTL`/`ECHONL`、`ICRNL`/`INLCR`/`IGNCR`/`ISTRIP`、`IXON` 流控、`OPOST`/`ONLCR`/`OCRNL`、`ISIG`（^C、^\、^Z，产生信号时清空队列），非规范模式按 `VMIN`/`VTIME`。初始设置与 Linux 的 `tty_std_termios` 相同。阻塞、ioctl 和作业控制检查在 `syscall/tty.rs`：`TCGETS`/`TCSETS*`、`TIOCGWINSZ`/`TIOCSWINSZ`（变化时给前台进程组发 SIGWINCH）、`TIOCGPGRP`/`TIOCSPGRP`、`TIOCSCTTY`/`TIOCNOTTY`、`TIOCGSID`、`TCFLSH`、`TCXONC`、`FIONREAD`、`TIOCSTI`、`TIOCGPTN`/`TIOCSPTLCK`。
- **会话与控制终端**：会话组长打开一个还没有会话的终端（没有 `O_NOCTTY`）时它成为控制终端，与 Linux 一样；`setsid` 放弃控制终端；会话组长退出或 `TIOCNOTTY` 时，前台进程组收到 SIGHUP 和 SIGCONT。后台进程组读控制终端时收到 SIGTTIN，设置 `TOSTOP` 时写终端收到 SIGTTOU，修改终端设置和 `tcsetpgrp` 时收到 SIGTTOU；这些信号被忽略或屏蔽、或者进程组是孤儿时，读返回 EIO，写和修改照常进行（Linux 的 `tty_check_change`）。
- **停止与继续**：停止信号的默认动作停住整个线程组，其他线程在下一次进入内核时一起停下，父进程收到 `CLD_STOPPED`；SIGCONT 在产生时就让线程组继续（即使被忽略或屏蔽），并丢掉挂起的停止信号，反之亦然。`wait4` 支持 `WUNTRACED`/`WCONTINUED`，`waitid` 支持 `WSTOPPED`/`WCONTINUED`/`WNOWAIT`，`SA_NOCLDSTOP` 生效。孤儿进程组不响应终端产生的停止信号；进程退出导致某个有停住成员的进程组变成孤儿时，按 POSIX 给它发 SIGHUP 和 SIGCONT（判定条件与 Linux 的 `kill_orphaned_pgrp` 相同：只有这次退出切断了进程组与会话里其他进程组的联系才算）。
- **VFS 挂载**：路径解析从"只认仓库的 inode"改成在一棵混合的树里进行：节点是仓库的 inode，或者合成文件系统的节点（`Node`）。`/proc` 和 `/dev/pts` 挂在仓库的目录上，`..` 能从合成文件系统回到仓库。`/proc/<pid>/fd/<n>` 是"魔法链接"：跟随时直接得到那个打开文件，所以 `cat /dev/stdin` 在 stdin 是管道时也能工作。
- **procfs**：`/proc/self`、`/proc/thread-self`；`/proc/<pid>/` 下的 `stat`、`status`、`statm`、`cmdline`、`comm`、`environ`、`maps`、`mounts`、`cwd`、`exe`、`root`、`fd/`、`task/<tid>/`、`oom_score_adj`（写入被接受但不起作用）；全局的 `cpuinfo`（来自 §4.4 的那份特性集合和 MIDR）、`meminfo`、`stat`、`uptime`、`loadavg`、`version`、`filesystems`、`cmdline`、`mounts`，以及 `/proc/sys/kernel` 下的 `hostname`、`osrelease`、`ostype`、`version`、`pid_max`、`random/boot_id`、`random/uuid` 等几项。文件在打开时生成完整内容。CPU 时间取宿主线程的 CPU 时钟（`pthread_getcpuclockid`），驻留内存用宿主的 `mincore`。为了让别的线程能读到某个进程的 fd 表、当前目录和地址空间，每个线程的这几样资源在 `Task::res` 里有一份共享的引用，替换时同步更新。busybox 的 `ps`、`top`、`df`、`free`、`mount`、`uptime` 输出正常。
- **`appine-run`**：stdin 和 stdout 都是终端时自动在 PTY 上运行（宿主终端切到 raw 模式，退出时恢复），`--pty` 强制、`--no-pty` 关闭。宿主的 SIGWINCH 由一个只写管道的信号处理函数转给桥接线程。
- **测试**：`hosts/cli/tests/rootfs_programs.rs` 增加到 26 个用例，其中三个交互式用例用 `--pty` 运行 `sh -i`，等输出里出现预期的文字再发下一个按键：管道和 `tty`/`stty size`、^Z/`jobs`/`fg`/^C 和 `$?` 为 130、在 `vi` 里输入两行并保存。另有 procfs（`ps`、`/proc/self/status`、`/proc/self/exe`、`df`、`cat /dev/stdin`）和 SIGSTOP/SIGCONT（经由 `/proc/<pid>/status` 观察状态）的用例。行规程有单元测试。整套 rootfs 测试连续运行 10 次没有失败；也在 `script` 提供的真实 PTY 上手工验证过登录 shell、`top` 和 raw 模式的恢复。

**改动和推迟项**：

- **`/dev` 不做内存里的 devfs**（修改 §8.2）：标准设备节点保留在仓库里，只有 `/dev/pts` 是动态的挂载。这正是 Linux 上"静态 `/dev` + devpts"的模型，节点每次启动时补齐，guest 删掉也能恢复。代价是 `/dev/shm` 在仓库里，内容会跨会话保留（Linux 上它是 tmpfs）。→ 已补上：仓库被第一个使用者打开时清空 `/dev/shm`、`/run`（M9 之前：剩下的小缺口）
- 宿主的输入结束（`--pty` 时 stdin 是管道并且关闭了）不算终端挂断：桥接线程不再读，会话照常运行到第一个进程退出。
- `/proc/<pid>/exe` 和 `maps` 里的路径是词法上规范化的路径，不解析符号链接（Linux 报告的是解析后的真实路径）。→ 已补上（M8 补测：JVM）
- `/proc/<pid>/stat` 的 stime、`loadavg` 的负载是 0。没有 sysfs。→ stime 已补上（M9 之前：剩下的小缺口）；`loadavg` 仍是 0，仍没有 sysfs
- 仍然没有 `pselect6`、间隔定时器（`setitimer`/`alarm`）。→ 已补上（M4）

## M4 结果（2026-10-24）

**结论**：`apk update && apk add git python3` 成功（44 个软件包，63 MiB），`git clone --depth 1 https://github.com/octocat/Hello-World.git` 成功，python3 能导入 `ssl`、`sqlite3`、`asyncio`、`hashlib`。达到退出标准。这一项需要外网，CI 里作为单独的一步运行（`APPINE_NETWORK_TESTS=1`），开发容器里经过宿主的 HTTP 代理也能通过。

- **TCP/UDP 透传**（§10）：guest socket 持有一个宿主非阻塞 socket，地址、选项和控制消息按字节原样传递（guest 和宿主是同一种 ABI）。阻塞由内核层在统一的等待机制里模拟，可以被信号打断，`SO_RCVTIMEO`/`SO_SNDTIMEO` 由内核层计时。支持阻塞和非阻塞的 `connect`、`accept4`、`sendmsg`/`recvmsg`，`MSG_PEEK`/`MSG_WAITALL`/`MSG_DONTWAIT`/`MSG_NOSIGNAL`（对方关闭时给发送方 SIGPIPE），`FIONREAD`，接口类 ioctl（`SIOCGIF*`、`SIOCGIFCONF`）。接口类 ioctl 与 socket 的地址族无关，AF_UNIX 和 netlink socket 上也能用（musl 的 `if_nametoindex` 用的是 AF_UNIX socket），这时借一个临时的宿主 socket。guest 的 raw ICMP socket 换成宿主的 ping socket。
- **DNS 存根**：打开仓库时如果没有 `/etc/resolv.conf` 就写一份 `nameserver 127.0.0.53`。发往 `127.0.0.53:53` 的 UDP 数据报（`sendto` 或者先 `connect`）不经过宿主网络：内核层释放 BKL 后交给宿主解析器，回答放进这个 socket 自己的队列，看上去来自 `127.0.0.53:53`；解析失败时回 SERVFAIL，让解析器立即失败而不是等到超时。Linux 开发宿主上的解析器转发给宿主 `/etc/resolv.conf` 里的服务器（UDP，3 秒超时，回答被截断时改用 TCP），所以超过 512 字节的回答也能完整交给 guest；Android 上按 §10 换成 `android_res_nsend`（M5）。发往存根的 TCP 连接没有截获，目前的解析器只在 UDP 回答被截断时才用 TCP。
- **netlink**：`NETLINK_ROUTE` 的 `RTM_GETLINK`、`RTM_GETADDR` 转储由宿主的 `getifaddrs` 生成，`RTM_GETROUTE` 返回空的转储，其他请求回 `EOPNOTSUPP`。另外合成了 `/proc/net/dev`。busybox 的 `ip addr`、`ifconfig` 和 musl 的 `getifaddrs` 都正常。§10 写的 `ConnectivityManager` 数据源在 M5 接入；Android 对 App 限制了 netlink 的部分请求，那时再确认宿主侧能拿到什么。
- **AF_UNIX**（进程内模拟，`core/kernel/src/unix.rs`）：`SOCK_STREAM`、`SOCK_DGRAM`、`SOCK_SEQPACKET`；`socketpair`；路径地址在仓库里建一个 socket 节点（已存在时 `EADDRINUSE`），抽象地址和自动绑定只在内核的名字表里，socket 关闭后地址随之释放；`listen` 最多容纳 backlog + 1 个等待 accept 的连接，与 Linux 相同。`SCM_RIGHTS` 传递的是打开文件本身，接收时装进接收进程的 fd 表（支持 `MSG_CMSG_CLOEXEC`，控制缓冲区放不下时置 `MSG_CTRUNC`）；`SO_PASSCRED`/`SCM_CREDENTIALS`、`SO_PEERCRED` 给出 guest 的 pid，uid、gid 总是 0。流式读取的合并规则和 poll 报告的事件按 Linux 的 `unix_stream_read_generic`、`unix_poll`。每个方向最多缓冲 208 KiB。
- **I/O 多路复用**：进程内的 epoll（水平触发，`EPOLLET` 按水平触发处理，支持 `EPOLLONESHOT` 和嵌套）、`pselect6`、`eventfd2`。`ppoll` 从 `fs.rs` 移到了 `poll.rs`，三者共用一套"先查进程内对象和宿主 fd，都没就绪再一起等"的逻辑。
- **间隔定时器**：`setitimer`/`getitimer` 的 `ITIMER_REAL`（musl 的 `alarm` 也经由它）。到期时刻记在线程组里，由宿主主线程负责：它本来就在等会话结束，现在等到最早的到期时刻，给进程发 SIGALRM，有间隔的重新计时，错过的周期不补发。按 CPU 时间计时的 `ITIMER_VIRTUAL`、`ITIMER_PROF` 返回 `EINVAL`。
- **文件系统**：`flock` 真正互斥（锁属于打开文件，关闭后释放，支持 `LOCK_NB`；apk 用它锁数据库）；扩展属性存在仓库新加的 `xattrs` 表里，12 个 xattr syscall 都实现了，`user.*` 只能加在普通文件和目录上；`memfd_create` 是一个没有名字的 inode，`F_ADD_SEALS`/`F_GET_SEALS` 的写入、增长、缩短封印会被检查；`statx`、`fallocate`（预留、打洞、清零）、`preadv`/`pwritev` 和 `preadv2`/`pwritev2`。
- **其他 syscall**：`getpriority`/`setpriority`（nice 值记录在线程组里，`/proc/<pid>/stat` 能看到，不影响宿主调度）、`sched_*`（不支持实时调度，`SCHED_FIFO`/`SCHED_RR` 返回 `EPERM`；musl 自己就不调用 `sched_getscheduler` 等几个）、`mlock` 一族和 `mincore`（只检查范围）、`prctl` 的 `PDEATHSIG`（父进程退出时发出）、`DUMPABLE`、`NO_NEW_PRIVS`、`CAPBSET_READ`、`GET_SECCOMP`、`GET_TIMERSLACK`。`getrusage`、`times` 和 `wait4`/`waitid` 的 rusage 现在给出 CPU 时间（M3a 时全是 0）；宿主的线程 CPU 时钟分不出用户态和内核态，全部算作用户态。
- **跑 apk、git、python3 时发现并修正的问题**：
  - fork 复制私有文件映射时读到了文件末尾之后的页，宿主发 SIGBUS（busybox 的 trigger 脚本里出现）。VMA 现在记录有文件内容的长度；只读的私有文件映射在 fork 之后直接共用宿主映射，mprotect 加写权限之前先复制一份自己的。
  - futex 的键原来是宿主地址。不同进程分别 mmap 同一个文件（`sem_open` 的 `/dev/shm` 文件）时宿主地址不同，跨进程唤醒丢失，python 的 `multiprocessing.Pool` 因此卡死。文件的共享映射现在以宿主文件的 (设备号, inode, 偏移) 为键。
  - 多个 appine 进程同时写同一个仓库时 SQLite 返回 `SQLITE_BUSY`，变成 `EIO`；更糟的是创建文件这类多步操作中途失败，留下指向不存在 inode 的目录项。现在设置了 10 秒的 busy_timeout，写事务用 `BEGIN IMMEDIATE`，创建文件、目录、符号链接、设备节点、硬链接和修改 xattr 都在一个事务里完成（导入 rootfs 时并入外层的大事务），所有语句都缓存预编译结果。仓库目录里新增一个 `lock` 文件：每个进程持有它的共享锁，只有能独占时才清理 nlink = 0 的 inode，否则会删掉别的进程还打开着的文件。
- **测试**：`tests/guest/net.c` 在两个执行层下检查 AF_UNIX（socketpair、跨 fork 的 `SCM_RIGHTS`、抽象地址上的 listen/connect 和双方的 `SO_PEERCRED`、`EPIPE`/SIGPIPE）、TCP/UDP 回环、`getifaddrs`、epoll/eventfd/select、`setitimer`/`alarm`，都不需要外网；rootfs 用例增加到 30 个：`ip`/`ifconfig`/`/proc/net/dev`/`resolv.conf`、syslogd 和 logger 经由 `/dev/log` 路径地址传日志、`flock -n` 互斥，以及需要外网的 apk + git + python3 用例。单元测试覆盖 netlink 转储、DNS 存根、epoll 的 `EPOLLONESHOT`、AF_UNIX 的读取合并规则和关闭语义、定时器重新计时、仓库的 xattr 和匿名文件、多个连接并发写。另外手工验证了 python3 的 `asyncio`、`multiprocessing.Pool`（3.14 默认的 forkserver 方式）、`socket.send_fds`/`recv_fds`、`os.memfd_create` 和封印、xattr。

**R5：apk 的元数据开销。** 测量条件：Apple Silicon 上的 colima 虚拟机，release 构建，Tier 1，仓库放在容器自己的文件系统上；先 `apk fetch -R git python3` 下载好 33 个包（23.6 MB），再离线安装，排除网络。原生是同一台虚拟机上的 `alpine:3.24` 容器。

| 操作 | 原生 | appine Tier 1 | 倍数 |
|---|---|---|---|
| `apk add`（33 个包） | 0.45s | 3.1s | 6.9× |
| `apk del` | 0.19s | 1.0s | 5.4× |
| `apk info`（只读，不改元数据） | 0.15s | 0.95s | 6.3× |
| 对 33 个包逐个 `gzip -dc` | 0.49s | 1.8s | 3.7× |

只读的 `apk info` 和安装慢得差不多，说明瓶颈不在每个元数据操作一个事务。`--stats` 显示这一轮里有 2.4 亿次单条指令交给 Tier 0 执行（主要是 Tier 1 还不翻译的 SIMD&FP）、1.37 万次代码缓存清空（munmap 之后整个缓存作废），这两项留给 M7。单看元数据操作（扣除 shell 循环本身）：创建空文件约 75µs、stat 约 15µs、删除约 40µs，原生都在 10µs 以内；本里程碑把所有语句改成缓存预编译结果、多步操作合并成一个事务之前分别是 115µs、25µs、85µs。R5 因此关闭，不做按 syscall 批量提交。

**改动和推迟项**：

- epoll 没有用宿主 epoll（修改 §10），见上。
- AF_UNIX 没有垃圾回收：socket 经由自己传递自己形成的环不会被释放（Linux 用专门的 GC 处理）；没有 `/proc/net/unix`。→ 都已补上：`/proc/net/unix`（M9 之前：兼容性补漏），垃圾回收（M9 之前：推迟项）
- 没有实现 `execveat`（musl 的 `fexecve` 会退回到 `/proc/self/fd/<n>`）、`membarrier`（musl 有基于信号的退路）、`pidfd_open`（python 会退回）、POSIX 定时器、`timerfd`、`signalfd`、`inotify`、文件记录锁（`F_SETLK` 仍然总是成功）。→ 都已补上：POSIX 定时器、timerfd、signalfd、记录锁（M6），`membarrier`（M8），inotify（M8 补测：JVM），`execveat`、pidfd（M9 之前：兼容性补漏）
- 1024 以下的端口、回环地址对其他 App 可见等 §10 的问题在 M5 的真机上确认。→ M5 没有接真机，仍待确认

## M5 结果（2026-10-24）

**结论**：APK 在 arm64 模拟器上装好即用。Android 14（API 34，4K 页）和 Android 17（API 37，16K 页内核）上，首次启动自动导入 rootfs、进入登录 shell；`apk update`、`apk add python3 git`、python3 导入 `ssl`/`sqlite3`/`asyncio`、`git clone https://…` 都成功（这里的网络连 GitHub 要经过宿主的代理）；instrumentation 测试在两种页大小上都通过。按决定，这一步没有提交 Play 测试轨道。这次没有接真机，"真机上装好即用"还要在手机上确认。

- **内核层接口**：新增 `Machine`（`core/kernel/src/machine.rs`），一个常驻的内核实例。`spawn` 启动进程（init 的子进程）；`open_console`、`read_console`、`write_console`、`resize_console`、`close_console` 管理控制台，即宿主持有主设备的进程内 PTY；另有 `wait`/`try_wait`、`kill_all`。方法可以在任意宿主线程上调用，阻塞的调用和 guest 线程一样走统一的等待机制。关闭控制台按 Linux 挂断终端的规则，给会话组长发 SIGHUP 和 SIGCONT。间隔定时器改由一个专门的宿主线程负责（M4 时由宿主主线程顺带负责）。`appine-run` 的 `run()` 改为基于 `Machine` 实现。另外新增公开函数 `write_file`，首次启动时用它往仓库里写配置文件。
- **原生库**（`hosts/android`）：cdylib `libappine.so`，用 `jni` 0.21。`Machine` 以 `Arc` 的裸指针交给 Kotlin，每次调用临时加一个引用，所以 `stop` 和别的线程上正在阻塞的读并发时也安全。出错时抛 `IOException`，stderr 转到 logcat。这个 crate 只为 Android 编译，其他宿主上是一个空库。NDK 29 默认把 LOAD 段按 16K 对齐，APK 里不压缩存放（`useLegacyPackaging = false`），16K 页设备可以直接从 APK 映射。
- **HAL 的 Android 部分**：
  - DNS 存根交给 `android_res_nsend`（默认网络，另加 30 秒兜底超时）。
  - `flock` 改用 libc，因为 Rust 标准库的 `File::lock` 在 Android 上没有实现。
  - Android 不让 App 读硬件地址，`SIOCGIFHWADDR` 返回的地址族是 0；内核层按接口是不是回环补上 `ARPHRD_LOOPBACK` 或 `ARPHRD_ETHER`，`ifconfig` 不再显示 `UNSPEC`。
  - netlink 仍然由宿主的 `getifaddrs` 提供数据。targetSdk 37 下 bionic 能列出接口和地址（没有 MAC），所以暂时不需要 §10 写的 `ConnectivityManager`（修改 §10）。
- **引擎**：
  - R6：Tier 1 把宿主 x18 当临时寄存器，而 bionic 在每个线程的 x18 里放着影子调用栈指针。现在进入翻译后的代码时把 x18 存进栈帧，退出时恢复。翻译后的代码从不直接调用宿主函数，只有这段时间里到达的宿主异步信号会看到错误的 x18，而 App 进程里不会有人给 guest 线程发信号。→ M7 改为翻译后的代码不碰 x18（R6 关闭）
  - `__builtin___clear_cache` 换成内联的缓存维护，做法与 compiler-rt 的 aarch64 实现相同：Rust 链接 Android 动态库时不带 compiler-rt，加载时找不到 `__clear_cache`。
  - 代码缓存仍是可读写可执行的匿名映射，`untrusted_app` 允许这样做，两个模拟器上 Tier 1 都正常。→ M7 改为 W^X 双映射
- **App**（`android/`）：AGP 9.4.1、Gradle 9.8.2，用 AGP 内置的 Kotlin 支持；minSdk 29，compileSdk 和 targetSdk 37，只打 arm64-v8a。Gradle 任务用 cargo-ndk 构建原生库（总是 release 构建），并把 rootfs 放进 assets。asset 叫 `rootfs.tgz`，因为 AAPT 会把以 `.gz` 结尾的 asset 自动解压。
  - `TerminalService`（§11.2）：`specialUse` 前台服务，持有内核和所有会话，通知里显示会话数，带"全部退出"。会话的回调由服务转发给当前的 Activity，Activity 销毁、重建不影响会话。最后一个标签页关闭时整个应用退出。
  - `MainActivity`：
    - 标签栏：点击切换，长按关闭，"+" 新开。
    - 终端：双指缩放字号，字号会保存。会话结束后按回车关闭它。
    - 扩展键：ESC、TAB、一次性的 CTRL/ALT、`-`、`/`、`|` 和方向键。
    - 用 `WindowInsetsCompat` 给系统栏和输入法让出位置（targetSdk 35 起内容默认画到系统栏后面）。
  - 终端库：vendor 了 Termux 的 `terminal-emulator`、`terminal-view`（提交 `8629e632`）。删掉了 JNI 和原生代码，改写了 `TerminalSession`，经由 `Pty` 接口接到控制台，线程结构和对外接口不变；shell 退出之后最多等读线程 0.5 秒，取完剩下的输出。扩展键栏是重新写的，因为 Termux 的那份属于 GPLv3 的主模块。来源和改动记在 `android/README.md`。
  - 首次启动（§11.4）：
    1. 先导入到 `rootfs.tmp`，完成后再改名，导入中途被杀时下次重来。
    2. 写入 `/etc/apk/repositories`、`/etc/hostname`、`/etc/hosts`（已有就不动）、`/etc/motd`，以及 `/etc/profile.d/appine.sh`（设置 `TERM`、`LANG`、彩色 `PS1`，显示 motd，切到 `$HOME`）。
    3. 在后台给 dl-cdn、清华、中科大、阿里云四个镜像并发测速：下载 APKINDEX 的前 128 KB，每个最多 5 秒。最快的写进仓库列表。

    在模拟器上，首次启动到出现提示符约 1 秒。
- **测试**：
  - `app/src/androidTest` 的 `MachineTest`：两个执行层下运行 busybox（guest 看到的页大小是 4096、`/dev/pts/N`、sha256），交互式 shell 的输入和 ^C，三个控制台并发。在两个模拟器上本地运行。
  - Linux 侧新增 `Machine` 的用例：两个会话同时运行、输入、改窗口大小、关闭控制台时 shell 收到 SIGHUP。
  - CI 新增一个 x86_64 上的 Android job：Android 目标的 clippy、`assembleDebug`、lint，APK 作为构件上传。
  - 手工验证了多标签页、`top` 这样的全屏程序、软键盘和扩展键、通知里的"全部退出"、按返回键回到桌面之后会话还在。

**改动和推迟项**：

- 不提交 Play 测试轨道（这次的决定），R2 留到提交时验证。
- netlink 没有接 `ConnectivityManager`，见上。
- 首次启动的 tarball 只按 sha256 校验，没有校验 GPG 签名。
- 还没有：wake lock 开关、`onTrimMemory`、被杀之后恢复标签页（M9）、DocumentsProvider（§11.3）、设置界面（选执行层、换镜像、重置 rootfs）。
- 没有真机，R1 的真机性能测量、1024 以下端口和回环地址对其他 App 可见等问题仍待确认。

## M6 结果（2026-10-25）

**结论**：在 appine 里从源码编译成功。Lua 5.4.7 `make linux` 用 51 秒，编出来的 `lua` 正常；SQLite 3.50.4 `./configure && make` 用 15 + 485 秒，编出来的 `sqlite3` 正常。两者都比原生慢约 10 倍。python3 测试集子集的 29 个测试文件（2929 个用例，跳过 307 个）全部通过。LTP 的 syscall 测试从 490 个通过增加到 849 个，剩下的失败大多是 appine 不打算支持的功能（见下）。达到退出标准。

**测量条件**：与 M4 相同：Apple Silicon 上的 colima 虚拟机（4 核），release 构建，Tier 1，单个 make 作业。原生是同一台虚拟机上的 `alpine:3.24` 容器。

| 负载 | 原生 | appine Tier 1 | 倍数 |
|---|---|---|---|
| Lua 5.4.7 `make linux` | 4.9s | 51s | 10× |
| SQLite 3.50.4 `./configure` | 1.2s | 15s | 12× |
| SQLite 3.50.4 `make` | 50s | 485s | 9.6× |

- **性能**：M6 开始时 Lua 要编 5 分 33 秒，找到的问题有三个：
  1. **代码缓存清空太频繁**。编译 `lvm.c` 这一个文件就清空了 19613 次：gcc 的垃圾回收器、malloc 频繁地 munmap、mprotect，原来每次都作废整个代码缓存。`Mm` 现在分开记两个代数：TLB 代数在任何映射变化时递增，代码代数只在可执行的映射被拆除、改了权限或者被 `MADV_DONTNEED` 时递增；只有后者变化时才清空翻译缓存。
  2. **翻译表太小**。表满了也要整体清空，表从 2^16 项加到 2^18 项。
  3. **加载器拷贝整个程序**。cc1 有 37 MB，每次 execve 都把各段拷贝进匿名内存。现在仓库里的程序以私有文件映射的方式映射各段（§5.2），代码页经由宿主 page cache 在进程之间共享；只有段之间共用一页、或者偏移没有对齐时才退回到拷贝。

  三项修好之后，编译 `lvm.c` 从 32.3 秒降到 7.4 秒（原生约 0.4 秒），整个 Lua 从 333 秒降到 51 秒。剩下的瓶颈很集中：编译 `lvm.c` 时有 1.34 亿次退回 Tier 0 执行单条指令，按类别是 SIMD 访存约 60%、SIMD 数据处理约 21%、整数访存的 TLB 未命中约 19%。前两类来自 musl 用 NEON 写的 `memcpy`、`strlen` 这类函数。退回 Tier 0 的指令只占翻译指令的 0.9%，执行次数却很高。所以 R1 的退路"先让 Tier 1 翻译 SIMD&FP"现在有了实测依据。
- **凭据与权限**（§11.1）：`Cred` 放在线程组里，包括 ruid/euid/suid/fsuid、四个 gid、附加组、能力集（有效、允许、可继承、边界）和 securebits。
  - 实现了 setuid 系列的 14 个 syscall 和 `capget`/`capset`，用户 ID 变化时按 capabilities(7) 的规则调整能力。
  - VFS 做 DAC 检查：路径上每个目录的搜索权限、open 的读写权限、新建和删除要求父目录可写、粘滞位、chmod/chown/utimes 的属主规则、access 用真实 ID。
  - 新文件的属主是 fsuid/fsgid，setgid 目录里继承目录的属组。
  - execve 时 setuid/setgid 位生效，并设置 `AT_SECURE`。
  - kill、setpriority、调度参数、prlimit 按凭据检查权限；`/proc/<pid>` 的属主是进程的有效 ID；PTY 从设备属于打开主设备的进程。
- **文件**：
  - fcntl 的记录锁：传统的 POSIX 锁属于 fd 表，关闭任何一个指向该文件的 fd 都会释放它在该文件上的全部锁；OFD 锁属于打开文件。锁会拆分和合并，F_SETLKW 能检测死锁（`core/kernel/src/locks.rs`）。
  - FIFO：同一个 inode 的所有打开共用一个管道，open 按 Linux 的规则等对端。
  - chroot（每个进程记录自己的根目录）、ETXTBSY、`RLIMIT_FSIZE`（SIGXFSZ）、`RLIMIT_NOFILE`。
  - 新增 `copy_file_range`、`splice`/`tee`/`vmsplice`（经由宿主缓冲区复制，不是零拷贝）、`name_to_handle_at`/`open_by_handle_at`（句柄就是 inode 编号），以及 `fadvise64`、`sync_file_range`、`readahead`、`syncfs`。
  - 管道容量可以用 `F_SETPIPE_SZ` 修改。
  - `pwritev2` 支持 `RWF_NOAPPEND`：Alpine 3.24 的 musl 用它实现 pwrite，原来返回 EOPNOTSUPP 时，musl 的退路会让 O_APPEND 文件上的 pwrite 全部失败。
  - 所有 guest 进程共用宿主进程的 fd，每个打开的仓库文件、socket 都占一个，所以 `Machine` 启动时把宿主的 fd 软限制提到硬限制。
- **内存**：
  - `mremap`：搬家时不复制数据，只把同一段宿主内存映射到新地址；扩展出来的部分是匿名内存，共享文件映射则接着映射文件。
  - `msync` 做参数检查。
  - `mlock` 一族记录哪些映射被锁住，实行 `RLIMIT_MEMLOCK`，`/proc/<pid>/status` 有 VmLck。
  - `madvise` 检查参数和区间，`mincore` 改用宿主的 mincore。
  - mmap 支持 `MAP_SHARED_VALIDATE`、`MAP_LOCKED` 和 `/dev/zero`；封印了写入的 memfd 不能有可写的共享映射。
- **进程间通信和定时器**：
  - System V 消息队列、信号量集（含 `SEM_UNDO`）和共享内存段，有 `/proc/sysvipc/*`，`/proc/sys/kernel` 下的限制可以修改（`core/kernel/src/ipc.rs`）。共享内存段是一段宿主内存；段被 shmat 的次数不单独记录，而是数各个地址空间里的映射。
  - POSIX 定时器：墙上时钟和进程、线程的 CPU 时钟，`SIGEV_SIGNAL`/`SIGEV_THREAD_ID`/`SIGEV_NONE`，超限次数记在挂起的信号里。
  - 另外还有：timerfd、signalfd、`rt_sigtimedwait`、`rt_sigqueueinfo`、按 CPU 时间计时的 `ITIMER_VIRTUAL`/`ITIMER_PROF`、`RLIMIT_CPU`（SIGXCPU）、进程和线程的 CPU 时钟。
  - epoll 的 `EPOLLET` 改为按状态变化触发，并检查嵌套和成环。
- **其他 syscall**：
  - 调度：调度策略、实时优先级和 `SCHED_DEADLINE` 的参数按线程记录（只记录，由宿主调度），`sched_setattr`/`sched_getattr`，CPU 亲和性。
  - prctl：补了 keepcaps、securebits、边界集、环境能力、timerslack 等选项，用不到的参数要求为 0。
  - 还有 `personality`、`getcpu`、`get_robust_list`、`sendmmsg`/`recvmmsg`、`sethostname`/`setdomainname`、`reboot`（只接受 Ctrl-Alt-Del 的开关）。
  - `/proc/<pid>/stat` 只有真正在内核里等待的线程才报告 S：刚 fork 还没运行的子进程原来也报告 S，LTP 里按状态同步的测试因此出现竞争。
- **LTP**：`scripts/build-ltp` 在 `alpine:3.24` 容器里原生编译 LTP 20260930（1562 个程序），`scripts/ltp` 每个测试启动一个新的 `appine-run`，同时把一份描述 appine 特性的内核配置交给 LTP（`KCONFIG_PATH`）。syscall 测试共 1552 个：

  | | M6 开始时 | M6 结束时 |
  |---|---|---|
  | 通过 | 490 | 849 |
  | 不适用（TCONF） | 310 | 393 |
  | 失败 | 752 | 310 |

  剩下的失败按原因：
  - 176 个需要块设备、挂载或者特定的文件系统：LTP 用 loop 设备建文件系统来测（`.needs_device`、`.all_filesystems`），appine 没有块设备。
  - 18 个是命名空间、ptrace、cgroup；18 个是 inotify/fanotify、AIO、io_uring、POSIX 消息队列、userfaultfd；18 个是文件租约和 core dump；14 个要修改宿主时间；2 个是内核模块、keyctl。这些都没有实现，见下面的推迟项。
  - 16 个是 musl 自己的行为，在真正的 Linux 上配 musl 也会失败：musl 的 `clone()` 拒绝 `CLONE_THREAD`，`sched_setscheduler` 不调用内核，`sigtimedwait` 遇到 EINTR 自动重试，`sendmsg`/`recvmmsg` 先在用户态读写参数，`fchmod` 对 O_PATH fd 退回到 `/proc/self/fd`。
  - 10 个程序在 LTP 的其他目录里，`scripts/build-ltp` 没有编。
  - 剩下 38 个是零散的语义差别，比如 `EPOLLEXCLUSIVE`、`/proc/<pid>/smaps` 和 `pagemap`、`MAP_GROWSDOWN` 的栈、一些 ioctl 的错误码。
- **测试**：
  - 单元测试新增凭据和能力的转换、DAC、粘滞位和 setgid 目录、记录锁的拆分合并和死锁检测、System V IPC、mremap、执行文件映射的 .bss。
  - `tests/guest/posix.c` 在两个执行层下检查消息队列、信号量（`SEM_UNDO`）、跨 fork 的共享内存、POSIX 定时器（超限次数、`SIGEV_THREAD`、CPU 时钟）、signalfd、管道之间的 splice/tee/vmsplice、mremap、降低权限之后的凭据规则、亲和性和实时策略、`RLIMIT_NOFILE`。
  - rootfs 用例新增一个：su 到 nobody 之后检查 DAC。
  - 另外，`APPINE_STRACE=1` 会打印每个 syscall 的参数和结果，查 LTP 失败时用到。

**改动和推迟项**：

- 没有：inotify/fanotify、文件租约（`F_SETLEASE`）和 `O_ASYNC`/SIGIO（`F_SETOWN` 等只记录）、core dump、AIO/io_uring、POSIX 消息队列、挂载、命名空间、ptrace、seccomp 过滤、文件能力和环境能力。→ inotify 已补上（M8 补测：JVM）；文件租约、`O_ASYNC`、AIO、POSIX 消息队列已补上（M9 之前：推迟项）；io_uring 决定不做（§15）
- 线程退出时不处理 robust futex 链表（只记录 `set_robust_list`）。→ 已补上（M9 之前：稳定性和内存）
- `MREMAP_DONTUNMAP` 不支持；私有文件映射不能用 mremap 扩展。→ `MREMAP_DONTUNMAP` 已补上（M9 之前：兼容性补漏、推迟项）；私有文件映射仍然不能扩展，返回 ENOMEM
- epoll 没有 `EPOLLEXCLUSIVE`。→ 已补上（M9 之前：稳定性和内存、推迟项）
- Tier 1 翻译 SIMD&FP 原本在 R1 的退路里。按上面的数据，建议把它放进 M7 作为第一项。→ 已补上（M7）

## M7 结果（2026-10-25）

**结论**：Tier 1 补全成正式版。编译 `lvm.c` 这一个文件从 M6 结束时的 7.4 秒降到 3.3 秒，是原生的 4.8 倍；SQLite `make` 从 485 秒降到 237 秒（4.7×），Lua `make linux` 从 51 秒降到 30.1 秒（6.1×），整数基准的几何平均是 1.77×。达到性能目标：CPU 密集的负载在 3–5 倍以内，短命进程多的构建脚本略高。100 万轮的三方 lockstep，以及新加的差分测试（6 组脚本，与 docker 里的原生 Alpine 逐字节比对输出），在 Tier 1 下全部通过。达到退出标准。

**测量条件**：与 M6 相同：Apple Silicon 上的 colima 虚拟机（4 核），release 构建，Tier 1，单个 make 作业，原生是同一台虚拟机上的 `alpine:3.24` 容器。M6 记的"`lvm.c` 原生约 0.4 秒"偏低，同样条件下重测是 0.69 秒。

| 负载 | 原生 | M6 | M7 | 倍数 |
|---|---|---|---|---|
| 编译 `lvm.c`（`gcc -O2 -c`） | 0.69s | 7.4s | 3.3s | 4.8× |
| Lua 5.4.7 `make linux` | 4.9s | 51s | 30.1s | 6.1× |
| SQLite 3.50.4 `./configure` | 1.2s | 15s | 9.2s | 7.7× |
| SQLite 3.50.4 `make` | 50s | 485s | 237s | 4.7× |
| `sh` 循环 200 次 fork + exec `/bin/true` | 0.25s | 0.73s（M3a） | 0.46s | 1.8× |
| `appine-bench` 几何平均 | — | 1.85×（M2） | 1.77× | |

CI 的 `ubuntu-24.04-arm` runner（Neoverse N2，2 vCPU）上，性能探针的几何平均从 M2 的 2.37× 降到 2.25×。这种核心比 Apple Silicon 窄，更接近手机的大核和中核：

| 程序 | M2（Neoverse N2） | M7（Neoverse N2） |
|---|---|---|
| sha256 | 2.06× | 1.88× |
| lz | 2.33× | 2.24× |
| sort | 1.98× | 1.80× |
| crc32 | 2.32× | 2.42× |
| vm | 3.37× | 3.15× |
| **几何平均** | **2.37×** | **2.25×** |

间接分支仍然最弱（`vm`），下一步的优化方向（`ret` 走宿主的返回地址预测）不变。crc32 从 2.32× 略升到 2.42×，原因还没有细查。

- **编译负载**按影响从大到小（括号里是编译 `lvm.c` 的时间）：
  1. **Tier 1 翻译 SIMD&FP**（7.4 → 5.0 秒）：guest 的 V0–V31、FPCR、FPSR 在翻译后的代码里一直放在宿主寄存器里，数据处理指令原样拷贝；SIMD 访存走和整数一样的 TLB 快速路径。退回 Tier 0 执行单条指令的次数从 1.34 亿降到 3179 万。
  2. **TLB 从 512 项加到 4096 项**（→ 4.0 秒）。模拟显示剩下的未命中里 86% 是直接映射的冲突，4096 项就能命中；清空 TLB 改为只处理填过的项，munmap 频繁的 gcc 清空 1.95 万次也不贵。cpu 结构体为此重新排了布局：生成的代码直接寻址的字段放在 TLB 前面。
  3. **跨页检查并进 TLB 比较，前向的链接跳过块入口的 kick 检查**（→ 3.7 秒）。
  4. **代码缓存里的 TLB 回填例程**（退回 Tier 0 从 696 万次降到 311 万次）：未命中时不退出到分发器，直接遍历页表。
  5. **`dc zva`、`mrs dczid_el0`、LL/SC 和 LSE 在 Tier 1 里翻译**：前两项来自 musl 的 memset，退回 Tier 0 降到 48 万次，剩下的基本是真正跨页的访存。
  6. **块不在条件分支处结束，条件分支直接链接到目标块**（→ 3.25 秒），翻译的块从 50 万降到 14 万。跳转缓存试过从 4096 项加到 16384 项，间接分支未命中从 374 万次降到 131 万次，但只快了 1%，每个线程却要多占 192 KB，所以保持 4096 项。

  剖析（perf 采样，按生成的每条宿主指令的用途归类）显示，翻译后的代码占八成以上的时间，其中 TLB 检查和访问本身约占三分之一，块之间的跳转（取指延迟）约占两成，进出例程降到 2% 左右。剩下的开销主要是 soft-MMU 访存链路上多出来的延迟和代码膨胀，见下面的推迟项。
- **短命进程**：SQLite 的 `./configure` 里，翻译占了 46% 的时间（236 万个块，平均每个 1.5µs）。
  - **翻译模板共享**（§4.2）：按 guest pc 和指令字比对，`./configure` 里 85% 的块直接拷贝模板。模板区 32 MB，写满时整体丢弃；同一个 pc 最多留 4 个版本，否则 JIT 反复改写同一处代码时查找会退化（lockstep 就是这样的负载，一度从 10 秒变成 766 秒）。
  - 翻译本身也提速：CTR_EL0 只读一次（这台虚拟机上 EL0 读它会陷入内核），工作区不再每次 malloc，每个块只查一次页表。
  - 退出的线程留下的 Tier 1 实例清空后放进一个两个位置的池子，新线程直接复用，不必重新映射代码缓存、在第一次写入每页时缺页。
  - 这台虚拟机上 EL0 的 `dc cvau`/`ic ivau` 也会陷入内核，每生成一个块都要付出这笔开销；真机上通常不陷入，所以这一项在真机上会小一些。
- **LL/SC**（§4.5，R4 关闭）：STXR 改写成两条指令之间没有访存的 `ldaxr`/`stlxr` 比较并交换循环。顺带修了两个原型的问题：Tier 1 把 CLREX 当屏障原样拷贝，没有清掉 guest 的独占标记；Tier 1 的 svc 退出也没有清。
- **自修改代码**（§4.2"失效"）：Tier 0 实现了 `mrs ctr_el0`（DIC = IDC = 0）和 `dc cvac`/`dc cvau`/`dc civac`/`ic ivau`（原来都是未定义指令，Node.js、Java、LuaJIT 这类带 JIT 的程序和 libffi 闭包都会收到 SIGILL）。`ic ivau` 以新的退出原因交给内核，内核把这一页记进 `mm` 的代码失效日志，打断共用地址空间的其他线程；可执行映射的拆除、改权限同样只作废涉及的页，不再整体清空翻译缓存。`tests/guest/jit.c` 在两个执行层下检查：同一页反复改写、只改一条指令、W^X 的 mprotect 切换、memfd 双映射（需要文件系统，不带 rootfs 时跳过）、另一个线程执行过的代码被改写，以及对未映射地址做缓存维护得到 SIGSEGV。去掉内核里的作废之后，这个测试在 Tier 1 下确实失败。
- **精确异常**：
  - 翻译后的代码里，访存出错都交给 Tier 0 从头执行那条指令，状态是精确的；差分测试的 `signals` 用例把 x19–x28 设成已知的值后触发各种同步异常，比对 `si_code`、出错地址、ESR 的 EC 和 WnR 以及这些寄存器，其中 x27、x28 在 Tier 1 里存放在上下文中。
  - 差分测试找出了两处与 Linux 不同的地方：数据访问的对齐错误原来报成 EC 0x26（SP 对齐），现在是 EC 0x24 加对齐错误的 DFSC，SP 对齐另用一个错误类别；`brk` 的 SIGTRAP 原来带 `esr_context`，Linux 不带（§7.3）。
  - 文件映射超出 EOF 的页不再放进页表（§5.2）：guest 访问时得到 SIGBUS（`BUS_ADRERR`），原来是宿主进程直接被 SIGBUS 杀掉。LTP 的 `mmap13` 因此通过。
- **W^X**：代码缓存和原生指令桩都改为双映射（§12 第 8 条），Linux 和 4K、16K 页的 Android 模拟器上的 App 进程里都没有 RWX 映射了。
- **宿主 X18**（R6 关闭）：临时寄存器改用宿主 X27，guest X27 存放在上下文里，翻译后的代码不再碰 X18。编译 `lvm.c` 和基准程序的耗时差别都在误差范围内。
- **每个线程的内存**：完整跑 LTP 时，`futex_cmp_requeue01`（fork 出 1000 个等待的子进程）被虚拟机的 OOM killer 杀掉，峰值常驻内存 3.3 GB，平均每个线程 3 MB 多。原因有三个，第一个从 M6 起就有：
  1. Tier 1 实例的几张表按最大规模用 calloc 分配。glibc 释放过同样大的块之后会调高 mmap 的阈值，之后的 calloc 改从堆里分配并逐字节清零，几 MB 的表整块占用内存。现在实例、各张表和 cpu 结构体都直接用 mmap 分配，没碰过的页不占内存。
  2. 翻译表按哈希散开，短命进程的几百个块在 2^18 项的表里会各自碰到一页新的内存。现在从 2^12 项开始，装到四分之三时加倍。
  3. 4096 项的 TLB 初始化时要逐项写成"无效"。现在 tag 存页号取反，0 表示无效，清零的内存就是空的 TLB。

  改完之后同一个测试的峰值是 410 MB，平均每个线程约 400 KB，编译 `lvm.c` 的耗时不变。
- **其他**：mprotect 只增加权限时不再刷新 TLB；`--stats` 增加模板命中和作废的块数。
- **测试**：
  - lockstep 的报告增加一列 `tier1-fallback`：Tier 1 退回 Tier 0 执行单条指令的次数，用来确认 SIMD、原子操作这些类别确实在翻译后的代码里执行，而不是因为 TLB 未命中退回 Tier 0。100 万轮全部一致。
  - 差分测试 `scripts/difftest`（§13.3 第 4 层）：busybox 的文本处理和压缩、fork 与信号、CPython（哈希、浮点格式化、decimal、unicode、sqlite3、线程）、gcc 编译运行 C 程序（浮点、自动向量化、原子操作、setjmp）并比对编译器输出的散列、Lua 和 SQLite 命令行、同步信号的 ucontext。两个执行层下都和原生一致。
  - 单元测试新增代码失效日志、文件映射超出 EOF 的页表和 SIGBUS 判定。
  - LTP 的 syscall 测试 849 个通过，与 M6 持平：新通过 `mmap13`（超出 EOF 的 SIGBUS）、`mprotect04`（改权限之后执行拷贝来的代码，要用 `ic ivau`）和 `splice07`；`clock_nanosleep02`、`epoll_pwait03` 变成失败，它们检查睡眠的超时精度（睡 1ms 实际睡了 1.6ms），用 M6 的二进制在同一环境下重测也失败，是虚拟机当时的计时器延迟。
  - App 的 instrumentation 测试在 4K、16K 页的模拟器上通过。

**改动和推迟项**：

- 跨进程共享翻译按指令内容比对，而不是原计划的按 `(inode, offset)`：不需要内核层告诉引擎页的来源，也天然处理了同一地址映射不同代码的情况。
- 还没做的优化：
  - `ret` 走宿主的返回地址预测、guest SP 常驻宿主寄存器（栈访问前的那条 `ldr` 约占 8% 的采样）。→ 已补上（M9 之前：栈指针常驻寄存器和返回预测）
  - 退出桩压缩（每条访存的回填桩加退出桩约 10 个字；M7 中途测得生成的代码是 guest 代码的 6 倍多）、多线程 JIT 下每条 `ic ivau` 都要打断其他线程。
- 成对的 LDXP/STXP、操作数是被占用寄存器的 CASP、`mrs ctr_el0`、`mrs ID_*_EL1`、写 FPCR/FPSR、`dc cvau`/`ic ivau` 仍交给 Tier 0 执行。
- 映射之后文件被截短，宿主仍会收到 SIGBUS 并退出（§7.4）。→ 已补上（M9 之前：稳定性和内存）
- 真机测量仍未做（R1）。

## M8 结果（2026-10-25）

**结论**：Go 和 Node.js 在 appine 里行为正确。Go 1.26 标准库 100 多个包的测试（`-short`）全部通过，包括 os、os/exec、syscall、net、runtime（含 cgo 和 CPU profiling）、crypto（含 tls、x509）和 go/types；Go 写的命令行工具 fzf、yq、age 正常。Node.js 24 的 JIT、WebAssembly 越界访问的 trap、worker_threads、子进程和回环上的 HTTP 都和原生一致，`node -e` 正常，`npm install express`（65 个包）连同起服务、发请求共 13 秒。新加的 Go 和 Node.js 差分测试在两个执行层下都和原生一致，LTP 的 syscall 测试 856 个通过。达到退出标准。

**测量条件**：与 M7 相同（Apple Silicon 上的 colima 虚拟机，4 核，release 构建，Tier 1）。原生是差分测试的 docker 镜像，和 appine 里的文件完全相同（Alpine 3.24 的 go 1.26.9、nodejs 24.22.1）。C 档只要求行为正确，这些数字用来指出下一步的方向：

| 负载 | 原生 | appine Tier 1 | 倍数 |
|---|---|---|---|
| `node -e 0` | 0.02s | 0.23s | 12× |
| node 里 3 亿次循环（JIT 生成的代码） | 0.70s | 1.32s | 1.9× |
| `go build` hello，冷缓存 | 2.2s | 19.2s | 8.7× |
| 同上，`-p 1`、`GOMAXPROCS=1` | 5.7s | 32.2s | 5.6× |
| `go build` hello，热缓存 | 0.07s | 0.57s | 8× |

V8 生成的代码在 Tier 1 下和普通程序一样快。`go build` 慢有两个原因：一是多线程扩展性差，原生用 4 个线程比单线程快 2.7 倍，appine 只快 1.65 倍（BKL，以及 futex 唤醒时叫醒所有等待者）；二是每个包起一个新的 compile 进程，一次冷编译翻译了 980 万个块（54% 直接拷贝模板），代码缓存清空了 221 次。

- **指令**：Go 的运行时读 `CNTFRQ_EL0`、npm 读 `CNTVCT_EL0`，两个 Tier 原来都当未定义指令。现在都读宿主的值（§4.4）。
- **信号**：
  - Go 的异步抢占（SIGURG 打断不调用函数的死循环，GC 才能 stop the world）和 nil 解引用转 panic（SIGSEGV 处理函数改 pc 跳进 `sigpanic`）没有改动就能工作，靠的是 M3a 的信号帧和 M7 的精确异常。
  - A1 证实：Go 在 arm64 上不设置 `SA_RESTORER`，信号处理函数经由 vDSO 的跳板返回。
  - CPU 时间定时器（`ITIMER_PROF`、`ITIMER_VIRTUAL`、按进程 CPU 时钟的 POSIX 定时器）到期时，信号原来交给第一个没屏蔽它的线程，通常是在睡眠的主线程。Linux 是在消耗 CPU 的线程的时钟中断里发现定时器到期的，所以由它处理。现在优先叫醒正在运行的线程，Go 的 profiler 才能采到 cgo 线程里的样本（`TestCgoPprofThread`）。
- **epoll**（§10）：Go 的 netpoller 全部用 `EPOLLET`，找到三个问题：
  1. 读到 EAGAIN 和新数据到达之间没有调用 epoll_wait 时，按状态变化判断的边沿会丢，os/exec 的管道测试会卡住。改为程序在一个方向上用完数据之后，这个方向上次报告的位作废。
  2. 边沿触发的关注上次报告过、仍然就绪的位（最常见的是一直可写的 socket），等待时宿主 poll 会立刻返回。epoll_wait 于是不停地释放、重新获取 BKL；`std::sync::Mutex` 不保证公平，别的线程几乎拿不到锁。net 的 `TestSplice` 6 次里卡住 2 次、慢到 270 秒 1 次，修好之后每次 2 秒。
  3. `epoll_ctl` 没有叫醒正在 epoll_wait 的线程，新加的 fd 要等别的事情把它叫醒之后才会被关注。
- **内存**：
  - V8 为每块 wasm 内存预留 8 GB 的 `PROT_NONE`。宿主映射原来按可写的私有内存计入提交量，在 3.6 GB 内存的虚拟机上直接失败（`Out of memory: Cannot allocate Wasm memory`），现在按 Linux 的规则决定是否计入（§5.2）。
  - fork 原来要逐页检查这 8 GB，node 每起一个子进程就多花一秒。从来没有可写过的匿名映射现在整段跳过（§5.3），node 里 fork + exec 从 1076 毫秒降到 50 毫秒。
  - wasm 越界访问的 trap 不需要额外的工作：V8 的 SIGSEGV 处理函数看到出错的 pc 在 wasm 代码里，就改 pc 跳到抛异常的桩，和 Go 的 nil 解引用是同一条路径。
- **文件系统**（Go 的 os 测试，`TestRoot*`、`TestChtimesOmit`）：
  - 路径解析按 Linux 的规则处理 `.` 和末尾的 `/`：`file/.`、`file/..`、`file/` 和指向 `file/` 的符号链接都是 ENOTDIR；新建的名字带末尾的 `/` 时（mknodat、symlinkat、linkat），名字已存在返回 EEXIST，否则 ENOENT；unlinkat 按目标返回 ENOENT、EISDIR 或 ENOTDIR；renameat2 的任何一边带末尾的 `/` 而源不是目录时返回 ENOTDIR。
  - `open(O_CREAT)` 遇到悬空的符号链接时在链接指向的位置创建文件。
  - utimensat 分别设置 atime 和 mtime，stat 报告真正的 atime（普通文件取宿主对象的 atime）。
- **其他 syscall**：
  - `membarrier`：V8 改写代码之后用 `PRIVATE_EXPEDITED_SYNC_CORE` 让其他线程同步指令流；实现是打断共用地址空间的其他线程，让它们离开 guest 代码。
  - `futex_waitv`（Linux 5.16，按 §7.5 的 ABI 契约应当有）：同时登记在多个键上，任何一个被唤醒就返回它的下标。
  - `RLIMIT_NOFILE` 的默认值改为与 Linux 相同的 1024/4096，`nr_open` 是 2^20（Go 的 `TestOpenFileLimit` 要把软限制提到硬限制）。
  - recvmsg 的 `msg_flags` 带上 `MSG_CMSG_CLOEXEC`（`TestSCMCredentials`）。
  - AF_UNIX 的 bind 拒绝最后一段是 `.` 或 `..` 的路径。原来 bind `"//"` 会在根目录里建出一个名为 `.` 的 socket。
  - clone 忽略 `CLONE_PIDFD`，与 5.2 之前的内核一样，Go 据此退回到不用 pidfd 的路径（`TestPidFD`）。M9 之前实现了 pidfd，Go 改走 pidfd 的路径。
  - ptrace 一律返回 EPERM，与 Yama 的 `ptrace_scope=3` 相同，程序会当作没有权限（`TestExecPtrace`）；原来是 ENOSYS。
- **调试**：`APPINE_STRACE=1` 的每一行带上从启动算起的时间和 syscall 的耗时，查 epoll 卡住时用到。
- **测试**：
  - Go 标准库的测试在 appine 里用 `go test -short` 运行。Alpine 的 go 包删掉了 `_test.go` 和 testdata，从同版本的源码包补回去。通过的包有 os、os/exec、os/signal、syscall、net、internal/poll、runtime、runtime/pprof、runtime/debug、reflect、sync、time、context、io/fs、path/filepath、math/big、compress/*、archive/*、crypto/*、encoding/*、hash/*、text/template、html/template、go/parser、go/types、strings、strconv、regexp 等。
  - 差分测试新增 `go` 和 `node` 两组（`tests/diff`），差分测试的仓库因此要重新生成：
    - Go：goroutine 和 select，不调用函数的死循环里的 GC（只能靠 SIGURG 抢占），`os/signal`，os/exec，nil 解引用转 panic。
    - Node.js：多态调用让 V8 反复优化、反优化，wasm 越界访问抛 RuntimeError，crypto、zlib、worker_threads、子进程，回环上的 HTTP。
  - 单元测试新增 `EPOLLET` 按方向作废、fork 跳过没写过的匿名映射、路径解析的 ENOTDIR。
  - LTP 的 syscall 测试 856 个通过（M7 是 849）。新通过 futex_waitv01–03、membarrier01、pipe15、ptrace02，以及 M7 时因为虚拟机计时延迟失败的 clock_nanosleep02、epoll_pwait03；新失败 nice05，它比较两个 nice 值不同的线程用掉的 CPU 时间，appine 不按 nice 分配 CPU，两者几乎相等，差值的正负是随机的。计时精度类的测试（这两个和 futex_wait05）在这台虚拟机上时好时坏，同一时刻用 M7 的二进制也一样失败。
  - lockstep 100 万轮全部一致；单元测试、rootfs 用例，以及 Android 目标的 clippy 都通过。

**改动和推迟项**：

- JVM 在 §1.2 的 C 档里，M8 没有测，随后补测，见下一节。
- 多线程负载的扩展性（见上面的 `go build`）：BKL 之外，futex 唤醒、信号这些修改都叫醒所有等待中的线程，线程多时代价明显。→ futex 已改为定向唤醒（M9 之前：futex 定向唤醒），拆分 BKL 放到发布之后
- 短命进程多的构建（`go build` 每个包一个 compile 进程）翻译开销大，可以考虑把翻译结果在进程之间、或者跨运行保存下来。→ 翻译已经跨 exec 保留（M9 之前：起进程的翻译开销），跨运行保存没有做
- `PROT_NONE` 的预留区仍然逐页写页表：V8 的每块 wasm 内存占 16 MB 页表。可以改成到 mprotect 成可访问时再填。→ 已补上（M9 之前：稳定性和内存）
- `MADV_DONTNEED` 用清零实现，对大块从没碰过的内存反而会占用内存，应当改用宿主的 madvise（宿主 16K 页时边缘部分仍要清零）。→ 已补上（同上）
- `EPOLLET` 的近似：程序没有用完数据（没读到 EAGAIN、读满了缓冲区）时，新到的数据不会再触发。
- 不按 nice 值分配 CPU（nice05）。→ 已补上：nice 交给宿主线程（M9 之前：推迟项）

## M8 补测：JVM（2026-10-25）

**结论**：OpenJDK 21 和 25 在 appine 里行为正确。覆盖线程、虚拟线程、JIT、GC、异常、子进程、网络、文件和信号的测试程序，在两个执行层下、各种 GC 和 JIT 配置下输出都和原生一致；jshell、jcmd、jstack 和 `kill -3` 的线程转储正常；Maven 和 Gradle 能从 Maven Central 下载依赖，编译项目、跑 JUnit 5 测试并打包。补测发现两个问题并已修好：经由符号链接启动的程序找不到自己的库（`/proc/self/exe` 没有解析符号链接），以及没有 inotify（WatchService 用不了）。

**测量条件**：与 M8 相同，原生是同一个仓库导出的 docker 镜像（Alpine 3.24 的 openjdk21 21.0.12、maven 3.9.16、gradle 8.14.4）：

| 负载 | 原生 | appine Tier 1 | 倍数 |
|---|---|---|---|
| 测试程序（`-XX:+UseSerialGC`） | 0.84s | 3.93s | 4.7× |
| 同上，ZGC | 0.97s | 4.91s | 5.1× |
| 同上，只用 C1 | 0.87s | 3.57s | 4.1× |
| 同上，纯解释（`-Xint`） | 7.58s | 28.4s | 3.7× |
| Maven `package`，依赖已在本地 | 1.68s | 18.5s | 11× |
| Gradle `build`，守护进程已在运行 | 0.52s | 7.5s | 14× |
| Gradle `build`，冷启动并下载依赖 | 8.9s | 64.7s | 7.3× |

Maven、Gradle 慢在 JIT 生成的代码上：Gradle 那次运行里，间接跳转（包括 `ret`）没命中跳转缓存、退回分发器 1.09 亿次；HotSpot 不停地给编译好的代码打补丁（内联缓存、调用点），作废了 82 万个块。

- **`/proc/<pid>/exe`**：原来是 execve 拿到的路径做词法规范化，不解析符号链接。musl 按 `/proc/self/exe` 展开主程序的 `$ORIGIN`，经由 `/usr/bin/java`（指向 `../lib/jvm/default-jvm/bin/java`）启动时找不到 `libjli.so`。现在与 Linux 的 `d_path` 一样，给出解析完符号链接的路径（`Vfs::real_path`），maps 里的名字相同。Go 的 `os.Executable`、Node.js 的 `process.execPath` 同样受益。
- **代码失效日志**（§4.2）：原来只保留 64 个代数。HotSpot 每装一个编译好的方法就 `ic ivau` 一串缓存行，睡一会儿的 GC、编译线程醒来几乎都落在日志之外，只能整体清空翻译缓存。一次 Maven 构建清空了 2449 次，其中 2388 次是这个原因。改成保留 4096 个代数之后，清空降到 248 次，翻译的块从 1466 万降到 128 万，用户态 CPU 时间少了 22%。墙钟时间没有变，因为这些重新翻译都在后台线程上，虚拟机的 4 个核本来就有空闲；核少的手机上应当能看出来。
- **inotify**（§8.3）：`inotify_init1`、`inotify_add_watch`、`inotify_rm_watch` 和对它的 read、poll、FIONREAD。事件的种类、顺序、`IN_ISDIR`（`*_SELF` 事件不带）、rename 的 cookie、`IN_ONESHOT`、`IN_EXCL_UNLINK`、`IN_MASK_ADD`、`IN_MASK_CREATE`、队列溢出、合并相同事件都与 Linux 相同。`IN_CLOSE_*` 在打开文件的最后一个引用释放时报告。新增 `/proc/sys/fs/inotify/*` 和 `/proc/<pid>/fdinfo/<fd>`（位置、标志、inode，eventfd 的计数，inotify 的监视）。Java 的 WatchService、Node.js 的 `fs.watch` 因此可用。
- **不用改就正常的**：
  - 隐式空指针检查、栈溢出保护页（第二次溢出还能抓到，说明黄区重新打开了）、safepoint，都靠 SIGSEGV。
  - Serial、Parallel、G1、ZGC（含分代 ZGC 和多重映射的堆）、Shenandoah；JDK 25 的紧凑对象头。
  - 虚拟线程，CDS 归档的映射。
  - jcmd/jstack 的 attach：SIGQUIT 加 AF_UNIX；jshell 经由 socket 用 JDI 控制另一个 JVM。
  - `sun.misc.Signal`、SIGTERM 触发关闭钩子，posix_spawn 加 jspawnhelper，NIO 的 epoll Selector，HttpClient 和 HttpServer，内存映射和文件锁，crypto 指令内建。
  - `-XshowSettings:system` 显示"No metrics available"，因为没有 cgroup（D 档）；JVM 退回到宿主的 CPU 数和内存，算出的参数与原生相同。
- **测试**：
  - 差分测试新增两组：`java`（上面的测试程序，加上 ZGC 运行一遍、jcmd attach）和 `inotify`（一个 C 程序逐步打印目录和文件上的事件序列、错误码、阻塞读，以及 WatchService 和 `fs.watch`）。差分测试的仓库多装 openjdk21-jdk。全部 10 组在两个执行层下都和原生一致。
  - 单元测试新增 `real_path` 和 inotify 的投递、合并、一次性监视、溢出；rootfs 用例里 `readlink /proc/self/exe` 的期望值改为 `/bin/busybox`。
  - LTP 的 syscall 测试 867 个通过（M8 是 856），新通过 inotify01、02、04、05、06、09–12 和 inotify_init1_01、02；inotify03、07、08 要挂载块设备。shmctl01 在全量并发运行时偶发失败（以前的全量运行里也出现过），单独重跑 5 次都通过。

**改动和推迟项**：

- JIT 负载的性能：跳转缓存是 4096 项直接映射，JIT 代码里间接跳转和 `ret` 的目标多，冲突严重；给编译好的代码打补丁按页作废翻译。见 R1 的退路。
- inotify 的近似：监视建立之前就打开的文件关闭时不报告；打开之后改名的文件，按 fd 的事件仍然报给原来的目录；`IN_DELETE_SELF` 在最后一个链接删除时报告，不等文件关闭；`max_user_instances`、`max_user_watches` 可写但不起作用。→ 都已去掉（M9 之前：推迟项）
- fdinfo 只有通用的几行和 eventfd、inotify 的内容，没有 epoll、timerfd、signalfd 的；挂载编号是合成的，没有 `/proc/<pid>/mountinfo`。→ 已补上（M9 之前：兼容性补漏）
- 没有 fanotify。

## M9 之前：稳定性和内存（2026-10-25）

开始 M9 之前，先补上推迟项里手机上最容易碰到的几处：一个 guest 程序就能让整个 App 崩溃的情况，以及让内存白白多占的两处。

- **截短已映射的文件**（M7 推迟，§5.2、§7.4）：一个进程映射着文件，另一个进程截短它，再访问超出新末尾的页，宿主进程收到 SIGBUS，所有会话一起退出。差分测试在上一版上复现了这一点（"Bus error"）。现在 truncate、ftruncate、`O_TRUNC` 之后，遍历所有地址空间，把映射这个文件、超出新末尾的页从页表里拿掉（`Mm::file_truncated`），别的进程的地址空间就地做 shootdown。guest 访问这些页得到自己的 SIGBUS，与 Linux 相同。VMA 因此给私有文件映射也记下宿主文件的标识，原来只有共享映射记了（futex 的键）。
- **`PROT_NONE` 不进页表**（M8 推迟，§5.2）：mprotect 加上权限时再按 VMA 放进去，去掉全部权限时拿掉；内核写 guest 内存（加载器）改为按 VMA 找宿主地址。C 里的解除映射和修改权限遇到没有中间表的地方，整张叶子表（2 MB）跳过，原来对 8 GB 的预留要逐页查两百万次。
- **`MADV_DONTNEED` 交给宿主**（M8 推迟，§5.2）：原来是清零，反而把要归还的页都碰了一遍。
- 两项合起来，创建 10 块 wasm 内存的 Node.js 进程，宿主峰值 RSS 从 536 MB 降到 118 MB；`java -version` 从 116 MB 降到 106 MB。
- **`restart_syscall`**（M3a 推迟）：nanosleep 和相对时间的 clock_nanosleep 被信号打断时记下到期时刻，返回 `ERESTART_RESTARTBLOCK`。有处理函数时是 EINTR，没有时（比如停止之后又继续）重新执行的 svc 改成 restart_syscall，接着睡剩下的时间。原来按原来的时长重新睡满：`sleep 10` 中途 Ctrl-Z、`fg`，要再睡 10 秒。
- **robust futex**（M6 推迟）：线程退出和 execve 时按 Linux 的 `exit_robust_list` 处理 `set_robust_list` 登记的链表，线程还持有的锁标上 `FUTEX_OWNER_DIED` 并唤醒一个等待者，等待者拿到 EOWNERDEAD。musl 的 `pthread_exit` 自己处理链表，所以这条路径管的是进程直接退出或被杀的情况；原来进程间共享的 robust 互斥锁的等待者会永远卡住。
- **`EPOLLEXCLUSIVE`**（M6 推迟）：参数检查与 Linux 相同（只能在加入时用，不能用于 epoll，不能配别的标志，加了它的关注不能修改）；唤醒仍然是全部唤醒。
- **测试**：差分测试新增 `edge-cases`（别的进程用三种方式截短映射着的文件、`MADV_DONTNEED`、8 GB 的 `PROT_NONE` 预留、停止又继续的 nanosleep、被处理函数打断的 nanosleep、进程间 robust 锁、`EPOLLEXCLUSIVE`），全部 11 组在两个执行层下都和原生一致。单元测试新增截短、`PROT_NONE` 不进页表、`MADV_DONTNEED`。LTP 的 syscall 测试 869 个通过（上次 867），没有新失败；多出来的 futex_wake05、nice05 以前就时好时坏。lockstep 和 clang 编译的引擎测试通过。

## M9 之前：性能剖析（2026-10-25）

用 6 个代表负载剖析了一轮（perf；Tier 1 写 `/tmp/perf-<pid>.map`，见 README），原生一侧是同一个仓库导出的 docker 镜像。负载分成两类：

- **文件系统和进程密集的**（shell 起外部命令、`git status`）：时间花在内核层。路径解析占 `git status` 的 47%、shell 的 28%，其中一半以上不是查询本身，而是 SQLite 每条语句单独开、关读事务时的 fcntl 加解锁（`unixShmLock`，git 的 24.5%）。shell 还有 14% 花在 fork 逐页扫描栈的 8 MB。
- **计算密集的**（gcc、python、apk）：80–96% 在翻译后的代码里。

这次做了第一类：

- **元数据缓存**（§8.1，M1b 起推迟）：目录项（包括不存在的名字）、inode 行、符号链接目标、目录列表放在内存里。自己的写入只去掉受影响的条目；仓库 `lock` 文件开头 8 字节是所有进程共享映射的提交计数器，提交之后加一，缓存发现它不是因为自己的提交而变了，就整体作废，所以多个进程共用一个仓库时也不会读到旧数据。
- 缓存没命中时，一次路径解析（包括 stat 取属性）里的查询共用一个读事务，读事务在第一次没命中时才开始。
- 宿主 object 文件的 stat 改为相对 `objects` 目录的 fd，不再每次在宿主上逐级解析整个路径。
- **fork 只看碰过的页**（§5.3）：写过的匿名映射按 `/proc/self/pagemap` 只检查宿主碰过的页。
- 测量方法：每个负载在同一个 appine 会话里连跑两次，取第二次（原生一侧同样）。元数据缓存和翻译模板都在 appine 进程的内存里，每次新起 appine-run 测到的主要是冷启动；手机上 App 是长期运行的进程，第二次更接近实际：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| shell 起 600 个进程 | 0.15s | 2.24s（15×） | 1.37s（9×） |
| `git status` 两次，5733 个文件 | 0.03s | 0.58s（19×） | 0.19s（6×） |
| 同上，第一次（含刷新索引） | 0.27s | 0.72s | 0.46s |
| gcc 编译 `lvm.c` | 0.69s | 3.19s | 3.18s（4.6×） |
| python 启动 5 次加循环 | 0.35s | 2.45s | 2.39s（6.8×） |

- 剖析中途的一个教训：对照镜像起初没有打包 `/var`，原生一侧的 apk 没有仓库索引要读，看上去 appine 慢了 180 倍，实际是 2.4 倍。git 也一样：每个新容器里 git 都要重新读一遍所有文件来刷新索引，热缓存下原生只要 0.02 秒。对照必须用完全相同的文件和状态。
- 测试：单元测试新增两个连接（相当于两个进程）交替写入时缓存的一致性；LTP 的全量运行会有很多 appine 进程同时用同一个仓库，868 个通过（上次 869；nice05 是随机的，getrusage03 因为当时虚拟机里可用内存不足 512 MB 跳过）。全部 11 组差分测试在两个执行层下通过；inotify 那组的阻塞读改用 mkdir，避免结果取决于父进程醒来的时机。

**留到后面的**：

- 起进程的剩余开销：每个新进程都要把常用代码从翻译模板里重新复制、重新链接一遍（shell 每个进程约 1150 个块，`appine_dbt_run` 自身占 31%），要根本解决得让翻译结果在进程之间直接共用；其次是宿主缺页（15%）。→ 翻译改为跨 exec 保留（M9 之前：起进程的翻译开销）
- 多线程争用 BKL：git 的并行 lstat 线程上，唤醒和锁争用约占 30%（R1 的退路：拆分 BKL、定向唤醒）。→ futex 已改为定向唤醒（M9 之前：futex 定向唤醒），BKL 没有拆
- 翻译后代码的质量：python 6.8×、gcc 4.6×；gcc 有 370 万次间接跳转没命中跳转缓存（返回地址预测）。→ 返回预测已补上（M9 之前：栈指针常驻寄存器和返回预测）
- `/proc/<pid>/maps` 里库的文件映射没有名字；brk 每扩一页就是一个新的 VMA，相邻的 VMA 不合并（apk 运行中有 800 个 VMA）。→ 已补上（M9 之前：兼容性补漏）

## M9 之前：起进程的翻译开销（2026-10-25）

上一轮剖析之后，shell 起进程最大的开销是每个新进程都把常用代码从翻译模板重新拷贝、重新链接一遍：起 600 个进程翻译了 69 万个块（99.5% 来自模板），`appine_dbt_run` 自身占 31%。

- 翻译不再随 exec 和线程退出清空（§4.2"翻译保留"），换地址空间之后第一次运行前逐块核对 guest 指令字。同一个负载翻译的块从 69 万降到 9419；对得上的块连同块之间的链接一起沿用。
- 热缓存下 shell 起 600 个进程从 1.37 秒降到 0.95 秒（原生 0.15 秒，9× → 6.3×）；其余负载不变（`git status` 0.19 秒，gcc 3.18 秒，python 2.36 秒）。
- 测试：引擎新增 `tests/dbt.rs`，检查 exec 了不同程序之后核对、池里的实例交给下一个线程之前核对、只改了一页时只作废那一页的块（另一页的块和到它的链接保留）。lockstep、全部 11 组差分测试在两个执行层下通过，LTP 868 个通过（与上一轮相同）。

## M9 之前：栈指针常驻寄存器和返回预测（2026-10-25）

计算密集的负载几乎全在翻译后的代码里。一个微基准把问题分开了：纯循环和原生一样快，调用密集的部分慢 3.6 倍。一条 `stp x29, x30, [sp, #-32]!` 原来要生成约 14 条宿主指令：从上下文取 SP、查对齐、算地址、TLB 检查、访存，回写时再取一次 SP、加偏移、存回去；函数返回还要走 7 条指令的跳转缓存查找，gcc 编译一个文件就有 370 万次没命中。

- **SP 常驻宿主 X26**（§4.2，M7 推迟）：guest X26 改为被占用的寄存器。以 SP 为基址的访存直接用 X26，回写只要一条 `add`；`add sp, …` 这类指令只把寄存器编号 31 换成 26。调用密集的部分从 0.358 秒降到 0.219 秒（原生 0.100 秒），gcc、python、apk 各快 4–6%。
- **返回预测**（§4.2）：见设计文档 §4.2 的"控制流"。gcc 的 lookup 退出从 369 万次降到 7.9 万次，耗时 3.02 → 2.72 秒（快 10%），python 2.23 → 2.16 秒。规整的微基准反而从 0.219 秒变成 0.253 秒：宿主的间接跳转预测器本来就猜得中那种模式，多出来的十几条压栈、弹栈指令成了纯开销；真实代码的返回目标分散，收益明显，所以保留。
- 两项加上这一轮的其他改动，Maven 热构建 18.5 → 17.3 秒，Gradle 热构建 7.5 → 6.2–6.5 秒。
- 测试：`tests/dbt.rs` 新增普通的调用返回、被调函数改了 x30 时按 x30 返回、x30 恰好等于空项标记时照样得到 pc 对齐错误、递归 200 层（超过影子栈的 64 项）。lockstep、全部 11 组差分测试在两个执行层下通过，LTP 869 个通过。差分测试 `java` 里等子进程输出的 `grep` 改用 `-s`：CI 上子进程还没创建输出文件时，grep 会多打印一行错误。
- 没做：SP 对齐检查（每次以 SP 为基址的访存两条指令）仍然保留；跳转缓存保持 4096 项（加到 64K 项能让 gcc 的 lookup 退出再少一个数量级，但返回预测之后剩下的已经不多）。

## M9 之前：futex 定向唤醒（2026-10-25）

原来任何修改都会在释放 BKL 时叫醒所有正在等待的线程，futex 唤醒也一样。现在 futex 的等待项记下等待线程的事件，FUTEX_WAKE 只通知被唤醒的线程，futex 等待者也不再被别处的修改叫醒（§6）。其余的等待（管道、socket、epoll、wait4 等）仍是全部叫醒。

- 两个线程用互斥锁和条件变量来回传递 10 万次：3.74 → 2.97 秒（原生 1.8–2.1 秒），20 万次等待里"全部叫醒"只剩 12 次。
- JVM 受益最大：Maven 热构建 17.5 → 15.7 秒，Gradle 热构建 6.1 → 4.8 秒；`go build` 冷编译 16.7 → 16.3 秒（M8 时是 19.2 秒）；`git status` 不变，它的线程争用在 BKL 本身。
- Gradle 那次运行里剩下的"全部叫醒"约 3.2 万次（等待 7.5 万次），每次白醒十几微秒，给每种对象建自己的等待队列的收益已经不大。拆分 BKL 仍放到发布之后（R1）。
- 测试：futex 的单元测试检查唤醒只通知被唤醒的线程。全部 11 组差分测试在两个执行层下通过（Go、Java、Node.js 都用大量 futex），LTP 868 个通过（与上一轮相比只有随机的 nice05 不同）。

## M9 之前：兼容性补漏（2026-10-25）

把推迟项里程序会直接碰到的接口缺口补上一轮：

- **`/proc/<pid>/maps`**：所有文件映射（程序、解释器、mmap 的库和文件）都显示路径、文件偏移、设备号和 inode，与 stat 一致。原来只有程序和解释器有名字，vDSO 那一行还被错标成解释器。
- **brk 和 VMA**：堆第一次扩展时预留 256 MB 的宿主地址空间（`MAP_NORESERVE`，不碰不占内存），之后的扩展只是加长同一个 VMA；收缩后再扩展的页清零，fork 出来的子进程照样预留。maps 里相邻、权限相同的匿名映射（以及同一个文件里接着的部分）与 Linux 一样合成一段：每次匿名 mmap 都是单独的宿主内存，VMA 本身合不起来。python 跑一会儿，maps 从 276 行变成 40 行左右。
- **实时信号排队**：挂起的信号从位图换成 Linux 的 `sigpending` 结构，标准信号同一种只挂一个，实时信号每次产生都排队，按先后投递，各带各的 siginfo。一个线程组最多排 `RLIMIT_SIGPENDING` 个（默认 16384，与 4 GB 内存的 Linux 相同）；排满时 sigqueue 返回 EAGAIN，kill 照样挂起但丢掉 siginfo。`/proc/<pid>/status` 的 SigQ 是真实的数字。
- **pidfd**：`pidfd_open`、`pidfd_send_signal`（也接受打开的 `/proc/<pid>` 目录）、`pidfd_getfd`（按 ptrace 的规则检查权限）、clone 的 `CLONE_PIDFD`、`waitid(P_PIDFD)`（非阻塞的 pidfd 没有事件时是 EAGAIN）。pidfd 拿着线程组的弱引用，pid 分给别的进程之后仍然指向原来那个；所有线程退出时 poll 可读，fdinfo 有 `Pid:`。另外实现了 `clone3`，包括 `CLONE_CLEAR_SIGHAND`；指定 pid 的 `set_tid` 和 `CLONE_INTO_CGROUP` 返回 EINVAL。Go 探测到这些之后，os/exec 改用 pidfd 等子进程、发信号（strace 里能看到 `waitid(P_PIDFD)` 和带 `CLONE_PIDFD` 的 vfork），Go 的差分测试照样通过。
- **`execveat`**：路径相对 dirfd，`AT_EMPTY_PATH` 执行 fd 本身（fexecve），`AT_SYMLINK_NOFOLLOW` 遇到符号链接是 ELOOP。新程序看到的 `AT_EXECFN` 是 `/dev/fd/<n>[/<路径>]`，comm 取文件名；用 close-on-exec 的 fd 执行脚本是 ENOENT（解释器打开它时 fd 已经关了），与 Linux 相同。
- **`/proc/net/{tcp,tcp6,udp,udp6,unix}`**：遍历所有进程的 fd 表收集 socket。TCP、UDP 的地址、协议和 TCP 状态向宿主 socket 查，不读宿主的 `/proc/net`（Android 10 起 App 读不了）；格式照抄 Linux，包括 IPv4 的两个文件每行补空格到固定宽度。只在 SCM_RIGHTS 消息里、或者还在 accept 队列里的 socket 不列出。
- **`/proc/<pid>/mountinfo`** 和 epoll、timerfd、signalfd 的 fdinfo：挂载编号与 fdinfo 的 `mnt_id` 一致，设备号与 stat 的 `st_dev` 一致。
- **零碎的**：
  - `/proc/sys/fs/pipe-max-size` 可写并且生效，没有 `CAP_SYS_RESOURCE` 时新管道的容量不超过它（LTP fcntl35）。
  - 老式的 `TCGETA`/`TCSETA*`（ioctl01）。
  - `/dev/urandom` 的 `RNDGETENTCNT` 和 `entropy_avail`（ioctl07）。
  - 带偏移的读写遇到不能定位的文件先报 ESPIPE，再看打开方式（pwritev202）。
  - `MADV_REMOVE`：有锁住的页或私有匿名映射是 EINVAL，不能获得写权限的是 EACCES，共享映射清零（madvise02）。
  - `MREMAP_DONTUNMAP`：与 5.7–5.12 一样只支持私有匿名映射，原地址留下空的映射。
  - 读写 epoll 和 pidfd 是 EINVAL。
  - kcmp（LTP 的 pidfd_getfd01 用它核对复制来的 fd）：文件、地址空间、fd 表、fs、信号处理表比较对象本身，`KCMP_EPOLL_TFD` 到 epoll 里找目标文件；appine 没有 IO 上下文，SysV 信号量的 undo 表也不在进程之间共享，这两种总是相同。
- LTP 的 sigtimedwait01、sigwaitinfo01 超时不是 appine 的问题：第一项测试等的是"空集合被信号打断返回 EINTR"，而 musl 的 `sigtimedwait` 遇到 EINTR 会自己重试，在原生 Alpine 上一样卡住；直接发 syscall 的 rt_sigtimedwait01 是通过的。
- 测试：差分测试新增 `compat`（以上各项，只比较与环境无关的列），共 12 组，在两个执行层下都和原生一致。原生一侧的容器改为不加 seccomp 过滤：Docker 默认的规则让 clone3 返回 ENOSYS、pidfd_getfd 返回 EPERM，比对的应当是内核本身的行为。内核版本相关的部分只测各版本相同的：6.9 起进程回收之后 pidfd 有 POLLHUP，`CLONE_PIDFD` 可以和 `CLONE_THREAD` 一起用，这里与 6.1 相同。单元测试新增堆的扩展、maps 的合并和信号的排队。LTP 的 syscall 测试 889 个通过（上次 868），新通过的是 execveat01/02、pidfd 的 8 个、kcmp01–03、fcntl35（两个）、ioctl01、ioctl07、madvise02、pwritev202（两个）和 splice06，没有新失败。getrusage03 由跳过变成失败：上次虚拟机可用内存不足 512 MB 没有运行，这次运行了，暴露出 `ru_maxrss` 总是 0。

**推迟项**：

- pidfd：`PIDFD_THREAD`、`/proc/sys/kernel/ns_last_pid`（pidfd_send_signal03 要用它让新进程拿到同一个 pid）。→ `ns_last_pid` 已补上（M9 之前：推迟项）；`PIDFD_THREAD` 是 6.9 的功能，appine 报告的是 6.1，不做
- 不产生 core dump，`WCOREDUMP` 总是 0（waitid10）。
- getrusage 的 `ru_maxrss` 总是 0（getrusage03）。→ 已补上（M9 之前：剩下的小缺口）
- `MREMAP_DONTUNMAP` 不支持文件映射；没有 `/proc/net/{icmp,raw}`。→ 都已补上（M9 之前：推迟项）

## M9 之前：剩下的小缺口（2026-10-25）

把推迟项和全量 LTP 的失败重新过了一遍：疑似 musl 的那些先在原生 Alpine 容器里跑，16 个在原生上同样失败（gethostname02、pathconf02、pwrite04、sbrk01、recvmmsg01、sendmsg01、readlink03、open13、clone08 等，sendto01 的第 4 项也是），不归 appine。剩下原生通过、appine 不通过的补上：

- **/dev/shm 和 /run 开机清空**：Linux 上它们是 tmpfs。仓库被第一个使用者打开时（拿得到排他锁，也就是没有别的 appine 进程在用，相当于开机）清空这两个目录的内容，目录本身保留。原来共享内存、命名信号量、pid 文件会一直留在仓库里，LTP 的 cachestat02 就撞上了前一天的运行留下的文件。
- **`/proc/<pid>/smaps`、`smaps_rollup`、`pagemap`**：驻留页按宿主内存算（mincore），匿名内存都算私有的脏页，文件映射都算私有的干净页；pagemap 每页 8 字节，在读的时候按位置现算（文件有 2^36 项），给驻留、文件页和独占位，与没有特权的进程在 Linux 上看到的一样不给页框号。maps 里没有名字的行末尾补上空格（Linux 就是这样），合并相邻映射时也比较是否锁住。
- **把页调进内存**：mlock（不带 `MLOCK_ONFAULT`）、`mlockall(MCL_CURRENT)`、`MAP_POPULATE`、`MAP_LOCKED`、`mlockall(MCL_FUTURE)` 之后的映射，每个宿主页读一个字节；原来只是记一下，mincore 和 smaps 都看不出来。
- **`ru_maxrss` 和内核态时间**：驻留内存的峰值在 getrusage、execve 和退出时取样（跨 execve 保留，子进程回收时并入父进程的 `RUSAGE_CHILDREN`），`/proc/<pid>/status` 的 VmHWM 也用它。只在这几个点取样，进程中途用了很多又释放掉的峰值会漏掉。内核态时间用宿主内核记的宿主线程的 stime（`/proc/self/task/<tid>/stat`），也就是宿主内核替它做的事（转发的 I/O、缺页等）；内核层自己在宿主用户态做的事算不进去，所以偏少，但 `time` 和 `top` 不再总是 0。最初试过"持有 BKL 的时间算内核态"，每次加锁、解锁多读两次时钟，最快的 syscall 慢了六成，放弃了。另一个教训：wait 每次被叫醒都要重扫子进程，算子进程用量这种开销大的事要等找到事件之后再做，不然 git status 慢了三成。
- **经由 `/proc/self/fd` 重新打开已删除的文件**（memfd、删除之后还开着的）：仓库里的 object 已经删了，没法按 inode 打开。改为直接复制宿主 fd（读写都按位置进行，偏移由各自的打开文件记着）；要更多的权限时经由宿主的 `/proc/self/fd` 重新打开（开发环境的 virtiofs 上不行，Android 的 ext4、f2fs 上可以）。`O_NOFOLLOW` 停在这种链接上是 ELOOP。memfd 还有可写的共享映射时加 `F_SEAL_WRITE` 返回 EBUSY。
- **unshare**：`CLONE_FILES`、`CLONE_FS` 复制 fd 表和根目录、当前目录，`CLONE_SYSVSEM` 执行并丢掉信号量的 undo，`CLONE_THREAD`、`CLONE_SIGHAND`、`CLONE_VM` 只在本来就没有共用时成功，命名空间与 clone 一样返回 EPERM。`/proc/sys/fs/nr_open` 可写，`RLIMIT_NOFILE` 和 unshare 复制 fd 表都按它检查。
- **`vm.max_map_count`**：VMA 达到上限（默认 65530，可写）时 mmap 返回 ENOMEM，拆分 VMA 的 munmap、mprotect、mlock 也一样。每个 VMA 都是一个宿主映射，宿主的映射数是整个 App 进程共用的；原来一个 guest 进程就能把它用完。appine 不合并相邻的匿名 VMA，同样的程序 VMA 可能比 Linux 上多。
- **POSIX 定时器的超限次数**：间隔很短、起点在很久以前时周期数超出 32 位，原来截断之后绕回了；现在按 128 位算，报告的次数封顶在 INT_MAX。过去的绝对时间原来被截成"现在"，丢了过去了多久；`timer_getoverrun` 改为在信号送达时更新，不再被之后的每次到期覆盖。
- **零碎的**：
  - tkill、tgkill 的实时信号排不上队是 EAGAIN。
  - futex requeue 的个数是负数时 EINVAL。
  - 阻塞的 connect 返回之前，再 connect 一次宿主 socket，让它从"正在连接"变成"已连接"，之后再 connect 是 EISCONN。
  - SIOCATMARK；修改接口的 SIOCSIF* 先读参数（EFAULT），再返回 EPERM。
  - 没有 `CAP_NET_ADMIN` 时 `SO_SNDBUFFORCE`、`SO_RCVBUFFORCE` 是 EPERM，有时退回到普通的 `SO_*BUF`。
  - 对内置的 init 调 setpriority 不再是 ESRCH。
- 性能：shell 起 600 个进程慢了约 1%（退出和 execve 时的驻留内存取样），syscall 密集的小循环和 `git status` 不变。
- 测试：差分测试 `compat` 新增 smaps 和 pagemap、重新打开已删除的文件、unshare、`ru_maxrss`、tkill、connect、接口 ioctl、memfd 封印；间隔 1 纳秒的定时器在原生一侧会造成信号风暴，结果取决于时机，留给 LTP 的 timer_settime03。单元测试新增开机清空的目录。全部 12 组差分测试在两个执行层下通过。LTP 的 syscall 测试 905 个通过（上次 889），新通过 17 个：getrusage03、times03、mlock05、mlock201、mmap12、mincore03、memfd_create01、munmap04、unshare02/03、tgkill02、futex_cmp_requeue02、connect01、setpriority02、setsockopt04、sockioctl01、timer_settime03；cachestat02 由失败变成跳过（走到了还没有的 cachestat）。shmctl01 这次失败，单独重跑三次都通过，是以前就有的全量并发时的偶发失败。unshare01、unshare04 剩下的失败是命名空间。

**推迟项**：

- `MAP_GROWSDOWN` 的映射不会向下增长（mmap18）：要在映射下方预留宿主地址空间，并在缺页时延伸 VMA。→ 已补上（M9 之前：推迟项），不需要预留：长出来的部分是一个新的 VMA
- cachestat；unshare 的命名空间。→ cachestat 已补上（M9 之前：推迟项）；命名空间不做

## M9 之前：推迟项（2026-10-25）

把前几轮推迟的、在单进程模型里做得了的补上；io_uring 不做（见下面的推迟项和 §15）。

- **信号驱动的 I/O**：`O_ASYNC`（`F_SETFL`、`FIOASYNC`），属主（`F_SETOWN`、`F_SETOWN_EX`，socket 上还有 `FIOSETOWN`/`SIOCSPGRP`）和 `F_SETSIG` 原来只记录，现在生效：文件变得可读、可写、出错或挂断时给属主（线程、进程或进程组）发 SIGIO，或者 `F_SETSIG` 设置的信号，带 `si_band`、`si_fd`（signalfd 也给出这两项），band 与 Linux 相同。读过之后仍然可读的（又来了数据）再通知一次。检查放在定时器线程上：内核状态的任何修改都会叫醒它，它顺便检查登记了 `O_ASYNC` 的文件，宿主 socket 的就绪也由它一起 poll。
- **文件租约**：`F_SETLEASE`、`F_GETLEASE`、`/proc/sys/fs/lease-break-time`。规则与 Linux 相同：只有属主（或有 `CAP_LEASE`）能设置；读租约要求自己不可写、没有人以写方式打开，写租约要求没有别的打开。以写方式打开或者截短打破所有租约，以读方式打开打破写租约：持有者收到 `F_SETSIG` 的信号（`POLL_MSG`）或 SIGIO；`O_NONBLOCK` 的 open 返回 EWOULDBLOCK，否则等持有者降级或放弃，超过 lease-break-time 强制降级。被写方式的打开打破时不能降成读租约。
- **POSIX 消息队列**：`mq_open`、`mq_unlink`、`mq_timedsend`、`mq_timedreceive`、`mq_notify`、`mq_getsetattr`；按优先级排序，同一优先级先进先出；`/proc/sys/fs/mqueue/*` 可写，属性按它们和 `CAP_SYS_RESOURCE` 检查。队列的 fd 能 poll、epoll，read 给出与 Linux 相同的状态行。`mq_notify` 支持 `SIGEV_SIGNAL`、`SIGEV_NONE` 和 musl 的 `SIGEV_THREAD`（经由 netlink socket 送 cookie，与 Linux 相同）。队列在内存里，App 进程重启就没了，相当于 Linux 重启后的 mqueue 文件系统。
- **原生 AIO**：`io_setup`、`io_destroy`、`io_submit`、`io_cancel`、`io_getevents`、`io_pgetevents`，`/proc/sys/fs/aio-nr`、`aio-max-nr`。读写（含向量）、`fsync`、`fdatasync` 提交时就同步做完，`IOCB_CMD_POLL` 在文件就绪时完成，可以取消；`IOCB_FLAG_RESFD` 通知 eventfd，`RWF_NOWAIT`、`RWF_APPEND` 生效。上下文的 id 是一页只读的空映射：libaio 看到环的魔数不对，就不在用户态读完成队列，改用 syscall。
- **epoll**：水平触发的关注在就绪的比 maxevents 多时轮流报告：从上次报告的后面接着查，效果与 Linux 把报告过的放到就绪链表末尾相同（epoll_wait14）。边沿触发的 `EPOLLEXCLUSIVE` 关注，同一次数据变化只报告给一个 epoll（epoll_wait16）：打开文件记着已经报告过的"数据代数"（读完、写满和读到数据的次数）。唤醒仍然叫醒所有等待者，没轮到的重新检查之后接着睡。
- **inotify 的三处近似都去掉了**（§8.3）：每个打开的仓库文件记着打开时经过的目录和名字（原来只有打开时已经有监视的才记），监视建立之前打开的文件关闭时也报告；rename 移动目录项时，经由它打开的文件跟着改（扫一遍所有 fd 表）；`IN_DELETE_SELF` 在还有经由被删除的那个名字打开着的文件时等它们都关闭，顺序（`IN_CLOSE_*`、`IN_DELETE_SELF`、`IN_IGNORED`）与 Linux 相同。`max_user_instances`、`max_user_watches` 生效。
- **nice 和亲和性交给宿主**：nice 改为按线程记录（Linux 的 `PRIO_PROCESS` 作用于线程，原来记在线程组上，第二个线程调 nice 拿到的是叠加的值），并设置到宿主线程上；guest 提高优先级时宿主线程最低到 -4，不超过 Android 上与显示有关的线程，guest 的忙循环不会拖慢界面。`SCHED_IDLE` 设成宿主的 19。`sched_setaffinity` 设置宿主线程的亲和性：guest 的 CPU i 是宿主允许本进程用的第 i 个 CPU，按启动时的快照（`available_parallelism` 读的是调用线程的亲和性，guest 线程被绑核之后再读会少报 CPU）。新线程开始运行时换成自己的设置，不继承创建它的宿主线程的。`RLIMIT_NICE`、`RLIMIT_RTPRIO` 的默认值改为 Linux 的 0，提高优先级按 `RLIMIT_NICE` 检查（Linux 的 can_nice）。宿主上没有权限时（普通 Linux 宿主不给降低 nice）保持原样；Android 的 init 给所有进程 `setrlimit nice 40 40`，开发容器也加上 `--ulimit nice=40:40`。另外 `SCHED_RESET_ON_FORK` 的子线程负的 nice 回到 0，`sched_*` 对还没回收的僵尸进程不再是 ESRCH。
- **内存**：
  - `remap_file_pages`：与 Linux 4.0 起一样用 mmap 模拟，在原地用同一个文件、新的偏移重新做共享映射。
  - `MREMAP_DONTUNMAP` 支持所有映射（Linux 5.13 起）：原来的地址上私有匿名映射变空；共享映射（文件或匿名）与新地址共用同一段宿主内存，相当于之后从文件或共享内存重新读进来；私有文件映射按 inode 重新打开仓库里的文件再映射，打不开（已经删除并回收）时给空的匿名内存。
  - `MAP_GROWSDOWN`：缺页地址在一个向下增长的私有匿名映射下面、中间没有别的映射时，把它延伸到那一页（mmap18），与下面那个可访问的映射之间要留出 256 页（`stack_guard_gap`）。宿主内存不必连续，长出来的部分是一个新的 VMA，所以不需要预留地址空间。共享匿名映射加它是 EINVAL，文件映射加它不增长。
  - cachestat（6.5）：在缓存里的页数用 mincore 查宿主文件映射，不依赖宿主内核的版本（宿主的 cachestat 在开发容器的 overlayfs 上也数不到底层文件的页）；脏页、写回、逐出几项是 0。
- **procfs、pidfd**：
  - `/proc/sys/kernel/ns_last_pid` 可读写（pidfd_send_signal03）。
  - `/proc/<pid>` 目录的 fd 记着打开时的那个进程：当作 pidfd 用时，进程回收之后是 ESRCH，即使 pid 已经给了新进程。`/proc/<pid>` 下的 inode 也随进程实例不同（加进了进程的编号），与 Linux 一样。
  - `/proc/net/{icmp,icmp6,raw,raw6}`，格式与 Linux 相同（raw 的本端"端口"是协议号，ping 和 raw 的对端端口是 0）。ping 和 raw socket 没有对端端口，宿主的 getpeername 不给对端，数据报 socket connect 的对端由内核层记下。guest 的 raw ICMP socket 在宿主上是 ping socket，`SO_TYPE` 照样报告 `SOCK_RAW`。
  - `/proc/net/unix` 列出只在 `SCM_RIGHTS` 消息里的 socket，和还没 accept 的连接（与 Linux 一样没有 inode，状态是 `SS_CONNECTING`）。
- **AF_UNIX 的垃圾回收**：只经由在途消息互相引用的 socket 形成 Rc 的环（打开文件 → socket → 接收队列 → 消息 → 打开文件），程序已经够不着，原来永远不会释放。现在与 Linux 的 unix_gc 一样：消息里带着打开文件的 channel 登记下来；关闭 unix socket（close、close_range、dup2 覆盖、execve）和进程退出之后，候选是所有引用都来自在途消息的 unix socket，去掉从候选的接收队列出发的引用之后还有引用的够得着，它们收到的也够得着；剩下的清空接收队列，环就断开了。别处临时多持有一个引用的不是候选，只会少回收、不会错收。Linux 6.8 起 GC 在工作队列里异步执行，所以这一项没法和原生比对，由单元测试检查。
- **零碎的**：fd 表没有空位时总是 EMFILE（`F_DUPFD` 的起点超过上限由它自己返回 EINVAL），`mq_open` 先装 fd 再登记名字，装不下时不留下队列。
- musl 的问题：mq_open01 的空名字一项要 EINVAL，那是 glibc 自己的检查，musl 直接交给内核，Linux 返回 ENOENT；sched_setscheduler04 用 libc 的 `sched_getscheduler`，musl 的这个函数直接返回 ENOSYS。这两项在原生 Alpine 上同样失败。用 libaio 的 AIO 测试跳过，因为 LTP 构建时没有 libaio。
- 测试：差分测试 `compat` 新增信号驱动的 I/O 和 `F_SETSIG`、文件租约（在 /dev/shm 里，原生一侧的 overlayfs 不让设写租约）、消息队列、AIO、`MREMAP_DONTUNMAP` 和 remap_file_pages，`/proc/net` 那组加上 icmp、icmp6、raw（CI 的 Ubuntu 宿主默认不让普通用户建 ping socket，差分测试之前把 `ping_group_range` 改成和 Android、Docker 容器一样允许所有组）；`inotify` 新增按 fd 的事件报给哪个目录。单元测试新增 inotify 推迟的 `IN_DELETE_SELF`、AF_UNIX 的垃圾回收、`MAP_GROWSDOWN` 的增长和守护间隔，以及 hal 的第一个单元测试（cachestat）。全部 12 组差分测试在两个执行层下通过。LTP 的 syscall 测试 944 个通过（上次 905），新通过 38 个：fcntl23–27、31–33（各两个）、mq_notify01–03、mq_timedreceive01、mq_timedsend01、mq_unlink01、io_cancel01、io_destroy02、io_getevents01、io_setup02、io_submit02–04、epoll_wait14、epoll_wait16、nice05、mmap18、remap_file_pages01/02、shmctl05（用 remap_file_pages）、cachestat02、pidfd_send_signal03；shmctl01 这次也通过了（全量并发时偶发失败）。没有新失败。cachestat01/04 要块设备，03 要 hugetlbfs。
- 性能：每次 open 多挂一个记着目录项的对象，rename 要扫一遍所有 fd 表，新线程开始时多两个宿主 syscall。与上一轮的二进制对比（同一会话里跑两遍取第二遍，各跑三次）：open+close 5 万次、rename 2 万次、起 300 个进程、300 个线程、`git status`（3000 个文件）都在噪声之内。

**推迟项**：

- io_uring 不做，保持 ENOSYS。之后试做过一轮又放弃了：环放在共享内存里，请求在提交者的上下文里先不等待地执行一次（Cx 加"不等待"模式，让现有的阻塞循环直接返回 EAGAIN），会阻塞的挂起，由定时器线程把提交者叫进内核推进（相当于 task_work），SQPOLL 靠一直置位 `IORING_SQ_NEED_WAKEUP` 实现。接口和 49 个操作码写完是 3800 行，liburing 的测试先抽了 10 个，过了一半，剩下的细节（DEFER_TASKRUN 的完成时机、多次接收的 recvmsg、提供的缓冲区等）还有很长一串。考虑到用的人很少（见 §15），代码没有留下。
- `PIDFD_THREAD` 是 6.9 的功能，appine 报告的是 6.1，不做。
- 内核访问 guest 内存（比如 read 写进还没长出来的栈）不会让 `MAP_GROWSDOWN` 的映射增长，返回 EFAULT。→ syscall 入口和构造信号帧之前会让它增长了（M9 之前：栈的大小上限）；内核别的访问仍然是 EFAULT
- `EPOLLEXCLUSIVE` 只保证报告给一个，唤醒还是全部唤醒。

## M9 之前：稳健性（2026-10-25）

开始 M9 之前，把设计文档 §12 里"一个 guest 程序就能弄垮整个 App"的几种情况补上：内核层 panic、引擎让宿主收到故障信号、guest 用掉太多内存、fork 炸弹。原来前两种直接终止宿主进程，后两种等 Android 的 LMK 杀掉整个 App（或者把宿主的线程和 fd 用完），所有会话一起丢掉。现在都只结束出问题的那个 guest 进程，别的会话照常运行。

- **内核层 panic**（§12 第 5 条，`core/kernel/src/bug.rs`）：运行循环在每次进入内核（syscall、同步异常、kick）的地方 `catch_unwind`。panic hook 先打印消息和位置，再写一行报告（pid、comm、线程、最后的 guest pc、哪个 syscall），用 SIGSEGV 结束整个线程组，相当于 Linux 的 oops 结束当前任务；之后获取 BKL 时忽略中毒标记。BKL 保护的状态可能只改了一半，但出问题的进程已经结束，大部分不一致随之消失。线程开始和退出时的 panic 仍然终止宿主进程。
- **引擎的宿主故障**（§7.4，`core/engine/src/c/guard.c`）：引擎装 SIGSEGV、SIGBUS、SIGILL 的处理函数，原来的处理方式记下来。`appine_cpu_run()`、`appine_dbt_run()` 改为经由 `appine_guard_run()` 运行：先 `sigsetjmp`，运行期间出了故障，处理函数记下信号、`si_code`、宿主 pc 和地址，`siglongjmp` 回去，返回新的退出原因 `APPINE_EXIT_HOST_FAULT`。
  - 跳回之后恢复宿主的 FPCR（翻译后的代码里装的是 guest 的），释放运行期间登记的锁（原生桩缓存的读写锁、翻译模板的互斥锁），解除对这个信号的屏蔽（`sigsetjmp` 不保存屏蔽字，省掉每次一个 syscall）。运行期间引擎不调用 malloc、不回调内核层，原来就是这样。
  - 出过故障的 Tier 1 实例的几张表可能只改了一半，不回到池里，直接释放；翻译模板整体丢弃，坏掉的可能是模板本身。
  - 内核层收到这个退出原因后，像 panic 一样报告并用 SIGSEGV 结束进程。
  - 不在保险期间的故障（内核层自己的、ART 没认领的别的线程的）先写一行报告，再交给原来的处理函数：Android 上 libsigchain 记着的是 debuggerd 的，照样生成 tombstone；Linux 开发宿主上是 Rust 标准库检查栈溢出的处理函数。别人用 kill 发来的（`si_code` ≤ 0）不当作引擎的故障。
- **debug 构建**按设计文档的约定仍然直接终止宿主进程（不装处理函数，panic 时 abort），方便就地调试；`APPINE_ON_BUG=kill` 改成 release 的做法。端到端测试用的钩子：debug 构建设置 `APPINE_TEST_HOOKS=1` 时，syscall `0xa991` 让内核层 panic，或者让一页在页表里指向 `PROT_NONE` 的宿主内存，guest 下次访问它时引擎在宿主上出错。
- **App 内存的上限**（§12 第 4 条，`core/kernel/src/oom.rs`）：
  - 量的是整个 App 进程的匿名内存：`/proc/self/status` 的 RssAnon + RssShmem + VmSwap。LMK 看的就是它；guest 的私有、共享内存之外，内核层为 guest 付出的翻译缓存、元数据缓存也在里面，仓库文件和程序的文件页可以回收，不算。默认上限是物理内存的一半，`MachineOptions::memory_limit` 和 `appine-run --memory-limit` 可以改。
  - guest 碰新的页由宿主内核直接处理，内核层看不到缺页，只能定时检查。有 guest 线程不在内核里等待时，定时器线程每 20 毫秒量一次，超过上限的 3/4 之后每 5 毫秒（这台虚拟机上读一次只要 1.6 微秒）；所有线程都在等待时内存不会再涨，检查停下来，某个线程要回到 guest 代码时经由一个原子标记叫醒它。最早用的 100 毫秒间隔下，不停分配内存的程序（Tier 1 下每秒碰好几 GB 的新页）在 512 MB 的上限上多用了 200–300 MB；改成 20/5 毫秒之后是 10–35 MB。
  - 超出时按 Linux 的 oom_badness 挑进程：驻留的页数，加上 `oom_score_adj` 按物理内存折算的页数，`oom_score_adj` 原来只记录，现在生效。第一版按上限折算，awk 的驻留内存超过整个上限时盖过了 +1000，改成与 Linux 一样按全部内存折算。挑中的用 SIGKILL 结束，报告写到 stderr；等它退完（最多 1 秒，卡在宿主 I/O 里的不再等也不再挑）再看要不要结束下一个。第一版等的是"任何正在退出的进程"，碰上一个无关的卡住的进程会白等，改成与 Linux 一样只等自己挑中的；挑中的进程已经在退出时不再结束它，与 Linux 的 `task_will_free_mem` 一样。
- **线程数的上限**（§6）：`/proc/sys/kernel/threads-max` 原来只显示 pid_max，现在生效，可以写（20 到 `FUTEX_TID_MASK`），对 root 也有效。默认按物理内存每 MB 一个（Linux 是每 128 KB 一个：appine 的每个线程是一个宿主线程，还占一个 eventfd），并且不超过宿主 fd 上限的一半。`RLIMIT_NPROC` 原来不限，现在默认是 threads-max 的一半，与 Linux 一样按真实用户 ID 数线程，root 和有 `CAP_SYS_RESOURCE`、`CAP_SYS_ADMIN` 的不受它限制。
- **性能**：与上一轮的二进制交替运行、各取最短：100 万次 `getppid` 从 59 毫秒变成 65 毫秒（每次进入引擎多一次 `sigsetjmp` 和一次 TLS 查找，约 6 纳秒；实际的 syscall 在 appine 里要几百纳秒到几微秒）；shell 起 300 个进程在噪声之内（406–436 毫秒对 422–429 毫秒）。
- **测试**：
  - 引擎新增 `tests/guard.rs`：两个执行层访问了指向 `PROT_NONE` 宿主页的页表项之后返回 `HostFault`，之后照常运行；Tier 0 在原生桩的读锁里出错之后锁被释放（否则接下来生成新桩要拿写锁，同一个线程会卡死）。这个测试也用 cargo-ndk 编成 Android 程序，在 4K（API 34）和 16K（API 37）的模拟器上直接运行，验证了 bionic 下的 `sigsetjmp`/`siglongjmp` 和 `ucontext`。
  - `tests/guest/bugs.c` 在两个执行层下用测试钩子触发内核层 panic、引擎故障、另一个线程正阻塞在内核里时的引擎故障，子进程都被 SIGSEGV 结束，父进程之后照常建线程、fork。`tests/guest/limits.c` 检查 `RLIMIT_NPROC` 的默认值、root 不受限制、普通用户到上限时 fork 返回 EAGAIN，以及超出 256 MB 的上限时吃内存的子进程被 SIGKILL 结束、父进程之后照常分配内存。
  - rootfs 用例新增两个：threads-max 的读写和对 root 生效（busybox sh 报告 "can't fork"）；`oom_score_adj` 为 1000 的进程先于吃内存的 awk 被结束（只在 Tier 1 下跑，Tier 0 的 debug 构建拼 128 MB 的字符串要十几秒）。
  - 两个模拟器上的 instrumentation 测试都通过：App 进程（release 构建，装着兜底）里 ART 照常工作。
  - 全部 12 组差分测试在两个执行层下都和原生一致（Go、Java、Node.js 都在默认的内存上限下运行）。LTP 的 syscall 测试 943 个通过（上次 944），唯一的不同是 shmctl01：以前就记录过的全量并发时的偶发失败，单独重跑 5 次都通过。新的 `RLIMIT_NPROC`、threads-max 默认值没有让任何用例变化。

**推迟项**：

- 引擎故障在 App 进程里的恢复（要经过 ART 的 libsigchain）没有直接测过：Android 上验证了 bionic 下的恢复（模拟器上直接运行引擎测试），以及 App 装上处理函数之后 ART 照常工作；libsigchain 把 ART 不认领的故障转给我们的处理函数，`siglongjmp` 离开它时它没有要恢复的状态（按它的源码）。→ 已补上：App 进程里的 instrumentation 测试（M9 之前：稳健性的收尾）
- 被结束的进程只在宿主 stderr（Android 上是 logcat）里留下报告，终端里只看到 shell 打印的 "Segmentation fault" 或 "Killed"。App 可以在标签页里提示，留给 M9 的外壳。
- 内存上限只看 App 自己：系统整体内存紧张时（`onTrimMemory`）还没有接到 OOM killer 上。
- `oom_score_adj` 的写入不检查权限：Linux 不让没有 `CAP_SYS_RESOURCE` 的进程把它调到 `oom_score_adj_min` 以下。→ 已补上（2026-10-26，单独修补）：没有这个能力时调到下限以下返回 EACCES，有它时写的值成为新的下限，fork 继承
- 线程开始、退出时的 panic 仍然终止宿主进程；定时器线程、`Machine` 方法（JNI 调用）里的 panic 还没有处理。→ 已补上（M9 之前：稳健性的收尾）

## M9 之前：稳健性的收尾（2026-10-25）

上一轮兜住了运行循环里的内核层 panic 和引擎的宿主故障，还剩三处：线程开始、退出时的 panic 仍然终止宿主进程；定时器线程和宿主调用（Android 上是 JNI 调用）里的 panic 没有处理；App 进程里经过 libsigchain 的恢复没有直接测过。这一轮补上（§12 第 5 条，`core/kernel/src/bug.rs`）。

- **统一的兜底**：`bug::catch` 执行一段内核代码，panic 时返回 None，由调用方收拾；不兜的时候（debug 构建的默认）直接终止宿主进程。运行循环原来的写法也改用它。
- **线程开始**：取 CPU 时钟和宿主 tid、设置 nice 和亲和性、读地址空间的代数这一段兜住，出错时和进入内核时一样用 SIGSEGV 结束这个进程，线程直接走退出的路径。
- **线程退出**：分几步分别兜住：结算（翻译统计、CPU 时间、驻留内存的峰值）并离开地址空间（robust futex、`clear_child_tid`、vfork）；结束进程（`finish_group`）；释放资源（地址空间、fd 表）；AF_UNIX 的垃圾回收。一步出错时，还有别的线程就结束这个进程，后面的步骤照样做，所以线程总会从内核里注销，进程总会结束，等它的父进程和宿主不会卡住。
  - `finish_group` 自己出错时退回到最要紧的两件事：记下宿主等着的结束方式，把进程交给父进程（变成僵尸进程并发 SIGCHLD，或者直接回收）。后一件从 `finish_group` 里提成 `hand_to_parent`，两边共用。子进程交给 init、孤儿进程组的信号这些就丢掉了。
  - 进程已经调用了 exit_group 时，它定下的退出码不变，和 Linux 的 oops 一样。
- **定时器线程**：原来这里 panic 会让定时器线程悄悄退出，之后所有进程的 alarm、POSIX 定时器、OOM killer、SIGIO 都不再工作。
  - `itimer::expire` 改为每个进程单独处理、单独兜住，这个进程的信号也在里面发。出错时清掉它的定时器，用 SIGSEGV 结束它，别的进程照常。
  - 内存上限的检查和信号驱动的 I/O 分不出是哪个进程的，出错时关掉这项功能：上限改为不限，清掉 `O_ASYNC` 的登记，并报告一行。
- **宿主调用**：`Machine` 的方法和 `import_rootfs`、`write_file` 经由 `bug::host_call` 执行，出错时只让这次调用失败（EIO 或新加的 `RunError::Internal`）。JNI 调用随之抛出 IOException，panic 不会穿过 JNI 边界让整个 App abort。
  - 读写控制台、改窗口大小时出错，还像终端挂断一样关闭那个控制台，会话组长收到 SIGHUP，标签页随之结束，不会卡在坏掉的控制台上。
  - `open_console` 因此改为返回 `Result`，JNI 的 openConsole 失败时也抛出 IOException（`TerminalSession` 本来就处理 `Pty.start` 的 IOException）。
  - 只读一张表的 `wait`、`try_wait`、`dbt_stats` 不需要兜。
- **测试钩子**：syscall `0xa991` 新增四种，用 `bug::trap` 布下：调用的线程退出时、这个进程下一个开始运行的线程、定时器线程处理这个进程时、宿主下一次调用时 panic。`bug::trap` 在没有测试钩子的构建里是空函数。测试钩子改为在 debug 构建或者打开 `test-hooks` 特性时存在：Android 的原生库总是 release 构建，`app/build.gradle.kts` 改为按变体分别构建 Rust 部分，debug 版 App 打开这个特性，release 版没有（检查过 .so 里没有测试钩子的字符串）。
- **测试**：
  - `tests/guest/bugs.c` 新增四种情况，子进程都被 SIGSEGV 结束：线程退出时（进程的最后一个线程，以及另一个线程正在 `pthread_join` 里等它）、新线程开始时、定时器线程上。之后父进程的 `ualarm` 照常到期，说明定时器线程还在。
  - 新用例 `host_calls_survive_kernel_bugs` 在测试进程里直接用 `Machine`：spawn 里出错时返回 `RunError::Internal`；读控制台时出错返回 EIO，会话组长被 SIGHUP 结束，控制台没了；之后照常新开控制台运行程序。内核层只在第一次撞上 bug 时读 `APPINE_ON_BUG`，所以这个用例在设置了环境变量的子进程里重新运行自己。几个用例同时编译同一个 guest 程序时会运行到写了一半的文件，编译改为先写临时文件再改名。
  - Android：`MachineTest` 新增 `bugsKillOnlyTheGuestProcess`。它把手工拼的三个静态 ELF 写进仓库，在 App 进程里、两个执行层下运行：
    - 模拟引擎故障：SIGSEGV 先到 libsigchain，ART 不认领，交给我们的处理函数 `siglongjmp` 回去，进程被 SIGSEGV 结束；
    - 内核层 panic：Rust 的 unwind 在 bionic 上照常工作，进程被 SIGSEGV 结束；
    - 在宿主调用里 panic：JNI 的读控制台返回 -1，会话组长被 SIGHUP 结束。
    之后 GC、栈溢出变成 StackOverflowError、busybox 都正常。4K（API 34）和 16K（API 37）模拟器上 4 个用例都通过，logcat 里有对应的报告。
  - 全部 12 组差分测试在两个执行层下都和原生一致。LTP 的 syscall 测试 941 个通过（上次 943）。多出来的两个失败是 futex_wait05、select02，都报"睡得太久"：用上一轮的二进制在同一时刻交替各重跑 3 次，结果完全一样，是虚拟机这时的计时延迟。shmctl01 仍是全量并发时的偶发失败。
- **性能**：正常路径上只多了几次 `catch_unwind`（不 panic 时没有开销）和一次 `bug::trap`（release 构建里是空函数）。和上一轮的二进制交替运行、各 5 次取最短：shell 起 300 个进程 451 → 442 毫秒，2000 次创建并 join 线程 191 → 189 毫秒，都在噪声之内。

**推迟项**：

- `appine-run` 的终端桥接线程（`terminal::bridge`）直接调用控制台的内核代码，没有兜住。它只在开发用的 `appine-run` 里，App 不经过它。
- `finish_group` 中途出错时，子进程交给 init、孤儿进程组的 SIGHUP/SIGCONT、IPC 的清理会丢掉。
- 结束进程的报告仍然只在 stderr（logcat）里（上一轮的推迟项，留给 M9 的外壳）。

## M9 之前：栈的大小上限（2026-10-26）

原来 execve 总是映射固定 8 MB 的主线程栈，`RLIMIT_STACK` 只是记下来的一个数：`ulimit -s unlimited` 返回成功但没有效果，深递归的程序（刷题时常这样用）照样在 8 MB 处段错误，调小也不起作用。这一轮改成与 Linux 一样按需增长（§5.4）。

- **按需增长**：execve 只映射参数、auxv 下面再多 128 KB（Linux `setup_arg_pages` 的 `stack_expand`），栈的 VMA 标成向下增长，guest 访问栈底下面的地址出错时延伸到那一页再重新执行，沿用上一轮给 `MAP_GROWSDOWN` 做的路径。延伸之后的大小按 `RLIMIT_STACK` 的当前值检查（Linux 的 `acct_stack_growth`），所以运行中 setrlimit 调高、调低都立即生效；`MAP_GROWSDOWN` 的映射也受它限制。
- **宿主内存**：上一轮的 `MAP_GROWSDOWN` 每长一次就是一个新的 VMA（一个宿主映射），主线程栈一页一页地长下去，几百 MB 就会用完 `max_map_count`。
  - 现在 execve 时在栈下面预留到 8 MB 宿主地址空间（`MAP_NORESERVE`），在这个范围里增长只是加长同一个 VMA。超出之后另外申请一段，下面再预留到与栈现在的大小相同，VMA 的个数按对数增加（1 GB 的栈是 8 个）。
  - `/proc/<pid>/maps` 把这几段合成一行 `[stack]`：与 Linux 一样按初始栈指针（`start_stack`）认出栈，原来是按固定的起点。
  - fork 时子进程照样保留栈下面的预留。fork 原来不把向下增长的标记复制给子进程，子进程里的 `MAP_GROWSDOWN` 映射不会增长，一起修正了。
- **内核访问**：栈不再预先映射 8 MB 之后，内核写进栈底下面的缓冲区会得到 EFAULT（上一轮的推迟项）。内核访问 guest 内存的地方有几十处，都只拿着地址空间的只读引用，不方便就地增长，所以只在两处补上：
  - syscall 入口发现 SP 已经低于栈底（程序分配了大的栈帧还没碰）时，先把栈延伸到 SP。arm64 没有 red zone，syscall 用到的栈上缓冲区都在 SP 以上。主线程上这个检查只比较一次 SP 和栈底；别的线程的 SP 在 mmap 区里，再比较一次上限就停。
  - 构造信号帧之前延伸到帧的起点。
- **布局**：mmap 区的上界原来固定在栈顶往下 1 GB 加 8 MB，现在与 Linux 的 `mmap_base` 一样按 execve 时的 `RLIMIT_STACK` 留出间隔：至少是原来的大小（默认布局不变），最多地址空间的 5/6。不限栈大小时 Linux 改用自底向上的 legacy 布局，这里仍然自顶向下，间隔取最大值，ld-musl 从 `0x7fff_bf7…` 移到 `0x1555_5549…`。
- **参数大小**：execve 的参数上限原来固定是 2 MB，现在按 Linux 的 `bprm_stack_limits`：栈大小上限的 1/4，不超过 6 MB，不少于 128 KB。setuid、setgid 程序的栈大小上限与 Linux 一样压到 8 MB，并写回进程的 `RLIMIT_STACK`。
- **测试**：
  - 内核层单元测试新增 4 个：就地增长到预留用完、之后按倍数申请、maps 里仍是一行 `[stack]`；上限按分开的几段合起来算、调高之后接着长；fork 之后子进程就地增长；mmap 区的上界。原来的 `MAP_GROWSDOWN` 用例加上 fork 之后的增长。
  - 新的 guest 程序 `tests/guest/stack.c`，两个执行层：
    - 用内联汇编把 SP 往下挪 1 MB（不碰那些页）再 read 进那里，挪 2 MB 再给自己发信号（处理函数在那里用 8 KB 栈），都成功；挪 16 MB 时 read 返回 EFAULT。分别去掉 syscall 入口、信号帧的增长时，对应的一项失败（EFAULT、SIGSEGV）。
    - 默认上限下递归用掉 4 MB 可以，16 MB 被 SIGSEGV 结束；setrlimit 调到 64 MB 之后 32 MB 可以，调到 16 MB 之后 24 MB 被结束。上一轮的二进制在调高上限这一项失败。
  - rootfs 用例新增一个：初始的 `[stack]` 是 132 KB；`ulimit -s 1024` 下 20000 个参数 E2BIG、10000 个可以，默认上限下 20000 个可以；`ulimit -s unlimited` 之后 ld-musl 映射在 `MAX_GAP` 下面。
  - 全部 12 组差分测试在两个执行层下都和原生一致。LTP 的 syscall 测试 944 个通过（上次 941），没有新的失败，多出来的 3 个是以前记录过的偶发失败（futex_wait05、select02 的计时，全量并发时的 shmctl01）；mmap18、setrlimit04（栈限制在 1 MB 时运行程序）和 execve 系列照常通过。
- **性能**：与上一轮的 release 二进制交替运行、各 5 次：100 万次 `getppid` 61.7–63.1 毫秒对 62.1–64.4 毫秒，在噪声之内；2000 次 fork 加 wait 从 240–243 毫秒降到 209–219 毫秒（栈的 VMA 从 8 MB 变成 132 KB，fork 查 pagemap 的范围小了）；第一次用到 6 MB 栈从 1.1–1.3 毫秒变成 1.8–2.1 毫秒（约 1500 次增长，每次 0.4 微秒左右）。

**推迟项**：

- 内核别的地方访问栈底下面时不会让栈增长，返回 EFAULT（Linux 上会增长）：指向 SP 下面的缓冲区（arm64 的 ABI 不让程序用 SP 下面的内存，只有故意这样做的程序会碰到），以及 `/proc/<pid>/mem`、`process_vm_writev` 访问别的进程的栈。
- 不限栈大小时没有用 Linux 的 legacy 布局（见上）；mmap 的地址提示仍然只在 mmap 区的上界以下才采用。
- 没有 `/proc/<pid>/limits`。

## M9 之前：访存检查和翻译表（2026-10-26）

剖析 gcc 和 python 时给每条生成的宿主指令记下用途（地址计算、TLB 检查、访问、退出桩……），再用 perf 的样本去对：翻译后的代码占八成时间，其中 TLB 检查约占三成，访问本身一成，地址计算和 SP 对齐检查半成，样本集中在取表项的那条 `ldp` 上。另外两处与此无关的问题也是剖析中发现的：栈帧跨页的访问每次都退给 Tier 0，以及 JIT 负载下翻译表的探测序列极长。

- **跨页访问**（§4.2）：回填例程原来遇到跨页就失败，由 Tier 0 执行这一条。函数的栈帧落在页边界上时，每次调用都要退出：gcc 编译两次退出约 90 万次（几乎都是 `stp`/`ldp` 和 `stp q`/`ldp q`），python 的解释器循环里有一处每次迭代退出两次，两次运行一共 1196 万次。现在两页都有权限、宿主地址连续时照样在宿主上一次完成，两页都在 TLB 里时只比对两个表项。回填成功之后直接回到访问本身，不再重新检查。
- **共用检查**（§4.2）：同一个块里以同一个寄存器为基址的几条访存只检查一次，组员和原生一样是一条 `[T2, #偏移]` 访问。翻译时从组长往后找组员，中间允许不写基址（按常数加减的记下偏移）、不用 T2 的指令和条件分支，覆盖的范围最多 512 字节。检查失败时以新的退出原因 R_UNGROUP 退给 Tier 0；同一个组长第二次失败时作废这个块，它和后面一个块长度以内的访存都不再当组长，翻译模板按实例记下的这些位置区分版本。第一次失败不作废，是因为栈还没长到那里这类失败 Tier 0 执行组员时就会解决：最初的做法一失败就作废，`access_ref` 的构造函数（15 条写入，对象在新长出来的栈上）一条接一条地失败了 15 次。gcc 编译里每轮约 14 次作废。
  - 同时省掉了两处小开销：一个块里 SP 检查过对齐、之后只按 16 的倍数改过时不再检查；访问落在对齐的 16 字节里时不可能跨页，取表项直接用第一个字节的页。
  - 静态统计：gcc、python 翻译的访存里 27% 和前面的访存共用检查。
- **翻译表的墓碑**（§4.2"失效"）：Maven 热构建里翻译表（开放寻址、线性探测）平均每次查找探测 125 个位置，插入 280 个。作废的块留下墓碑，原来插入时从不复用、只在整体清空时去掉；HotSpot 反复改写、重新翻译同一处代码，每翻译一次就在同一个位置附近多占一个槽，扩容时墓碑还全部重新插入，挤在同一个哈希位置上。现在插入时复用探测路径上的第一个墓碑，表里有四分之三不空时，墓碑占一半以上就原样重建（去掉墓碑），否则加倍。探测降到平均 1.5 个位置。起初以为是共用检查让 Maven 慢了 10%（三个版本交替各跑 4 次：HEAD 19.4 秒，只有跨页修改 19.8 秒，加上共用检查 19.0–22.0 秒），数了探测次数才找到这里：同一次构建在两个版本里的探测分别是平均 125 和 213 次，差别来自块的分布，问题本身早就存在。
- 测量条件与前几轮相同（Apple Silicon 上的 colima 虚拟机，4 核，release，Tier 1，同一个会话里跑第二次），与这一轮之前的二进制对比：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| gcc 编译 `lvm.c` | 0.69s | 2.99s（4.3×） | 2.47s（3.6×） |
| python 启动 5 次加循环 | 0.38s | 2.24s（5.9×） | 1.88s（4.9×） |
| apk info 20 次 | 19.5s | 46.0s（2.4×） | 39.5s（2.0×） |
| `go build` hello，冷缓存 | 2.2s | 16.6s | 14.3s（6.5×） |
| Maven `package`，依赖已在本地 | 1.68s | 19.4–20.2s | 15.8–15.9s（9.4×） |
| Gradle `build`，守护进程已在运行 | 0.52s | 4.7–4.9s | 2.9–3.0s（5.6×） |
| shell 起 600 个进程、`git status` | 0.16s、0.02s | 0.93s、0.17s | 不变 |

  整数基准的几何平均 1.71×（M7 是 1.77×）。调用密集的微基准不变。
- **测试**：
  - 引擎测试新增 6 个：跨页的读、写、成对访问不退出（第一次两页都不在 TLB 里，第二次只比对表项）；宿主页不相邻时退给 Tier 0、结果正确；跨进只读页的写、跨进没有映射的页的读、绕回地址空间开头的地址，都在第二页报告精确的错误，第一页不被写；函数的序言和尾声（含被占用的 x16、x17、x27）6 条访存只检查一次；合起来跨到宿主上不相邻的页时，第二次失败之后不再分组；条件分支后面的组员访问没有映射的页、分支跳走时照样正确。实例来自共用的池子，带着别的测试记下的不再分组的位置，所以这几个测试的代码各放在自己的地址上。
  - lockstep 新增 `same_base` 类别：同一基址的 2–4 条访存（整数、SIMD&FP 的单个和成对访问，前后变址，中间夹着按常数加减基址的 ADD/SUB，最后一条可以把基址覆盖掉），范围可以跨页；报告里新增 Tier 1 共用检查的访存条数，`cargo test` 检查这个类别确实分了组。20 万个用例与硬件一致。
  - `--stats` 新增共用检查的访存条数和作废的分组次数。
  - 全部 12 组差分测试在两个执行层下都和原生一致。LTP 的 syscall 测试 942 个通过（上次 944），多出来的两个失败是以前记录过的偶发项：shmctl01 单独运行 3 次都通过；futex_wait05（"slept for too long"）这时用上一轮的二进制同样失败，nanosleep01、clock_nanosleep02 也一起失败，是虚拟机的计时。

**推迟项**：

- mremap 扩大映射时，新增的部分是另外申请的一段宿主内存，和原来的不连续（另起一段的栈也是这样）。realloc 长大的数组在这些边界上，跨页访问仍然退给 Tier 0，共用检查失败两次之后不再分组。可以改成用宿主的 mremap 原地扩展。
- 共用检查只能跟一个基址：临时寄存器只有三个，交错访问两个基址时只有一个能合并。
- 剩下的开销：TLB 检查仍占 gcc 的 23%、python 的 28%；块入口的 kick 检查 6–8%（经由返回进入的块可以考虑跳过）；Maven 里 `table_resize` 占 3.7%（实例回到池里时翻译表缩回 4096 项，新线程再一次次加倍）。

## M9 之前：多线程共用翻译和定向唤醒（2026-10-26）

用户要求第一个版本就有尽量好的性能，原来放到发布之后的项目（拆分 BKL 等）也可以现在做。先剖析各负载再定，结果多线程的部分（JVM、`go build`）和内核层的唤醒占了大头，BKL 本身反而不是瓶颈。

**测量方法的修正**：以前的基准仓库在 `target/` 里，colima 经由 virtiofs 把 macOS 的目录共享进虚拟机，宿主的每次 open、stat 都要跨虚拟机往返（`fuse_dentry_revalidate`、`virtio_fs`）。原生对照组跑在虚拟机本地盘上，所以文件系统、起进程类负载的倍数被夸大了：同一个 HEAD 二进制起 600 个进程，virtiofs 上 0.93 秒，本地卷上 0.39 秒（原生 0.15 秒）。手机上的仓库在 ext4/f2fs 上。这一轮起基准仓库拷进 docker 卷 `appine-bench`，下面的数字都是在那里测的，"之前"一列是这一轮开始时的 HEAD（351800b）在同样条件下的结果。

- **getrusage 取样限流**（Maven −24%）：`ru_maxrss` 每次都用 mincore 扫一遍写过的映射，JVM 频繁调用 getrusage、堆又大，这占了 Maven 12% 的 CPU，而且是持着 BKL 做的。现在距上次取样不到 10 毫秒时沿用上次的峰值；execve、退出时的取样不受限制，LTP 的 getrusage03 仍然通过。
- **代码失效记录二分查找**：每个线程回到 guest 代码时，原来把 4096 个代数的记录整条扫一遍再收集成 Vec（Maven 4.7%），现在二分找到起点。
- **只改代码时不等、不打断**：JIT 的 `ic ivau` 原来要持着 BKL 打断所有线程、等它们退出（Maven 里 `Mm::sync` 和 `sched_yield` 合计 6%）。现在翻译是共用的（见下），改代码的线程重新进入 guest 代码之前作废一次就行，作废块的入口跳往转发桩，其他线程不必打断；与硬件上别的核在上下文同步之前可能还执行旧指令一样。membarrier 的 SYNC_CORE 仍然等。kick 退出从 43 万次降到 4 万次。
- **`ic ivau` 按页合并、`dc` 不退出**（§4.2）：HotSpot 刷新缓存的例程一次 Maven 构建执行 `ic ivau`、`dc cvau` 各 87 万次，每条都退给 Tier 0，`ic ivau` 还要进内核。现在 Tier 1 里 `dc cvac/cvau/civac` 只按读访问检查地址；`ic ivau` 把所在的页记在 cpu 结构体里（原来的 `reserved3`，现在叫 `ivau_page`），同一页不再退出，`dsb`/`isb` 时才以 `APPINE_EXIT_CODE` 交出来；因为别的原因退出时内核层取走它。Tier 0 不变。
- **唤醒分通道**（§6，单线程 `git status` 86 → 36 毫秒）：每关一个文件，`OpenFile` 的析构都请求唤醒，释放 BKL 时叫醒所有登记的等待者：定时器线程、等进程结束的宿主线程、在 wait4 里等子进程的 shell，它们醒来抢 BKL 又睡下（`eventfd_write` 9%、`try_to_wake_up` 8%、锁争用 5%）。现在 `request_wake_files` 只叫醒关心文件的等待者，关闭文件、读写管道、flock、epoll、socket、inotify 这些修改都改用它；wait4、`Machine::wait` 和定时器线程（没有 `O_ASYNC` 文件时）只听其余的通道。
- **翻译模板按桶分片加锁**（`go build` −13%）：所有进程的所有线程共用一把模板锁，`go build` 里 glibc 互斥锁的等待和唤醒合计约一成。现在 64 片，分配模板区另用一把只持有片刻的锁，整体丢弃时持有全部的锁。模板区放大到 128 MB 时几乎不再新翻译，但耗时不变，所以保持 32 MB。
- **建块时直接链接、合并同步**：建块（翻译或拷贝模板）时目标已经翻译过的出口直接链接，模板为此记下链接桩的位置和目标。剖析发现 `dbt_run` 自身的时间几乎全在指令缓存同步上：`ic ivau` 之后的 `dsb ish` 要等作废广播完成，单独同步一行约 0.12 微秒，批量作废时每行只要 7.5 纳秒（微基准，`target/mem/cmo`）。所以只有一个线程时，新块和链接点合在一起同步一次。`go build` 9.2 → 8.8 秒。
- **同一地址空间的线程共用翻译**（§4.2，`go build` −11%）：原来每个线程一个 Tier 1 实例，Go 的一个 `compile` 进程有 7 个线程，各翻译 3–5 万个块，合计 18 万；`go build` 里一共建了 625 万个块、链接退出 840 万次。现在代码缓存、翻译表和作废记录按地址空间共用（`DbtSpace`，Mm 持有），线程只有自己的跳转缓存、影子返回栈和统计（`Dbt`）。`go build` 建块降到 272 万、链接退出 360 万。要点：
  - 分发器不持锁查翻译表：插入只用空位（先写 pc、再写 code），作废只把 pc 改成墓碑，墓碑不再复用（复用时读者可能读到新 pc 配旧 code），重建表时去掉。换表之后等正在查旧表的线程查完再释放：线程查表时 cpu 结构体里的序号（原来的 `reserved`，现在叫 `dbt_lookup_seq`）是奇数。
  - 翻译不持锁：在线程自己的暂存区里翻译或拷贝模板（`prepare`），持锁时只分配位置、拷贝、修正跳往例程的 `b`、链接、同步、放进表（`install`）。翻译之前先拷一份 guest 指令字，装进去之前与内存比对，不一致（期间别的线程改写并作废了这段代码）就重新翻译。持锁翻译时，Maven 一次构建在锁上等了 1.14 秒（4.5 万次争用）；改完之后再把作废按批同步（先写好一批转发桩、同步一次，再改这批块的入口、同步一次），取锁时先自旋 200 次，争用降到 1.2 万次、0.24 秒。
  - 新块先同步指令缓存再放进表里，否则别的核可能经由刚回填的链接跳进还没同步的代码。链接点的回填不持锁：链接桩以跳到下一条的 `b` 开头，`b` 改 `b` 在 CMODX 规则里可以并发修改；条件分支不在规则里，只在地址空间里只有一个线程时直接改写。
  - 整体清空要停住所有线程：发起的线程置上 flushing、打断其他线程，放开锁等它们停在分发器顶部（park）或者离开 `dbt_run`，再清空；各线程按代数清空自己的跳转缓存和影子返回栈。线程那边先登记 cpu、再计入 running、再看 flushing，发起的一方先置 flushing 再看 cpu，等待时先读 parked 再读 running，保证不会漏掉正在跑的线程。宿主故障作废实例时叫醒停着的线程，其余线程改用 Tier 0 跑到进程被结束。
  - JIT 负载的代码缓存撑得更满：代码缓存放大到 128 MB（块跳往例程、块之间的链接都是一条 `b`，±128 MB 是上限；试过 256 MB，超过 128 MB 之后跳飞，Maven 段错误），翻译表上限 2^19 项；块的记录记满时作废的超过八分之一就压缩掉它们的记录、重建表；超过 256 页的作废逐个看所有的块，不整体清空。Maven 加 Gradle 一轮整体清空从 21 次降到 4 次。
  - 剖析中途 `go build` 重复跑到第 6 次时 4 个线程卡在 `b .` 上：线程 A 作废块 X 时先改入口、再从表里删掉，线程 B 在这之间不持锁查到 X，进去经由转发桩退出，拿查到的 X 回填 X 的入口。现在持锁时重新查表（锁内表里只有有效的块），不持锁回填时目标不能是回填位置本身。用 gdb 抓栈的脚本在 `target/mem/hangcatch.sh`。
  - 与每线程各一份对照（同一个二进制加开关，交替运行）：`go build` 共用 8.0 秒、各自 9.1 秒；Maven 7.9 对 7.7 秒、Gradle 1.81 对 1.74 秒，JVM 共用时仍慢 2–4%，原因没查清（锁已经不是了，可能是代码局部性）。内存上一个进程只有一份代码缓存。
- **跳转缓存没命中时在翻译后的代码里查表**（§4.2）：间接跳转、返回预测落空时原来退出到分发器，Maven 加 Gradle 一轮 2600 万次。现在代码缓存开头有一个查表例程（`emit_lookup`），像分发器一样不持锁查共用的翻译表，查到了填跳转缓存再跳过去，查不到才退出；退出降到 17 万次。
- **池子按程序保留翻译**（§4.2，`go build` 8.0 → 6.8 秒，`node -e 0` 100 → 58 毫秒）：用完的实例原来块数超过 32768 就清空再放回池里，node 一次启动约 5 万个块，根本留不下来；池里只有两个，而且不认程序，sh fork 出来的子进程在 exec 之前就会拿走一个，核对时对不上（那时跑的还是 sh 的代码）便整体清空。现在池里 6 个，按程序（可执行文件的 inode）作键，优先还给同一个程序；没有同键的就拿空的或新建，不清空别人留下的；保留的上限放宽到 13 万块，池里保留的代码总共最多 64 MB，超出时先去掉空的、再去掉最久没用的。`go build` 的一个个 compile 进程、一次次 `node -e` 都能拿回前一个留下的翻译，核对一遍比重新拷贝模板、链接、同步指令缓存便宜得多。
- **仓库缓存的哈希和 `name_of`**：元数据缓存的键是 inode 号和文件名，换成 FxHash，默认的 SipHash 在 stat 路径上占 4–5%。打开相对路径时要算出绝对路径（`/proc/<pid>/fd` 显示用），逐级查名字的 `Store::name_of` 原来没有缓存，也不在快照读事务里，每条查询单独开关一次 WAL 读事务；现在按目录缓存，目录里的名字有改动时整组去掉。
- **BKL 先自旋**：取 BKL 时先 `try_lock` 100 次，lstat 这样几微秒的临界区不必睡下再被叫醒，释放时也不必 futex 唤醒。并行 lstat 的 `git status` 34–38 → 24 毫秒，仍比单线程（19 毫秒）慢：BKL 把并行的 lstat 串行化了，见推迟项。
- **普通文件的读写不持 BKL**：宿主的 pread/pwrite 原来持着 BKL 做，冷数据要等闪存时所有进程的系统调用都跟着等（一个标签页在 `apk add`，另一个标签页的 shell 就会卡）。现在取出宿主 fd 和位置后放开 BKL 读写；用文件位置的读写，同一个打开文件同时只有一次（Linux 的 f_pos_lock），其余的等它做完；O_APPEND 的写仍持锁，几个进程同时追加时不会互相覆盖。
- **池里的翻译可以释放**：`Machine::trim_memory`（引擎的 `trim_dbt_pool`）丢掉池里保留的翻译，留给 Android 外壳在 `onTrimMemory` 时调用。
- 测量条件：Apple Silicon 上的 colima 虚拟机（4 核），release，Tier 1，仓库在 docker 卷上，同一个会话里跑第二次；"之前"是这一轮开始时的 HEAD（351800b）在同样条件下的结果：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| `go build` hello，冷缓存 | 2.2s | 11.9s（5.4×） | 6.8–7.1s（3.2×） |
| Maven `package`，依赖已在本地 | 1.7s | 14.4s（8.5×） | 7.5s（4.4×） |
| Gradle `build`，守护进程已在运行 | 0.42s | 2.6s（6.2×） | 1.65–1.74s（4.0×） |
| `node -e 0` | 20 毫秒 | 100 毫秒（5.1×） | 58 毫秒（2.9×） |
| `git status`（5733 个文件），单线程 / 并行 lstat | 16 / 10 毫秒 | 86 / 72 毫秒 | 18 / 23 毫秒 |
| shell 起 600 个进程 | 0.15s | 0.39s（2.6×） | 0.33s（2.2×） |
| python 启动 5 次加循环 | 0.36s | 1.58s（4.4×） | 1.54s（4.3×） |
| gcc 编译 `lvm.c` | 0.69s | 2.32s（3.4×） | 2.35s（3.4×） |
| apk info 20 次 | 18.3s | 38.6s（2.1×） | 39.1s（2.1×） |

  `git status` 的"之前"一列是 virtiofs 上测的（本地卷上的 HEAD 没有单独测），其余都在本地卷上。
- **测试**：
  - 引擎测试新增：池里同一个键的实例保留翻译、核对后直接用，代码变了时作废重新翻译，`trim_dbt_pool` 之后重新翻译。
  - 差分测试 c-programs 组新增一个程序：4 个线程共用一个 fd 用文件位置并发读（每块恰好读到一次）和并发写（4000 条记录都在、互不覆盖），两个进程同时 O_APPEND 追加。
  - 全部 12 组差分测试在两个执行层下都和原生一致；共用翻译之后，java、node、go 三组又各重复了 3 遍。`go build` 重复 20 遍没有再卡住。
  - LTP 的 syscall 测试 942 个通过（上次 942），与上次相比只有 getrusage03 因为全量并发时可用内存不足 512 MB 跳过（单独运行通过），shmctl01（以前记录过的偶发失败）这次通过。读写不持锁之后，读写、偏移、mmap、截短相关的 245 个用例重跑，失败的 21 个都是上次就失败的（块设备、O_DIRECT 等）。

**推迟项**：

- JVM 负载共用翻译时仍比每个线程各一份慢 2–4%，锁已经不是原因，可能是代码局部性（所有线程的块交错在一起）。
- 并行 lstat 被 BKL 串行化：`git status` 开着并行 lstat 比单线程还慢。要不持 BKL 的 stat 路径（缓存全命中、不经过合成文件系统时），涉及 VFS、仓库缓存、fd 表、凭据的线程安全。→ 已补上（M9 之前：不持 BKL 的 stat），带 dirfd 的和 fstat 仍持 BKL
- fork 仍然整份复制碰过的页（shell 里 clone 占 14%），fork 之后马上 exec 时都白做了；写时复制（§5.3 的 v2）没做。大进程的 fork 持着 BKL 复制，期间别的进程的系统调用都要等。→ 已补上（M9 之前：fork 写时复制）
- SQLite 的自动检查点（WAL 满 1000 页时，要 fsync）持着 BKL 做。
- python 的解释循环仍是 5 倍，六成时间在 TLB 检查上；按访存点缓存上次的页只做了可行性统计（同一访存点连续落在同一页：python 93%、gcc 85%），没实现：共用代码缓存之后槽位的并发更新、换地址空间时的作废、跨页检查都麻烦，而且没命中时分支多半猜错。
- Go 程序的 x26（闭包上下文）、x27（汇编器临时寄存器）、x28（g）用得很多，正好是被占用的寄存器；可以按程序换一组。
- 池里保留的翻译最多 64 MB，App 还没有在 `onTrimMemory` 时调用 `Machine::trim_memory`（M9 的外壳）。

## M9 之前：不持 BKL 的 stat（2026-10-26）

上一轮的推迟项：并行的 lstat 被 BKL 串行化，`git status` 开着并行 lstat（git 的默认）比单线程还慢；`go build`、JVM、node 的线程池也会并行 stat。

- **做法**（§6、§8.1）：
  - 仓库的元数据缓存挪进 `store::Meta`（`Store` 和不持 BKL 的线程共用的 Arc）。持 BKL 的线程是唯一修改缓存的一方，修改时持有读写锁的写锁，自己读时不加锁；别的线程持读锁读。所以持 BKL 的路径没有多出加锁的开销，只有填缓存、作废时取写锁。
  - 只用缓存的路径解析（`vfs::Cached`）与 `Vfs::walk_in` 一一对应：不检查搜索权限（只给 root 用），进入挂载点或者缺了什么就返回 None，交回正常路径，由它填进缓存。还没处理的组件放在栈上的定长数组里，不分配内存；object 的名字也在栈上拼（`hal::stat_at` 改收 `&CStr`，原来每次都分配一个 CString）。普通文件在持读锁时向宿主 stat object：inode 编号删除后被重新使用时，结果不会一半来自旧文件。
  - 运行循环在 svc 退出之后、取 BKL 之前先试 `syscall::fast`：只处理 newfstatat 和 statx，要求凭据有 `CAP_DAC_READ_SEARCH` 或 `CAP_DAC_OVERRIDE`、`AT_FDCWD` 或绝对路径、没有在跟踪系统调用（`APPINE_STRACE`）、没有待作废的 `ic ivau`，参数不合法时也交回正常路径（由它报 `EINVAL`、`EFAULT`）。guest 内存经由 Cpu 绑定的页表访问（新加的 `Cpu::translate`），期间线程仍然标记着在执行 guest 代码，解除映射的线程等它用完再释放宿主内存。挂起的信号和 kick 留到下一次进入 guest 代码时处理：kick 的标志还在，`dbt_run` 立即退出。
  - 线程记下自己的根目录、当前目录和凭据（`Fast`），每次持 BKL 的系统调用结束时看一个全局计数变了没有，变了就重新记下。chdir、fchdir、chroot、unshare(`CLONE_FS`)、setuid 一类和 setuid 程序的 execve 让计数加一：当前目录由 `CLONE_FS` 的线程共享，凭据由线程组共享，都可能被别的线程改掉，过时的线程退回正常路径。
- 测量条件同上一轮（colima 4 核、本地卷、Tier 1、同一会话里第二次）。微基准 `target/mem/pstat.c`：每个线程对 8 个路径（含一个不存在的）轮流 lstat 20 万次；原生一侧的根目录是 docker 的 overlayfs，每一级都要经过它，appine 只对 objects 目录做一次单级 fstatat，所以单线程比原生还快，不完全可比：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| `git status`（5733 个文件），单线程 / 并行 lstat | 16 / 10 毫秒 | 18–19 / 24–25 毫秒 | 18 / 13–14 毫秒 |
| 并行 lstat 微基准，1 / 2 / 4 个线程，每次的微秒数 | 0.60 / 0.68 / 1.2 | 0.65 / 1.5 / 4.1 | 0.55 / 0.63 / 0.9 |
| `go build` hello，冷缓存 | 2.2s | 7.07s | 6.82–7.01s |
| Maven / Gradle 热构建 | 1.7 / 0.42s | 7.55 / 1.71s | 7.47 / 1.67–1.71s |
| shell 起 600 个进程；gcc 编译 `lvm.c`；python | 0.15 / 0.69 / 0.36s | 0.33 / 2.35 / 1.54s | 0.32 / 2.29 / 1.51s |

  单线程的 `git status` 没变：不持 BKL 的路径省下的取锁、建 `Cx`、克隆凭据，和宿主 fstatat（占这条路径的七成）相比不多。并行时 BKL 的自旋从 10% 降到 0.8%，futex 唤醒从 6.3% 降到 0.8%，剩下的原子操作（读写锁、引用计数）合计约 2%。
- **测试**：
  - 单元测试：用正常的路径解析填好缓存之后，只用缓存解析 20 条路径（符号链接、`..`、结尾的 `/`、`ELOOP`、`ENOTDIR`、不存在的名字），两个起点、跟随与否都和正常解析的结果完全相同；挂载点、没解析过的名字、删除之后都交回正常路径。
  - 差分测试 c-programs 组新增一个程序：主线程 2000 轮交替 chdir 和改名，每轮之后另一个线程马上用相对和绝对路径 stat，必须看到新的状态；另外两个线程不同步地一直 stat 改名中的文件，拿到的总是同一个 inode；setuid 到普通用户之后，另一个线程 stat 0700 目录里的文件得到 `EACCES`。分别去掉 chdir 和改凭据处的计数加一，这个测试都能发现。第一版没能发现：检查线程记下的旧当前目录里从没查过这些名字，缓存总不命中，结果总走正常路径；改成先在起始目录里查一次同名的文件。
  - cargo test、两层的 12 组差分测试都通过。LTP 的 syscall 测试 941 个通过（上次 942），只差 epoll_pwait03：计时精度测试，睡 1 毫秒实测中位数 1.6 毫秒，上一轮的二进制现在同样失败，是这次虚拟机里的计时，与改动无关。

**推迟项**：

- 带 dirfd 的 fstatat、fstat、readlinkat、faccessat 仍持 BKL：dirfd 和 fstat 要不持 BKL 读 fd 表（`Rc<RefCell<FdTable>>`），readlinkat、faccessat 可以照搬，但还没看到哪个负载并行地大量调用它们。
- 非 root 的进程仍持 BKL stat：要检查每一级目录的搜索权限，凭据也要能不持 BKL 读。
- `go build` 里 BKL 已经不是瓶颈（`dispatch` 合计 3.4%，锁争用 0.2%），86% 的时间在翻译后的代码里。

## M9 之前：fork 写时复制（2026-10-26）

上上轮的推迟项：fork 整份复制碰过的页。微基准 `target/mem/forkbench.c`（进程写过若干 MB 内存之后反复 fork，子进程立即 `_exit`，父进程 waitpid）显示 fork 的时间随写过的内存线性增长，100 MB 时 21 毫秒，原生 0.6 毫秒；复制期间持着 BKL，别的进程都要等，子进程还多占一倍内存。

- **写时复制**（§5.3）：写过的私有匿名映射 fork 时不复制，父子进程的页表都指向父进程的宿主页、去掉写权限，按页记账（拥有者借出，别的地址空间借用，fork 家族共用一张借出记录）。借用方写时复制一份到自己 VMA 里对应的位置；拥有者写、munmap、mremap、`MADV_DONTNEED` 之前先让借用方各自复制；拥有者销毁时页成了孤儿页留给借用方；最后一个借用方还回来时马上恢复拥有者的写权限。内核往 guest 内存写（`Mm::write`、`write_kernel`）、futex 的键、mprotect 都要照顾到。
  - 第一版跑 shell 负载时偶尔段错误：父进程写借出的页缺页之后等 BKL，这期间子进程 exec 还回了页、写权限已经恢复，拿到 BKL 一看不是写时复制的页，就当成真的段错误了。现在缺页时页表已经有这次访问要的权限就重新执行，与 Linux 一样（引擎报告的是出错的那一页，跨页时是后一页，不会白白重试）。
  - 逐页记账一开始很贵：100 MB 7.4 毫秒，其中 BTreeMap 逐个插入两成多、SipHash 一成多、每页一次 malloc。改成按地址顺序合并遍历、最后一次性建表，按页地址的乘法哈希，只有一个借用方时不分配，页表项按连续的段改，还回时按拥有者分组，降到 2.3 毫秒。
- **宿主线程池**（§6）：剖析只 fork 不 exec 的循环，`pthread_create` 占了四分之一（虚拟机里叫醒新线程要跨核中断，还要给内核栈分配 vmap 区）。现在 guest 线程结束之后它的宿主线程最多留 5 秒、最多留 8 个，给下一个 guest 线程用。CPU 时间改成从线程开始时的读数算起；亲和性在复用时总是重新设置；线程名每次设置。
- **线程结束的开销**：内核态时间原来读 `/proc/self/task/<tid>/stat`（线程结束时占五分之一），线程自己结束时改用 `getrusage(RUSAGE_THREAD)`。
- **唤醒再细分**（§6）：
  - 子进程的状态变化、宿主在等的进程结束、线程结束各用一个通道，wait4 只听子进程，`Machine::wait` 只听它在等的进程，定时器线程都不听。原来每个子进程结束都把定时器线程和宿主的等待线程叫醒。
  - 管道的读写、两端的打开和关闭只叫醒等这个管道的线程和没有指明对象的等待者（poll、epoll）；普通文件、目录关闭时不叫醒任何人，除非加过 flock、OFD 锁或者租约。shell 负载里唤醒占的 CPU 从 7.4% 降到 2.3%，CPU 总用量少了一成；墙钟时间没变，这些唤醒在别的核上。
- 测量条件同上一轮（colima 4 核、本地卷、Tier 1）。原生一侧的 fork 微基准跑在 docker 容器里：

| 负载 | 原生 | 之前 | 之后 |
|---|---|---|---|
| fork、子进程 `_exit`、waitpid，进程写过 0 MB | 72 微秒 | 118 微秒 | 90 微秒 |
| 同上，写过 1 MB | 72 微秒 | 349 微秒 | 116 微秒 |
| 同上，写过 10 MB | 131 微秒 | 2.3 毫秒 | 287 微秒 |
| 同上，写过 100 MB | 0.61 毫秒 | 21 毫秒 | 2.3 毫秒 |
| shell 起 600 个进程 | 0.15s | 0.32–0.33s | 0.30–0.31s |
| `git status`，单线程 / 并行 lstat | 16 / 10 毫秒 | 18 / 13–14 毫秒 | 16–17 / 13 毫秒 |
| `go build`；Maven；Gradle 热构建 | 2.2s；1.7s；0.42s | 6.8–7.0s；7.5s；1.67–1.71s | 7.0s；7.5s；1.60–1.68s |
| gcc 编译 `lvm.c`；python | 0.69s；0.36s | 2.33–2.35s；1.53–1.55s | 2.33–2.34s；1.51–1.53s |

  `go build`、JVM、python 的 subprocess 都用 vfork 或 `CLONE_VM`，不经过写时复制，持平；gcc 一行是与上上轮的二进制交替运行的结果。
- **测试**：
  - 单元测试新增三个：父子进程各写各的、孙进程向原来的拥有者借页、拥有者销毁之后的孤儿页、借用方解除映射时孙进程先复制；mprotect（包括 `PROT_NONE`）往返、`MADV_DONTNEED`、mremap 之后各写各的；借来的页取 futex 键时先换成自己的。
  - 差分测试新增 fork 组：4 个子进程和父进程同时写 8 MB；三个线程不停地按顺序写计数时反复 fork，子进程看到的是某一瞬间的一致快照、之后不受父进程的线程影响；借出期间父进程 munmap、mremap、`MADV_DONTNEED`、子进程 mprotect 往返；子进程里两个线程用互斥锁和条件变量交替 4000 次；7 层嵌套 fork；python 的 `os.fork`。让 fork 时不去掉父进程页表项的写权限，这一组就出错。第一版两边都因为缺 `_GNU_SOURCE` 编译失败、输出相同而"通过"了，看了实际输出才发现。
  - cargo test、两层的 13 组差分测试都通过。LTP 的 syscall 测试 943 个通过（上次 941，上上次 942）：与上上次相比 futex_wait05、getrusage03 变成通过；shmctl01 这次失败（全量并发时子进程还没 shmdt 完就超时），单独运行通过，用写时复制之前的二进制并发跑 shm 子集也会偶发同样的失败，是以前记录过的问题。上次失败的 epoll_pwait03 这次通过。

**推迟项**：

- 写过 100 MB 的进程 fork 仍是原生的 3.8 倍，时间在逐页记账上；可以按 VMA 的区间记，只在拆开、拷贝时再细分。
- fork 时子进程的私有匿名映射各自 mmap 一段宿主内存，exec 时再 munmap，占空进程 fork 的一成多：可以合成一次 mmap，或者推迟到第一次写。每个新线程仍然新建 Cpu（TLB 要 mmap）。→ 起进程的宿主开销：小映射合成一段，Cpu 留着复用。
- `/proc/<pid>/smaps` 的 Rss 没算借来的页（`/proc/<pid>/status`、getrusage 的驻留内存算了）。

## M9 之前：交错访存共用检查（2026-10-26）

目标是 python 的解释循环：`sum(i * i % 7 for i in range(3000000))` 原生 0.15 秒，appine 0.80 秒（5.3 倍），翻译后的代码里六成时间在 TLB 检查上，样本几乎都落在取表项的 `ldp` 上。

- **先量清楚检查贵在哪**（原生的微基准 `target/mem/mb/chain.c`，追指针、全在 L1 里）：裸的读 0.89 纳秒一次；加上现在的检查，所有访问都在同一页时 1.22 纳秒，访问的页在 8 或 16 页里随机变时 3.5 纳秒。同一页反复访问时 CPU 能预测取表项的地址、提前读出来，检查几乎不花时间；页一变，"guest 地址 → 加 7 → 取下标 → 算表项地址 → 取表项 → 读"整条链排在每次读的前面，多出约 7 个周期。python 的解释器正是一环扣一环的读。
- **没有采用的两个实验**：
  - 按访存点缓存上次的页（每个线程 1024 个槽，按 pc 直接映射，槽的地址与 guest 地址无关，命中时读只等 guest 地址）：python 循环只快了 5%（0.80 → 0.76 秒），gcc 反而慢了 43%（2.31 → 3.31 秒），python 整体也变慢。槽只在访存点的页不变时命中，而这时 CPU 本来就能预测表项的地址；页变化的访存点槽同样不命中，还要多走一个猜错的分支。每个读访存点多出约 26 个字的桩，gcc 这种冷代码多的负载翻译和代码缓存的开销都涨了。按翻译顺序分配槽（排除冲突）也一样。
  - 用第一个字节的页做下标、跨页检查挪到旁路（链上少一次加法）：微基准里 3.52 → 3.24 纳秒，真实负载上多出的两条指令更贵，python 循环反而 0.79 → 0.84 秒。
  - 只在微基准里试过：16 字节的表项（移位 4 位）没有差别；tag 和 addend 分成两个数组、按下标用寄存器偏移直接读 addend 能到 2.63 纳秒（快四分之一），但要让 x28 指向 addend 数组，cpu 结构体的字段只能放到 `ldur` 够不着的负偏移上，要么再让出一个宿主寄存器，这一轮没做。
- 之后又在真实负载上试了微基准里省得最多的两种（对 4a553bc 交替运行 3 轮）：让出 guest 的 x25，宿主 x25 指着另存的一份 addend 数组，按下标一条 `ldr` 取出（链上少了算表项地址那一步，指令多一条），python 循环、gcc 都没有可测的变化；再加上第一个字节做下标（链上只剩取下标、取 addend、读三步，指令多三条），python 循环 0.75 → 0.78 秒、gcc 2.27 → 2.33 秒。两条 ldr 分开取 tag 和 addend（不用 ldp）也没有收益。真实负载不是纯粹受延迟限制的追指针，检查多一条指令的代价比链上省下的周期大，现在的排法是指令最少的。
- **交错的访存也共用检查**（§4.2）：同一基址的组员之间原来不能有别的访存、不能有用到 T2 的指令，python 热块里 x7、x10 两个基址的访存互相交错，15 次检查一次也省不下。现在组长检查完把宿主基址存进 cpu 结构体的 `dbt_groups[槽]`（同时最多 4 组），被别的访存或者用 T2 的指令隔开的组员先从槽里取回来（一条 `ldr`，地址是常数，可以提前取出），组员的访问不必等 TLB 查表。一个块执行期间映射不会变，宿主基址在基址改变之前一直有效。那个热块的检查从 15 次降到 8 次，剩下的都是追指针的读。翻译时记下 T2 属于哪一组，没标"取回"的组员遇到 T2 不是自己组的就 abort（plan_group 的 bug）。
- **加载器**：为了判断段之间有没有共用页，原来把每个段的每一页放进 BTreeMap（busybox 加 ld-musl 四百来页，每次 execve 都做），现在按段的起止排序之后看相邻的两段，只在共用页、要逐页合并权限时才建那张表。
- A/B 测量（同一会话里 HEAD 和工作区交替运行 3 轮，`target/mem/ab.sh`）：

| 负载 | 原生 | HEAD | 之后 |
|---|---|---|---|
| python 循环 | 0.15s | 0.80–0.84s | 0.74–0.75s |
| python 启动 5 次加循环 | 0.36s | 1.52–1.53s | 1.46–1.51s |
| gcc 编译 `lvm.c` | 0.69s | 2.35–2.38s | 2.29–2.32s |
| shell 起 600 个进程 | 0.15s | 0.29s | 0.27–0.28s |
| `go build` 冷编译 | 2.2s | 6.90–6.99s | 6.65–6.74s |
| Maven 热构建 | 1.7s | 7.49–7.64s | 7.41–7.56s |

- **测试**：lockstep 新增 interleaved 类：两个基址（指向的范围可以重叠）交错的 4 条访存，中间可能夹一条用三个被占用寄存器的运算，30 万个用例与真实硬件一致，其中 26.7 万条访存走了分组；让组员总是从 0 号槽取回基址，第 2 个用例就报不一致。cargo test（170 个）、两层的 13 组差分测试都通过。LTP 的 syscall 测试 943 个通过，与上一轮相同（shmctl01 仍是以前记录过的全量并发时的偶发失败）。

**推迟项**：

- 追指针的读仍要走完整的检查链。缩短这条链的排法都多了指令，在真实负载上没有收益（见上）；要降低检查的代价只能减少检查的次数或者指令数。
- JVM 负载里块入口占翻译后代码的两成（Maven）。块入口的 kick 检查不能去掉（shootdown、停住全世界要靠它打断循环），这些样本多半是跳进新块时前端的开销，还没有办法单独量。
- shell 负载里 execve、退出时为 `ru_maxrss` 取驻留内存样本合计约占 4%（每个映射一次 mincore），fork 出来的子进程的样本在 Linux 上有语义（getrusage03 要求子进程与父进程相当），不能简单地省掉。→ 起进程的宿主开销：相邻的映射合起来量，文件映射的结果缓存。

## M9 之前：起进程的宿主开销（2026-10-26）

上一轮的推迟项：shell 负载里 execve、退出时为 `ru_maxrss` 取样约占 4%，fork 时子进程的私有映射各自 mmap、exec 时再 munmap，每个新线程新建 Cpu。剖析 `ls`、`cat | wc` 的循环发现宿主内核占了 46%（缺页、munmap、mmap、mincore、唤醒），为此写了微基准 `target/mem/spawnbench.c`：fork 之后 `_exit`、fork 之后 exec `/bin/true`、vfork 之后 exec、`posix_spawn`、`posix_spawn` 一个 `sh -c 'cat /etc/passwd | wc -l'`，原生一侧在 Alpine 容器里跑同一份源码。

- **放开 BKL 之后再叫醒、再交还内存**（§6、§5.2）：
  - 释放 BKL 时叫醒等待者原来在放开之前：子进程退出时叫醒等着的 shell，它醒来马上去拿 BKL，发现还被占着又睡下，等这边放开再被叫醒一次（剖析里 wait4 醒来之后的 `lock_contended` 占了一成的上下文切换）。现在等待者的 eventfd、FUTEX_WAKE、信号、宿主线程池里接手新线程的空闲线程都攒到放开 BKL 之后再叫醒。
  - 持有 BKL 时释放的宿主内存（进程退出、execve 销毁地址空间，munmap）放开之后再交还，首尾相接的合起来交还：munmap 次数从每轮 62 次降到 19 次，拆页表、刷 TLB 也不再让别人等 BKL。OOM killer 看到还有没交还完的内存时这一轮不挑进程。
  - 没有采用：交给后台线程 munmap。shell 负载快了 3%，fork、exec 的微基准反而慢了 10%–20%：后台线程的 munmap 与 fork 时的 mmap 在宿主的 `mmap_lock` 上争抢，每次叫醒后台线程还要一次跨核中断。
- **驻留内存取样**（§6）：每个 VMA 一次 mincore（还要分配一个数组），文件映射里没映射的页要在 page cache 里逐页查找。现在宿主上首尾相接的 VMA 合起来量一次（execve 时先后建立的映射大多挨着）；只有 page cache 页的文件映射（没写过的私有映射、共享映射）驻留多少与哪个进程无关，结果按文件的一段缓存 1 秒。mincore 每轮从 72 次降到 25 次。只在取样时少算了这一项（ablation）的话，exec 微基准快 6%、管道快 7%。
- **翻译池的核对**（§4.2）：池里 busybox 的翻译攒着 sh、ls、cat、wc 各个 applet 的块，每次 exec 第一次运行前都要逐块比对 guest 指令字，代码页还要逐页缺页（管道负载里 memcmp 占 7.7%，加上它引起的缺页约一成）。现在地址空间不再使用时把没写过、有可信版本的私有文件映射记给翻译，新的地址空间里同一段地址是同一个版本的同一段文件时，里面的块不再比对。版本是宿主 object 的 ctime 和大小，ctime 离现在不到 2 秒的、有过可写共享映射的文件不给版本。
- **execve 读程序头**：原来把整个程序文件映射进来解析 ELF，程序和解释器各多一次 mmap、缺页、munmap；现在 pread 开头 4 KB，不够时再 pread。
- **Cpu 结构复用**：每个线程的 Cpu（含 4096 项的 TLB，约 130 KB）原来新映射一段、逐页缺页、线程结束时拆掉，现在留 8 个给下一个线程，只清掉填过的 TLB 项。
- **只读文件映射共用宿主映射**（§5.2）：有可信版本的只读私有映射（程序和库的代码段）所有地址空间共用一份宿主映射，宿主页表还在，不必重新映射、缺页。宿主上是只读映射，用它的 VMA 要写之前先复制（mprotect 加写权限，加载器给只读段清零 .bss 的开头），漏了会直接出错而不是悄悄改了别人的代码。缺页每轮从 174 次降到 69 次（与上一条、取样的改动合计）。
- **fork 的小映射合成一段**（§5.2）：子进程自己的 1 MB 以内的映射（堆、栈除外）切自同一段宿主内存，要计入提交量的和不计入的各一段。代价是这样的映射解除映射之后宿主内存要等整段都不用了才交还。
- **execve 放开 BKL 建地址空间**（§6）：给 BKL 的持有时间按 syscall 记账（临时插桩），管道负载里 execve 持锁最多，每次约 38 微秒，其次是 clone 和 exit_group。加载器拆成两半：路径解析、打开程序和解释器、检查格式在锁里做；映射各段、构造初始栈在锁外做，只碰新的地址空间和共用的只读文件映射（宿主内存的引用计数改成 Arc，`FileMaps` 自带锁）。回来时线程已经被要求退出（别的线程 exit_group、execve）就放弃。
- A/B 测量（同一会话里 HEAD 和工作区交替运行 3 轮；`target/mem/spawnab.sh`、`shellab.sh`、`ab.sh`）：

| 负载 | 原生 | HEAD | 之后 |
|---|---|---|---|
| fork、子进程 `_exit`、waitpid | 62 微秒 | 78–92 微秒 | 71–75 微秒 |
| fork 之后 exec `/bin/true` | 154 微秒 | 221–232 微秒 | 162–165 微秒 |
| vfork 之后 exec | 130 微秒 | 198–215 微秒 | 145–151 微秒 |
| `posix_spawn` | 131 微秒 | 206–223 微秒 | 155–156 微秒 |
| `posix_spawn` 一个 `sh -c 'cat \| wc'` | 393 微秒 | 825–911 微秒 | 535–543 微秒 |
| shell 起 600 个进程 | 0.15s | 0.27–0.28s | 0.20–0.22s |
| `go build` 冷编译；Maven 热构建 | 2.2s；1.7s | 6.6–6.8s；7.25–7.3s | 6.5–6.55s；6.9–7.0s |
| gcc 编译 `lvm.c`；python | 0.69s；0.36s | 2.24–2.31s；1.40–1.45s | 2.27–2.28s；1.43–1.45s |

- **测试**：
  - 差分测试新增 rewrite 组：同一个 inode 就地换了代码（`cat >`、`dd conv=notrunc`、可写的共享映射），之后执行的必须是新代码，改写前后等 3 秒，让翻译池复用上一次的翻译、不再核对。让版本比较时忽略 ctime 和大小，这一组就输出旧代码的结果。
  - cargo test（170 个）、两层的 14 组差分测试、gcc 13 编译 C、Android clippy 都通过（Android 上的 clippy 把已经带 const 的 `thread_local!` 报成可以加 const，按项关掉了这个 lint）。LTP 的 syscall 测试 943 个通过，与上一轮相同：shmctl01 这次通过，futex_wait05 失败（睡 1 毫秒实际睡了 1.6 毫秒，HEAD 的二进制这时单独跑也一样，是虚拟机这时的计时问题）。
  - 跑了 16 次全量 cargo test：一次卡在 `ctrl_c_and_ctrl_z_control_the_foreground_job`（Tier 1，作业停着、shell 在等输入，测试在等它退出），重放这个用例 40 次（6 个并发）、之后 15 次全量都没再出现，原因还没查明。一次 `ipc_timers_credentials_and_scheduling` 失败，HEAD 的二进制单独跑这个程序 30 次也失败 1 次：解除屏蔽、送达挂着的定时器信号之后，检查之前又到期了一次（周期只有 10 毫秒），检查的是最后一次送达的超限次数。改成只看解除屏蔽之后送达的第一个，之后连跑 20 次都通过。

**推迟项**：

- 管道负载仍是原生的 1.4 倍：两头的进程各自 execve、退出，BKL 上仍有排队（exit_group、clone 的持锁时间次于 execve），宿主内核里的缺页、munmap 合计还占三成。进程退出时宿主是一段段解除映射（逐页刷 TLB），Linux 销毁整个地址空间时只刷一次，这部分省不掉，除非 guest 进程的匿名内存来自更大的整块。→ BKL 的等待和持锁：管道 1.15 倍。
- python 启动（`import json, re, subprocess, asyncio`）没有变化，时间在 guest 代码和读文件上。
- 上面那次 PTY 用例卡住的原因。

## M9 之前：BKL 的等待和持锁（2026-10-26）

上一轮的推迟项：管道负载里 BKL 上还有排队。先量了几个进程同时起进程的扩展性：guest 里 4 个 spawnbench 同时 fork 后 exec，每个进程每轮 690 微秒，总吞吐与只有一个时（166 微秒）一样；原生 4 个同时跑每轮 180 微秒，接近 4 倍。按 syscall 记 BKL 的持有和等待时间（临时插桩，`target/mem/bklinstr.py`）：一次起进程持锁约 80 微秒，execve 27、clone 22、exit_group 20，锁一半以上的时间被占着；上下文切换里 57% 是等 BKL。另外量了真实的并行构建：guest 里用 `xargs -P4` 编译 Lua 的 33 个 .c，比 `-P1` 快 2.45 倍（原生 2.7 倍），编译为主的负载受 BKL 影响不大。

- **自旋**（§6）：原来每个等锁的线程自旋 100 次（不到 1 微秒）就睡下，持锁的线程放开时它们都已经睡着，叫醒它们的这段时间锁空着。改成按时间自旋：
  - 自旋 10 到 60 微秒效果差不多，120 微秒反而变差（自旋的线程占了持锁线程的核），不自旋最差。取 30 微秒；读时钟比自旋一圈贵得多，每 64 圈看一次（第一版每圈都读，光读时钟就占了总样本的 15%）。
  - 自旋时只读"有人持有"的标志，看起来空了才去抢。
  - 所有等锁的线程都自旋时，自旋占了 28% 的 CPU 样本；改成同时只有一个线程自旋、其余直接睡下，墙钟时间的收益一样，自旋降到 16%。
- **fork 的宿主映射预先建好**（§5.2）：小映射切出来的那一段、堆和栈预留的地址空间都是 `MAP_NORESERVE`、没碰过的内存，同一个程序反复 fork 时长度总是那几个。预先映射好，持锁时直接拿来用，用掉的放开 BKL 之后补上。小映射合成的那一段也不再分计入、不计入提交量两段。
- **小映射不读 pagemap**（§5.3）：16 页以内写过的匿名映射 fork 时全部借出，不为它们读 `/proc/self/pagemap`。
- **页表放开 BKL 之后释放**（§5.2）：销毁的地址空间的页表（逐个释放中间表）和宿主内存一起放到放开 BKL 之后。
- **execve 记着程序文件**（§6）：最近执行过的 32 个程序文件，打开着的宿主文件和读进来的开头留着，同一个版本（宿主 ctime 和大小，同翻译池的判断）再执行时直接用，省掉打开、fstat、pread、关闭。
- **没有采用：放开 BKL 之后再取驻留内存样本**。execve 时持锁时间里最长的是 mincore 取样（等宿主的 `mmap_lock`），换掉的地址空间要等放开 BKL 之后才交还宿主内存，样本可以在那之后、交还之前量；退出时的样本加了"还没量完"的计数，回收它的父进程先放开 BKL 等它量完。持锁时间 execve 从 23 降到 15 微秒，但 4 路并行起进程反而从 530 慢到 650 微秒：锁外的 mincore 与别的线程交还内存的 munmap 在 `mmap_lock` 上争抢，持锁时同一时刻最多只有一个 mincore。撤回。
- A/B 测量（同一会话里 HEAD 和工作区交替运行；`target/mem/parab.sh`、`par4ab.sh`、`ab.sh`）：

| 负载 | 原生 | HEAD | 之后 |
|---|---|---|---|
| 4 个进程同时 fork 后 exec，每个每轮 | 180 微秒 | 640–680 微秒 | 455–505 微秒 |
| 4 个进程同时 `posix_spawn` 一个 `sh -c 'cat \| wc'`，每个每轮 | — | 2.55–2.85 毫秒 | 2.17–2.40 毫秒 |
| `posix_spawn` 一个 `sh -c 'cat \| wc'` | 393 微秒 | 525–555 微秒 | 434–468 微秒 |
| fork 后 exec `/bin/true` | 154 微秒 | 154–166 微秒 | 147–160 微秒 |
| shell 起 600 个进程 | 0.15s | 0.21–0.22s | 0.18–0.20s |
| `git status` 单线程 / 并行 lstat | 16 / 10 毫秒 | 16–17 / 11–14 毫秒 | 16 / 12–13 毫秒 |
| `xargs -P4` 编译 Lua；`go build`；Maven | 1.68s；2.2s；1.7s | 6.47s；6.53–6.56s；6.89–7.04s | 6.41s；6.50–6.57s；6.92–7.03s |

- **测试**：cargo test（170 个）、两层的 14 组差分测试、Android clippy 都通过。LTP 的 syscall 测试 944 个通过（上一轮 943）：futex_wait05 这次通过，shmctl01 也通过。在 guest 里就地改写程序之后再执行（rewrite 组）照样执行新代码，程序文件的缓存按版本失效。

**推迟项**：

- 4 个进程同时起进程仍是原生总吞吐的 2.7 分之一：每次起进程持锁约 60 微秒（execve、clone、exit_group 各约 20），其中 execve 和退出时的驻留内存取样、fork 时复制可写的私有文件映射（数据段）引起的缺页、页表的建立占了大半。取样放到锁外反而更慢（见上），要么让取样不靠 mincore（页表记脏位、增量计数），要么把进程的生命周期拆出 BKL。
- 可写的私有文件映射（程序的数据段）fork 时仍然逐页复制，可以像私有匿名内存一样写时复制。
