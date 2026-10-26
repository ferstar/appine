# M9 之前：平均负载、sysfs 和 limits（2026-10-27）

← [M9 之前：拆 BKL（2026-10-26）](38-split-bkl.md) · [索引](../milestones.md) · [M9 之前：ptrace 的第一步，strace（2026-10-27）](40-ptrace-strace.md) →

把推迟项里顺手能补的几个内核兼容性缺口补上（ptrace 另做）：

- **平均负载**（`/proc/loadavg`、sysinfo 的 `loads`，M3b 起一直是 0）：与 Linux 的 `kernel/sched/loadavg.c` 一样，每 5 秒取一次可运行的线程数，按 1、5、15 分钟做指数衰减平均，定点数的系数和取整都与 Linux 相同（`loadavg.rs`）。可运行的是不在内核里等待的 guest 线程，等 BKL 的也算，相当于 Linux 的 R 加 D。
  - 取样由定时器线程做。所有线程都在等待时它停下来，不为平均负载每 5 秒醒一次：下一个要执行 guest 代码的线程叫醒它（与内存上限的检查共用一个标志，`Shared::resume_checks`），停下来期间错过的取样按 0 补上；读的时候定时器线程还没来得及补的，读的一方先补上。这相当于 Linux 的 NO_HZ 在空闲的 CPU 上做的事。
  - 两个忙循环跑 15 秒，每 5 秒读一次，1 分钟的平均值依次是 0.16、0.31、0.44，与按 Linux 的公式算出来的相同；停下来 10 秒之后读到 0.37，补上了两次 0。`uptime`、`top` 的负载不再总是 0。`sleep 24` 期间，定时器线程在 18 秒里只醒了一次（就是发现所有线程都在等待、停下来的那次取样）。
  - `/proc/loadavg` 的第四列是可运行的线程数和全部线程数（原来固定是 1），第五列是最近分配的 pid（原来是现存的最大 pid）。
- **`/proc/<pid>/limits`**（`task/<tid>/` 下也有）：格式与 Linux 的 `proc_pid_limits` 相同，包括列宽、`unlimited`，以及没有单位的行末尾留着的空格。
- **sysfs**（原来 `/sys` 只是仓库里的一个空目录）：只有 CPU 的拓扑，即 `/sys/devices/system/cpu` 下的 `online`、`possible`、`present`、`offline` 和各个 `cpu<n>`（`online`、`topology/*`、`regs/identification/midr_el1`、`revidr_el1`）。
  - CPU 都一样（与 `/proc/cpuinfo` 一致），各自是一个核，没有超线程，都在一个封装里。掩码和列表的写法与 Linux 的 `%*pb`、`%*pbl` 相同（4 个 CPU 时 cpu2 的 `core_cpus` 是 `4`，8 个时 cpu0 的是 `01`）。
  - busybox 的 `nproc --all` 数的就是这里的 `cpu<n>` 目录，原来一个也数不到，输出 1，现在与 `nproc` 相同。lscpu、glibc 的 `get_nprocs` 也读这里。
  - 文件与 Linux 一样报告 4096 字节、0 块，目录是 0755；statfs 是 `SYSFS_MAGIC`，设备号 0:22。`/proc/mounts`、`mountinfo`、`/proc/filesystems` 里有了它，fdinfo 的 mnt_id 能在 mountinfo 里找到（没有挂载点的 pipe、socket 从 4 改成 5）。
  - 没有 `/sys/kernel/mm/transparent_hugepage`，相当于没有编进 THP 的内核：appine 不支持大页，Go、jemalloc 读不到它就不会去用。
  - procfs 的 `net/`、`sys/` 这些固定的目录和 sysfs 共用一套按路径表查找、列目录的代码。合成文件系统里的节点的设备号都按它所在的文件系统给（epoll 的 fdinfo 原来把 `/dev/pts` 目录也报成 procfs 的）。
- **删除之后仍然打开着的文件的 `futimens`**（[文件操作的宿主开销](33-file-ops-host-cost.md)那一轮的推迟项）：普通文件的时间记在宿主 object 上，原来按 inode 编号拼出 object 的名字再设置，object 删掉之后是 ENOENT。按 fd 设置时（futimens，`AT_EMPTY_PATH`）改为直接设置打开着的 object。
- **测试**：单元测试新增平均负载的取样、停下来之后按 0 补上、停下来之后在下一次取样之前恢复。`rootfs_programs` 新增 `nproc --all`、`online`、topology 里的文件和 `/proc/self/limits`（与 `ulimit -n` 一致）。差分测试 `compat` 新增 sysfs 的内容、大小和文件系统类型，`/sys` 在 mountinfo 里的设备号和 mnt_id，`/proc/loadavg` 的格式（数值取决于宿主，不比），`/proc/self/limits` 的每一行与 getrlimit 一致且列宽与 Linux 相同，删除之后仍然打开着的文件的 futimens 和 `AT_EMPTY_PATH`。possible、present 在有的宿主上比 online 多，不比。变异检验：改动之前的二进制在新加的检查上失败。cargo test（178 个）、clippy、Android clippy、两层的 14 组差分测试通过。LTP 的 syscall 测试 942 个通过，与[拆 BKL](38-split-bkl.md)那一轮记的相同；上一次全量运行（939 个）失败的 clock_nanosleep02、epoll_pwait03、nanosleep01、poll02 都是计时类，这次通过。mmap21_02 这次失败（[偶发项](../milestones.md#已知的偶发失败)，日志被重跑覆盖了），单独重跑 5 次都通过。

**推迟项**：

- sysfs 只有 CPU 的拓扑：没有 `cpufreq`（Node.js 的 `os.cpus()` 的频率是 0）、`cache/`、`/sys/class`、`/sys/fs/cgroup`。`cpu<n>/online` 是只读的（Linux 上 root 可以写它让 CPU 下线）。
