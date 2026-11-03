# 7. 信号与 ABI

## 7.1 信号帧

信号帧**逐字节照抄 Linux arm64 的布局**：

- `siginfo` + `ucontext`；`sigcontext` 包含 `fault_address`、`regs[31]`、`sp`、`pc`、`pstate`（NZCV 和 DIT），后跟 `__reserved` 区里的上下文记录：每次都带 `fpsimd_context`；只在同步异常时带 `esr_context`（EC、WnR、DFSC 由我们合成）；以一个空记录结尾。**不带** `sve_context`/`za_context`。
- 帧下方压入 `frame_record {fp, lr}`，整体按 16 字节对齐。`AT_MINSIGSTKSZ` 报告 4720，与没有 SVE/SME 的 arm64 Linux 相同。
- 完整支持 `sigaltstack`。返回地址：设置了 `SA_RESTORER` 时用 `sa_restorer`，否则指向 vDSO 里的 `__kernel_rt_sigreturn`。

## 7.2 vDSO

- 一个最小但真实的 ELF 镜像（`core/kernel/src/vdso.rs`，运行时按字节拼出），通过 `AT_SYSINFO_EHDR` 交给 guest。只有一页，导出 `LINUX_2.6.39` 版本的符号，每个进程映射一份私有拷贝（调试器可以在里面下断点）。
- `__kernel_rt_sigreturn` **必须恰好是 `mov x8, #139; svc #0` 这两条指令**，libgcc 的 unwinder 靠逐字节匹配它们来识别信号帧。
- 时间函数只写成 `svc` 存根，进内核之后不取 BKL（[kernel/process 的 map](../../.claude/maps/kernel/process.md)）。v1.x 的快速路径（未实现）：guest 代码直接读 `CNTVCT_EL0`，配合一个由我们更新的 vvar 页。

## 7.3 投递精度

| 类型 | 规则 |
|---|---|
| 访存出错（SIGSEGV/SIGBUS） | pc 指向出错的那条指令；`si_addr` 是精确的 guest 地址；`si_code` 区分 `SEGV_MAPERR`/`SEGV_ACCERR`；基址寄存器的回写不能提前生效 |
| `brk #imm` | SIGTRAP，`TRAP_BRKPT`；信号帧里没有 `esr_context` |
| 对齐错误 | SIGBUS，`BUS_ADRALN`；ESR 的 EC 按数据访问（0x24）、SP 未对齐（0x26）、PC 未对齐（0x22）区分 |
| `udf` 和白名单外的编码 | SIGILL，`ILL_ILLOPC` |
| 异步信号 | kick 置位 CPU 上下文里的 pending 标志：解释器在指令边界检查，DBT 在经由后向边或间接分支进入块时检查（[engine 的 map](../../.claude/maps/engine.md)）。引擎随即退出，由内核层投递 |

- `rt_sigreturn` 恢复全部通用寄存器和 FPSIMD 寄存器，校验每个记录的 magic 和 size。信号处理函数**可能修改 pc**（Go 的异步抢占、V8 的 wasm trap），所以恢复后引擎按新的 pc 重新查找翻译。
- syscall 重启语义照搬 Linux（`ERESTARTSYS` 等四种，配合 `restart_syscall`），像 Linux 那样**另外保存一份 `orig_x0`**；各个系统调用用哪一种也照 Linux。

## 7.4 宿主侧信号

宿主侧要处理的信号用普通 `sigaction` 安装，libsigchain 会把我们的处理函数链在 ART 自己的处理函数之后。

- **越过 EOF 的 SIGBUS**：都事先避开了（[kernel/mm 的 map](../../.claude/maps/kernel/mm.md)），仓库的 object 文件只有 appine 自己会改。
- **引擎的 bug**（`core/engine/src/c/guard.c`）：引擎装了 SIGSEGV、SIGBUS、SIGILL 的处理函数。引擎运行期间出了故障，处理函数 `siglongjmp` 回运行函数，由它返回 `APPINE_EXIT_HOST_FAULT`，内核层只结束这个 guest 进程（§12 第 5 条）。为此运行期间引擎不调用 malloc、不回调内核层，拿到的锁都登记下来，跳回之后释放；用过的 Tier 1 实例直接释放，翻译模板整体丢弃。App 进程里 ART 的处理函数不认领的故障才交给我们。
- 其余的故障先往 stderr 写一行报告，再交给原来的处理函数（Android 上是 debuggerd）。
- 打断 guest 线程不用宿主信号：kick 只是置位 pending 标志（§7.3），等待中的线程经由 eventfd 唤醒（[kernel/process 的 map](../../.claude/maps/kernel/process.md)）。

## 7.5 auxv 与内核版本

