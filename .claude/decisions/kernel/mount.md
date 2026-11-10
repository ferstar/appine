# Mounts (kernel/mount) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mount"`
>
> Last distilled: 2026-11-10 (HEAD = e09877d)
>
> Mount decisions made before kernel/mount was split from kernel/vfs keep their `kernel/vfs` tags and live in `.claude/decisions/kernel/vfs.md`: D2, D4, D6–D8, D10, D30, D32, D35, D48, D52 and D53.

## Active

### D1: kernel/mount is split out of kernel/vfs, with the boundary inside two files

- **What**: kernel/mount owns `syscall/mount.rs` except vfs's tmpfs and overlay option parsing and the overlay node sweep, plus the mount table in `vfs/mod.rs` (`Mount`, `Mounts`, `MntNs`, `Propagation`, `Index`, `GroupAlloc` and the `Vfs` methods that add, copy, move, propagate, unmount and collect mounts and namespaces).
- **Why**: The mount table, mount syscalls, propagation and mount namespaces made up a large part of the vfs map, while most VFS changes never touch them.
- **Tradeoffs**: The boundary runs inside `vfs/mod.rs` and `syscall/mount.rs`, and mount-flag checks stay in kernel/vfs, so read-only or remount changes usually read both maps.
- **Watch out**: A mount-table method added in the vfs part of `vfs/mod.rs` needs the registry's description updated, or it falls between the two maps.
- **Source**: d2b1dc3

### D2: The mount map points at the pre-split mount decisions in the vfs snapshot

- **What**: The kernel/mount map's See line lists the vfs snapshot's mount decisions (D2, D4, D6–D8, D10, D30, D32, D35, D48, D52, D53), which keep their kernel/vfs tags and IDs.
- **Why**: Those decisions are indexed by their kernel/vfs tags, yet readers of the mount map still need to find them.
- **Tradeoffs**: The list has to follow later vfs distills that add or rewrite mount entries.
- **Watch out**: Low risk; a new mount decision tagged kernel/vfs instead of kernel/mount would land in the wrong snapshot.
- **Source**: a536142

### D3: The kernel/mount map is verified on its own

- **What**: The map was fully remapped (five research concerns, 23 claims checked by four independent verifiers) instead of carrying facts verified as part of kernel/vfs, and adds bug items for detached mounts outliving their last use, anonymous-namespace mounts in propagation, and further 6.1 fidelity gaps.
- **Why**: Facts inherited through the split had not been checked on their own and could have been dropped or distorted.
- **Tradeoffs**: The 6.1 behaviour behind the fidelity gaps and the propagation bug rests on knowledge of the kernel, not 6.1 source, and stays under To verify.
- **Watch out**: Lazily unmounted mounts, unattached open_tree clones and mounts under removed mount points can hold overlay, FUSE, procfs and tmpfs instances until an unrelated collection, and remount still checks no instance owner.
- **Source**: e09877d
