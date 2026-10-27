# M9 之前：cgroup v2 的视图（2026-10-27）

← [M9 之前：跟踪停住的进程、跟踪者先取走结束（2026-10-27）](57-ptrace-stopped-reaped.md) · [索引](../milestones.md) · [M9 之前：seccomp（2026-10-28）](59-seccomp.md) →

运行时按物理内存定堆的大小：JVM 的默认最大堆是物理内存的 1/4，Node.js 按物理内存给 V8 定上限。但 guest 能用的只有 OOM killer 的上限，默认是物理内存的一半（§12 第 4 条），几个运行时一起用满时，就有进程被结束。它们在容器里改按 cgroup 的 `memory.max` 算：JVM 的容器支持（UseContainerSupport），Node.js 经由 libuv 的 `uv_get_constrained_memory`。原来 appine 没有 cgroup，JVM 报告"No metrics available"。

- **一个只读的 cgroup v2**（`cgroup.rs`）：所有 guest 进程在同一个 cgroup 里，看起来与 docker 用 cgroup 命名空间的容器一样（§8.2）。
  - `/proc/<pid>/cgroup` 是 `0::/`。`/proc/cgroups` 列出 cpuset、cpu、cpuacct、memory、pids，层级都是 0，JVM 据此认出 cgroup v2。
  - cgroup2 只读地挂在 `/sys/fs/cgroup`，mountinfo、`/proc/mounts` 的那一行与 docker 的相同（选项 `nsdelegate,memory_recursiveprot`），statfs 是 `CGROUP2_SUPER_MAGIC`。`/proc/filesystems` 加上 cgroup2。
  - 写文件、建文件和子 cgroup、删除、改权限都是 EROFS，与 docker 默认的挂载一样。
- **文件**（37 个，权限与 Linux 相同）：
  - cgroup.*：controllers 是 `cpuset cpu memory pids`，procs、threads 列出 guest 的进程和线程，type 是 domain，events、freeze、max.depth、max.descendants、stat、subtree_control 是没有子 cgroup 时的值。
  - memory.*：
    - `memory.max` 是 OOM killer 的上限（不限制时是 max）；
    - `memory.current` 是它量的 App 的内存，`memory.peak` 是量到过的最大值；
    - 换出到 zram 的内存已经算在 current 里，所以 `memory.swap.max` 是 0；
    - `memory.events` 的 max、oom、oom_kill 是超出上限、因此要挑进程、结束了进程的次数（OOM killer 新记的计数）；
    - min、low、high、oom.group、swap.current、swap.high、swap.events、events.local 是不限制时的值。
  - cpu.*：没有配额和权重，`cpu.max` 是 `max 100000`，weight 100；`cpu.stat` 是所有 guest 线程用掉的 CPU 时间，按 6.1 的格式。
  - cpuset.*：effective 是 guest 看到的所有 CPU 和内存节点 0，cpus、mems 是空的，partition 是 member。
  - pids.*：current 是 guest 的线程数，max 不限（线程数的限制在 threads-max 和 `RLIMIT_NPROC`），events 是 `max 0`。
- **效果**：colima 的虚拟机（3.8 GiB）上，默认上限 1.91 GiB。

  | | 之前 | 之后 |
  |---|---|---|
  | JVM 的默认最大堆 | 978 MiB | 490 MiB |
  | Node.js 的 V8 堆上限 | 2.09 GiB | 1.05 GiB |

  之前 Node.js 的堆上限比 guest 能用的还大。`java -XshowSettings:system` 报告 `Provider: cgroupv2`，Memory Limit 是上限，`process.constrainedMemory()` 也是。
- **测试**：新的差分测试 `cgroup`（第 19 组），比对：
  - `/proc/<pid>/cgroup`、`/proc/cgroups`（只比列出的控制器都在层级 0 上、有 cpu：新内核不列没有 v1 支持的 cpuset、memory，CI 的 runner 内核版本不一）、挂载的那一行和 statfs；
  - 与宿主无关的文件的权限和内容，取决于宿主的文件（memory.max、pids.max、用量、pid）的权限和格式。CI 的宿主内核比 6.1 新：cgroup.stat、memory.events、cpu.stat 多了项（`nr_subsys_*`、`sock_throttled`、`nice_usec`），`memory.peak` 可写（6.12 起），容器的 `pids.max` 有限制。这些只比 6.1 就有的部分；
  - 写、建、删、改权限都是 EROFS；
  - JVM（`OperatingSystemMXBean.getTotalMemorySize`）、Node.js（`process.constrainedMemory`）看到的上限与 `memory.max` 一致：原生一侧 docker 没有设 `--memory`，`memory.max` 是 max，两者都是物理内存。

  rootfs 测试新增 `cgroup_memory_files_follow_the_oom_killer`：`--memory-limit none` 时 `memory.max` 是 max；128M 时是 134217728，awk 吃内存被结束之后 `memory.events` 的三项都变了，`memory.peak` 超过上限。

  变异检验：上一轮的二进制上 `cgroup` 整组都不同；`memory.max` 没接到上限的话，rootfs 测试失败（差分测试只比自洽，查不出）。cargo test（194 个）、clippy、Android clippy、两层的 19 组差分测试通过。LTP 的 syscall 测试 957 个通过（与上一轮相同）：getrusage03 这次通过，nanosleep01 这次失败，都是[偶发项](../milestones.md#已知的偶发失败)，nanosleep01 单独重跑时上一轮的二进制在同一时刻也失败。

**推迟项**：

- 只有这 37 个文件：没有 memory.stat、memory.numa_stat、memory.reclaim、io.*、hugetlb.*、rdma.*、misc.*、cgroup.kill 和压力（PSI）的文件，`/proc/cgroups` 只列这五个控制器。→ memory.stat 已补上（M9 之前：cgroup 的 memory.stat）；其余仍没有
- 只读，不能建子 cgroup、迁移进程、设置限制：要按 cgroup 管资源的程序（systemd、容器运行时）用不了，它们本来也在 D 档（§1.2）。
- `memory.peak` 只是 OOM killer 量的时候（有线程运行时每 20 毫秒）和读 memory.current、memory.peak 时取到的最大值，两次之间的峰值看不到。
