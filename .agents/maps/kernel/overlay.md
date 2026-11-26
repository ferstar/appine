# kernel/overlay Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/overlay.md` for the paired decision history (decisions from before the split are D39, D40 and D49 in `.agents/decisions/kernel/vfs.md`, and D45 and D46 in `.agents/decisions/kernel/process.md`).
>
> Verified: 2026-11-26 (full refresh; 15 adversarial claims independently verified — 12 confirmed, 3 partial, 0 refuted; all partial statements, audit findings and confirmed bugs applied)
>
> Maintained: 2026-11-27 (targeted verification only since the full mapping; latest change: per-instance origin, impure and opaque xattr caches with rollback-safe updates and upper-removal eviction)

## Responsibilities

The overlay presents an ordered set of store directories as one filesystem. Reads resolve a visible
name through its layers. Namespace and metadata changes go to an optional upper directory. The
module owns the layer options, merged lookup and listing, copy-up, whiteouts, overlay identity,
and reclamation of cached names. Its root is `core/kernel/src/vfs/overlay.rs`. The VFS and syscall
adapters connect these operations to mounts, open files, permissions, tmpfs quotas and watches.

## Key types

- `Node::Overlay(MountId, OvlId)` identifies a cached name and the mount through which it was
  reached. `Overlays` allocates increasing IDs across instances in one VFS. Bind mounts share the
  instance and its node IDs.
- `Instance`, indexed by overlay device number, holds the upper root, ordered lower roots, mounter
  filesystem IDs, pinned tmpfs devices, `Options` and per-instance private xattr caches (`origins`
  storing `Absent`/`Invalid`/`Valid(Ino)`, `impure` booleans, and `opaque` booleans).
- `OvlNode` holds the parent ID, name, instance device, upper and participating lower inodes,
  validated origin, cached children, recency clock (`seen`) and syscall stamp (`touched`). The
  instance root is its own parent. `real()` selects the upper or first lower inode.
- `CopyUp::{All, Copied, NoData}` select share-if-possible, immediate data copy, or metadata with an
  empty regular-file object. `Attrs::Overlay` carries the overlay ID through attribute changes.
- `Hold` increments a process-wide reference count by overlay ID and decrements it on drop.
  `OpenFile::ovl` records overlay identity for supported backing-file kinds, `ovl_holds` owns
  references to named nodes, and `ovl_lower` keeps a replaced host handle alive. `ExeFile::ovl`
  includes a hold on the executed overlay node.

## Public entry points

- Mounts and traversal use `overlay_layers`, `overlay_dir`, `Vfs::new_overlay`, `overlay`,
  `overlay_child`, `overlay_stat`, `overlay_readdir` and `overlay_prefix`. `overlay_child` returns the
  overlay child before crossing any mount on it.
- File and namespace syscalls use `open_overlay`, `follow_copy_up`, `copy_up`, `unlink_overlay`,
  `link_overlay` and `rename_overlay` in `syscall/fs.rs`. `Vfs::overlay_upper`, `overlay_remove` and
  `overlay_rename` delegate to `Overlays::upper_of`, `remove` and `rename`.
- `syscall::mount::sweep_overlays` gathers external references and calls `Vfs::sweep_overlays`.
  `Overlays::remove_instance` handles whole-instance teardown during mount collection.

## Data flow / lifecycle

### Mount creation and visibility

`Options::parse` splits on unescaped commas, skips empty options, and uses the last supplied
value for a repeated key. Unknown keys, malformed switches and invalid enum values yield
`EINVAL`. With no upper, parsing discards workdir, index and volatile before checking conflicts.
`metacopy=on` conflicts with explicit redirect off or nofollow. `nfs_export=on` conflicts with
explicit index off or metacopy on. `userxattr` conflicts with index on, metacopy on or redirect on.

