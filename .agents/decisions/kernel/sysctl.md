# /proc/sys sysctls (kernel/sysctl) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/sysctl"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on kernel/sysctl made before it was split from kernel/procfs keep their `kernel/procfs` tags and live in `.agents/decisions/kernel/procfs.md`: D16, D22 and the sysctl parts of D8, D9, D15, D21, D25, D28 and D31.

## Active

### D1: /proc/sys sysctls have a dedicated module

- **What**: kernel/sysctl owns the `sys/*` portion of `STATIC`, `RECORDED`, `recorded`, `recorded_value`, `NET_CONF4`, `NET_CONF6`, `PseudoNode::{Static(b"sys/..."), NetConf, NetSysctl}`, `sysctl_kind`, `is_net_sysctl` and the `/proc/sys` dispatch arms in `core/kernel/src/pseudo.rs`.
- **Why**: `/proc/sys` sysctl tables, permission rules, namespace scoping and recorded sysctls needed a boundary separate from general procfs views.
- **Tradeoffs**: Pre-split decisions remain in `.agents/decisions/kernel/procfs.md`, and section/variant ownership inside `pseudo.rs` lives in `.agents/MODULES.md`.
- **Watch out**: Changes touching `open_pseudo`, namespace-scoped regeneration, `netconf::SYSCTLS` or `RECORDED` keys require reading `kernel/sysctl` together with `kernel/procfs`, `kernel/files`, `kernel/netns` or the backing subsystem map.
- **Source**: e0719858

### D2: Namespace-sensitive sysctls bind at file lookup

- **What**: Pin network, IPC, UTS or PID namespace files as appropriate, resolve directory children in the reader's namespace and apply user limit writes to the opener's user namespace.
- **Why**: An open fd must retain its target across setns while shared directories stay traversable.
- **Tradeoffs**: Node equality uses immutable namespace identity.
- **Watch out**: Static net directories must not pin the lookup-time namespace.
- **Source**: 2609ab43, 3a90a0ef, fa00c503, 014c5dfd

### D3: Bridge filtering has a namespace sysctl

- **What**: Expose bridge-nf-call-iptables with a default of one and normalize nonzero signed inputs to one.
- **Why**: Guest policy controls whether bridge traffic reaches filtering.
- **Tradeoffs**: The recorded switch represents a Boolean.
- **Watch out**: Preserve per-namespace state.
- **Source**: f7ddba97

### D4: Sysctl writes apply handler-specific offset rules

- **What**: Distinguish numeric, string and bitmap behavior, including replacement or merging of reserved port ranges and the unsigned and IPv6 exceptions.
- **Why**: A shared generic offset rule diverges on repeated writes.
- **Tradeoffs**: Handlers maintain separate parse and position conventions.
- **Watch out**: Do not overwrite a string on a nonzero offset.
- **Source**: 68130f74, 6fcaff37

### D5: pid_max and UTS sysctls affect allocation and poll observers

- **What**: Bound PID allocation by the configured limit and advance per-name event counters when hostname or domain name changes.
- **Why**: Storing the values alone leaves allocator and poll behavior inconsistent.
- **Tradeoffs**: Allocation must also respect the configured minimum.
- **Watch out**: Wake only the matching UTS observers.
- **Source**: 4e279fa9

## Superseded

- ~~Pinning static network-sysctl directories to the namespace used at lookup~~ → replaced by **D2** in fa00c503 (2026-11-23)