- auxv 依次提供：`AT_SYSINFO_EHDR`、`AT_MINSIGSTKSZ`、`AT_HWCAP`（[engine 的 map](../../.claude/maps/engine.md)）、`AT_PAGESZ=4096`、`AT_CLKTCK=100`、`AT_PHDR`/`AT_PHENT`/`AT_PHNUM`、`AT_BASE`、`AT_FLAGS`、`AT_ENTRY`、`AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID`、`AT_SECURE`、`AT_RANDOM`、`AT_HWCAP2`、`AT_EXECFN`、`AT_PLATFORM="aarch64"`。
- `uname -r` 固定报告 `6.1.0-appine`，当作一份 **ABI 契约**：没写明的地方都按 Linux 6.1 的语义实现。比 6.1 新的功能一般不做，例外是 6.5 的 `cachestat`、6.12 起 `/proc/<pid>/fd` 里的项不能当挂载点、6.2 起 time 命名空间的放宽（原生对照的内核都比 6.1 新）。
- 未实现的 syscall **一律返回 `ENOSYS`，绝不崩溃**，同时记录一条本地调试日志（`APPINE_LOG=1`）。有意不实现的：io_uring（§15）。别的返回 `ENOSYS` 的编号见[未实现的系统调用](../rounds/139-unimplemented-syscalls.md)。
- guest 改不了宿主的时间：settimeofday、clock_settime 和要改东西的 adjtimex 是 `EPERM`（当作没有 `CAP_SYS_TIME`）；adjtimex 的查询取宿主的 NTP 状态，Android 上按未同步合成。

## 7.6 ptrace

照 Linux 6.1 实现，strace（含 `--seccomp-bpf`）、gdb、delve、lldb、valgrind 能用。与 Linux 不同或者 appine 自己的地方：

- 被跟踪的是线程，跟踪者记的是进程：Linux 只接受跟踪者线程自己发的请求，appine 放宽到跟踪者进程里的任何线程。没有 Yama 的限制。跟踪 pid 1 是 ESRCH（内置的 init 不是 guest 进程，[kernel/process 的 map](../../.claude/maps/kernel/process.md)）。
- 停在系统调用上时把寄存器存一份，跟踪者读写的是这一份，继续时写回。PTRACE_SYSCALL 让线程继续之后，它的系统调用都交给持有 BKL 的路径（[kernel/process 的 map](../../.claude/maps/kernel/process.md) 的快速路径不走），vDSO 里读时钟的存根不停，与 Linux 上它们不进内核一样。
- regset：`NT_PRSTATUS`、`NT_PRFPREG`、`NT_ARM_TLS`、`NT_ARM_SYSTEM_CALL`、`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH`；没有 SVE、PAC、MTE、SME 的。
- 单步由解释器执行一条指令（`appine_cpu_step`）。
- 改写被跟踪进程的内存（POKETEXT、`/proc/<pid>/mem`）照 `FOLL_FORCE`：没有写权限的私有映射先复制一份再写。写过的地方作废翻译（[engine 的 map](../../.claude/maps/engine.md)），写过的映射不再算作可信的来源。
- **硬件断点和观察点**：报告 6 个断点、4 个观察点的槽，只有解释器检查。有断点的线程只用 Tier 0；有观察点的线程照常用 Tier 1，被观察的页不进它的数据 TLB，这些访问交给 Tier 0 执行那一条指令，由它检查观察点。

## 7.7 core dump

照 Linux 6.1：默认动作是生成 core 的信号结束进程时，`RLIMIT_CORE` 不小于一页、进程可以 dump 才写；文件名按 `core_pattern`（不支持管道形式）和 `core_uses_pid`；格式与 arm64 Linux 写的相同（`PT_LOAD` 按 `coredump_filter`，`PT_NOTE` 里每个线程的寄存器、进程的 `NT_PRPSINFO`、`NT_SIGINFO`、`NT_AUXV`、`NT_FILE`），gdb 可以事后分析（`coredump.rs`）。先结束其他线程并等它们退出，它们退出时把寄存器留在线程组里。

## 7.8 内核日志

- 内核层有一份自己的日志（`core/kernel/src/klog.rs`），与 Linux 6.1 的 printk 环形缓冲区一样：128 KB、最多 4096 条，满了丢掉最老的。宿主内核的日志拿不到，不在里面。
- 记进去的：guest 写 `/dev/kmsg` 的；OOM killer 结束进程（§12 第 4 条）；内核层、引擎的 bug 结束进程（§12 第 5 条）；低端口换了端口（§10）。
- 读的接口与 Linux 相同：`/dev/kmsg`、syslog(2)（`dmesg`）、`/proc/kmsg`，权限按 `dmesg_restrict`、`CAP_SYSLOG`。