After mount resolves and checks the target and parses options, `overlay_layers` checks lower-path
syntax first. It splits on unescaped colons and unescapes each component. A missing or empty
component or more than 500 lower paths gives `EINVAL`. An upper requires nonempty upper and work
paths. It resolves the upper and checks writability before resolving work. Both must use the same
mount ID and neither directory may equal or contain the other. Without an upper, at least two lower
paths are required. Each path resolves from the caller's root and cwd, following symlinks. A layer
must be a store directory in the caller's mount namespace with no locked child mount under it.
Path-resolution errors propagate.

After resolving **all** lowers, the overlap check returns `ELOOP` for a lower equal to or beneath
upper or work. Mount setup replaces any existing `work/work` tree in a store transaction with an
empty directory of mode zero owned by the mounter's fsuid/fsgid. This precedes `plan_attach`, so a
later attachment-planning failure can leave the reset visible. Copy-up subsequently creates objects
directly in the upper. `new_overlay` installs the instance root on its new mount device. The
instance records tmpfs pins while resolving upper, work and lower paths in that order.

A lower-only instance contributes `RDONLY` to its superblock flags across all binds. A read-write
filesystem remount returns `EROFS`. A bind remount can change per-mount flags while effective
access stays read-only. Mountinfo displays the mount and superblock flags separately. The mounts
listing and statfs include effective read-only status. `Options::show` emits the supplied layer
paths with mount-option escaping, then selected nondefault options. It omits `xino`.

### Lookup, origin and directory listing

Lookup probes the upper child, then the parent's lower directories from highest to lowest. A
character device with zero device number is a whiteout. An upper whiteout makes the name absent.
A lower whiteout stops the scan while preserving any already found higher directory. A
non-directory wins only as the first visible entry and stops the scan. A looked-up child directory
contributes to the merge through the first directory marked `opaque=y`, inclusive; instance
construction installs all supplied lowers on the root without checking root opacity, and root
`readdir` iterates all of them. Marker names use the instance's private prefix, `trusted.overlay.`
by default or `user.overlay.` with `userxattr`. Layer traversal uses store lookups, so submounts
inside those directories are invisible. Directory search, readlink and executable loading use the
topmost real inode.

A successful lookup refreshes a cached child under the same parent/name or allocates a fresh
node. For an upper child, `Instance::origin` first consults the instance cache. On a miss it reads
the private origin xattr. Its exact encoding is `OVL`, version byte 1, then one little-endian
store inode number. The candidate must be a linked inode of matching type with a positive inode
number smaller than the upper's, at or beneath a mounted lower root. Non-directories must have
exactly one lower link. Missing, malformed or stale handles give no origin. Other store errors
propagate. `Instance::origin` caches all three outcomes (`Absent`, `Invalid`, `Valid(Ino)`), and
`lookup`/`readdir` also cache `opaque` (upper and lower directories) and `impure` (upper
directories) per instance; cache hits skip on-disk xattr reads and validation for that instance.
`has_origin_xattr` consults `origins` and caches absence on a miss.

`stat` uses the real inode's metadata, the overlay device and the origin inode number when
available. A directory with more than one participating layer reports one link. Path stat uses the
real backing inode for tmpfs block counts and inode flags. `readdir` keeps the first entry for each
name, suppresses whiteouts and emits entries in byte-name order. For upper entries in an impure
directory it calls the origin resolver. In a pure directory it can reuse a matching cached child's
origin, including a cached absence. A cold listing still calls the origin resolver even if the
parent has no impure marker. `getdents64` adds dot entries and caches the resulting list on the
directory description. `statfs_of` reports overlay magic and store capacity even when writes draw
on a quota-limited tmpfs upper.

### Copy-up and isolation

`copy_up` returns an existing upper immediately. For a full regular-file copy-up (`All`), it checks
for a shared mapping of the host object's device/inode in any of the kernel's tasks that can
acquire write permission, or a registered host writer on that object. Either selects `Copied`. On
the sharing path, `Store::try_share` probes **both** backing objects under the store write
transaction. A conflicting mapping pin, including a read-only pin in another kernel, makes the
caller copy bytes. Successful sharing points both store inodes at one held object. When the share
candidate has a non-COW lower writer, `copy_up` calls `rehome`, which updates each matching
description only if it can borrow it and reopen its host object (skipping it on either failure).
Writable descriptions of a held object reopen read-only and carry the COW flag. `prepare_write`
splits held data before mutation.

