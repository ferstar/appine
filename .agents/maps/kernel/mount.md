# kernel/mount Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/mount.md` for the paired decision history (decisions from before the split are D2, D4, D6–D8, D10, D30, D32, D35, D48, D52 and D53 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-26 (full mapping: 18 adversarial claims checked by independent verifier — 13 confirmed, 5 partial corrected below, 0 refuted; 4 recorded debt bullets verified and 5 audit refinements/bugs folded in)

## Responsibilities

`kernel/mount` owns boot mounts, guest mount syscalls, the mount graph across namespaces, per-instance
mount state, propagation, locks, namespace copy and detached-tree collection. It supplies mount data and
change counters to procfs.

The boundary runs through `core/kernel/src/syscall/mount.rs` and the mount table and namespace methods in
`core/kernel/src/vfs/mod.rs`. Tmpfs option parsing belongs to `kernel/vfs`, overlay options, layer
preparation and the overlay node sweep to `kernel/overlay`. VFS owns path walking, file-operation flag
checks, chroot and tmpfs storage. Namespace syscall dispatch and accounting belong to `kernel/ns`.

## Key types

- `Node` pairs a mount ID with a store inode, pseudo node, overlay ID or FUSE ID. `in_mount` retags the
  same object. A `Dentry` records its parent node and name.
- `Mount` records `id`, namespace `ns`, filesystem kind `fs`, instance device `dev`, source and a root
  tagged with its own ID. `point` identifies the node on the parent mount (`parent()` derives its ID).
  `at` distinguishes the store dentry used as a point. `root_at` records a bound source name, `root_gone`
  its deletion. A namespace root, anonymous clone root or disconnected mount can have no point.
- `flags` holds `ms::PER_MOUNT`, `locks` the in-place and access restrictions, `expiring` the unmount
  mark. `owner` records instance ownership on each mount (None means initial user namespace). `pin` holds
  a strong namespace reference for nsfs or a dynamically mounted mqueue. Separate `ns_pin` holds the
  stashed nsfs inode/dentry for inotify.
- `Mounts` owns the attached `list`, `detached`, ID/device allocators, instance tables and kernel-wide
  `collect_pending`. `by_id` indexes both vectors by ID and location. `children` indexes both by parent,
  preserving table order within each vector.
- `MntNs` has an increasing namespace ID, owner and `anon` bit. A task's `FsContext` holds it alongside
  root, cwd and umask. `Vfs` owns the boot namespace and registers later ones weakly. Namespace-file
  references and `OpenFile::tree` can outlive their creators.
- `Propagation` has a peer group, a master group (zero for absent) and an unbindable bit. A shared mount
  can also be a slave. `Target` carries namespace, point, optional dentry, group and master chain.
- Temporary `Index` assigns positions across the whole attached list, child links for entries with a
  distinct parent and spot entries for entries with a point (so a parentless anonymous root gets a
  position only). Its peer and slave maps exclude live anonymous namespaces. `GroupAlloc` reserves both
  groups and masters from both mount lists and hands out the lowest free positive number per allocation.

## Public entry points

| Interface | Entries |
| --- | --- |
| Dispatch | `umount2` (39), `mount` (40), `pivot_root` (41), `open_tree` (428), `sys_move_mount` (429), `mount_setattr` (442). fsopen/fsconfig/fsmount/fspick fall through to `ENOSYS`. |
| Namespace and graph | `Vfs::open` / `open_with_gpu`, `init_ns`, `find_ns`, `pinned_ns`, `ns_root`, `copy_ns`, `anon_ns`, `mounts`, `mount`, `mount_ns`, `mount_root` / `any_mount_root`, `children`, `subtree`, `count_in`. |
| Mutation | `add_mount` / `add_mount_unattached`, `clone_mount` / `clone_ns_mount`, `add_copies`, `attach_tree`, `move_mount`, `attached`, `propagation_targets`, `propagate`, `propagated_busy`, `umount_tree`, `set_propagation`, `set_mount_prop`, `update_mount`, `set_mount_attrs`. |
| Consumers | `mount_flags`, `readonly`, `sb_flags`, `mount_dev`, `dev_of`, `fs_of`, `mounts_from`, `root_path`, `type_of`, `sb_options`, `mount_events_of`, `mount_nodes`, `is_store_point`, `is_point_in`, `roots_removed`, `detach_store_point`. |
| Lifetime | `vfs::released`, `released_detached`, `has_dead_ns`, `defer_collect`, `collect`, `syscall::mount::{collect_released,collect_unused}`. |

