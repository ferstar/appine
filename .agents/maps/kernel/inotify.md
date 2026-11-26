# kernel/inotify Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/inotify.md` for the paired decision history (decisions from before the split are D13, D46 and D51 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-26 (full mapping, 12 claims independently checked across watch identity and limits, queue delivery, open and exec lifetimes, overlay and host events)
>
> Maintained: 2026-11-27 (targeted verification only since the full mapping; latest change: slab-backed open records with intrusive per-key lists and reusable name buffers)

## Responsibilities

`kernel/inotify` records guest filesystem events in memory. Syscalls report operations directly.
The Android Files provider supplies a separate stream of store changes. The module owns:

- instances (`inotify_init1`), their watches and event queues, the per-user limits and the reads;
- the per-kernel registry of watches, keyed by store inode number or by the key of an overlay,
  FUSE or cgroup2 node;
- the hooks the file syscalls call at Linux's fsnotify call sites, and the event sequences each
  reports;
- the `Opened` record retained by eligible open file descriptions, which gives fd-based events their directory and
  name and reports `IN_CLOSE_*`;
- replaying the Android Files app's changes as events (`host_change`), and cgroup2 file changes
  (`kernfs_modified`).

Root: `core/kernel/src/syscall/inotify.rs`.

Outside the module: where the hooks are called — the file syscalls and renames in `kernel/vfs`,
opens in `kernel/files`, executable loading in `kernel/exec`, file I/O in `kernel/fileio`, FUSE operations in `kernel/fuse`, cgroup2 file changes in
`kernel/cgroup`; readiness, epoll and `O_ASYNC` of inotify fds (`kernel/poll`);
`/proc/sys/fs/inotify/*` (`kernel/sysctl`) and the fdinfo text's place (`kernel/procfs`); the Files app's change list
(`kernel/store`) and the machine's `store_changed`, which finds each kernel and takes its BKL
(`kernel/process`); overlay node lifetime and the sweep that keeps watched nodes (`kernel/overlay`).

## Key types

- `Inotify` — one instance: watches by descriptor, the event queue with its byte total (for
  `FIONREAD`), an overflow mark and a count of events queued (the `EPOLLET` generation), the queue
  limit taken at creation, the creator's effective uid (charged for instances and watches) and a
  weak reference to the `Registry`, from which closing it removes its watches.
- `Event` — descriptor, mask, cookie and name (padded to 16 bytes).
- `Registry` (`Kernel::inotify`, one per kernel) — watch state in `Watches` (the (instance,
  descriptor) pairs on each key, per-user instance and watch counts, and `doomed` slot indices for
  removed inodes whose `IN_DELETE_SELF` waits for files still open through the removed name, cleaned
  up when all watches on the key disappear), the last rename cookie, the three limits under
  `/proc/sys/fs/inotify/`, the flat `records` slab with its intrusive `free` list and per-key head
  index `opened` (hashed with multiplicative `InodeHasher`), and the weakly tracked stashed nsfs
  inode instances (`ns_stashed`) with their monotonic instance counter.
- `NsPin` — a shared pin held by open nsfs file descriptions (`OpenFile::ns_pin`, including
  `O_PATH`, `open_tree` and `NS_GET_USERNS`/`NS_GET_PARENT` fds) and nsfs mounts (`Mount::ns_pin`,
  bind mounts and `open_tree(OPEN_TREE_CLONE)` mounts), mirroring Linux's `ns->stashed`. Dropping
  the last pin clears `ns_stashed` without removing existing watches.
- `Opened` — a lightweight `(Weak<RefCell<Registry>>, slot)` handle on a file description, an
  executed image or an ELF interpreter mapping, ref-counted via `OpenRecord::refs` in the registry
  slab. Each `OpenRecord` holds the file key, opening directory and pre-reserved `name` buffer
  (reused across renames and across slot reuse after close), close mask, unlinked flag
  (`IN_EXCL_UNLINK`), and intrusive doubly-linked list links indexed by `Registry::opened`. Dropping
  the last reference unlinks the slot, reports `IN_CLOSE_WRITE` or `IN_CLOSE_NOWRITE` (and advances
  `doomed`), clears `name`, and pushes the slot onto `free`.
- `ExecOpens` — logical opens and header-read counts retained while a FUSE lookup suspends exec.
  A retry reuses the matching prefix and closes the suffix when the key or dentry changes.
