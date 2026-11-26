# Cgroup v2 and device BPF (kernel/cgroup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/cgroup"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions made before kernel/cgroup was split from kernel/procfs retain their `kernel/procfs` tags in `.agents/decisions/kernel/procfs.md`: D4–D6, D9, D24, D26, D37, D41 and the cgroup parts of D1–D3, D13, D25, D31 and D38.

## Active

### D1: Cgroups and device BPF share one policy module

- **What**: kernel/cgroup owns cgroup.rs, bpf.rs and syscall/bpf.rs, with syscall/exit observation callers in kernel/process and timer callers in kernel/time. Installing a device-BPF program descriptor uses the calling process's limit.
- **Why**: The tree and device programs form a coherent subsystem distinct from synthetic file adapters.
- **Tradeoffs**: Node adapters, namespace mechanics, syscall hooks, freezer waits and OOM selection remain in their existing modules.
- **Watch out**: The split inherits verified facts, Linux-version uncertainty and device-filter gaps; preserve every observation caller and do not treat boundary checks as a new full verification.
- **Source**: cf029f9a, 8a17e4a9; d053be24; 39e55b4b, 576501d3

### D2: Observe cgroup transitions at mutation time and deliver at checkpoints

- **What**: Latch exact changes at each mutation and coalesce deliveries, restricting directory notifications to instantiated children.
- **Why**: A value can change and return to its old value before the next checkpoint.
- **Tradeoffs**: The kernel tracks observed and instantiated files.
- **Watch out**: Call task-change hooks outside a mutable thread-group borrow.
- **Source**: affb1e2d, 0f5c5132; b65799c8, e6e88b55

### D3: Existing directory watches follow controller visibility

- **What**: Enabling pids or memory registers newly visible event files for watched child directories, and disabling a controller retires its observation state.
- **Why**: A watch installed before enablement otherwise missed later changes.
- **Tradeoffs**: Controller enablement rechecks visible files in watched child directories.
- **Watch out**: Native directory notifications can differ for a child event file whose dentry has never been instantiated.
- **Source**: b65799c8

### D4: Cgroup file access tracks controller lifetime and delegation

- **What**: Check file visibility and generation for open handles, preserve per-node ownership and modes, and require migration access at the common ancestor.
- **Why**: Controllers can disappear and delegated subtrees need enforceable boundaries.
- **Tradeoffs**: File attributes and generations require bookkeeping.
- **Watch out**: A stale handle must return ENODEV even if a file of the same name reappears.
- **Source**: 2609ab43, 3a90a0ef, 865d24c1

### D5: memory.high reclaims and cpusets constrain effective affinity

- **What**: Count threshold crossings or growth, trim DBT storage without killing on memory.high, and intersect task affinity with hierarchical effective CPUs.
- **Why**: The controllers need observable effects on memory pressure and scheduling.
- **Tradeoffs**: Host allocation cannot be stalled and cpu.max remains storage only.
- **Watch out**: Recompute affinity on migration and cpuset writes.
- **Source**: e6e88b55

## Superseded

- ~~Unconditional content comparisons for observed cgroup files in affb1e2d (pre-split kernel/procfs D38)~~ → replaced by **D2** in 0f5c5132 (2026-11-17).
- ~~Disallowing chmod/chown delegation on cgroup nodes (kernel/procfs D4)~~ → replaced by **D4** in 865d24c1 (2026-11-25)
