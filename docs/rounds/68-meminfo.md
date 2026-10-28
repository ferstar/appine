# M9 之前：/proc/meminfo 照搬宿主的（2026-10-28）

← [M9 之前：cgroup 的 memory.stat（2026-10-28）](67-memory-stat.md) · [索引](../milestones.md) · [M9 之前：glibc 发行版的探测（2026-10-28）](69-glibc-probe.md) →

[负载普查](62-workload-survey.md)的问题 6：free 显示可用内存 650 MB（原生 3.4 GB），htop 的内存条几乎是满的。

- **原因**：guest 的 `/proc/meminfo` 和 sysinfo 由宿主的 sysinfo 算，它只给得出总量和空闲量：`MemAvailable` 就是 `MemFree`，`Buffers`、`Cached` 都是 0，宿主的页缓存全算作已用。`AnonPages` 是总量减空闲量。按 `MemAvailable` 判断的程序（libuv 的 `uv_get_available_memory`、psutil）读到的同样偏小。
- **改动**：
  - hal 的 `host_meminfo` 读宿主的 `/proc/meminfo`，guest 的 `/proc/meminfo` 照搬那 16 项，与 docker 的容器看到的一样；guest 能用的上限另由 cgroup 的 `memory.max` 告诉程序。读不了宿主的文件时退回 sysinfo，可用量按空闲加缓冲区估计。
  - guest 的 sysinfo 也照搬：除了总量和空闲量，`sharedram`（Shmem）、`bufferram`（Buffers）、`totalswap`、`freeswap` 原来都是 0。
- **测试**：
  - 差分测试 `compat` 新增：`MemTotal` 与原生相同（两边是同一台宿主），`MemAvailable` 不小于 `MemFree`，`Cached` 不是 0；sysinfo 的 `totalram`、`totalswap` 与 `/proc/meminfo` 一致，`bufferram`、`sharedram` 不是 0。上一轮的二进制上 `Cached`、`bufferram`、`sharedram` 都是 0。
  - free 显示已用 372 MB、可用 3304 MB，与原生的 `/proc/meminfo` 相同；普查的 `tui` 里 htop 的内存条是 426M/3.81G（原来 2.88G 到 3.66G）。
  - cargo test（200 个）、clippy（含 Android）、两层的 20 组差分测试通过。LTP 的 syscall 测试 958 个通过（上次 957）：futex_wait05 这次通过，没有新的失败，sysinfo01、sysinfo02 照常通过（sysinfo03 与上一轮一样跳过）。

**推迟项**：

- 没有在 Android 上确认 App 能读宿主的 `/proc/meminfo`（读不了时退回 sysinfo，可用量就又偏小了），仍待真机。
- 只照搬了这 16 项；Linux 的 `/proc/meminfo` 还有四十多项（`Committed_AS`、`VmallocTotal`、`HugePages_*` 等），程序要读的话仍然没有。