`upper_of` recursively copies missing parents first. Each missing ancestor completes its own
copy-up store operation before its child's operation starts (each releases its savepoint, though
SQLite may defer the enclosing database commit); a later child failure can therefore leave a copied
parent. The operation recreates the directory, regular file, symlink target or special node with
the lower owner, permission bits for directories/files/special nodes, and device number where
applicable. It copies all xattrs outside the active private prefix and sets the lower inode flags
on the upper. For a directory or singly linked non-directory it writes the origin handle and marks
the receiving upper directory `impure=y` in the same operation (`mark_impure` evicts `dir` from the
`impure` cache during the transaction so rollback cannot poison it). It restores the copied inode's
atime/mtime and the upper parent's prior atime/mtime. After success the node gains an upper, and
the instance caches `origins` (`Valid(lower)` or `Absent`), `impure: true` on the upper parent when
an origin was recorded, and `opaque: false` for a copied-up directory. A non-directory clears its
lower vector. A directory retains its lower directories. An uncopied regular file truncated to zero
uses `NoData`.

### Tmpfs accounting

`backing_tmpfs_dev` identifies the device for namespace and data changes. For an overlay,
`overlay_upper_tmpfs_dev` follows the upper's parents to its store root. A detached store root
identifies a tmpfs upper, whose device is the first layer pin. A tmpfs lower alone does not make
upper writes chargeable to it. For mappings, `Vfs::mapped_tmpfs_dev` instead identifies the
layer supplying the actual inode through its parent/name, with an unlinked-inode fallback.

`overlay_upper_charged` counts distinct uncopied nodes through the target and its ancestors.
It checks room for those inodes and the data pages before copying, then charges after success.
A regular `NoData` copy needs zero pages. With tracked lower tmpfs ranges, a full copy uses
`Copied` and counts data pages within EOF, including instantiated mapping pages. Only data ranges
transfer to the upper's tracking. Unused reservations stay on the lower. Otherwise the helper
uses `tmpfs_pages`, including the charge for a long symlink. Regular writes, truncate and
fallocate use upper-device accounting for page changes.

### Open files, mappings and metadata

`open_overlay` takes scoped holds on the target and its supplied overlay parent before a lease
break or FIFO open can release the big kernel lock (BKL). A rename during the wait can move the
target away from its original parent. For an existing regular file, write access **or**
`O_TRUNC` runs the read-only, permission and inode-flag checks, the `O_NOATIME` owner and
`O_DIRECTORY` checks, then executable write denial. `executing_overlay` checks both the backing
inode and the overlay ID held by a running group. Only then does it break overlay-keyed leases
and copy up. A nonblocking lease refusal therefore leaves the lower file uncopied.
`open_store_with` opens the chosen real inode. Regular-file `O_DIRECT` is decided by the mount
type, so an overlay can accept it with a tmpfs upper.

Directory and ordinary `O_PATH` kinds carry the overlay node directly. Regular files, device
and pipe kinds, PTYs with a device inode, and `O_PATH` reopens carrying a pinned regular backing
can record `OpenFile::ovl`. `OpenFile::node` gives that ID precedence. Regular fstat uses open
backing metadata with the displayed overlay inode number. Named FIFOs and device-node stat paths
use their node. Procfs reopens of a deleted overlay executable use the recorded host file,
the current real inode and the held overlay identity for metadata, leases and write checks.

