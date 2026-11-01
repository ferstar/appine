# M9 之前：cgroup 的 pids.max 和 memory.max 起作用（2026-11-01）

← [M9 之前：cgroup 树：建子 cgroup、迁移进程（2026-11-01）](163-cgroup-tree.md) · [索引](../milestones.md)

[podman 和 docker 的普查](157-container-survey.md)的问题 9 的第二步的前一半：[上一轮](163-cgroup-tree.md)的限制只按格式记下，这一轮让 `pids.max`、`memory.max` 起作用（`docker run --pids-limit`、`--memory`，podman 的同名选项，systemd 的 `TasksMax=`、`MemoryMax=` 写的就是它们）。`cgroup.freeze` 要做透明的冻结和系统调用重启，下一轮单独做。照 Linux 6.1 的 `kernel/cgroup/pids.c`、`mm/memcontrol.c`、`mm/oom_kill.c`。

## 做法

- **pids.max**（`cgroup::pids_charge`，Linux 的 `pids_can_fork`）：fork、clone、clone3（含建线程、`CLONE_INTO_CGROUP` 放进别处的）之前，新任务要去的 cgroup 和它的每个开了 pids 的祖先，任务数加 1 都不能超过 `pids.max`，超过是 EAGAIN，放在 RLIMIT_NPROC、threads-max 的检查之后。
  - 任务数（`pids.current`）与 Linux 一样到回收为止都算：进程的活着的线程，主线程已经退出的（僵尸进程、主线程先退出的进程）再加 1。根的原来是线程数，现在也这样算。
  - 失败记进离新任务的 cgroup 最近的开了 pids 的 cgroup 的 `pids.events`（6.1 的语义：不往上加，新内核改成往上加、另有 `.local`），第一次时内核日志记 `cgroup: fork rejected by pids controller in <路径>`。
  - 迁移进来不看上限（`pids_can_attach` 只是记账），`pids.current` 可以超过 `pids.max`。
  - `pids.max` 的范围改成 0 到 2^22（Linux 的 `PIDS_MAX` 是 `PID_MAX_LIMIT + 1`，上一轮少了一个）。
- **memory.max**（`oom.rs`）：OOM killer 原来只量整个 App，现在也量设了 `memory.max` 的 cgroup（子树里的进程的驻留内存之和，与上一轮的 `memory.current` 相同）。超出时与 Linux 的 memcg OOM 一样：
  - 那个 cgroup 记一次 `max`，从它下面按 oom_badness 挑一个进程（`oom_score_adj` 按 cgroup 的上限折算，-1000 的不挑）结束，记 `oom`，被结束的进程所在的 cgroup 记 `oom_kill`。上次挑中的进程还没退完时等它，不再记 `max`，也不另挑（它的内存马上就会还回来；它停着不分配，Linux 也不记）。
  - 事件按 `memcg_memory_event` 记：自己的 `memory.events.local`，自己和祖先的 `memory.events`。App 的上限的事件记在根上，原来 `Oom::events` 的那一份去掉了。内核日志的 `oom_memcg=`、`task_memcg=` 写真实的路径（原来总是 `/`）。
  - `memory.oom.group`：被结束的进程所在的 cgroup 到超出的 cgroup 之间、设了它的最上面那个，里面的进程一起结束（-1000 的除外），记 `oom_group_kill`；与 Linux 的 `mem_cgroup_scan_tasks` 一样，挑中的那个在 `oom_kill` 里又算一次。
  - 调低 `memory.max` 时马上量，已经超出就结束一个（Linux 的 `memory_max_write` 回收不了时 OOM，这时不记 `max`）；之后的照常定时检查。写了 `memory.max` 之后叫醒定时器线程马上检查一次。
  - 子 cgroup 的 `memory.peak` 是量到过的最大用量（定时检查和读 `memory.current`、`memory.peak` 时取样），原来就是当时的用量。
  - 量 cgroup 要对每个进程的已写过的映射做 mincore，在定时器线程里拿着 BKL：实测几十 MB 的进程一两百微秒，node 占 185 MB 时 0.8–1.6 毫秒，1000 个 1 MB 映射的进程 0.3–2 毫秒。所以只量设了上限的 cgroup，检查的间隔至少是这次耗时的 20 倍，占 BKL 的时间不超过 5%；代价是大的 cgroup 超出之后发现得晚一些。
