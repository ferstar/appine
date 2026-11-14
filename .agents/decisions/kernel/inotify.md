# inotify (kernel/inotify) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/inotify"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1)
>
> inotify decisions made before kernel/inotify was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D13, D46 and D51.

## Active

### D1: kernel/inotify is split out of kernel/vfs
- **What**: kernel/inotify owns `syscall/inotify.rs`: instances, watches and per-user limits, the registry keyed by store inode or by overlay, FUSE or cgroup2 node key, the hooks at fsnotify's call sites, the `Opened` record and the replay of the Files app's and cgroup2 files' changes; the file's doc comment now says cgroup2 files do get events (through `kernfs_modified`).
- **Why**: inotify is the largest of the parts moved out of vfs, with its own registry, limits and replay of outside changes, and the vfs map (99 KB) could not be read in one Read.
- **Tradeoffs**: Hooks span vfs and fuse, cgroup change producers belong to kernel/cgroup, and procfs still owns limits and fdinfo, so producer changes consult both maps.
- **Watch out**: A filesystem change added without its hook, now that the hook list lives in another map.
- **Source**: cc160c1; cf029f9a
