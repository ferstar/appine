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
2. 同一基本块内，对同一页的 TLB 检查外提。
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
- `MREMAP_DONTUNMAP` 不支持；私有文件映射不能用 mremap 扩展。→ `MREMAP_DONTUNMAP` 已补上（M9 之前：兼容性补漏、推迟项）
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
- 内核访问 guest 内存（比如 read 写进还没长出来的栈）不会让 `MAP_GROWSDOWN` 的映射增长，返回 EFAULT。
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

- 引擎故障在 App 进程里的恢复（要经过 ART 的 libsigchain）没有直接测过：Android 上验证了 bionic 下的恢复（模拟器上直接运行引擎测试），以及 App 装上处理函数之后 ART 照常工作；libsigchain 把 ART 不认领的故障转给我们的处理函数，`siglongjmp` 离开它时它没有要恢复的状态（按它的源码）。
- 被结束的进程只在宿主 stderr（Android 上是 logcat）里留下报告，终端里只看到 shell 打印的 "Segmentation fault" 或 "Killed"。App 可以在标签页里提示，留给 M9 的外壳。
- 内存上限只看 App 自己：系统整体内存紧张时（`onTrimMemory`）还没有接到 OOM killer 上。
- `oom_score_adj` 的写入不检查权限：Linux 不让没有 `CAP_SYS_RESOURCE` 的进程把它调到 `oom_score_adj_min` 以下。
- 线程开始、退出时的 panic 仍然终止宿主进程；定时器线程、`Machine` 方法（JNI 调用）里的 panic 还没有处理。
