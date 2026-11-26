# Loop devices (kernel/loop) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/loop"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Loop-device decisions made before kernel/loop was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D21.

## Active

### D1: kernel/loop is split out of kernel/vfs
- **What**: kernel/loop owns `syscall/loopdev.rs`: `/dev/loop-control` and `/dev/loop<N>`, openers and exclusive claims, binding, status and capacity requests, I/O at the device offset, block ioctls and the `/sys/block/loop<N>` values, and the store and procfs maps point here for backing files and sysfs.
- **Why**: Loop devices are one file with their own device numbers, ioctls and sysfs values, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: The device-node dispatch that reaches loop devices stays in kernel/vfs.
- **Watch out**: Low risk beyond the small map going stale.
- **Source**: cc160c1

### D2: Loop backing files retain their mounts

- **What**: Include accessible loop backing descriptions in the mount holder scan.
- **Why**: A mount can remain busy with no installed backing fd.
- **Tradeoffs**: A borrowed loop table can defer collection.
- **Watch out**: Inspect the backing description's mount.
- **Source**: 4e279fa9

### D3: Loop clear advances the disk sequence for explicit and automatic detach

- **What**: Share the disk sequence counter with each loop instance and return conventional non-zoned answers to block ioctls.
- **Why**: Userspace observes media changes after either clear path.
- **Tradeoffs**: Zoned operations remain unsupported.
- **Watch out**: Final-close autoclear must update the same sequence as LOOP_CLR_FD.
- **Source**: e6495409