## Data flow / lifecycle

### Boot and mount identity

`open` delegates to `open_with_gpu(dir, false)`. Exclusive store boot cleanup empties `dev/shm` and `run`
and removes old detached directories. Joining an active store skips cleanup. Each VFS builds these
private, unlocked mounts, owned by the initial user namespace, in the order shown, with no superblock flag
entries. All use relatime.

| Point | ID | Instance and additional flags |
| --- | ---: | --- |
| `/` | 1 | Parentless store, device `0x801`, source `appine`. |
| `/proc` | 2 | Procfs, nosuid/nodev/noexec. |
| `/dev` | 7 | Tmpfs, mode 0755, nosuid. |
| `/dev/pts` | 3 | Devpts, nosuid/noexec. |
| `/dev/shm` | 8 | Tmpfs, mode 01777, nosuid/nodev. |
| `/dev/mqueue` | 9 | Initial IPC mqueue, nosuid/nodev/noexec. |
| `/sys` | 4 | Sysfs, nosuid/nodev/noexec. |
| `/sys/fs/cgroup` | 6 | Writable cgroup2 on `CGROUP_POINT` if sysfs mounted, nosuid/nodev/noexec. |
| `/run` | 10 | Tmpfs, mode 0755, 20% of RAM in pages, 819200 inodes, nosuid/nodev. |

Missing point directories skip the corresponding mounts. Ordinary startup falls back to the store /dev
directory if tmpfs creation or population fails. GPU startup requires a populated /dev tmpfs and
successful GPU node creation. /tmp stays in the store. ID 5 is the fdinfo fallback for node-less
descriptions. New mount IDs start at 11. Device allocation starts at `0x30`, including boot tmpfs.

### Legacy mount dispatch and instance creation