A regular description opened on the lower can follow later copy-up for reads, fstat, lseek,
regular copy-out and new mmap. `follow_copy_up` compares backing inode numbers, swaps the host
handle, sets COW for a writable description of a held object, and retains the old handle in
`ovl_lower` for in-flight I/O. For mmap, `mapping_object` returns `EAGAIN` if the chosen object is
held; the caller runs `make_private` (settling a sole remaining store-inode user in place rather
than copying) and retries before pinning it. An earlier mapping keeps its backing object across
subsequent copy-up.

Path truncate checks file type, read-only state, immutable flag, write access, append-only flag,
executing identity and size limit before breaking the lease. It holds the target through the
overlay lease break and copy-up. Chmod, chown and utimensat resolve `Attrs::Overlay`, check the
shown metadata and copy up for the change. Timestamp updates via an overlay fd address the
resulting inode by number. `FS_IOC_GETFLAGS` on regular files and directories reads the supported
flags from the current real inode. `FS_IOC_SETFLAGS` checks read-only state, ownership,
`CAP_LINUX_IMMUTABLE` for protected-bit changes, then unsupported bits before copying up. Supported
flags are immutable, append-only, nodump and noatime.

Xattr reads use the real inode. Listing removes the selected private prefix before normal
namespace filtering. The overlay xattr handlers return `EOPNOTSUPP` for that prefix. Writes
check read-only state and generic visible-inode permissions before copying up; `security.capability`
value validation and `CAP_SETFCAP` checks run afterward on the copied upper inode. ACL changes also
check the shown protected flags and ACL rules. Removing a missing lower-only ACL can fail before
copy-up.

### Creating and linking names

`may_create_overlay` checks parent search permission, visible existence (`EEXIST`), read-only
state (`EROFS`), then the parent flags and write/search permission. Non-directory creation has
a separate trailing-slash check. Mknod checks device capability/filtering where required and
rejects an explicit whiteout with `EPERM` before parent copy-up. `check_overlay_create_tmpfs`
checks the missing parent chain and new name together. Replacing an upper whiteout needs zero
additional name inodes. `create_dir` obtains the charged upper parent. `create_in` removes the
whiteout, creates the entry, applies any supplied default ACL, and marks a replacement directory
opaque in one transaction. Named file creation relooks up the overlay child for the returned fd.

`link_overlay` checks destination existence and trailing slash, search, read-only state, mount
identity and link permissions before copying the source. It copies the source and destination
parent as needed, reserves the destination name, and links in the upper. The receiving directory
gets an impure marker in the link transaction if the node has an origin **or the private origin
xattr exists**. This marking step needs no handle validation. AF_UNIX pathname bind creates a
socket inode through the upper parent. Connect resolves the visible real inode for the socket
registry and checks write permission and socket type.

### Removing, renaming and detaching mounts

For unlink/rmdir, special dot names precede read-only state and lookup. Trailing slash,
parent search, deletion permissions, protected flags and victim type precede the local
mount-point check. `EBUSY` precedes rmdir's merged-view `ENOTEMPTY`. `lower_has` uses the first
lower occurrence of the name, including a whiteout there. Removal copies the parent as needed,
removes the upper entry and residual whiteouts in an empty directory, and installs a
mounter-owned whiteout if the name would reappear from below. It unhooks the cached child and
records it as an orphan. Parent copy-up, whiteouts and removed upper entries enter the tmpfs
calculation. A last-link regular victim still open or mapped retains its charge.

Rename rejects different mount IDs before its dot-name checks. Within one overlay it checks
trailing slashes (ORing source and destination trailing-slash flags and testing only the source
type), the visible source and destination permissions (including write access on directories moved
across parents), then local mount points. The same node, or two names of the same real inode
outside exchange, returns success here, even with `RENAME_NOREPLACE`. Moving non-directories are
copied up **before** the remaining tmpfs check and `Overlays::rename`. That method checks
destination lookup (required for exchange), no-replace, incompatible types and merged emptiness
before returning `EXDEV` for a moving directory with lower layers, including the reverse side of an
exchange. An ordinary overwrite only needs the destination's existing upper, if any.

