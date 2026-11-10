# kernel/mount Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/mount.md` for the paired decision history (not yet created; decisions from before the split are D2, D4, D6–D8, D10, D30, D32, D35, D48, D52 and D53 in `.claude/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-10 (split out of the kernel/vfs full mapping of the same day, whose mount concern and mount claims its independent verifiers checked; the boundary was checked against the source at the split)

## Responsibilities

`kernel/mount` keeps the mounts every mount namespace sees and the syscalls that change them:

- the global mount table: ids, namespaces, mount points, per-mount flags, locks and propagation;
  boot mounts; and the per-instance side tables (superblock flags, procfs pid namespaces, devpts
  options, mqueue devices);
- `mount(2)`: new mounts of each filesystem type, binds, remounts, propagation changes and moves;
  `umount2`, `pivot_root`, and the new mount API subset (`open_tree`, `move_mount`,
  `mount_setattr`);
- mount propagation between peers and slaves (Linux's `fs/pnode.c`), mount locks inherited across
  user namespaces, and the `fs.mount-max` limit;
- mount namespaces: copying at `CLONE_NEWNS`, the anonymous namespaces of open_tree clones,
  entering one with setns, and collecting dead namespaces, detached mounts and the instances
  nothing mounts any more.

Root: `core/kernel/src/syscall/mount.rs`, and in `core/kernel/src/vfs/mod.rs` the mount table
(`Mount`, `Mounts`, `MntNs`, `Propagation`, `Index`, `GroupAlloc`) and the `Vfs` methods that add,
copy, move, propagate, unmount and collect mounts and namespaces.

Outside the module:
- `kernel/vfs`:
  - path resolution across mounts;
  - the mount-flag checks in file operations;
  - what unlink and rename do to mount points;
  - `chroot`;
  - the tmpfs and overlayfs instances and their options, though their option parsing and the
    overlay node sweep sit in `syscall/mount.rs`.
- `kernel/fuse`: FUSE connections and their mount options.
- `kernel/procfs`: the content of procfs, sysfs, devpts and cgroup2, and the mountinfo text.
- `kernel/process`: the namespace mechanism, `max_mnt_namespaces`, and setns's dispatch.

## Key types

- `Mount` — id, namespace, filesystem, root node, mount point (`None` for a tree root, an
  open_tree clone's root, or a detached mount not locked in place), the node and dentry it sits on,
  per-mount `MS_*` flags (`ms::PER_MOUNT`), device number, source, `MNT_EXPIRE` mark, pinned
  namespace (nsfs and mqueue mounts), locks (`lock`: Linux's `MNT_LOCKED` and `MNT_LOCK_*`),
  propagation (`Propagation`: peer group, master group, unbindable), and for its root the
  directory and name of a bound file (`root_at`) and whether that name was removed
  (`root_gone`).
- `Mounts` — the global list (also the mountinfo order; the root filesystem's entry stays first),
  detached mounts, the id counter (from 11) and device counter (from 0x30), and per-instance side
  tables by device number (tmpfs, procfs pid namespaces, devpts instances, superblock flags), plus
  mqueue devices by IPC namespace.
- `Fs` — Store, Tmpfs, Proc, Pts, Sys, Cgroup, Nsfs, Mqueue, Overlay, Fuse; which kinds get a new
  instance and their own device number per mount (tmpfs, procfs, devpts, overlay, FUSE, except
  that a mount with a `/dev/fuse` fd already mounted mounts its instance again); sysfs and cgroup2
  use the boot device, nsfs 0x4, mqueue one device per IPC namespace.
- `MntNs` — namespace id, owning user namespace, `anon` flag (open_tree clones). A task's
  namespace lives in its `FsContext` (root, cwd, umask, mount namespace), not in nsproxy.
- `Index` and `GroupAlloc` (`vfs/mod.rs`) — the propagation index built for one operation (position,
  peer members, slaves, children, places) and peer group numbering (the smallest free number,
  master numbers counting as used).

## Public entry points

- **Table and namespaces**: `Vfs::open` (boot mounts), `mounts`/`mounts_from` (with `sb_options`,
  `type_of` and `root_path`, what mountinfo shows), `count_in`, `find_ns`/`ns_root`/`pinned_ns`/
  `init_ns`, `copy_ns`, `anon_ns`, `mount`/`mount_root`/`children`/`subtree`, `mount_ns`,
  `is_point_in`,
  `mount_nodes` (every attached and detached mount's root and point, which the FUSE and overlay
  node sweeps keep), `mount_events` (the counter polled through mounts and mountinfo).
- **Changes**: `clone_mount`, `attach_tree`, `add_mount`, `add_copies`, `move_mount`,
  `remove_mount`, `umount_tree`, `propagation_targets`, `propagate`, `propagated_busy`,
  `set_propagation`, `attached`, `update_mount`.
- **Instances**: `sb_flags`/`set_sb_flags`, `sb_readonly`, `readonly`, `mount_flags` (read by
  `kernel/vfs`'s checks), `dev_of`, `fs_of`, `proc_pid_ns`/`set_proc_pid_ns`,
  `devpts_options`/`set_devpts_options` (`kernel/tty`), `mq_dev` (`kernel/ipc`).
- **Mount points that file operations remove**: `is_store_point`, `detach_store_point`,
  `roots_removed` (from `kernel/vfs`'s unlink and rename).
- **Collection**: `collect`, `released_detached`, `has_dead_ns`, `vfs::released`;
  `syscall::mount::collect_unused` (task exit) and `collect_released` (syscall end).
- **Syscalls**: `mount`, `umount2`, `pivot_root`, `open_tree`, `move_mount`, `mount_setattr`
  (`fsopen`/`fsconfig`/`fsmount`/`fspick` have no handler: ENOSYS).

## Data flow / lifecycle

### Mount table and boot mounts

- One global table holds every namespace's mounts; each `Mount` records its namespace. Ids are
  never reused; the list order is the mountinfo order.
- A namespace holds at most `fs.mount-max` mounts (`Kernel::mount_max`, default 100000, as Linux's
  `count_mounts`): a new mount, a bind with the submounts it copies, or attaching a detached tree
  that would pass it, in its own namespace or in any namespace its propagated copies go to, is
  `ENOSPC` before anything changes (`plan_attach`). Detached trees in anonymous namespaces are
  not limited.
- Boot (`Vfs::open`): when it is the store's only user (the store's exclusive flock), the store
  clears its `/dev/shm` and `/run` (not `/tmp`) and old detached directories. Mounts, in order:
  `/` (id 1, store, `rw,relatime`, source `appine`, no mount point — Linux's invisible rootfs),
  `/proc` (2), `/dev` tmpfs (7, `mode=755`, nosuid and relatime only — neither nodev nor noexec;
  `dev::populate` fills it, falling back to the store directory if that fails), `/dev/pts` (3,
  devpts, without nodev), `/dev/shm` tmpfs (8, 1777, nosuid and nodev), `/dev/mqueue` (9, the boot
  IPC namespace), `/sys` (4), `/sys/fs/cgroup` cgroup2 (6, only if sysfs mounted), `/run` tmpfs (10,
  20% of RAM, 819200 inodes, nosuid and nodev); proc, mqueue, sysfs and cgroup2 are nosuid, nodev
  and noexec; later ids start at 11. Id 5 is not a mount (fdinfo `mnt_id` of pipes and sockets). A
  mount whose point directory is missing is logged and skipped; `/tmp` is a store directory. Boot
  instances have no superblock flags entry.
- Instances: procfs, devpts, tmpfs, overlay and FUSE mounts are new instances with their own
  device numbers (a FUSE mount given a `/dev/fuse` fd that is already mounted mounts that instance
  again with its flags); mqueue maps to the mounter's IPC namespace; sysfs and cgroup2 share the
  boot device, and a cgroup2 mount is rooted at the caller's cgroup-namespace root. Superblock
  flags and mqueue device entries of dead instances are never pruned (ids and device numbers only
  grow, so they are never reused).
- Every change calls `mounts_changed`, which bumps the mount event counter polled through mounts
  and mountinfo (`kernel/procfs`) and makes `kernel/vfs`'s no-BKL resolver rebuild its mount
  points.

### mount(2) and the new mount API

- Order follows 6.1's `path_mount`: copy arguments, walk the target and lift to the topmost
  mount, strip `MS_MGC_VAL`, `MS_NOUSER` is `EINVAL`, `may_mount` (`CAP_SYS_ADMIN` in the user
  namespace owning the caller's mount namespace), then remount, bind, propagation, move, new mount.
- New mounts: proc, devpts, sysfs, cgroup2, tmpfs (options in `kernel/vfs`'s tmpfs), mqueue,
  overlay (`kernel/vfs`'s overlayfs), `fuse` and `fuse.<subtype>` (`kernel/fuse`); anything else
  `ENODEV`. Check order: no type `EINVAL`, unknown type `ENODEV`, the namespace capability checks
  and `mount_too_revealing` (`EPERM`), a missing point `ENOENT`, a point outside the caller's
  namespace `EINVAL`, the same-instance `EBUSY` below, a non-directory point for a directory root
  `ENOTDIR`, then `ENOSPC` from `plan_attach`. A new instance (tmpfs, procfs, devpts, overlay, a
  new FUSE connection) takes the read-only, sync, dirsync, mand and lazytime flags it is mounted
  with as its superblock flags (`Vfs::sb_flags`, by device number, shared by its binds); sysfs,
  cgroup2 and mqueue mounts reuse an instance and leave its flags. sysfs, cgroup2 and
  same-namespace mqueue mounted on the root of a mount of the same filesystem are `EBUSY`.
  mqueue needs `CAP_SYS_ADMIN` over the IPC namespace, proc over the owner of the caller's pid
  namespace, sysfs over the owner of its network namespace and cgroup2 over the owner of its
  cgroup namespace (`EPERM`). In a mount namespace
  not owned by the initial user namespace, proc and sysfs also need a fully visible instance
  (6.1's `mount_too_revealing`, `EPERM`): a mount of that filesystem's root in the namespace whose
  locked read-only and access-time flags the new mount keeps, with no locked mount over anything
  but the empty `/sys/fs/cgroup`; the new mount takes over those locked flags (read-only and
  access-time locks). A new mount belongs to the namespace of the mount its point is in.
- Bind: the new mount sees the source node with the source mount's filesystem, flags, device and
  source, and keeps its locked flags but is not locked in place itself; `MS_REC` copies the
  submounts under the source from one snapshot, in one pass over a child index (`plan_copies`,
  then `Vfs::add_copies` in one borrow), locks included; without `MS_REC` a source with a mount
  locked in place on or under it is `EINVAL` (`has_locked_children`); binding a namespace file
  creates an nsfs mount that pins the namespace (a mount-namespace file must be newer than the
  caller's namespace).
- Remount needs the exact mount root of an attached mount in the caller's namespace (`EINVAL`) and
  only `may_mount`: unlike 6.1's `do_remount`, no `CAP_SYS_ADMIN` over the user namespace owning
  the instance is asked for, even when the superblock flags change; flags change on this mount
  (atime bits are kept if none are
  given), and without `MS_BIND` the instance's superblock flags too (read-only, sync, mand and
  lazytime as given, dirsync kept: Linux's `MS_RMT_MASK`); clearing a locked ro, nodev, nosuid or
  noexec or changing locked atime flags
  is `EPERM` before anything changes (`can_change_locked_flags`); going read-only while regular
  or synthetic files are open for writing (in fd tables, or backing a loop device) is `EBUSY`:
  through this mount with `MS_BIND` or `mount_setattr` (`mnt_make_readonly`), through any mount of
  the same device when the instance turns read-only without it (`sb_prepare_remount_readonly`). A
  tmpfs remount without
  `MS_BIND` reconfigures the shared instance (`size=`/`nr_inodes=`/`inode32|64` per
  `shmem_reconfigure`). Remounting an overlay without an upper layer read-write without
  `MS_BIND` is `EROFS` (6.1's `ovl_remount`).
- Propagation types (`fs/pnode.c`): one of `MS_SHARED`, `MS_SLAVE`, `MS_PRIVATE`,
  `MS_UNBINDABLE` (plus `MS_REC`/`MS_SILENT`; mount_setattr's `propagation` too) on the root of a
  mount in any namespace or detached. Shared puts the mount in a peer group of its own (numbered as
  `mnt_alloc_group_id`, the smallest free) unless it has one; slave makes a shared mount with peers
  a slave of its group, and the last member of a group a slave of its own master (none: private),
  its group's slaves going along (`do_make_slave`); private and unbindable also drop the master.
  A bind or an open_tree clone of a shared mount is its peer, of a slave a slave of the same
  master; an unbindable mount cannot be bound or cloned (`EINVAL`), recursive copies leave
  unbindable mounts out (`EPERM` for one locked in place); a namespace copy's mounts are their
  originals' peers, or their slaves in a less privileged namespace (`CL_SHARED_TO_SLAVE`); a
  mount attached under a shared mount (new, bind, move) becomes shared with the mounts below it
  (`attach_recursive_mnt`); unmounted mounts become private. `MS_MOVE` of a mount in a shared
  parent, or of a tree holding an unbindable mount onto a shared one, and `pivot_root` with
  put_old's mount or the parent of new_root or of the root shared, are `EINVAL`.
- Mount events (`fs/pnode.c`): a tree attached under a shared mount (new mount, bind, nsfs bind,
  `MS_MOVE`, `move_mount`) is copied to every mount the parent propagates to that sees the mount
  point (`Vfs::propagation_targets`, planned before attaching): its peers, then each slave group,
  depth first. Copies in peers are peers of the original's mounts; copies in a slave are slaves
  of the copies in the nearest group above that got some, and peers of each other when the slave
  group is shared (masters are kept per group, so this matches `propagate_one`'s `last_source`
  walk). A mount already on the place in a target goes on top of the copy (tucked under it). In a
  namespace owned by a user namespace other than the caller's mount namespace's owner, copies are
  locked as by `lock_mnt_tree`, except their root's lock in place. `umount2` (`Vfs::umount_tree`)
  also takes down, for each unmounted mount with a shared parent, the mount on the same place in
  each mount of the parent group's propagation tree, once nothing is mounted on it but one mount
  covering its root and other mounts going (marked deepest first) and unless locked in place
  (locked ones stay); a mount covering the root of one that goes moves to the place of the
  nearest mount above that stays (`restore_mounts`). Without `MNT_DETACH`, such a mount in use
  (no submounts but a covering one) makes the unmount `EBUSY` (`propagate_mount_busy`). Removing
  a mount point (`detach_mounts`) and namespace teardown do not propagate. Copies are appended to
  the mount list in target order, so mountinfo lists them in a different order than Linux (which
  commits them in reverse).
- `MS_MOVE` (a mount locked in place `EINVAL`; the mount-max plan counts only the propagated
  copies, the moved mounts already being in the namespace), `pivot_root` (each path looked up as a
  directory
  in turn, so a file `new_root` is `ENOTDIR` whatever `put_old` is; then 6.1's checks, a locked
  `new_root` `EINVAL`; the old root's lock moves to the new root, so the old one
  can then be detached; `chroot_fs_refs` for tasks whose root or cwd was the old root in the same
  namespace).
- `open_tree` (without `OPEN_TREE_CLONE`: an `O_PATH` fd; with it: `CAP_SYS_ADMIN`, a detached
  clone in a new anonymous namespace held by the fd, which counts against `max_mnt_namespaces`
  (`ENOSPC`, `kernel/process`); without `AT_RECURSIVE` a source with a locked mount under it is
  `EINVAL`, and the clone's root is not locked in place), `move_mount` (shares `move_tree` with
  `MS_MOVE`; attaching a clone moves its mounts into the caller's namespace;
  `MOVE_MOUNT_SET_GROUP` is always `EINVAL`), `mount_setattr` (size and flag checks per
  `build_mount_kattr`; per-mount flags, `AT_RECURSIVE` over the subtree, `EPERM` before any change
  when a mount would lose a locked flag; idmapped mounts end in `EINVAL` after the userns fd
  checks — `EBADF` for a bad fd, `EPERM` for the initial namespace — all before the path walk;
  the caller's-namespace check is skipped for every mount of an anonymous namespace, not only a
  clone's root).
- `umount2`: unknown flags `EINVAL`; lookup; `may_mount` (`EPERM`); not a mount root, not in
  the caller's namespace or locked in place `EINVAL`; `MNT_EXPIRE` (with `MNT_FORCE`/
  `MNT_DETACH` or on the caller's root `EINVAL`, busy `EBUSY`, otherwise marks then `EAGAIN`;
  the mark is never cleared, and copies of a mount start unmarked, as Linux's `clone_mnt` leaves
  them); the
  tree root `EBUSY` (`EINVAL` with `MNT_DETACH`); without `MNT_DETACH`, the caller's root or a
  busy mount `EBUSY` (busy means submounts, task roots/cwds, nodes of open fds, pinned PTYs — not
  mappings, running executables, mqueue fds or loop devices' backing files); `MNT_FORCE` has no
  effect and asks for no capability (6.1's `can_umount` wants `CAP_SYS_ADMIN` for it). `MNT_DETACH`
  moves the
  subtree to the detached list, where nodes keep working: the mount root and every unlocked mount
  below it lose their mount point, while a mount locked in place stays on its detached parent
  (6.1's `disconnect_mount`), so an fd kept in the detached tree still finds what hides a path.

### Mount namespaces and cleanup

- `CLONE_NEWNS` (clone/unshare) calls `copy_ns`, which clones every attached mount of the source
  namespace (ids allocated first, then remapped; mounts pinning a mount namespace and anything
  under them are skipped) and remaps the task's root and cwd. tmpfs instances are shared between
  copies. When the new namespace's owner is not the source's (a user namespace was created
  with it, or the caller is in one), every copy is locked (6.1's `lock_mnt_tree`): locked in place
  and its ro, nodev, nosuid, noexec and access-time flags locked, so a user privileged only there
  cannot uncover what a mount hides or loosen a sandbox's flags.
- `setns` into a mount namespace needs `CAP_SYS_ADMIN` over the target's owner plus
  `CAP_SYS_CHROOT` and `CAP_SYS_ADMIN` in the caller's namespace and an unshared fs; root and cwd
  are reset to the namespace root.
- Namespaces are held by `Rc`; the table keeps weak references. `collect` drops dead namespace
  entries, detaches the mounts of dead namespaces (locked ones staying on their parents, as for
  `MNT_DETACH`), drops detached mounts no longer in use and not connected below one that is
  (disconnecting a connected mount whose parent goes), deletes
  tmpfs instances with no remaining mount and no live overlay with a layer in them, forgets
  overlay instances (and their nodes) with none, aborts and forgets FUSE instances with none
  (their daemons' reads then fail with `ENODEV`), prunes procfs and devpts instance entries, and
  repeats if releasing pins killed another namespace.
  It runs on every `umount2`; at task exit when some mount namespace has lost its last reference;
  and at the end of a syscall or of a thread when that thread let go of an open file, cwd or root
  in a detached mount (`released`, recorded per thread by open files' drop, pinned PTY ends and
  `FsContext`'s `set_cwd`, `set_root` and drop, up to 16 mount ids and then "any"), so a lazily
  unmounted mount, an open_tree clone or a tmpfs tree goes once its last use does, as with Linux's
  last `mntput`.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher (the mount syscalls; collection at syscall end).
- `kernel/process`:
  - clone and unshare with `CLONE_NEWNS` (`copy_ns`);
  - setns and nsfs pins (`find_ns`, `pinned_ns`, `ns_root`);
  - collection at task exit.
- `kernel/vfs`:
  - path resolution, which lifts to mounts, climbs out of mount roots and computes paths through
    the table;
  - the read-only and other mount-flag checks (`readonly`, `mount_flags`, `sb_flags`);
  - unlink and rename detaching mount points (`is_store_point`, `detach_store_point`,
    `roots_removed`);
  - open files and `FsContext` reporting released mounts;
  - statfs's flags;
  - the mount event counter (`mount_events`) attached to an opened mounts or mountinfo file for
    poll.
- `kernel/procfs`:
  - mountinfo and mounts (`mounts_from`, `sb_options`, `type_of`, `root_path`);
  - procfs instances' pid namespaces.
- `kernel/tty`: devpts instance options.
- `kernel/ipc`: `mq_dev`.
- `kernel/fuse`: `mount_nodes` for its node sweep.

### Outbound

- `kernel/vfs`:
  - walks and nodes;
  - new tmpfs, overlay and FUSE instances (`new_tmpfs`, `new_overlay`, `new_fuse`) and tmpfs
    reconfiguration;
  - the fd tables that `in_use` and `writers` scan;
  - loop devices' backing files (`Loops::backing_files`).
- `kernel/store`: detached directories for tmpfs instances.
- `kernel/procfs`: the `Pseudo` views of mounted synthetic filesystems, and the pid namespace of a
  procfs instance.
- `kernel/process`:
  - namespace pins and counts (`nsproxy`'s `NsRef`/`Counted`, `max_mnt_namespaces`);
  - `Kernel::mount_max`;
  - every task's root and cwd (`in_use`, pivot_root's rewrite).
- `kernel/cred`: `capable_wrt` over namespace owners, and `CAP_SYS_ADMIN`/`CAP_SYS_CHROOT`.
- `kernel/fuse`: FUSE mount options and connections, and their abort at collection.
- `kernel/ipc`: the IPC namespace that an mqueue mount shows.

## Invariants and gotchas

- Mount ids are never reused; nodes in detached mounts keep their ids and devices.
- Whatever can keep a detached mount in use must say when it lets go (`released`): a task's root
  and cwd change only through `FsContext::set_root`/`set_cwd`, and a new kind of holder counted by
  `syscall::mount`'s `in_use` needs the same, or its mount lingers until another collection.
- A detached mount has a mount point only while it is locked in place on a detached parent;
  lifting to mounts (`mounted`) and climbing out of a mount root (`root_point`) search the detached
  list too, while everything acting on attached mounts (`mount`, `mount_root`, `children`,
  mountinfo) looks at the attached list only.
- Locks are per mount and only ever added by copying a namespace into one with another owner,
  by binding, by propagating into such a namespace, by `pivot_root` (the old root's `MOUNT` lock
  moves to the new root) and by new proc and sysfs mounts (read-only and access-time locks taken
  over from the fully visible instance); a bind's own root drops `lock::MOUNT`, so
  code that creates a mount from another must decide which locks it inherits (`clone_mount`,
  `bind`, `add_copies`, `propagate` keep or clear them).
- The mount table is a list, and a namespace may hold 100000 mounts: anything that visits many
  mounts must index them once (by parent in `plan_copies` and `tree_of`, by id in `mounts_from`, by
  id, group, master, parent and mount point in `Index` for propagation and unmounting, group
  numbers handed out by `GroupAlloc`), since a scan of the table per mount holds the BKL for tens
  of seconds at that size (some paths still do, see Confirmed bugs). Propagation stacks thousands
  of mounts on one directory, so `mounted` climbs past eight stacked mounts through an index.
- Every attach path in `syscall::mount` pairs `plan_attach` (before attaching: the targets, and
  `ENOSPC` for every namespace concerned) with `propagate` (after); planning first keeps the
  tree's own mounts out of the targets. A mount on another mount's root keeps in `at` the
  directory it was found in, which is in the parent mount; targets whose point is their own root
  therefore carry no `at`, and places are matched by mount point, `at` only telling links of one
  file apart.
- `Node` comparisons in pivot_root's root/cwd rewrite include the mount id, so only tasks in the
  same mount namespace are moved.

## Confirmed bugs / technical debt

- **Remount needs no capability over the instance:** a remount without `MS_BIND` checks only
  `may_mount` (the caller's mount namespace owner), and instances record no owning user namespace,
  so root of a user namespace that unshared a mount namespace can make the shared boot root
  filesystem read-only for every namespace (when nothing has a file open for writing there) or
  resize a shared tmpfs; read-only is not a locked flag unless the copied mount already had it.
  6.1's `do_remount` wants `CAP_SYS_ADMIN` over the superblock's user namespace (`EPERM`).
- **umount ignores mappings and executables:** a mount whose files are only mapped, being executed
  or held by mqueue fds is not busy, so `umount2` succeeds where Linux returns `EBUSY` (and a tmpfs
  instance can then be reclaimed while still mapped).
- **Quadratic mount operations and stale comments:** recursive propagation changes
  (`set_propagation` through `change_propagation`/`make_slave`), `attached`, `move_tree`'s
  unbindable check and `mount_setattr` with `AT_RECURSIVE` (which also rebuilds the fast path's
  mount points per mount) scan the whole mount table once per mount in the subtree. Comments in
  `syscall/mount.rs` (its header, remount, `change_type`, `MOVE_MOUNT_SET_GROUP`, mount_setattr)
  and on `vfs::Propagation` still say mounts do not propagate or that remount changes only one
  mount, and a doc line about flags and the `MNT_EXPIRE` mark is left above `set_propagation`.
- **Mount fidelity gaps:** `MNT_EXPIRE` marks are never cleared by an access to the mount, and LTP
  umount2_02's mark-then-expire sequences fail; `umount2` with `MNT_FORCE` asks for no capability
  (6.1's `can_umount` wants `CAP_SYS_ADMIN`) and does nothing more (no FUSE abort);
  `MOVE_MOUNT_SET_GROUP` is always `EINVAL` although peer groups now exist.

## Open questions

- Should `mount_setattr` skip the caller's-namespace check for every mount of an anonymous
  namespace, or only for an open_tree clone's root?
- Should loop devices' backing files (and other fds held outside fd tables) make a mount busy for
  `umount2`, as they already do for remount's writer check?

## To verify

- The remount `EBUSY` with writers found by Starnix's syscall tests was observed on a native 6.8
  kernel and attributed to 6.1 from knowledge of the kernel.
