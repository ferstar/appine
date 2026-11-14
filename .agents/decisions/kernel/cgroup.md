# Cgroup v2 and device BPF (kernel/cgroup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/cgroup"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1)
>
> Decisions made before kernel/cgroup was split from kernel/procfs retain their `kernel/procfs` tags in `.agents/decisions/kernel/procfs.md`: D4–D6, D9, D24, D26, D37, D41 and the cgroup parts of D1–D3, D13, D25, D31 and D38.

## Active

### D1: Cgroups and device BPF share one policy module

- **What**: kernel/cgroup owns cgroup.rs, bpf.rs and syscall/bpf.rs, with syscall/exit observation callers in kernel/process and timer callers in kernel/time.
- **Why**: The tree and device programs form a coherent subsystem distinct from synthetic file adapters.
- **Tradeoffs**: Node adapters, namespace mechanics, syscall hooks, freezer waits and OOM selection remain in their existing modules.
- **Watch out**: The split inherits verified facts, Linux-version uncertainty and device-filter gaps; preserve every observation caller and do not treat boundary checks as a new full verification.
- **Source**: cf029f9a, 8a17e4a9