The upper rename transaction removes a hidden destination whiteout or residual whiteouts in a
replaced directory. An ordinary move leaves a whiteout at the old name if a lower would show
through. A moved directory becomes opaque when its new parent has lower directories. Exchange
applies opacity in both directions as needed and leaves no source whiteout. Cross-parent moves
mark the receiving directory impure by the same origin-presence rule as link, in both directions
for exchange. Committed `rename` publishes the newly marked `impure` and `opaque` entries into the
instance caches. The source ID follows its new name. Exchange moves both IDs. Ordinary overwrite
orphans the displaced ID. Unlink, rmdir and overwrite drop cached `origins`, `impure` and `opaque`
entries (`forget_upper`) on directory removal or loss of an upper inode's last link. A surviving
hard link can still report the same displayed number.

`Vfs::is_point_in` compares overlay IDs across bind aliases within the caller's namespace.
After removal or ordinary replacement, `detach_overlay_point` marks attached **and detached**
mounts rooted at the lost ID `root_gone`. When any attached mount point matches that ID, it also
detaches those mount subtrees, schedules collection and notifies affected namespaces (matching only
mount roots marks `root_gone` and returns without scheduling collection or notifying namespaces). A
source mount in another namespace follows the moved ID. With active watches, `copy_up` reports new
upper entries in parent-first order only after a successful copy-up (`link_overlay` copying a
separate destination parent via `create_dir` omits `IN_CREATE` for that parent). Removal, link and
rename report upper changes before overlay events.

### Node reclamation and instance teardown

`InSyscall` records a start counter and syscall number. `post_syscall_sweeps` calls the overlay
sweep after detached-mount collection and before clearing that stamp. A pass is due at
`max(1024, sweep_at)` orphans or **above** `max(16384, nodes_sweep_at)` total nodes.
`syscall::mount::sweep_overlays` gathers every task's root/cwd, attached and detached mount
roots and points, the recorded parent directories in `Mount::at` and `root_at`, and overlay
inotify watch keys. If a task's resources, filesystem context or the watch registry cannot be
borrowed, it defers the whole pass.

`oldest` is the earliest active start stamp among *other* tasks outside the explicit
`waits_on_files_only` syscall-number list, or `u64::MAX` if there are none. Listed calls include
read/write, polling, futex and child waits. An unlisted path syscall can retain a walked ID
while the BKL is released. `Overlays::sweep` snapshots the holds before examining the cache.

- For orphans it traverses normal children and separately indexed orphan children. It keeps a
  candidate subtree if the traversal encounters an external reference, a hold, or an orphan
  **removed** at or after `oldest`. It sets `sweep_at` to twice the remaining orphan count.
- If more than 16,384 nodes remain, live eviction pins roots, external references, holds,
  surviving orphans and nodes **touched** at or after `oldest`. It walks ancestors of the pins
  and walks children under surviving orphans (skipping descent past any already-pinned intermediate
  child). It evicts batches of unpinned leaves in `(seen, id)` order, removing matching entries
  from their parents, until 12,288 nodes remain or no eligible leaf exists. The next size trigger
  is strictly above `max(16384, remaining + max(4096, remaining / 4))`. Calls to `node` and
  `node_mut` update the syscall stamp independently of the lookup-recency clock.

