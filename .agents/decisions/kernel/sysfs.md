# Synthetic /sys filesystem (kernel/sysfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/sysfs"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on kernel/sysfs made before it was split from kernel/procfs keep their `kernel/procfs` tags and live in `.agents/decisions/kernel/procfs.md`: D14, D29, D42 and the sysfs parts of D2, D3, D9, D25 and D31.

## Active

### D1: The synthetic /sys filesystem has a dedicated module

- **What**: kernel/sysfs owns `SYS_GPU`, `SYS_STATIC`, `SYS_*_DIR`, `*_FILES`, `CGROUP_POINT`, `PseudoNode::Sys*`, the `sysfs` section and the sysfs dispatch arms in `core/kernel/src/pseudo.rs`. The overlay redirect_always_follow parameter reports N.
- **Why**: The `/sys` hierarchy (CPU topology, `/sys/class/net`, `/sys/block/loop<n>`, overlay parameters and conditional DRM render-node paths) needed a boundary separate from procfs.
- **Tradeoffs**: Pre-split decisions remain in `.agents/decisions/kernel/procfs.md`, and section/variant ownership inside `pseudo.rs` lives in `.agents/MODULES.md`.
- **Watch out**: Changes to CPU counts, loop/net/GPU sysfs views, `CGROUP_POINT` mounting or `missing_path` exclusions require reading `kernel/sysfs` together with `kernel/procfs`, `kernel/mount`, `kernel/loop` or `kernel/netns`.
- **Source**: e0719858, 6fcaff37

### D2: Sysfs network views belong to their network namespace

- **What**: Give noninitial namespaces their own sysfs devices, virtual interface statistics and queue defaults, and scope busy checks to the matching instance.
- **Why**: Host interface state and mount identity must not leak into unrelated namespaces.
- **Tradeoffs**: Device mappings need collection with dead namespaces.
- **Watch out**: The initial namespace retains the boot sysfs device.
- **Source**: b51818cf, 014c5dfd

### D3: Loop device links under sysfs exist independently of GPU support

- **What**: Place dev/block in the static sysfs tree and link device numbers to the corresponding loop node.
- **Why**: Device-number lookup is needed even without a renderer.
- **Tradeoffs**: The links are synthesized.
- **Watch out**: Avoid duplicate dev entries when a GPU is present.
- **Source**: 3e95c96a
