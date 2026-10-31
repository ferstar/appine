# M9 之前：未实现的系统调用（2026-10-31）

← [M9 之前：LRCPC2（2026-10-31）](138-lrcpc2.md) · [索引](../milestones.md)

第 138 轮之后全量核对推迟项时，拿分发表（`core/kernel/src/syscall/mod.rs`）对照 Linux 6.1 的系统调用表（`include/uapi/asm-generic/unistd.h`），发现一批返回 ENOSYS 的编号没有写进任何一篇。这一轮只记下来，不改代码。

已经记过的不再列：io_uring（[推迟项](19-deferred.md)、§15），rseq、mbind、landlock、fchmodat2（[glibc 发行版的探测](69-glibc-probe.md)），get_mempolicy（[负载普查](62-workload-survey.md)），fanotify（[M6](08-m6.md)、[M8 补测](11-m8-jvm.md)），新的挂载接口（[mount 的子集](105-mount-subset.md)、§8.2）。arm64 上本来就没有的编号也不算：244–259 是留给各个架构的，295–402 没有分配，403–423 只给 32 位。

## 怎么核对的

- Linux 6.1 的 `kernel/sys_ni.c` 里列为 `COND_SYSCALL` 的，要配了对应的选项才编进内核，没配时 Linux 也返回 ENOSYS。下面按这一点分成两组。
- 写了一个程序逐个调用（参数都不改状态：只读的 adjtimex、对 pid 1 的 ioprio_get、`SYSLOG_ACTION_SIZE_BUFFER`、fd 无效的 process_madvise 等），在 appine 和原生（colima 的 6.8 内核，Alpine 容器，带和不带 docker 默认的 seccomp 各一次）里各跑一遍。appine 里全是 ENOSYS。
- 在 appine 里（差分测试的仓库）跑用得到它们的命令：
  - busybox 的 `dmesg`：`klogctl: Function not implemented`。原生容器里没有 CAP_SYSLOG，是 EPERM。
  - busybox 的 `adjtimex`：`Function not implemented`。原生打印时钟的状态。
  - `ionice -c3 true`、`ionice -p 1`：`ioprio_set failed`、`ioprio_get failed: Function not implemented`。原生正常。
- 历次普查（第 62、69、70、92、103 轮）报告的未实现的系统调用里没有这些编号。
- 原生容器带着 docker 默认的 seccomp 时，keyctl、bpf、perf_event_open、userfaultfd、acct、process_madvise 这些也都失败（EPERM），在容器里跑的程序本来就会碰到这些调用失败。

## 与 Linux 6.1 不一样的

Linux 6.1 上一般都有，appine 是 ENOSYS：

| 编号 | 系统调用 | Linux 6.1 | 原生（6.8，不带 seccomp） |
|---|---|---|---|
| 437 | openat2 | 5.6 起总有 | 成功 |
| 171、266 | adjtimex、clock_adjtime | 总有；只读的查询（modes 为 0）谁都能调 | 成功 |
| 30、31 | ioprio_set、ioprio_get | 有块设备层（`CONFIG_BLOCK`）就有 | 成功 |
| 116 | syslog | 有 printk 就有 | 要 CAP_SYSLOG，容器里是 EPERM |
| 440 | process_madvise | 有 MMU、`CONFIG_ADVISE_SYSCALLS`（默认开）就有 | fd 无效时 EBADF |
| 448 | process_mrelease | 有 MMU 就有 | fd 无效时 EBADF |

## 与没配对应功能的 Linux 6.1 一样的

这些属于宿主内核的事，或者 appine 里没有对应的东西，返回 ENOSYS 与没配这些选项的 Linux 相同：

| 编号 | 系统调用 | 说明 |
|---|---|---|
| 18 | lookup_dcookie | 只给 oprofile 用，6.8 删掉了，原生（6.8）也是 ENOSYS |
| 42 | nfsservctl | Linux 3.1 起总是 ENOSYS，原生一样 |
| 60、443 | quotactl、quotactl_fd | 磁盘配额，appine 没有块设备 |
| 89 | acct | 进程记账（`CONFIG_BSD_PROCESS_ACCT`） |
| 104、294、105、273、106 | kexec_load、kexec_file_load、init_module、finit_module、delete_module | 换内核、装卸内核模块 |
| 217–219 | add_key、request_key、keyctl | 内核密钥环（`CONFIG_KEYS`） |
| 224、225 | swapon、swapoff | 交换区（`CONFIG_SWAP`），guest 的内存由宿主管 |
| 237–239、450 | set_mempolicy、migrate_pages、move_pages、set_mempolicy_home_node | NUMA（`CONFIG_NUMA`），同一组的 mbind、get_mempolicy 以前记过 |
| 241 | perf_event_open | 性能计数器（`CONFIG_PERF_EVENTS`），硬件计数器在宿主上 |
| 280 | bpf | eBPF（`CONFIG_BPF_SYSCALL`） |
| 282 | userfaultfd | `CONFIG_USERFAULTFD`，[M6](08-m6.md) 的 LTP 失败里提过 |
| 288–290 | pkey_mprotect、pkey_alloc、pkey_free | arm64 的 6.1 没有内存保护键，原生（6.8）也是 ENOSYS |
| 447 | memfd_secret | `CONFIG_SECRETMEM` |

452 起的编号都是 6.1 之后才有的，ENOSYS 与 appine 报告的 6.1 一致（fchmodat2 以前记过）。6.5 的 cachestat（451）是例外，已经实现了（§7.5）。

**推迟项**：

- openat2：`RESOLVE_*` 的几种限制（不越出起点、把起点当作根、不跟随符号链接、不跨挂载、不跟随魔法链接）要在路径解析里加上。
- adjtimex、clock_adjtime：至少可以回答只读的查询。改时钟与 settimeofday 一样，宿主的时间 guest 改不了。
- ioprio_set、ioprio_get：appine 没有 I/O 调度，可以只记下、读回优先级，或者像 nice 那样交给宿主线程（[推迟项](19-deferred.md)那一轮）。
- syslog：appine 没有内核日志，可以按 Linux 的权限检查（`dmesg_restrict`、CAP_SYSLOG）回答一份空的日志。
- process_madvise、process_mrelease：用得少，有程序要时再做。
- 上面第二组不做，与没配对应功能的 Linux 一样。