An evicted live name can receive a new ID at its next lookup. The instance private xattr caches
(`origins`, `impure`, `opaque`) survive eviction, and persisted origin handles can restore displayed
identity after remount. `Vfs::collect`
drops an instance and all its nodes only after its device disappears from both attached and
retained detached overlay mounts. It includes the remaining instances' layer pins when collecting
tmpfs devices.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/mount` creates and collects instances and exposes their superblock options.
  `kernel/process` dispatches the syscall-end sweep.
- `kernel/vfs`, `kernel/files`, `kernel/fileio` and `kernel/xattr` route node operations here.
  `kernel/mm` follows the current real file for new mappings. `kernel/unix` routes pathname
  socket bind and connect through the merged namespace.
- `kernel/locks` and `kernel/inotify` consume overlay keys. Exec, open descriptions and procfs
  carry overlay identity across execution, fd transfer, unlink and magic-link reopen.

### Outbound

- `kernel/store` supplies inode and dentry queries, transactions, xattrs, flags, host objects,
  sharing and mapping pins. `kernel/vfs` supplies tmpfs page ranges, capacity checks and charges.
- Mount tables supply namespace membership, mount references and detachment. Credentials and
  file helpers check access, open backing objects, break leases and prepare COW writes.
- The sweep reads task syscall stamps, filesystem contexts, mount records, holds and watch keys.
  Copy-up and namespace operations issue inotify events to the relevant upper and overlay keys.

## Invariants and gotchas

- `Node::ino()` yields no store inode for an overlay. Use `real()` for backing data and
  `shown_ino` for displayed identity. Keep `Node::Overlay` for permissions, copy-up, watches
  and mount lifetime. `store_dir` returns `EXDEV` for overlay directories. Creation obtains the
  upper via `create_dir`.
- `overlay::key` tags a cached node ID for inode-keyed tables. Copy-up preserves that ID.
  Distinct hard-link names can share a displayed inode while receiving different regular-file
  lock, lease and watch keys. The directory/name cache, instance origin cache and open host
  handle each have different lifetimes.
- A raw ID held across a BKL release needs a scoped hold, a swept external reference, or the
  syscall age guard. In particular, exec retains a walked node through unlocked ELF loading
  and waits before installing the `ExeFile` hold. A relative FUSE create can retain an overlay
  start directory for `display_path`. Neither path qualifies for a blanket file-only exemption.
  Cross-kernel collisions in the process-wide hold table can delay reclamation.

## Confirmed bugs / technical debt

- Overlay and synthetic directories cannot supply layers. Directory redirect xattrs are never
  followed, even when the option parser accepts a redirect setting. Sysfs reports
  `redirect_always_follow` as N. Overlay directories also reject `O_TMPFILE` with
  `EOPNOTSUPP` after read-only and creation-permission checks.
- The no-BKL cached stat walk falls back at mount crossings and cannot serve overlay paths.
  Each whiteout uses a separate database character-device inode. Long-running unlisted syscalls
  can keep removed nodes after the last explicit holder drops them.
- Ordinary overlay rename between aliases of one real inode succeeds even with `RENAME_NOREPLACE`,
  and a distinct existing destination can return `EEXIST` after copying up and charging a lower
  regular source. Overlay rename also ORs source and destination trailing-slash flags and checks
  only the source type, which can misfire on cross-type `RENAME_EXCHANGE`.
- Copy-up leaves side effects on partial failure: a child copy-up failure can leave a copied parent
  uncharged (since `overlay_upper_charged` posts the chain's charge only after success) and without
  an inotify event; `security.capability` value validation and `CAP_SETFCAP` run after copy-up so
  a rejected set/remove can leave a copied, charged upper; `link_overlay` copying a separate
  destination parent via `create_dir` omits `IN_CREATE` for that parent.
- Two regular-file hard-link names can report the same `st_ino` while locks, leases and inotify
  watches use distinct keys derived from their per-(parent, name) overlay node.
- `getdents64`'s removed-directory `ENOENT` check covers only direct store nodes, so an open
  removed overlay directory can still serve cached entries.
- Surviving orphan subtree pinning during live eviction only pushes a child when newly inserting it
  into the pinned set, so an already-pinned intermediate child stops descent and leaves deeper
  unpinned leaves eligible for eviction.
- AF_UNIX bind uses the generic one-inode tmpfs reservation and charge after its whiteout-aware
  parent check. Replacing an upper whiteout with a socket can return `ENOSPC` at the inode limit
  or count one extra inode on success.
- Mount setup recreates `work/work` through raw store operations outside tmpfs quota accounting.
  A later attach-planning failure can leave the work directory reset.

## Open questions

None recorded.
