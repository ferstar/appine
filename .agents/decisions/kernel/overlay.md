# Overlay filesystem (kernel/overlay) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/overlay"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Overlay decisions made before kernel/overlay was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D39, D40 and D49; those tagged `kernel/process` live in `.agents/decisions/kernel/process.md`: D45 and D46.

## Active

### D1: kernel/overlay is split out of kernel/vfs
- **What**: kernel/overlay owns `vfs/overlay.rs`, the overlay methods of `Vfs`, `open_overlay`, `follow_copy_up`, `unlink_overlay`, `link_overlay` and `rename_overlay`, and the overlay options and node sweep in `syscall/mount.rs`: mounting and layers, lookup and node lifetime, merged readdir and stat, copy-up, namespace changes and opening.
- **Why**: overlayfs is the largest part of the VFS with its own file and Linux counterpart (`fs/overlayfs`), like kernel/fuse, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: The invariants on `Node::ino` and node holds bind code in kernel/vfs too, which keeps only a pointer to them, and every overlay commit so far also changed the VFS core, so overlay changes read two maps.
- **Watch out**: A new file operation that skips the overlay rules because they now live in another map.
- **Source**: 5f327de