`mount` reads optional type and source, probes non-null data, then reads it as a C string up to 4096 bytes
(failure after the first-byte probe becomes empty data). It walks the target, saves the reached node and
lifts to the topmost mount, discarding the dentry if that lift changes the node. After stripping the
legacy magic prefix it rejects `MS_NOUSER` (`EINVAL`), then requires `may_mount` (`SYS_ADMIN` over the
caller's mount namespace owner, else `EPERM`). Branch priority is remount, bind, propagation change, move,
new mount. Remount uses the saved node. Other branches use the lifted target. Per-mount flags default to
relatime unless noatime is set. Strictatime clears both.

For new mounts, a null type gives `EINVAL`, unknown type `ENODEV`. Proc, sysfs, cgroup2 and mqueue
additionally require `SYS_ADMIN` over the caller's PID, network, cgroup or IPC namespace owner,
respectively. Proc/sysfs then check visibility. Next, `may_mount_on` rejects pseudo OpenFd, OpenExe, Fd
and FdInfo points (`ENOENT`), `check_mnt` requires the caller's attached namespace (`EINVAL`), a
same-instance conflict gives `EBUSY`, and a non-directory gives `ENOTDIR`. Type-specific checks precede
quota planning. The mount belongs to the destination's namespace. Propagation follows instance setup.

| Type | Instance identity, view and owner |
| --- | --- |
| proc | Fresh device. `proc_pid_ns` resolves noninitial PID namespace by device, initial by default. Owner is PID namespace's owner. |
| devpts | Fresh device in `PtsRoot(dev)`. Per-device options translate UID/GID in caller's namespace. Owner is mounter's user namespace. |
| tmpfs | Fresh device and detached store directory with parsed mode, ownership and limits. Owner is mounter's user namespace. |
| overlay | Fresh device and overlay root. Overlay supplies layers, options and tmpfs layer pins. Owner is mounter's user namespace. |
| fuse or fuse.subtype | New connection/device owned by mounter's user namespace, or existing device and connection owner on reuse. |
| sysfs | Initial network namespace uses boot device, others reuse a device per registered network namespace. Initial user namespace owns the instance. |
| cgroup2 | Boot device, caller's cgroup namespace root ID, empty filename and generation zero. Initial user namespace owns it. |
| mqueue | `mq_dev` returns `0x13` for initial IPC, lazily allocates per namespace otherwise. The mount pins the IPC namespace and takes its owner. |

Store (shown as ext4) and nsfs (device `0x4`) cannot be newly mounted by name. At a mount root the
same-instance check compares sysfs network namespaces, the mqueue root's IPC ID, or cgroup2 kind alone.
Repeated proc/devpts mounts create new instances. Fresh procfs, devpts, tmpfs, overlay and fresh FUSE
connections initialize superblock flags from caller-selected ro, sync, dirsync, mand and lazytime;
first-use sysfs and mqueue devices in noninitial namespaces set only ownership, and reuse preserves
instance flags.

FUSE parses options first. An empty type suffix, subtype in both type and options, or missing/unopened fd
gives `EINVAL`. An already attached FuseDev selects the registered connection or gives `ENOTCONN` if gone.
Only a fresh connection requires rootmode, user_id, group_id and a FuseDev opened in the current user
namespace. It is mounted and propagated, the device file is attached and FUSE_INIT queued without waiting
for the daemon's reply.

### Visibility, locks and bind trees

`visible_locks` implements mount_too_revealing. A mount namespace owned by the initial user namespace bypasses the check.
Otherwise it requires a same-kind full-root procfs/sysfs witness in the caller's namespace whose locked
read-only and atime settings the request honors (locked ro requires a ro request; locked atime requires
identical noatime, nodiratime and relatime bits) and that has no directly in-place-locked child except at
the empty sysfs `CGROUP_POINT`. Device/PID/network namespace equality is ignored, only read-only and atime
locks transfer, and no qualifying witness gives `EPERM`.

Ordinary bind walks a nonempty source following final symlinks, checks `may_mount_on` and requires
matching directory status (`ENOTDIR`). Unbindable source, wrong attached namespace on either side, or a
nonrecursive bind that would uncover a locked child gives `EINVAL`. `has_locked_children` checks direct
children whose point equals the source or whose recorded directory (otherwise point) is below it.

For `MS_REC`, `plan_copies` indexes an attached snapshot before insertion. A depth-first walk records
parent indices, prunes unbindable branches and returns `EPERM` if a pruned root is locked in place. After
quota planning, `add_mount_unattached` inserts the bind root in the attached list. It shares filesystem,
device, flags and source, records a bound file's dentry, inherits locks except `MOUNT` and takes `of_bind`
propagation. Its owner and pin fields start empty. `add_copies` retags child points, preserves child
locks, owner, pins and root deletion metadata, and clears expiry. `attached` applies the shared parent's
type to the complete tree before propagation.

A namespace-file source takes `bind_ns` even through a foreign source mount. Namespace lookup can give
`ENOENT`. A pinned mount namespace must be strictly newer than the caller's (`EINVAL`). Check destination
namespace, then mountability, then require a non-directory. After planning, create a zero-flag nsfs mount
with namespace and stashed-inode pins. This branch checks pin age against the caller. The additional
recipient-age check belongs to `move_tree`.

### Remount and read-only transitions

Remount needs the exact root of an attached mount in the caller's namespace (`EINVAL`). Omitted atime
options retain old bits. `keeps_locked` rejects clearing locked ro/nodev/nosuid/noexec or changing the
locked atime triple (`EPERM`). Full tmpfs remount parses option syntax and translates UID/GID before
checking `SYS_ADMIN` over the recorded instance owner, then tests a requested read-only transition for
busy holders before validating and applying new block/inode limits. Bind remount skips instance checks.

| Read-only transition | Reason for `EBUSY` |
| --- | --- |
| Bind remount sets a previously clear per-mount RDONLY bit | `writers` contains this ID. |
| Full remount changes the effective superblock from rw to ro | `sb_busy` finds a writer or deleted holder on this device, including through detached mounts. |
| `mount_setattr` sets ro | After every selected mount passes its lock checks, a mount turning ro has a writer. |

`writers` checks writable open descriptions in task fd tables and loop backings. It counts regular files
except memfds identified by the seals map, synthetic Proc files and FUSE handles, using their node's
mount. O_PATH fds, special files and mappings alone contribute no writer.

After testing writers by `mount_dev`, `sb_busy` checks fds, Unix transfers, loop backings, task
roots/cwds, distinct address spaces and store/overlay executable records. The deleted node must resolve to
this device and be a non-memfd store inode with a successful zero-link stat, an unlinked overlay node, or
a cached FUSE node marked unlinked. An individual unborrowable open file is skipped in the deleted-file
predicate. `Mm::mapped_file_inos` returns backing open files for FUSE fill-backed mappings, private or
shared, and mount/inode-label pairs for other file mappings (store, tmpfs, overlay backing inodes, and
`FuseSnapshot` tagged keys).

A full rw remount of a lower-only overlay gives `EROFS` after the busy test. For tmpfs, evict unused
unlinked files and apply reconfiguration before flags. A nonzero size/inode limit cannot restrict an
unlimited instance or fall below usage (`EINVAL`). Omitted limits stay, zero requests unlimited,
inode32/64 can change, and root mode/uid/gid stay. Other types ignore remount data. Full remount changes
device-wide ro, sync, mand and lazytime, preserving dirsync. Lower-only overlay adds effective superblock
ro. Both remount forms finish by updating this mount's flags and advancing the fast-path epoch.

### Propagation changes, planning and attachment

Legacy change-type uses `any_mount_root` on the lifted target, including detached roots, with no
subsequent caller-membership check. It accepts exactly one of shared/slave/private/unbindable plus
optional REC/SILENT. Recursive change follows connected children on both lists.

- Shared keeps existing groups and masters, assigns each ungrouped mount a fresh group, and clears
  unbindable. A fresh mount under a shared parent does this in `add_mount_inner`. Bind and move use
  `attached` on the complete subtree.
- Slave makes a departing shared member a slave of the old group if any members remain. An ungrouped mount
  retains its state, including unbindable. Private clears the record. Unbindable clears group and master
  and sets its bit. `leave_groups` applies the batch, finds groups emptied across both lists and memoizes
  their old master chains to redirect all affected slaves to the surviving master or zero.
- `of_bind` preserves group/master and clears unbindable. `of_slave_copy` makes a shared source's group
  the copy's master and clears its group. For an ungrouped source the master stays. Both cases clear
  unbindable.

Before attachment, `propagation_targets` visits the shared parent's other peers, then slave groups depth
first. It records each recipient's nearest-first master chain. An unshared parent has no recipients. A
live anonymous clone reserves group numbers but cannot receive via the peer/slave maps. An already
attached source can contain recipients of its own move. New copies never re-enter the plan.

Visibility requires the translated point to equal the recipient root or the root to be in the underlying
filesystem ancestor set of the point (store dentry parent when available). The VFS computes this set once
with normalized mount IDs. Targets at their own root drop `at`, since the original directory may be
outside that mount. `Index::at_place` prefers an exact optional dentry match at the point, otherwise its
first occupant.

`plan_attach` aggregates additions by namespace. Charge one for a new mount, root plus planned descendants
for a bind, all attached anonymous entries for an anonymous move, zero locally for an ordinary move, and a
full tree per recipient. `any_ns_over` counts attached entries only for positive additions. Exceeding
`fs.mount-max` gives `ENOSPC` before topology changes. Equality passes, as can a zero-growth move after
lowering the limit. Anonymous clone creation and namespace copy do not run this check. Earlier path access
and overlay work-directory preparation can already have had effects.

`propagate` snapshots the source breadth first and indexes existing occupants. Peer copies take bind
propagation. Each slave copy takes the per-node master from the closest upstream group that received
copies, using the original at the top. Unshared slaves get no group. Shared slave peers reuse one new
group per tree position. Trees append in recipient order, parent before child. An existing occupant moves
onto the copy root. Copies retain instance metadata and clear expiry. If the recipient namespace owner
differs from the caller's mount namespace owner, `locked_tree` adds in-place and atime locks and locks any
set ro, nodev, nosuid and noexec bits. Every propagated root clears its in-place lock. Descendants retain
theirs.

### Modern mount API and fd lifetime

`walk_at` honors per-operation empty-path and final-symlink flags. Empty without permission gives
`ENOENT`. Otherwise AT_FDCWD selects cwd, or an fd supplies its node (`EBADF` if absent) and a linked
dentry for a non-directory. Named lookups lift during the walk. `..` lifts after stepping up but stops at the task root. No-lookup results such
as dot, the starting root, an empty-path fd or magic-link target stay on the reached mount.

- `open_tree` accepts `AT_EMPTY_PATH`, `AT_NO_AUTOMOUNT`, `AT_SYMLINK_NOFOLLOW`, `AT_RECURSIVE`,
  `OPEN_TREE_CLONE` and `OPEN_TREE_CLOEXEC`. It rejects
  unknown flags or recursive without clone (`EINVAL`), then checks `may_mount` only for clone, before
  lookup. Plain open returns an O_PATH description with a path when available, using a synthetic name for
  internal namespace files. Clone returns an O_PATH description pointing to a **parentless mount on the
  attached list in an anonymous namespace** held by `OpenFile::tree`.
- A procfs, detached or derived namespace file outside an attached nsfs root gets a fresh nsfs clone with
  namespace and inode pins. An exact attached nsfs root follows regular cloning, skips source namespace
  membership, inherits pins and clears `root_at`. This path rejects unbindable sources. Other ordinary
  sources must be attached in the caller's namespace. Nonrecursive clone checks locked children. Recursive clone
  uses the bind copy plan. The clone root keeps instance metadata and flag locks, clears expiry and
  in-place lock. Children keep their locks.
- `alloc_anon_ns` checks and charges `Counted::Mnt` using the mount namespace owner and caller's effective
  UID (`ENOSPC`). Both forms pin FUSE paths and nsfs inodes as applicable before `Cx::install_fd`. CLOEXEC
  follows `OPEN_TREE_CLOEXEC`. Installation takes the lowest free, unreserved slot below the caller's soft
  limit even with a shared fd table. `nr_open` limits table growth. Slot exhaustion or prohibited growth
  gives `EMFILE`. A failed clone installation can leave a dead anonymous namespace for collection.
- `sys_move_mount` checks capability before flags and walks source before target, each with its own
  symlink and empty-path flags. SET_GROUP uses the reached nodes. Normal moves share `move_tree` with
  legacy MS_MOVE. It lifts the destination and discards its dentry if changed. Source must be an exact
  attached root. Only a parentless live anonymous root bypasses source namespace membership. Destination
  must belong to the caller. An in-place lock or directory-status mismatch gives `EINVAL`. The point must
  pass `may_mount_on`. An attached source in a shared parent cannot move, nor can a tree containing an
  unbindable mount move onto a shared mount. Moving an attached tree inside itself gives `ELOOP`.
- `move_tree` finds the minimum mount-namespace ID pinned in the subtree. A caller ID at least that large
  gives `EINVAL` before quota planning. A recipient ID at least that large gives `EINVAL` after planning,
  before mutation (`mnt_ns_loop`). `attach_tree` retags all attached-list entries of the anonymous
  namespace into the caller's namespace without changing mount IDs (detached records retain their old tag)
  and sets the root point. `release_anon_ns` clears matching tree references on mutably borrowable
  fd-table and Unix-transfer descriptions. Open fds keep their mount IDs. When the last strong reference
  goes, the next count check can purge the weak charge. An ordinary move changes the point in place. Both
  apply shared-parent handling and propagate.
- `MOVE_MOUNT_SET_GROUP` requires exact attached roots in live non-anonymous namespaces and `SYS_ADMIN` in
  both owners. Only then check matching fs and device, and whether the retagged target root is at or below
  the source root. Source must have a group or master. Target must have neither, so an otherwise
  unbindable target qualifies. Assignment clears unbindable, using `of_bind` for equal owners and
  `of_slave_copy` for different owners. Points stay in place.

### Attribute preparation and pivot

`mount_setattr` checks flags, then size (over 4096 gives `E2BIG`, under 32 gives `EINVAL`), then
capability. Nonzero extension bytes give `E2BIG`. Zero set, clear and propagation succeeds before fd/path
lookup. Propagation must be zero or one known type. Unknown attributes give `EINVAL`. To change atime,
clear its whole mask and select relatime, noatime or strictatime. Nodiratime is independent. For ordinary
bits set wins over clear.

Clearing IDMAP or an out-of-range IDMAP fd gives `EINVAL`, a missing in-range fd `EBADF`. Only a Proc file
holding a user namespace passes the kind check. Initial user namespace gives `EPERM`. Other ID maps end in
`EINVAL`, all before walking. The reached node must be an exact attached root. Only a parentless anonymous
root bypasses caller-membership checks. A valid recursive operation includes its attached descendants.
`set_mount_attrs` checks every lock before computing writers. Only after both checks pass does it update
flags and optional propagation. Actual flag changes notify once per batch.

`pivot_root` requires `may_mount`, then resolves new_root as a directory before put_old (file new_root
gives `ENOTDIR` regardless of put_old). Only put_old gets an extra lift. All three mounts must belong to
the caller's namespace. Shared put_old mount, shared parent of either root, or locked new_root gives
`EINVAL`. Next, an unlinked store directory at new_root or put_old gives `ENOENT`. Either path in the
current root mount gives `EBUSY`. Both roots must be exact mount roots with put_old at or below new_root
and new_root at or below the current root.

Three point changes detach new_root from its point, place the old root on put_old, then place new_root at
the old root's former point/dentry. The old root's in-place lock transfers if set. For each task, root and
cwd independently switch only on full-node equality with the former root, including mount ID.

### Unmount, expiry and holder accounting

`umount2` rejects unknown flags (`EINVAL`) before reading the path. `resolve_mountpoint` walks without
clearing expiry, following the last symlink unless UMOUNT_NOFOLLOW. Walk errors precede `may_mount` and
the extra initial-user-namespace `SYS_ADMIN` for force (`EPERM`). Then lift the reached node and require
an exact, attached, in-namespace root without an in-place lock (`EINVAL`). “Own root” means the mount
containing the caller's root directory.

- EXPIRE with force, detach or own root gives `EINVAL`. A direct child or holder gives `EBUSY`. The first
  eligible request sets the mark and returns `EAGAIN`. A still-marked eligible request continues. Ordinary
  access through `touch_mount` clears a mark. Propagated busy checks come after first-pass marking.
- Valid `MNT_FORCE` on a FUSE node aborts the connection before checking for a parentless root or final
  busy holders. A parentless root gives `EINVAL` with detach or `EBUSY` otherwise. Without detach, own
  root, direct children, holders or eligible busy propagation counterparts give `EBUSY`. Lazy detach skips
  these final busy checks. Success calls `umount_tree`, collects against a fresh holder set and advances
  the fast-path epoch.
- `try_in_use` scans task roots/cwds, VMA mount pins in distinct address spaces, fd-table, Unix-transfer
  and loop backing descriptions, and executable mount IDs in groups with live threads. `note_file_mount`
  counts node mounts, filesystem-opened mqueues and pinned PTY endpoints. Standalone mq_open fds and
  sockets have no such holder. `ExeFile::mnt` includes store, overlay and FUSE. Unmount expects a complete
  scan.
- `propagated_busy` tests a counterpart in the parent's propagation tree when it has zero children or one
  covering its root. Its own holder ID blocks regardless of lock. `umount_tree` selects the original
  subtree, finds counterparts per shared-parent group and normalized place, then marks candidates as
  non-covering children go. Unlocked marked candidates seed extra removals, which include their
  non-covering descendants. Surviving children inherit the place of the highest consecutive removed
  ancestor.
- `detach` moves records to the detached list and makes them private. Only a locked mount whose distinct
  parent leaves in the same batch keeps its point/dentry. Existing references can still traverse those
  coverings. Namespace teardown and mount-point name removal detach locally without unmount propagation.

### Copying and entering namespaces

Clone rejects `CLONE_NEWNS | CLONE_FS`. Unshare with NEWNS implies unsharing fs. Both check namespace
limits and call `new_mnt_ns` with the resulting credentials' user namespace. Without a VFS only the
namespace object changes. `copy_ns` snapshots attached source mounts, omits each mount pinning a mount
namespace and its descendants, then preallocates all surviving IDs. Parents can follow children in list
order after a move or pivot. Root, point and dentry parents are retagged when mapped. The caller
translates root and cwd likewise, keeping original IDs for unmapped references.

Instance devices, owners, pins and deleted-root state survive copying. Expiry clears. Same-owner copies
retain propagation and locks. A different owner applies `of_slave_copy` and `locked_tree` even to the
root. The VFS registers the new namespace weakly. `find_ns` returns the initial namespace or upgrades a
weak reference. `pinned_ns` searches both mount lists for a held namespace.

Setns validates all requested kinds before commit. Mount entry requires `SYS_ADMIN` over the target owner
and both `SYS_CHROOT` and `SYS_ADMIN` in the current or simultaneously requested user namespace, using
original credentials. `EPERM` precedes mount-only rejection of a shared fs (`EINVAL`). A combined pidfd
switch can privately copy a shared fs, preserving umask, if all kinds allow it. User-namespace entry
itself rejects a shared fs. Root and cwd both become the topmost mount on the target namespace's
parentless root.

### Removed mount points and deferred collection

Name removal checks for a local mount point (`EBUSY`). Store matching uses inode plus recorded
directory/name when available. `roots_removed` marks matching bound source names gone on both lists,
recording a former dentry for a root without one on last unlink. Overlay remove and overwriting rename
call `detach_overlay_point` by overlay ID across bind aliases and mark matching roots gone. Both store and
overlay point removal detach subtrees on the deleted node, set `collect_pending` when mounts leave and
notify affected namespaces.

- `FsContext` reports replaced/dropped root and cwd. `OpenFile::drop` reports its mount, a distinct node
  mount, a pinned PTY and a wildcard for a held tree. VMA `MountPin` and `ExeFile` report their mount on
  drop. Per host thread, `released` deduplicates IDs and collapses a seventeenth distinct release to
  `ANY_MOUNT`.
- At syscall end and thread exit, `collect_released` and `collect_unused` first attempt unlinked-tmpfs
  eviction. `released_detached` consumes this thread's vector and the kernel-wide pending bit. Pending, a
  matching detached ID, a leading wildcard with any detached mount, or a dead weak namespace triggers a
  fresh scan. Unavailable task resources, file, group, loop or Unix channel make `collect_now` call
  `defer_collect` for retry on any host thread. Unix holders include blocked sends, packet rights and
  pending receive delivery. Installed fds take over at delivery.
- `collect` clears pending, prunes dead namespace references/events and detaches mounts outside the live
  set. `keep_used` retains held detached mounts plus connected descendants, disconnecting a surviving
  child whose parent goes. Both lists determine instance liveness. Remove unmounted overlays and nodes,
  then unused FUSE instances (abort a borrowable connection, otherwise wake readers). Live overlay layers
  retain their tmpfs devices. Prune unused procfs/sysfs/devpts entries and dead tmpfs ranges, then remove
  detached tmpfs directories outside the table borrow. Removal errors are logged. Collection repeats if
  dropping records releases the last strong reference to another registered mount namespace (`has_dead_ns`).

## Dependencies (inbound / outbound)

### Inbound

- Syscall dispatch runs mount collection after Unix/FUSE sweeps, including before a successful exec's
  ptrace stop. Thread teardown collects after releasing task resources. Clone, unshare and setns consume
  the namespace API.
- VFS walking, stat/statfs and file operations use mount traversal, flags, devices and removal hooks. FUSE
  and overlay node sweeps retain `mount_nodes` roots, points and both dentry parents across both lists.
- Procfs consumes `mounts_from` for the target task's root and namespace, omitting unreachable paths.
  Mountinfo prints 0 for a parentless root, root-in-filesystem (with `//deleted`), flags, propagation
  tags, type/subtype, source (`none` if empty) and superblock options. `/proc/<pid>/mounts` combines mount
  and superblock read-only. `pseudo::poll_event` attaches the target namespace's counter at open.
- Sysctl exposes `Kernel::mount_max` (default 100000, range 1 through `i32::MAX`). Procfs, sysfs, tty and
  IPC read PID/network namespace views, devpts options and mqueue devices. Cgroup pseudo nodes delegate
  file visibility and generation to the cgroup tree.

### Outbound

- VFS/store provide walking, ancestry, link counts, boot locking and detached roots. Tmpfs, devpts,
  overlay and FUSE supply instance state. FUSE owns connection abort and eviction, overlay its node cache
  and layer pins.
- Credentials and namespace code supply capability checks, references and counts. Inotify supplies nsfs
  inode pins. Task, fd, memory, exec, Unix and loop code supply holder identities. The caller's fd
  allocator enforces descriptor limits.

## Invariants and gotchas

- Preserve both indices on mutation. Append with `push_attached`, change attachment with `set_point`,
  rebuild after vector repartition or bulk edits. `update_mount` debug-asserts unchanged ID and parent and
  masks flags. List position is mountinfo order. Find the namespace root by its missing point,
  particularly after pivot.
- Traversal from an existing reference can consult detached entries even when an attached-root or caller
  namespace check fails. `mounted` prefers the latest attached match, then detached, and builds a point
  index after eight successful lifts. Static `Index` work is per pass. Materializing full slave master
  chains and copying a subtree to every recipient add costs beyond a single linear table scan.
- `mounts_changed_ns` rebuilds cached store mount points, fast-path eligibility and the attached expiry
  count. It increments/wakes each distinct live non-anonymous namespace supplied **per invocation**.
  Counters start at one. One syscall can invoke the helper several times. The shared fast-path epoch is
  separate. Propagation-only updates, expiry marking and root-name marking alone generate no mount poll
  event.

## Confirmed bugs / technical debt

- Ordinary bind roots drop instance owner and pin metadata (`owner`, `pin`, `ns_pin`). A full remount
  through such a bind can require initial-user-namespace authority for an instance owned elsewhere, and an
  ordinary mqueue bind lacks the source mount's IPC reference.
- The incremental device allocator can reach the fixed store device `ROOT_DEV` (`0x801`). Device-keyed
  superblock flags and busy checks can then conflate distinct instances. Collection also leaves the
  superblock-flag and mqueue device maps populated after instance or namespace turnover.
- Read-only accounting has holder gaps: `writers` omits writable descriptions held solely in Unix
  transfers or by a FUSE `Fill` mapping; `sb_busy` omits the FUSE executable record, tests a
  mapping-only overlay holder by backing-store link count (which can disagree with overlay visibility),
  and tests a `FuseSnapshot` VMA's tagged FUSE key as a store inode instead of checking FUSE deletion.
- `bind_ns` checks pinned mount-namespace age only against the caller before propagating copies carrying
  that pin, without rejecting propagation recipients as new as or newer than the pinned namespace (unlike
  `move_tree`).
- An explicit `released(ANY_MOUNT)` following ordinary IDs can be appended mid-vector. The consumer
  recognizes a wildcard only in the first slot, so that hint alone can miss a detached rescan (dead-namespace
  detection and the pending bit remain independent triggers).

## Open questions

Should the cgroup2 same-instance check distinguish roots from different cgroup namespaces? The current
check at a cgroup2 mount root uses filesystem kind alone.

## To verify

Linux 6.1 source parity remains unverified for superblock ownership and read-only holder accounting,
namespace-file ordering, propagation copy/group order, and mount walk, lock and unmount error precedence.
Historical Linux 6.8 observations cover individual cases.