- Keys: store inode numbers stay below 2^62; overlay nodes take top bits 10 (`overlay::key`, also
  the lock tables' key), FUSE nodes 11 (`fuse::key`), cgroup2 files and directories 01
  (`cgroup::key`).

## Public entry points

- Syscalls `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`; `read` through
  `kernel/fileio`'s `IoPath::Inotify`; `FIONREAD` in `kernel/files`' ioctl dispatch.
- Hooks for the file syscalls: `active` (whether any watch exists), `path_event`,
  `path_event_unlinked`, `file_event`, `file_io_event`, `dir_event`, `cookie`, `opened`,
  `unlinked`, `renamed`, `deleted`, `watched_at`, `key_of`.
- `host_change` (the Files app's changes, from the machine's `store_changed`), `kernfs_modified`
  (cgroup2 file changes), `pin_ns` (nsfs dentry/inode pinning from open nsfs files and nsfs mounts).
- `Registry::has_open` (store inode cleanup), `Registry::watched` (the overlay sweep and FUSE's `in_use`), `Registry::watches` (cgroup observation);
  `Inotify::ready`, `queued`, `pending` and `fdinfo` (`kernel/poll`, `kernel/procfs`).

## Data flow / lifecycle

### Instances and watches

- `inotify_init1` accepts `O_CLOEXEC|O_NONBLOCK`; per-user instance (`EMFILE`) and watch
  (`ENOSPC`) limits and the queue limit come from `/proc/sys/fs/inotify/*` (defaults 128
  instances, 2^20 watches, 16384 queued events).
- Watches are keyed by store inode number, an overlay node's key, a FUSE node's key or a cgroup2
  node's key, whose changes `kernel/cgroup` reports as `IN_MODIFY`; watches on other synthetic
  nodes are accepted and counted. They receive no filesystem activity events. Explicit removal
  still queues `IN_IGNORED`. `inotify_add_watch` unwraps `/proc/<pid>/fd/*`
  (`OpenFd`) and `/proc/<pid>/exe` (`OpenExe`) magic links to watch the underlying store/overlay/
  FUSE inode (including memfds and unlinked files, checking `MAY_READ` against `store.attrs`), and
  records store-node `sdev` from `vfs.dev_of(&node)` so tmpfs watches show their tmpfs device in
  fdinfo. Adding a watch needs read permission on what the store or the overlay shows (a FUSE node
  `fuse_permission`'s; synthetic nodes are not checked);
  bind aliases of a node share one descriptor per instance. Store, overlay and FUSE use their
  respective node IDs; synthetic nodes are compared within their filesystem device. Namespace
  files (`PseudoNode::NsFile`) additionally compare the live stashed nsfs inode instance
  (`ns_instance`, keyed by namespace ID across aliases such as `pid`/`pid_for_children`): lookups
  while an open fd or nsfs mount holds an `NsPin` share one descriptor, whereas unpinned lookups
  (or lookups after the last pin drops) allocate a fresh instance and receive a new descriptor
  while leaving earlier watches active and removable. Adding an
  existing watch replaces its mask, merges it with `IN_MASK_ADD`, or fails with `EEXIST` for
  `IN_MASK_CREATE`. Other flags include `IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_EXCL_UNLINK` and
  `IN_ONESHOT`.
- read returns whole events (`EINVAL` if the first does not fit) and waits on the instance when empty. An empty nonblocking
  read returns `EAGAIN` and an interrupted wait returns `ERESTARTSYS`;
  `FIONREAD`, poll readiness and fdinfo read the instance. No fanotify (`ENOSYS`), no dnotify.

### Reporting

- Delivery skips the watch lookup when no keyed watch exists. A path event goes first to the parent
  directory's watches with the name (not for `*_SELF`), then to the inode's own. A full queue
  drops events and queues one `IN_Q_OVERFLOW` before checking duplicate tail merges; otherwise
  matching consecutive events can merge. Removal queues `IN_IGNORED` even when the watch
  omitted `IN_DELETE_SELF`. A matching one-shot event also removes the watch. The ignored
  record goes through the same capacity check as other queued records. Directory events carry `IN_ISDIR`, `IN_DELETE_SELF` and `IN_MOVE_SELF` never do.
- Hooks follow the fsnotify call sites: open (`IN_CREATE`, or `IN_MODIFY` for any truncating open
  including `O_RDONLY|O_TRUNC`, then `IN_OPEN`), mkdir, mknod, symlink and AF_UNIX bind, link
  (`IN_ATTRIB` on the source, then `IN_CREATE`), unlink (`IN_ATTRIB`, delete-self, `IN_DELETE`),
  rmdir (delete-self, `IN_DELETE`), rename (a cookie pair, then as `fsnotify_move`: a replaced
  object's `IN_ATTRIB`, with `IN_ISDIR` for a directory, the moved one's `IN_MOVE_SELF`, the
  replaced one's delete-self; exchange reported both ways), attribute changes (`IN_ATTRIB`,
  skipped for a no-op `chown(-1, -1)` that clears nothing; also for utimensat changing both times,
  `IN_ACCESS` or `IN_MODIFY` when only access or modification time changes, as `fsnotify_change`),
  writes, truncation and fallocate (`IN_MODIFY`), reads and getdents (`IN_ACCESS`), the last close
  of an open file (`IN_CLOSE_*`).
- `IN_EXCL_UNLINK` filters path-based open/access/modify/close events (`path_event_unlinked`,
  `file_io_event`, `Opened::drop`) on both directory and file watches when the dentry is
  marked unlinked (by `unlinked` on unlink, rmdir, overwriting rename, or host removal across store,
  overlay and FUSE, or when reopened from an unlinked fd); metadata and truncation calls
  (`file_event`: fchmod, fchown, futimens, ftruncate, fsetxattr/fremovexattr) are not filtered.
  Cgroup2 attribute changes through a path or a regular cgroup file descriptor also notify the directory.
  Cgroup2 nodes are never excluded.
- Path-based operations resolve the path a second time to find the directory, only when watches
  exist (`watched_at`, falling back to a FUSE node's recorded parent/name if cached attrs staled).
  fd-based events (I/O, truncation, close) go to the directory and name the open went through,
  recorded in `Opened` (the dentry a Linux open file references): every non-`O_PATH` open of a
  regular file, named FIFO, node-backed PTY, `FileKind::Device` or keyed directory, FUSE file or
  cgroup2 file — as well as `open_by_handle_at` and `memfd_create` — records one, whether or not
  watches exist. Store, overlay and FUSE `O_PATH` fds track the dentry with a zero close mask and
  emit no open or close event. A reopened FIFO gets its own record at the original dentry; memfds
  and their `/proc/<pid>/fd/*` reopens are not marked unlinked for `IN_EXCL_UNLINK`. Cgroup2 files
  report `IN_OPEN`, `IN_MODIFY` (via `file_io_event` on writes) and `IN_CLOSE_*`. Unlinks and
  renames update the records of open files,
  looking up records for the affected key even with no watches; after a deletion the records stay (marked
  unlinked), so non-excluded events still go to the original directory.
- Exec reports the program, nested scripts and ELF interpreter under the key each was opened
  through. Header reads report access even on an executable cache hit. An empty file or a file
  rejected with `ETXTBSY` reports open and close without access. The address space retains
  the main open and the interpreter's non-overlay file mappings retain theirs. Watches added after exec
  can therefore receive the last close. A suspended exec reuses logical opens and their read
  counts on retry, so repeating the same prefix does not replay its notifications.
- `IN_DELETE_SELF` waits until the last open file of the removed name closes (`deleted`); removing
  the last watch on a key (`inotify_rm_watch`, instance drop, or one-shot/`IN_DELETE_SELF`
  delivery) removes any pending `doomed` entry.
- Through an overlay the hooks report to the overlay's nodes, as Linux's VFS reports on overlay
  inodes and dentries: the node and its overlay directory with its name (an open file's events
  through the node it was opened through, whose upper inode copying up changes; path-based
  attribute changes and truncation find the node by resolving the path again), not the layers'
  inodes behind them, so changes made in a layer directly do not reach them. Removal checks the
  post-removal upper link count (or `st.nlink <= 1` for a lower-only entry) before emitting
  `IN_DELETE_SELF`, so unlinking one hard link while another remains never emits `IN_DELETE_SELF`.
  Operations through an overlay also report to upper-layer watches first: creations (`IN_CREATE`),
  copy-ups (`IN_CREATE` in the upper directory, including parent directories copied up by `copy_up`,
  `unlink_overlay` and `rename_overlay`), hardlinks (`IN_ATTRIB` on the upper inode and `IN_CREATE`
  on the upper directory), removals (`IN_ATTRIB`/`IN_DELETE_SELF`/`IN_DELETE` on the upper entry
  and `IN_CREATE` for any created whiteout), renames (`IN_MOVED_FROM`/`IN_MOVED_TO`/`IN_MOVE_SELF`
  plus `IN_CREATE` for any whiteout), and path/fd attribute changes (`chmod`, `chown`, `utimensat`,
  `setxattr`/`removexattr`).
- Files app changes (`host_change`, under the target kernel's BKL) map `Created`, `Removed`,
  `Moved`, `Opened` (write opens) and `Written` to the same event sequences; the provider reports
  no access, close-nowrite, read-open or attribute events. An `Opened` that gave a held file an
  object of its own calls `kernel/fileio`'s `host_unshared` before checking for watches. It moves
  open descriptions of that file to its current object. A committed split followed by a failed
  provider open replays only `Unshared`, without an open event. Shared mappings already pin their backing.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs`: the hooks from the metadata and namespace syscalls, renames (`renamed`) and
  removals (`deleted`); the overlay sweep reading `watched`.
- `kernel/files`: the hooks from opens (`opened`) and closes, and `FIONREAD`; `kernel/fileio`:
  I/O hooks and the instance's read.
- `kernel/exec`: logical opens and reads during execve and execveat.
- `kernel/fuse`: hooks for FUSE namespace changes; `watched` in `in_use`.
- `kernel/cgroup`: `kernfs_modified` for cgroup2 file changes.
- `kernel/sysctl`: the registry's limits behind `/proc/sys/fs/inotify/*`; `kernel/procfs`: the
  instance's fdinfo lines.
- `kernel/poll`: readiness (`ready`), the `queued` generation and the instance's wake object.
- The machine (`kernel/process`): `store_changed` calling `host_change` for the Files app.
- The dispatcher: the three syscalls.

### Outbound

- `kernel/vfs`: `Node`, path resolution (`read_path`, `watched_at`'s second walk), overlay and
  FUSE keys; `kernel/files`: `OpenFile`; `kernel/fileio`: `host_unshared`.
- `kernel/store`: the `Change` records of the browse API.
- Credentials supply the final `MAY_READ` check on store, overlay and FUSE targets.
- The kernel's wake objects (`request_wake_object`).

## Invariants and gotchas

- Skip a second notification-only path walk when `active` is false. Open-record bookkeeping,
  rename and unlink tracking, inode retention and provider rehoming also serve later watches or
  other kernel consumers and continue with an empty watch registry.
- A hook given the store inode behind something reached through an overlay reports to the layer,
  where no overlay watch sees it, so overlay paths must pass the node's key (`key_of`,
  `file_key`).
- Store inode numbers and the keys of overlay nodes (top bits 10), FUSE nodes (11) and cgroup2
  files (01) must never collide; a new kind takes a free pattern and every `of_key`, `id_of` and
  `is_key` check must test both bits. cgroup2 nodes are never taken as unlinked.
- Watches hold `Node` values without counting: the overlay sweep keeps watched overlay nodes and
  FUSE's forget scan keeps watched FUSE nodes, so both scans must keep reading the registry.
- The overlay sweep gives up for a round when the registry is borrowed.

## Confirmed bugs / technical debt

- **Overlay upper-layer event gaps (intentionally left out):** overlay file I/O and open/close
  events (`IN_OPEN`, `IN_ACCESS`, `IN_MODIFY`, `IN_CLOSE_*`) are not duplicated to upper-layer
  watches (each `OpenFile` carries one `Opened` record keyed to the overlay node), copy-ups emit
  `IN_CREATE` directly in the upper directory rather than a workdir-to-upper `IN_MOVED_TO`, and
  parent directory copy-ups inside `create_dir` creations (`open(O_CREAT)`/`mkdir`/`mknod`/
  `symlink`/`bind`) are not separately reported. For hard links created through an overlay and
  later all removed, Appine checks the real post-removal upper link count and emits
  `IN_DELETE_SELF` on the last remaining link (where Linux 6.1 with `index=off` drops the new
  link's dentry on `ovl_link` so the original overlay inode's `i_nlink` stays elevated and never
  emits `IN_DELETE_SELF`).

## Open questions

None.

## To verify

- Per-hook ordering against every Linux fsnotify call site (only spot-checked; the differential
  test `inotify` covers the common sequences).