- **关掉控制器**：父 cgroup 的 `subtree_control` 关掉一个控制器时，子 cgroup 里它的接口文件的值、`pids.events`、内存的事件和峰值都丢掉（Linux 销毁那个 css），再开时是默认值。

## 测试

- 新的差分测试组 `cgroup-limits`（原生一侧是特权容器）：先像 docker-in-docker 那样把容器根里的进程挪进 `init`、在根上开 pids 和 memory（容器的根不是真正的根，有进程时开不了）。
  - pids：`pids.max` 写错的值（-1、2^22+1、不是数字）和边界；子孙 cgroup 里的任务算在上面开了 pids 的那个里；僵尸进程算、回收之后不算；第三个线程的 `pthread_create`、之后的 fork 是 EAGAIN；`pids.events` 记在哪个 cgroup 上；迁移进来超过上限；shell 里 fork 失败；关掉再开 pids 之后上限和事件回到默认。
  - memory：`memory.max` 32M（`memory.swap.max` 0，有交换分区的宿主上也不换出）里不停分配的进程被 SIGKILL 结束，`memory.events` 和 `.local`、父 cgroup 的累加，`memory.peak` 超过 16M；分配了 48M 之后把上限调到 16M，进程被结束；`memory.oom.group` 时 shell、sleep 和分配的进程一起结束，`oom_kill` 是 4、`oom_group_kill` 是 1；最后容器根的 `memory.events` 是三个子 cgroup 累加的。`max` 只比是否大于 0（Linux 每次分配碰到上限都记，有几十次），调低上限的那次 `oom` 只比是否大于 0（Linux 在等进程退出时反复记，几千次）。
- 两个执行层下都与原生逐字相同，Tier 1 多跑了一次。写用例时撞到的：awk 的 `printf` 的参数里 `>` 是输出重定向，要加括号；仓库里的 `/tmp` 是持久的，上次留下的 fifo 要先删；调低 `memory.max` 时 Linux 在写的时候就结束了进程，appine 原来稍后才结束，ash 打不打印 `Killed` 跟着变（后来也改成写的时候就结束，用例照样去掉了 `wait` 的 stderr）；Tier 1 下同步结束进程之后，定时检查在它退完之前又记了一次 `max`，改成等它的时候不记。
- 变异检查：不检查 pids、不量 cgroup 的内存、调低上限时不结束进程，用例在三处都与原生不同。第一次做变异检查时用例卡住了（等着被结束的进程永远在 `pause()`），改成最多等 10 秒，内核不对时用例失败而不是卡住。
- rootfs 测试里 OOM killer 的三个（`cgroup_memory_files_follow_the_oom_killer` 等）照旧通过。
- cargo test（297 个）、clippy（含 Android）、格式、两层的全部差分测试通过。LTP 的 syscall 测试 1011 个通过（上次 1012）：futex_wait05（睡得太久）这次失败，是[偶发项](../milestones.md#已知的偶发失败)，其余逐项相同。
- 在 appine 里实际跑：podman 不加 `--cgroups disabled`、带 `--pids-limit`、`--memory` 时建好了容器的 cgroup，停在 crun 给它挂设备过滤的 eBPF 程序（`bpf create` 是 ENOSYS），与 docker 停在同一处，容器里的限制要等下一步之后才能实际看到。

**推迟项**：

- `memory.max` 是定时检查的：不停分配的进程在被发现之前会超出几十 MB（大的 cgroup 检查得更稀，超得更多），`memory.peak` 也就超过上限；Linux 在分配时就挡住，峰值正好是上限。调低 `memory.max` 时 Linux 等到用量降下来才返回，这里结束进程之后马上返回。
- `memory.current` 是各个进程的驻留内存之和：fork 之后共用的页、共享的映射在每个进程里各算一次，没有页缓存、内核内存；Linux 每页只记在第一次用它的 cgroup 上。
- `memory.high`（限速回收）、`memory.low`、`memory.min`（保护）、`memory.swap.max`、`memory.swap.high` 只记下，不起作用，`memory.events` 的 low、high 总是 0；没有 `memory.pressure`。
- `cpu.max`、`cpu.weight`、`cpuset.cpus` 不起作用：宿主线程的调度不归 appine 管（要做的话可以按周期停下超额的线程，与冻结用同一套办法）。
- `cgroup.freeze`：下一轮。
