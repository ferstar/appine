# kernel/vfs Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/vfs.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 22 claims checked by 3 independent verifiers — 18 confirmed and 4 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: `O_SYNC`/`O_DSYNC` from open; POSIX locks in unshared fd tables; set-id chown as 6.2; AIO; module references; loop devices; readv/writev by kind; timerfd clocks; faulting buffers; Starnix's bugs; owners and lock ranges)

## Responsibilities

`kernel/vfs` is the layer between file-related syscalls and the filesystems behind them (the
inode store and the synthetic filesystems). It provides:

- path resolution into mount-aware nodes (Linux `struct path`), including symlinks, `..` across
  mounts, search permission, `protected_symlinks`, magic links and openat2's `RESOLVE_*`;
- the global mount table, boot mounts, `mount(2)`/`umount2`/`pivot_root`/`chroot` and the new mount
  API subset, mount namespaces' tables and their cleanup, tmpfs instances and their accounting;
- fd tables and open file descriptions, open flags, `fcntl`, and data I/O on regular files (host
  `pread`/`pwrite` without the BKL), `sendfile`/`copy_file_range`/`splice`;
- namespace and metadata syscalls (stat family, statfs, chmod/chown/utimensat/truncate/access,
  mkdir/mknod/unlink/rename/link/symlink, getdents64, getcwd/chdir) and extended attributes;
- in-memory pipes and FIFOs, advisory locks (`flock`, POSIX, OFD) and leases, device nodes and
  their in-process implementations (character devices and loop devices, the only block devices),
  eventfd/timerfd objects;
- inotify, including replaying changes made by the Android Files app;
- native AIO (`io_setup` to `io_pgetevents`), with I/O done at submit and pending polls.

Root: `core/kernel/src/vfs/mod.rs`, `vfs/dev.rs`, `fd.rs`, `pipe.rs`, `locks.rs`,
`syscall/fs.rs`, `syscall/mount.rs`, `syscall/xattr.rs`, `syscall/lease.rs`,
`syscall/inotify.rs`, `syscall/timerfd.rs`, `syscall/aio.rs`, `syscall/loopdev.rs`.

Outside the module: the store's own semantics (`kernel/store`), procfs/sysfs/cgroup2/devpts
content (`kernel/procfs`), terminals (`kernel/tty`), sockets (`kernel/net`, `kernel/unix`),
poll/epoll and the eventfd syscalls (`kernel/poll`), the message queues behind the mqueue
filesystem (`kernel/ipc`), the BKL, the no-BKL fast path and the namespace mechanism
(`kernel/process`), credentials and file capabilities at exec (`kernel/cred`).

## Key types

- `Node` — `Store(MountId, Ino)` or `Pseudo(MountId, PseudoNode)`; the mount it was reached
  through is part of its identity, so one directory mounted twice is two nodes. `Dentry =
  (Node, name)` is where a result was last found.
- `Pseudo` trait — what a synthetic filesystem offers the walker: lookup, parent, name, readlink,
  `follow`/`follow_dentry` (magic-link jumps), `magic`, `protected_symlinks`.
- `resolve` — openat2's `RESOLVE_*` bits; `SCOPED = BENEATH | IN_ROOT`.
- `Vfs` — the store handle, the mount table and the no-BKL resolver state.
- `Mount` — id, namespace, filesystem, root node, mount point (`None` for a tree root or an
  unmounted mount), the node and dentry it sits on, per-mount `MS_*` flags, device number, source,
  `MNT_EXPIRE` mark, pinned namespace.
- `Mounts` — the global list (also the mountinfo order), detached mounts, id and device counters,
  per-instance side tables (tmpfs, procfs pid namespaces, mqueue devices, devpts instances).
- `Fs` — Store, Tmpfs, Proc, Pts, Sys, Cgroup, Nsfs, Mqueue; which kinds get a new instance and
  their own device number per mount.
- `MntNs` — namespace id, owning user namespace, `anon` flag (open_tree clones). A task's
  namespace lives in its `FsContext` (root, cwd, umask, mount namespace), not in nsproxy.
- `TmpfsParams` / `Tmpfs` — options and usage (`(pages, inodes)`, counted lazily).
- `Cached` / `Pending` — the lock-free resolver used by fast-path stat (64-component stack).
- `FdTable` — slots (`FileRef` plus per-fd `cloexec`), the soft `RLIMIT_NOFILE`, `holds_locks`.
- `OpenFile` (`FileRef = Rc<RefCell<OpenFile>>`) — kind, access mode and status flags, position,
  display path, dentry, mount, owner/signal for `F_SETOWN*`, epoll/fasync bookkeeping, inotify
  `Opened` record, `pos_busy`, lock ownership, open_tree namespace, memfd data.
- `FileKind` — Host, Regular, Directory, Path, Pipe, Device, Pty, Socket, Epoll, EventFd,
  TimerFd, SignalFd, Inotify, Pidfd, MessageQueue, SeccompNotify, Bpf, Proc.
- `IoPath` — the route one I/O call takes, chosen so no borrow of the open file is held during I/O.
- `Pipe` / `PipeEnd` — buffer, capacity, reader/writer counts and open counters; FIFOs keyed by
  inode in a weak map.
- `Locks` (owners `Posix(Weak<FdTable>)` or `Ofd(Weak<OpenFile>)`, byte ranges, waiters),
  `FlockHolder`, `Lease`.
- `Device` — Null, Zero, Full, Random, Kmsg, LoopControl, Loop (tty, ptmx and pts are routed
  separately); a Loop open file also carries its `LoopOpen` (the device and whether it was opened
  `O_EXCL`), whose drop is the device's release.
- `Loops` (`Kernel::loops`, devices by number plus a disk-sequence counter) and `Loop` — backing
  open file (`None` when unbound), offset, size limit, size in bytes, flags, name, logical and soft
  block sizes, `BLKROSET` and disk read-only state, read-ahead, opener count and exclusive claim.
- `EventFd`, `TimerFd`.
- `Inotify`, `Event`, `Registry` (one per kernel, watches keyed by store inode), `Opened`.
- `AioContext` — one io_setup context: its capacity `nr`, completed `io_event`s, pending polls and
  the `dead` mark set by io_destroy; held by the thread group (`ThreadGroup::aio`), keyed by its id.
  `Kernel::aio_nr`/`aio_max_nr` back `fs.aio-nr` and `fs.aio-max-nr`.

## Public entry points

- **Resolution**: `Vfs::resolve`, `walk`, `walk_resolve(…, follow, flags)`, `resolve_parent`,
  `resolve_parent_with`, `found_path`, `path_of`, `reach`; `Cached::stat` (fast path).
- **Mounts**: `Vfs::open` (boot mounts), `mounts`/`mounts_in`, `copy_ns`, `anon_ns`,
  `clone_mount`, `attach_tree`, `subtree`, `collect`, `readonly`, `mount_flags`, `dev_of`,
  `fs_of`, `tmpfs_*`; syscalls `mount`, `umount2`, `pivot_root`, `chroot`, `open_tree`,
  `move_mount`, `mount_setattr` (`fsopen`/`fsconfig`/`fsmount`/`fspick` have no handler: ENOSYS).
- **fds and I/O**: `openat`, `openat2`, `close`, `close_range`, `dup`/`dup3`, `fcntl`, `pipe2`,
  `lseek`, `read`/`write` and the vector and positional forms, `preadv2`/`pwritev2`, `sendfile`,
  `copy_file_range`, `splice`/`tee`/`vmsplice`, `ftruncate`, `fallocate`, `memfd_create`,
  `timerfd_*`; ioctls `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`/`FIONREAD`, random ioctls, the
  block and loop ioctls on loop devices and the `LOOP_CTL_*` ioctls on `/dev/loop-control`.
- **Metadata**: `newfstatat`/`fstat`/`statx`, `statfs`/`fstatfs`, `fchmod*`, `fchown*`,
  `utimensat`, `truncate`, `faccessat`/`faccessat2`, `umask`, `mkdirat`, `mknodat`, `unlinkat`,
  `renameat2`, `linkat`, `symlinkat`, `readlinkat`, `getdents64`, `getcwd`, `chdir`/`fchdir`,
  xattr calls.
- **Locks**: `flock`, fcntl lock commands, `F_SETLEASE`/`F_GETLEASE`; `locks::closed`,
  `lease::break_leases`.
- **Devices**: `dev::ensure_dev_nodes` (store `/dev`), `dev::populate` (boot `/dev` tmpfs);
  `Loops::numbers`, `sysfs_exists` and `sysfs_read` (the `/sys/block` view for `kernel/procfs`).
- **inotify**: `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`, read; hooks `active`,
  `path_event`, `file_event`, `dir_event`, `cookie`, `opened`, `renamed`, `deleted`,
  `watched_parent`; `host_change`.
- **AIO**: `io_setup`, `io_destroy`, `io_submit`, `io_cancel`, `io_getevents`, `io_pgetevents`;
  `aio_released` (execve, group exit).
- **Kernel-internal**: `create_exclusive`, `unlink_path`, `write_at` (core dumps),
  `FdTable::with_host_stdio` (boot).

## Data flow / lifecycle

### Path resolution

- Start (`start_dir`): an empty path is `ENOENT` before the dirfd is examined (`AT_EMPTY_PATH`
  returns the fd's own node, or the cwd for `AT_FDCWD`); absolute paths start at the process root;
  `AT_FDCWD` at the cwd; otherwise the fd must carry a node (`O_PATH` fds do) and be a directory
  (`ENOTDIR`; bad fd `EBADF`).
- `walk_resolve` runs `walk_in` inside one store read snapshot. Empty components are dropped; a
  trailing `/` appends `.`, forcing a directory and following a trailing symlink.
- Per component: name over 255 bytes is `ENAMETOOLONG`; the current node must be a directory
  (`ENOTDIR`); search permission (store directories only, skipped when the credentials have
  `CAP_DAC_READ_SEARCH` or `CAP_DAC_OVERRIDE`); `.` is skipped; `..` follows Linux's
  `follow_dotdot` (stays at the process root, climbs stacked mount points at a mount root, takes
  the parent, enters anything mounted there; a namespace-tree root or detached root stays put);
  other names are looked up and then lifted to the topmost mount stacked on the result.
- Symlinks (intermediate, or last with `follow`): over 40 is `ELOOP`; `protected_symlinks`
  (default on) applies to the last component of store links only (`EACCES`); `NO_SYMLINKS` or a
  nosymfollow mount is `ELOOP`. Magic links (`/proc/<pid>/fd/*`, `cwd`, `root`, `exe`, `ns/*`)
  first run the procfs access check (`EACCES`; only for `cwd`, `root`, `exe` and `ns/*` — `fd/*`
  has none), then `NO_MAGICLINKS` (`ELOOP`), `NO_XDEV` across mounts and scoped resolution
  (`EXDEV`), and jump straight to the target node (a deleted regular file, pipe or socket jumps
  to the open file itself). Ordinary absolute
  targets restart at the root (`EXDEV` under `BENEATH`, or under `NO_XDEV` when the root has not
  yet been taken or the link is on another mount); relative targets continue from the link's
  directory.
- openat2: `open_how` size and flag validation per 6.1's `build_open_flags`; `BENEATH|IN_ROOT` is
  `EINVAL`; scoped resolution starts at the dirfd, and `IN_ROOT` absolute paths start there too.
  `RESOLVE_CACHED` is a pre-check only (`EAGAIN` for create, truncate and `O_TMPFILE`; 6.1's macro
  comparison also catches `O_DIRECTORY`), otherwise a normal walk.
- Creation (`resolve_parent_with`): trailing slashes trimmed; the prefix is walked with follow; a
  slash-free name returns `(cwd, name)` without a search check (callers check); `.`/`..` as the
  last name are left to the caller.
- `O_CREAT`: `O_EXCL` turns follow off and any existing name is `EEXIST`; on `ENOENT` a loop
  follows dangling store symlinks to create their targets (same 40-link cap and restrictions;
  `NO_XDEV`'s "root taken" rule is approximated).
- Back to paths: `reach` climbs through mount points (or the recorded dentry for mounts on files)
  and stops as unreachable at a namespace-tree root, a detached root, or a tmpfs root; a deleted
  directory is `ENOENT`. `getcwd` prefixes `(unreachable)`; `/proc/<pid>/{cwd,root}` are computed
  live against the reader's root; an fd's display path is computed once at open, with
  ` (deleted)` appended later when nlink is 0.
- `Cached` (fast-path stat without the BKL) is used only when the root is in the root
  filesystem's mount, the credentials search everything, that mount is attached and not
  nosymfollow, and the path is absolute or relative to a cwd in that mount. It returns `None` on
  any cache miss, more than 64 pending components, any store inode that is a mount point, and
  trailing links that `protected_symlinks` might refuse; it never enters synthetic filesystems.
  Mount table changes call `mounts_changed`, and syscalls refresh threads with `fast_changed`.

### Mount table and boot mounts

- One global table holds every namespace's mounts; each `Mount` records its namespace. Ids are
  never reused; the list order is the mountinfo order.
- Boot (`Vfs::open`): when it is the store's only user, the store clears its `/dev/shm` and
  `/run` (not `/tmp`). Mounts, in order: `/` (id 1, store, no mount point — Linux's invisible
  rootfs), `/proc` (2), `/dev` tmpfs (7, `mode=755`, nosuid and relatime only — neither nodev nor
  noexec; `dev::populate` fills it, falling back to the store directory if that fails),
  `/dev/pts` (3, devpts), `/dev/shm` tmpfs (8, 1777), `/dev/mqueue` (9, the boot IPC namespace),
  `/sys` (4), `/sys/fs/cgroup` cgroup2 (6, only if sysfs mounted), `/run` tmpfs (10, 20% of RAM,
  800k inodes); later ids start at 11. Id 5 is not a mount (fdinfo `mnt_id` of pipes and
  sockets). A mount whose point directory is missing is skipped; `/tmp` is a store directory.
- Instances: procfs, devpts and tmpfs mounts are new instances with their own device numbers;
  mqueue maps to the mounter's IPC namespace; sysfs and cgroup2 share the boot device, and a
  cgroup2 mount is rooted at the caller's cgroup-namespace root.

### mount(2) and the new mount API

- Order follows 6.1's `path_mount`: copy arguments, walk the target and lift to the topmost
  mount, strip `MS_MGC_VAL`, `MS_NOUSER` is `EINVAL`, `may_mount` (`CAP_SYS_ADMIN` in the user
  namespace owning the caller's mount namespace), then remount, bind, propagation, move, new mount.
- New mounts: proc, devpts, sysfs, cgroup2, tmpfs, mqueue; anything else `ENODEV`. sysfs, cgroup2
  and same-namespace mqueue mounted on the root of a mount of the same filesystem are `EBUSY`.
  mqueue needs `CAP_SYS_ADMIN` over the IPC namespace. A new mount belongs to the namespace of the
  mount its point is in.
- Bind: the new mount sees the source node with the source mount's filesystem, flags, device and
  source; `MS_REC` snapshots and copies submounts (`copy_submounts`); binding a namespace file
  creates an nsfs mount that pins the namespace (a mount-namespace file must be newer than the
  caller's namespace).
- Remount needs the exact mount root; flags change on this mount only (atime bits are kept if
  none are given). A tmpfs remount without `MS_BIND` reconfigures the shared instance
  (`size=`/`nr_inodes=`/`inode32|64` per `shmem_reconfigure`).
- Propagation flags are validated (one type, plus `MS_REC`/`MS_SILENT`) and have no effect;
  everything behaves as private.
- `MS_MOVE`, `pivot_root` (6.1's checks after resolving both paths, `chroot_fs_refs` for tasks
  whose root or cwd was the old root in the same namespace), `chroot` (`CAP_SYS_CHROOT` in the
  caller's user namespace and search permission).
- `open_tree` (without `OPEN_TREE_CLONE`: an `O_PATH` fd; with it: `CAP_SYS_ADMIN`, a detached
  clone in a new anonymous namespace held by the fd), `move_mount` (shares `move_tree` with
  `MS_MOVE`; attaching a clone moves its mounts into the caller's namespace;
  `MOVE_MOUNT_SET_GROUP` is always `EINVAL`), `mount_setattr` (size and flag checks per
  `build_mount_kattr`; per-mount flags, `AT_RECURSIVE` over the subtree; idmapped mounts end in
  `EINVAL` after the userns fd checks — `EBADF` for a bad fd, `EPERM` for the initial namespace).
- `umount2`: unknown flags `EINVAL`; lookup; `may_mount` (`EPERM`); not a mount root or not in
  the caller's namespace `EINVAL`; `MNT_EXPIRE` (with `MNT_FORCE`/`MNT_DETACH` or on the caller's
  root `EINVAL`, busy `EBUSY`, otherwise marks then `EAGAIN`; the mark is never cleared); the
  tree root `EBUSY` (`EINVAL` with `MNT_DETACH`); without `MNT_DETACH`, the caller's root or a
  busy mount `EBUSY` (busy means submounts, task roots/cwds, nodes of open fds, pinned PTYs — not
  mappings, running executables or mqueue fds); `MNT_FORCE` has no effect. `MNT_DETACH` moves the
  subtree to the detached list, where nodes keep working and the mount root has no parent.
- Mount flags are enforced where Linux checks them: ro (`EROFS` in creation after `EEXIST`,
  before directory write permission; in unlink/rmdir/rename before lookup; attribute changes;
  write opens of regular files, with `O_TRUNC` checked before permission), noexec (exec `EACCES`,
  `PROT_EXEC` mmap `EPERM`), nosuid (exec ignores set-id bits and file caps), nodev (device opens
  `EACCES`), nosymfollow (`ELOOP`); atime flags are only recorded and reported.
- Removing or renaming a mount point in the caller's namespace is `EBUSY`; mounts on it in other
  namespaces are detached. Rename and link across mounts are `EXDEV`, even between binds of one
  filesystem.

### Mount namespaces and cleanup

- `CLONE_NEWNS` (clone/unshare) calls `copy_ns`, which clones every attached mount of the source
  namespace (ids allocated first, then remapped; mounts pinning a mount namespace and anything
  under them are skipped) and remaps the task's root and cwd. tmpfs instances are shared between
  copies.
- `setns` into a mount namespace needs `CAP_SYS_ADMIN` over the target's owner plus
  `CAP_SYS_CHROOT` and `CAP_SYS_ADMIN` in the caller's namespace and an unshared fs; root and cwd
  are reset to the namespace root.
- Namespaces are held by `Rc`; the table keeps weak references. `collect` drops dead namespace
  entries, detaches the mounts of dead namespaces, drops detached mounts no longer in use, deletes
  tmpfs instances with no remaining mount, and repeats if releasing pins killed another namespace.
  It runs on every `umount2`, and at task exit only when some mount namespace has lost its last
  reference; closing fds never triggers it, so reclaim of unused detached mounts, open_tree clones
  and tmpfs trees waits for the next such event (or the next boot).

### tmpfs

- A tmpfs instance is a detached store directory (`create_detached_dir`); files are ordinary
  objects. Options follow 6.1's `shmem_parse_one` (`size=` with suffixes or `%`, `nr_blocks=`,
  `nr_inodes=`, octal `mode=`, `uid=`, `gid=`, `inode32`/`inode64`; `huge=`/`mpol=` parsed and
  ignored; unknown `EINVAL`); root mode defaults to 1777 owned by the mounter's fsuid/fsgid.
- Accounting per instance: one inode per name (each hard link) plus the root; regular files by
  size rounded up to pages (no holes); symlinks of 128 bytes or more one page. Limits default to
  half of RAM in pages for both; 0 is unlimited. Creates check room (`ENOSPC`); writes are clipped
  to the remaining room; truncation is never limited; unlinking the last name frees immediately.
  statfs and remount recount the whole tree; boot instances start uncounted.

### fd tables and open files

- Allocation is the lowest free slot at or above the minimum and below the table's limit (no free
  slot `EMFILE`; `F_DUPFD` with min at or over the limit `EINVAL`; dup3 targets over it `EBADF`).
  `prlimit(RLIMIT_NOFILE)` sets the limit on the table itself, so a table shared with another
  process through `CLONE_FILES` changes that process's limit too (Linux checks each allocating
  process's own rlimit).
- dup and fork share one `OpenFile` (offset and status flags shared). Clone without
  `CLONE_FILES` copies the slots and resets `holds_locks`. Every thread holds two references to its
  table (task state and task resources): `unshare(CLONE_FILES)`, execve (after the other threads
  are gone) and `CLOSE_RANGE_UNSHARE` copy only when there are more than two, that is when another
  thread or process shares the table; a copy is a new owner of POSIX locks, so an unshared table
  keeps its locks. execve then applies close-on-exec.
- Closing (close, dup3 over an fd, close_range, close-on-exec) removes the slot, runs
  `locks::closed` (POSIX locks of this table on that file) and `unix::released`; the last
  reference's `Drop` wakes lock and host-socket waiters and reports `IN_CLOSE_*`.
- `OpenFile` keeps only the access mode, `O_APPEND`, `O_NONBLOCK`, `O_PATH`, `O_DSYNC`, `__O_SYNC`
  and `O_LARGEFILE` from open (so `F_GETFL` never shows `O_DIRECT`, `O_NOATIME` or `O_ASYNC` given
  at open); `F_SETFL` changes `O_APPEND|O_NONBLOCK|O_ASYNC|O_DIRECT|O_NOATIME` (registering fasync
  when `O_ASYNC` turns on, with no `O_NOATIME` ownership check). `F_SETOWN`/`F_GETOWN`,
  `F_SETOWN_EX`/`F_GETOWN_EX` and the socket ioctls `FIOSETOWN`/`SIOCSPGRP` and
  `FIOGETOWN`/`SIOCGPGRP` translate pids through the caller's pid namespace (owners are stored as
  global ids; in a child pid namespace a pid unknown there is stored as a number that matches
  nothing, and an owner no longer in it reads back as 0); `F_SETOWN(INT_MIN)` is `EINVAL`.
  `F_SETSIG`, pipe size and seals are also handled in fcntl.
- Boot: with host stdio, fds 0–2 wrap the host's fds (`FileKind::Host`, reads poll the host fd
  and read without the BKL; writes ignore guest `O_NONBLOCK`); with a console, the PTY slave.

### Opening

- `open_path_with` (shared by openat, openat2 and `create_exclusive`): resolve; on create, check
  `may_create` (search, `EEXIST`, `EROFS`, write+search) and create with `mode & ~umask` and
  `new_owner` (setgid directories); a just-created file skips permission checks and truncation.
- Store nodes (`open_store`): symlink `ELOOP`; directory with write access or `O_CREAT` `EISDIR`;
  `O_TRUNC` on ro `EROFS`, else it adds `MAY_WRITE`; devices on nodev `EACCES`; permission; cgroup
  BPF device filter; `O_NOATIME` by a non-owner without `CAP_FOWNER` `EPERM`; write on ro
  `EROFS`; writing a file some task is executing `ETXTBSY`; break leases; `O_TRUNC` removes
  set-id bits and truncates. Then dispatch by type: regular files open the host object, character
  devices by device number (see Devices), block devices as loop devices (major 7; others `ENXIO`),
  FIFOs the shared pipe, sockets `ENXIO`.
- Pseudo nodes: permission from the synthetic stat, ptrace checks for process files, nodev for
  PTYs, `EISDIR`/`EROFS`/`EACCES` for writes the filesystem does not allow; content is generated
  at open.
- Every non-`O_PATH` store open records an inotify `Opened` (directory, name, close mask), whether
  or not watches exist; `O_TMPFILE` is not implemented: openat2 rejects invalid combinations as
  Linux does, but open/openat reach the directory branch (`EISDIR`, or a plain read-only open of
  the directory without write access).

### Data I/O on regular files

- Without an explicit offset, `unlocked_io` emulates `f_pos_lock`: wait while `pos_busy` is set,
  take it, snapshot the position, run host `pread`/`pwrite` with the BKL released, then advance.
  Reads and writes go in 64 KiB chunks, releasing `pos_busy` between chunks. Only `unlocked_io`
  and the copy path take `pos_busy`; lseek and `O_APPEND` writes update the position without it
  (directories never do unlocked I/O, so getdents is unaffected).
- `O_APPEND` writes run under the BKL at the current host length; `pwrite` on an `O_APPEND` file
  also appends unless `RWF_NOAPPEND`. After a write, `write_sync` syncs the object (with the BKL
  released) when the open file has `O_DSYNC` (fdatasync) or `O_SYNC` (fsync), or the call passed
  `RWF_DSYNC`/`RWF_SYNC`; `sendfile`, `copy_file_range` and `splice` into such a file do not.
- No holes: `SEEK_DATA` returns the offset and `SEEK_HOLE` the size. `fallocate` mode 0 extends
  the length, `PUNCH_HOLE` (with `KEEP_SIZE`) and `ZERO_RANGE` write zeros under the BKL, other
  modes `EOPNOTSUPP`.
- `sendfile` and `copy_file_range` between distinct regular store files use host `sendfile`
  without the BKL; they fall back to chunked copies for `O_APPEND` output, the same inode, a
  sealed output file, tmpfs output, or host `EINVAL`/`ENOSYS`/`EOPNOTSUPP`/`EXDEV`.
  `copy_file_range` rejects nonzero flags (`EINVAL`) and `O_APPEND` output (`EBADF`); `sendfile`
  appends to `O_APPEND` output instead of returning `EINVAL`. `splice`, `tee` and
  `vmsplice` copy through buffers (one side of `splice` must be a pipe; data leaves the pipe only
  after the write succeeds).
- Writes, truncation and fallocate on tmpfs files go through the instance's accounting.
- readv and writev follow Linux's `read_iter`/`write_iter` split: pipes, terminals and sockets
  (and eventfd reads) do one read or write over all the iovecs (one read of up to 64 KiB scattered
  in order; writes in 64 KiB pieces across iovec boundaries until one is short; sockets through
  `kernel/net`), so a pipe readv returns what was there instead of waiting again and small pipe
  writes stay atomic. Everything else goes iovec by iovec and stops at an incomplete one, as Linux
  does for files with only `read`/`write` (eventfd writes, timerfd, signalfd, inotify, `/dev/kmsg`
  reads). A total of 0 returns 0 after the offset and mode checks without reaching the file.
- Copying to and from the guest buffer grows a grows-down mapping below it first (`kernel/mm`). A
  read whose buffer faults partway returns the bytes copied before the faulting page (`EFAULT` when
  none were), as Linux's `copy_to_user` does: a regular file's position moves only past what was
  copied, while data from other files (a pipe, a terminal) is already consumed (Linux leaves a
  pipe buffer whose copy came up short in the pipe). A faulting write buffer still fails before
  anything is written, except that a pipe writev writes the iovecs before the faulting one.

### Metadata and namespace syscalls

- Stat: asm-generic `struct stat` with uid/gid mapped into the caller's user namespace; `st_dev`
  from the mount; `st_blksize` 4096; `st_blocks` from size (no holes; short symlinks and sysfs
  files 0); directories report size 4096 on the root filesystem. statx always reports
  `STATX_BASIC_STATS` (no btime, mount id or attributes). statfs: the store reports ext4's magic
  with the host's `statvfs` of the objects directory, tmpfs its own magic and limits (recounted),
  synthetic filesystems their magic with zero sizes; `f_flags` from the mount.
- access/faccessat: real IDs unless `AT_EACCESS` (capabilities only when the real uid is root in
  the caller's namespace); `EROFS` for `W_OK` after the permission check.
- chmod drops `S_ISGID` when the caller is not in the file's group and lacks `CAP_FSETID` over
  it. chown needs mapped IDs (`EINVAL`) and either `CAP_CHOWN` over the file or an owner keeping
  the uid and choosing one of its groups; an argument of -1 always passes. On a store
  non-directory it then clears setuid, setgid when group-executable or when the caller is neither
  in the file's group nor has `CAP_FSETID` (Linux 6.2's `setattr_should_drop_sgid`; 6.1 cleared
  only group-executable setgid), and file caps; when a set-id bit would go, the caller must be the
  owner or have `CAP_FOWNER` (`EPERM`, bits kept), so `chown(-1, -1)` by another user fails on
  `su` but succeeds on a file with only caps (which lose them, as on Linux). utimensat: explicit
  times need the owner or `CAP_FOWNER`,
  `UTIME_NOW` also accepts write permission; the both-`UTIME_OMIT` shortcut runs after path
  resolution. Writes and truncation call `remove_privs` (`CAP_FSETID` checked in the initial
  namespace).
- truncate: `EROFS`, `EISDIR`, `EINVAL` for non-regular, write permission, `ETXTBSY`,
  `RLIMIT_FSIZE`, leases; ftruncate `EINVAL` unless writable and regular, `EBADF` on `O_PATH`.
- mknodat: regular, char, block, FIFO, socket (directory `EPERM`); devices need `CAP_MKNOD` and
  pass the device filter (whiteouts exempt). renameat2: `RENAME_NOREPLACE`, `RENAME_EXCHANGE`
  (`RENAME_WHITEOUT` `EINVAL`); `EXDEV` before the `.`/`..` checks. linkat: `EXDEV` for synthetic
  nodes or different mounts, then `protected_hardlinks`, then directory write permission;
  `AT_EMPTY_PATH` needs no capability, a magic link followed with `AT_SYMLINK_FOLLOW` reaches the
  same store node, and a source whose link count is already 0 is accepted (no `EMLINK` limit
  either).
- getdents64 takes the listing (with `.` and `..`) on the first call and refreshes it only on a
  seek to 0; `d_off` is index+1. chdir/fchdir/chroot check search only on store directories.
  umask lives in the fs context.
- `protected_symlinks` and `protected_hardlinks` exist (default 1); `protected_regular` and
  `protected_fifos` do not.

### Extended attributes

- Names need a `security.`, `system.`, `trusted.` or `user.` prefix (`EOPNOTSUPP` otherwise); bad
  name lengths `ERANGE`; values over 64 KiB `E2BIG`. `system.*` (including POSIX ACL names) is
  stored raw with no ACL semantics and no permission check of its own, so anyone who can resolve
  a file can set or remove them; unprivileged `trusted.*` reads `ENODATA`, writes `EPERM` and is
  hidden from listings.
- `security.capability`: writes accept v2/v3 (the magic is checked under a mask, so stray flag
  bits pass) and need `CAP_SETFCAP` in the initial namespace, so root in a user namespace cannot
  set file caps; values are stored without rootid conversion; reads turn v3 with rootid 0 into
  v2. Synthetic nodes: get `ENODATA`, set/remove `EOPNOTSUPP`
  (`EROFS` first on ro), empty list; pipes and sockets `EOPNOTSUPP`.

### Pipes and FIFOs

- Data lives in process memory. `pipe2` accepts `O_CLOEXEC|O_NONBLOCK|O_DIRECT` (`O_DIRECT` is
  accepted and ignored: no packet mode). Capacity 64 KiB, capped at `pipe-max-size` (1 MiB)
  without `CAP_SYS_RESOURCE`; `F_SETPIPE_SZ` rounds up to a power of two (at least a page;
  `EPERM` over the max, `EBUSY` below the buffered data).
- FIFO open: one `Pipe` per inode; write-only nonblocking with no reader `ENXIO`; read-only
  nonblocking succeeds; read-write never waits; blocking one-ended opens wait until a peer is
  present or has opened since (`ERESTARTSYS` on a signal); an access mode of 3 is treated as
  read-write.
- Writes: no readers `SIGPIPE`+`EPIPE` (or a short count); writes up to `PIPE_BUF` (4096) are
  atomic; larger writes are partial; nonblocking `EAGAIN`. Reads return 0 without writers.
  `FIONREAD` reports the buffered bytes.

### Locks and leases

- All in-process, keyed by store inode (bind mounts share them); the host `flock` is used only on
  the store's `lock` file. `flock` and fcntl locks are separate tables.
- `flock`: one entry per open file; relocking converts; `LOCK_NB` `EAGAIN`; released on `LOCK_UN`
  or when the open file goes.
- POSIX locks belong to the fd table and are released by any close of the file in that table
  (close, close_range, dup2 replacement, close-on-exec, seccomp addfd); OFD locks belong to the
  open file; the two kinds share one table and conflict with each other. Ranges follow
  `flock64_to_posix_lock`: a start past `OFFSET_MAX` is `EOVERFLOW` and below 0 `EINVAL`; a
  positive length may end at `OFFSET_MAX` (`EOVERFLOW` past it), 0 means to the end and beyond,
  and a negative one covers the bytes before the start (`EINVAL` below 0). Ranges split and merge;
  `F_SETLKW` detects deadlocks for POSIX requests only (`EDEADLK`; the wait chain may pass through
  OFD waiters, which Linux excludes) and waits on the lock wake channel. Files without a store
  inode (anonymous pipes, sockets, PTYs, synthetic files, eventfds) silently succeed and
  `F_GETLK` reports unlocked; named FIFOs have an inode and lock normally.
- Leases (regular files only, owner or `CAP_LEASE`): read leases need no writers, write leases
  no other opens. Opening for write or truncating breaks all leases, opening for read breaks write
  leases: the holder gets `F_SETSIG`'s signal or `SIGIO`; a nonblocking open returns `EAGAIN`,
  others wait until the holder downgrades or `lease-break-time` (45 s) forces it. `F_NOTIFY`
  (dnotify) is `EINVAL`.

### Devices

- `dev::populate` creates in the boot `/dev` tmpfs, and `ensure_dev_nodes` in the store's `/dev`:
  `null`, `zero`, `full`, `random`, `urandom`, `kmsg`, `tty`, `console`, `ptmx`, `loop-control`
  and `loop0`–`loop7` (0660, group `disk`, gid 6), the `fd`/`stdin`/`stdout`/`stderr` links and the
  `pts`/`shm`/`mqueue` directories.
- Opening dispatches by device number for any node anywhere (unless nodev): 5:0 and 5:1 the
  caller's controlling terminal, 5:2 the ptmx of the devpts next to the node, major 136 a boot
  devpts slave, 1:3/5/7/8/9/11 `Device`, 10:237 loop-control; block 7:N the loop device N (created
  if missing, like Linux's legacy autoload); anything else `ENXIO`.
- `/dev/zero`, `/dev/full` and the random devices fill the whole buffer per read; randomness
  comes from the host `/dev/urandom` and never blocks (entropy count fixed at 256; the random
  ioctls follow 6.1's `random_ioctl`).

### Loop devices

- Follow 6.1's `drivers/block/loop.c` and the block-device parts of `block/fops.c` and
  `block/ioctl.c`. Eight devices exist from boot; loop-control's `LOOP_CTL_ADD` (a number, or the
  lowest unused for a negative one; `EEXIST`), `LOOP_CTL_REMOVE` (`EINVAL` negative, `ENODEV`
  missing, `EBUSY` bound or open) and `LOOP_CTL_GET_FREE` (lowest unbound, else a new device) also
  create or delete `/dev/loop<N>` in the boot `/dev`; other requests are `ENOSYS`. Read and write
  on loop-control are `EINVAL`.
- Open counts an opener; `O_EXCL` claims the device (`EBUSY` while another open holds it); the
  first opener sets the soft block size (largest power of two up to a page dividing the size).
- `LOOP_SET_FD`/`LOOP_CONFIGURE` bind an unbound device (`EBUSY` otherwise, or when another open
  holds the claim) to a regular file or a bound loop device that does not lead back to it
  (`EBADF` for itself or `O_PATH`, `EINVAL` otherwise); configure flags outside
  RO/AUTOCLEAR/PARTSCAN/DIRECT_IO, bad block sizes and any encryption are `EINVAL`, offsets over
  `i64::MAX` `EOVERFLOW`. The device is read-only when the file or the device's own open file is
  not writable; direct I/O is always dropped; partscan is kept but nothing is scanned. Size is the
  file length past the offset, capped by the size limit, in whole sectors, fixed until
  `LOOP_SET_CAPACITY` or a `LOOP_SET_STATUS` that changes offset or limit.
- `LOOP_CLR_FD` with other openers only sets autoclear; otherwise it unbinds at once (6.1; later
  kernels defer it to the close). The last opener's release unbinds an autoclear device. Unbinding
  resets offset, limit, size, flags, name and block size but keeps the disk read-only state, as
  Linux does.
- Status: `LOOP_SET_STATUS(64)` needs write mode or `CAP_SYS_ADMIN` (`EPERM`), may set and clear
  autoclear and only set partscan; `LOOP_GET_STATUS(64)` reports the backing file's device, inode
  and rdev as stat shows them (`EOVERFLOW` when the old structure cannot hold them).
  `LOOP_CHANGE_FD` swaps the file of a read-only device for one of the same size.
  `LOOP_SET_DIRECT_IO` is `EINVAL` to enable; `LOOP_SET_BLOCK_SIZE` takes 512 to 4096.
- I/O: reads past the end return 0 (an unbound device has size 0), zeros past a shrunk backing
  file; writes on a read-only device are `EPERM`, at or past the end `ENOSPC` (`O_APPEND`
  ignored). Data goes through the backing open file's own read and write paths at the device
  offset (regular files without the BKL), with no cache in between; backing errors are `EIO`.
  lseek accepts only SET/CUR/END within the size; fsync syncs the backing file of a bound writable
  device; epoll refuses the device (`EPERM`); poll reports it ready.
- Block ioctls: sizes, logical/physical/minimum block size, read-only get/set (`BLKROSET` needs
  `CAP_SYS_ADMIN`), read-ahead, `BLKBSZSET`, `BLKZEROOUT` (writes zeros), `BLKFLSBUF`,
  `BLKSECTGET`, `BLKROTATIONAL`, `BLKGETDISKSEQ`; `BLKDISCARD` is `EOPNOTSUPP` (no discard),
  `HDIO_GETGEO` `ENOTTY`, `BLKRRPART` and `BLKPG` `EINVAL` (no partitions) or `EACCES`; anything
  else `EINVAL`, as the loop driver answers.

### eventfd and timerfd

- eventfd: counter or semaphore mode; writing `u64::MAX` is `EINVAL`; overflow blocks; every
  read/write wakes the object (the syscalls live in `kernel/poll`).
- timerfd: realtime, monotonic, boottime and the alarm clocks; expirations are kept on the clock's
  base (`itimer::Expiry`: relative realtime and monotonic on the host's monotonic clock, absolute
  realtime on its realtime clock, boottime on its boottime clock); settime resets the expiration
  count. `TFD_TIMER_CANCEL_ON_SET` on an absolute realtime timer records the wall-clock jump count:
  after a jump (the host's clock set, or the device resuming) the timer counts one expiration and
  the next read is `ECANCELED` (the count dropped, the timer still armed).

### inotify

- Instances: `inotify_init1` accepts `O_CLOEXEC|O_NONBLOCK`; per-user instance (`EMFILE`) and
  watch (`ENOSPC`) limits and the queue limit come from `/proc/sys/fs/inotify/*` (defaults 128
  instances, 2^20 watches, 16384 queued events).
- Watches are keyed by store inode; watches on synthetic nodes are accepted, counted and never
  report. `IN_MASK_ADD`/`IN_MASK_CREATE`, `IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_EXCL_UNLINK`,
  `IN_ONESHOT`; store targets need read permission.
- Reporting: with no store watches every hook costs one emptiness check. An event goes first to
  the parent directory's watches with the name (not for `*_SELF`), then to the inode's own.
  Identical consecutive events merge; a full queue drops events and queues one `IN_Q_OVERFLOW`.
  `IN_IGNORED` follows removal, `IN_DELETE_SELF` and a matching `IN_ONESHOT`.
- Hooks follow the fsnotify call sites: open (`IN_CREATE` or `IN_MODIFY` for a truncating open,
  then `IN_OPEN`), mkdir/mknod/symlink/AF_UNIX bind, link (`IN_ATTRIB` on the source, then
  `IN_CREATE`), unlink/rmdir (`IN_ATTRIB`, delete-self, `IN_DELETE`), rename (cookie pair, then
  `IN_MOVE_SELF`; exchange reported both ways), attribute changes (`IN_ATTRIB`, also for
  utimensat whichever times change), writes,
  truncation and fallocate (`IN_MODIFY`), reads and getdents (`IN_ACCESS`), last close of an open
  file (`IN_CLOSE_*` to the directory and name it was opened through).
- `IN_DELETE_SELF` waits until the last open file of the removed name closes. Renames update the
  `Opened` records of open files (scanning all open files, even with no watches).
- Files app changes (`host_change`, run under the target kernel's BKL) map `Created`, `Removed`,
  `Moved`, `Opened` (write opens) and `Written` to the same event sequences; there are no
  access, close-nowrite, read-open or attribute events from the provider.
- read returns whole events (`EINVAL` if the first does not fit), blocks unless nonblocking;
  `FIONREAD`, poll readiness and fdinfo read the instance. No fanotify (`ENOSYS`), no dnotify.

### Native AIO

- A context's id is the guest address of a 4096-byte read-only anonymous page that io_setup maps
  whatever `nr` is. The page never holds a ring header, so libaio's user-space ring check fails and
  it always makes the syscall. Contexts belong to the thread group: a non-thread child (fork, vfork,
  `CLONE_VM` without `CLONE_THREAD`) starts with none; execve and the last thread's exit release
  them.
- io_setup: `*ctxp` must be 0 and `nr` from 1 to `0x1000_0000 / 32` (`EINVAL`); each context
  charges `2 * nr` to `aio-nr` against `aio-max-nr` (65536; `EAGAIN`).
- io_submit, per iocb: `aio_reserved2` must be 0; the fd must exist (`EBADF`), and with
  `IOCB_FLAG_RESFD` the resfd must be an eventfd; reads need a readable file and writes a writable
  one (`EBADF`); unknown `aio_rw_flags` are `EOPNOTSUPP` and unknown opcodes `EINVAL`; a context
  whose queued events and pending polls reach `nr` is `EAGAIN`. The first failure is returned when
  nothing was submitted, otherwise the count so far.
- PREAD, PWRITE, PREADV, PWRITEV, FSYNC and FDSYNC run synchronously inside io_submit through the
  regular read, write, vectored and fsync paths (blocking pipe or socket I/O blocks io_submit), and
  their result or negative errno is queued at once. `aio_offset` is used only for positional files;
  `RWF_NOWAIT` and `RWF_APPEND` are set temporarily as `O_NONBLOCK`/`O_APPEND` on the shared open
  file.
- `IOCB_CMD_POLL` completes at once when ready; otherwise it stays pending and is checked only by
  io_getevents and io_pgetevents on that context (one-shot, reporting every ready bit). The resfd
  eventfd, kept as an fd number and looked up again, is signalled when an event is queued.
- io_getevents and io_pgetevents (`min_nr <= nr`, relative timeout, optional temporary sigmask):
  complete ready polls, then return once `max(min_nr, 1)` events are queued, `min_nr` is 0 or the
  deadline has passed; a destroyed context returns what is queued, else `EINVAL`; a signal is
  `EINTR`; otherwise wait for any kernel wake. Events are removed before they are copied out.
- io_cancel can cancel only a pending poll (an event with result 0, then `EINPROGRESS`); everything
  else has already completed (`EINVAL`). io_destroy marks the context dead, drops pending polls
  without events, releases the charge, unmaps the page and wakes waiters, which take what is
  queued; it does not wait for a submit still blocked in I/O.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher; `kernel/process` (clone/unshare/setns, execve's fd-table handling and
  mount-flag checks, task exit's `collect`, the fast-path `Cached` resolver, AIO release at execve
  and exit); `kernel/mm` (mmap `PROT_EXEC` on noexec); `kernel/procfs` (mountinfo, fd links,
  fdinfo, sysctls for pipes, leases, inotify, AIO and `protected_*`, the `/sys/block` view of the
  loop devices); `kernel/unix` (AF_UNIX bind
  creating store nodes); `kernel/poll` (poll/epoll reading open files, eventfd syscalls);
  `kernel/ipc` (queue fds, `mq_dev`); the machine (boot, `store_changed` for inotify); core dumps.

### Outbound

- `kernel/store` (lookups, snapshots, objects, namespace changes, detached directories, xattrs,
  `fs_stats`, the metadata cache reader); `kernel/procfs` (the `Pseudo` views, `pseudo::stat`,
  writability); `kernel/tty` (controlling terminal, ptmx, devpts instances); credentials
  (`permission`, `owns`, `capable_wrt`, `new_owner`, `FileCaps`); `appine_hal` (pread, pwrite,
  poll, lseek, fsync, `copy_file`, `statvfs`, `memory_info`, `fill_random`); the kernel's wake
  objects and `Cx::unlocked`; signals (`SIGPIPE`, `SIGIO`, `F_SETSIG`); `bpf::check_device`;
  `kernel/ipc` (mqueue objects); `cgroup`; `pidns`/`nsproxy`; for AIO, `kernel/poll` (`poll_once`,
  the temporary sigmask) and `kernel/mm` (the context page).

## Invariants and gotchas

- A node's mount is part of its identity: `..`, root checks, `BENEATH`, `EXDEV` and busy checks
  all compare mounts, so binds of one filesystem are distinct.
- Mount ids are never reused; nodes in detached mounts keep their ids and devices.
- `mounted()` matches a whole node, so a mount on a file shows through every hard link to it in
  the same mount; the process root itself is never lifted (as in Linux, a mount on `/` is not seen
  by absolute lookups).
- Synthetic directories are always searchable; procfs gates per-process data in readlink/follow.
- An fd's display path is frozen at open; later renames do not change `/proc/<pid>/fd/N`.
- Locks and leases are keyed by inode only and live in one kernel; they are not visible to other
  kernels or the host.
- Every store open allocates an inotify `Opened` record and every rename that moves open files
  scans all open files, even with no watches.
- Unlocked I/O snapshots the position before releasing the BKL and writes it back afterwards, so
  any other code that moves the position must take `pos_busy` or accept being overwritten.
- `Node` comparisons in pivot_root's root/cwd rewrite include the mount id, so only tasks in the
  same mount namespace are moved.
- AIO contexts belong to the thread group, not the address space, so a `CLONE_VM` child cannot use
  its parent's contexts (Linux keeps them per mm). The capacity check runs before I/O that may
  release the BKL, so concurrent submitters can exceed `nr`.
- A bound loop device holds its backing open file, and a loop over a loop holds the lower device's
  open file, so the lower device keeps that opener until the upper one is unbound. Loop I/O takes
  the backing file out of the device before reading or writing it (the BKL may be released), and
  the device's file position has no lock, as block devices have none in Linux.
- A loop device's opener count lives in its `LoopOpen`, dropped with the open file: anything that
  keeps an open file alive (a dup, a fork, an SCM_RIGHTS message, a loop binding) keeps the device
  open, and autoclear runs only when the last such reference goes.

## Confirmed bugs / technical debt

- **Writes keep a setgid bit chown would clear:** `remove_privs` clears setgid only with group
  exec, as 6.1 does, while chown follows 6.2 and also clears it when the caller is outside the
  file's group without `CAP_FSETID`; since 6.2 Linux's writes do the same as its chown.
- **`/proc/<pid>/fd` has no access check:** listing, readlink and following `fd/N` skip the
  ptrace-read check that `cwd`, `root`, `exe` and `ns/*` do, so any process can see the paths of
  other users' open files and follow the links to their nodes, reaching files in directories it
  cannot search, and reopen their pipes with no permission check (Linux denies with `EACCES`).
  See the matching `kernel/procfs` item.
- **Relinking unlinked files creates dangling names:** `linkat` accepts an nlink-0 source (through
  `AT_EMPTY_PATH`, which also lacks Linux's `CAP_DAC_READ_SEARCH` check, or `/proc/self/fd/N`
  with `AT_SYMLINK_FOLLOW`); the inode stays doomed, so after commit the reaper deletes its object
  and the new name points at nothing (Linux returns `ENOENT`). A stale inotify delete-self entry can
  then fire for later watches. See the matching `kernel/store` item.
- **`system.*` xattrs are unprotected:** any user who can resolve a file can set or remove its
  `system.*` attributes, including POSIX ACL names, which are stored raw and never enforced (Linux
  requires the owner or `CAP_FOWNER` and validates ACLs); root in a user namespace cannot set file
  caps at all.
- **File position races:** lseek and `O_APPEND` writes change the position without the
  `pos_busy` lock, so a concurrent unlocked read or write on the same open file can overwrite the
  new position (Linux takes `f_pos_lock` in lseek too).
- **`sendfile` to an `O_APPEND` file appends** instead of returning `EINVAL` as Linux's
  `do_splice_direct` does (splice and copy_file_range check this).
- **umount ignores mappings and executables:** a mount whose files are only mapped, being executed
  or held by mqueue fds is not busy, so `umount2` succeeds where Linux returns `EBUSY` (and a tmpfs
  instance can then be reclaimed while still mapped).
- **utimensat always reports `IN_ATTRIB`:** Linux reports `IN_ACCESS` for atime-only and
  `IN_MODIFY` for mtime-only changes.
- **Locks on inode-less files are no-ops:** `flock` and `F_SETLK` on anonymous pipes, sockets and
  PTYs succeed without locking, although processes sharing an inherited fd could use them to
  coordinate on Linux.
- **Small inotify leaks and ordering:** a delayed delete-self entry is never removed if every
  watch disappears before the last close; an event identical to the last one is merged even when
  the queue is full, where Linux queues `IN_Q_OVERFLOW`.
- **Pending AIO polls complete late or never:** a pending `IOCB_CMD_POLL` is checked only inside
  io_getevents on its context, so its resfd eventfd is not signalled until some thread calls it
  (the poll iocb, resfd and epoll pattern never fires); and io_getevents waits for kernel wakes
  only, with no host fds or timerfd deadlines, so a poll on an inet socket, a host fd or a timerfd
  is noticed only on an unrelated wake or the timeout, and without a timeout it may block forever.
  The resfd is an fd number looked up at completion, so a closed or reused fd loses the signal or
  sends it to another eventfd; io_cancel's event signals nothing.
- **AIO writes ignore append and sync flags:** PWRITE and PWRITEV on a regular file write at
  `aio_offset` even with `RWF_APPEND` or an `O_APPEND` file, `RWF_DSYNC`/`RWF_SYNC` never sync
  (only the file's own `O_DSYNC`/`O_SYNC` do), and a negative offset is not rejected but cast and
  failed later.
- **AIO results leak restart codes and can be lost:** an interrupted blocking read or write inside
  io_submit stores `-ERESTARTSYS` (-512) in `io_event.res` instead of `-EINTR`, and io_getevents
  drops the events it took when copying them out faults (Linux leaves them in the ring).
- **`aio-nr` counts twice `nr`:** each context charges and reports `2 * nr`, as Linux did before
  4.12; 6.1 charges `nr`, so only half as many events fit under `aio-max-nr` and `aio-nr` reads
  double.
- **`copy_file_range` checks differ from 6.1's `generic_copy_file_checks`:** a directory is `EBADF`
  instead of `EISDIR` and a pipe `EBADF` instead of `EINVAL`; overlapping ranges in one file are
  copied instead of `EINVAL`; a length that overflows the offset is `EINVAL` instead of
  `EOVERFLOW`, and a range past the largest file size `EINVAL` instead of `EFBIG` (LTP
  copy_file_range02, which also expects `EPERM` on an immutable file).
- **Setgid survives creation in a setgid directory:** a file created with `S_ISGID` and group
  execute in a setgid directory by a caller outside the directory's group and without
  `CAP_FSETID` keeps the bit; Linux strips it (CVE-2018-13405; since 6.0 also through umask and
  ACLs), so such a user can make setgid files of a group it is not in (LTP creat09).
- **getdents64 lists a removed directory:** an open directory that has been rmdir'ed still lists
  entries; Linux returns `ENOENT` for a dead directory (LTP getdents02).
- **No inode flags:** `FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` are not supported on store or tmpfs
  files, while 6.1's tmpfs supports append-only, immutable, nodump and noatime (LTP unlink09,
  copy_file_range02).
- **atime ignores the guest's settings:** a regular file's atime is its host object's, so reads
  update it by the host's rules whatever `O_NOATIME` or the mount's `noatime`, `nodiratime` and
  `strictatime` say, and directories never get an atime of their own (LTP open12, mount03).
- **Loop device gaps:** no partitions (partscan is only a flag); no discard, and `fallocate` on the
  device is `ENODEV` (Linux punches or zeroes); mmap of a loop device is `ENODEV` (Linux maps
  block devices); `O_DIRECT` alignment is not enforced; zone, tracing and persistent-reservation
  ioctls are `EINVAL` where Linux answers `ENOTTY` or `EOPNOTSUPP`; `BLKGETDISKSEQ` advances
  only when a device is created or bound, not when it is unbound.
- **Fidelity gaps:** statx never reports btime, `stx_mnt_id`, attributes or `STATX_DIOALIGN`; no
  `protected_regular`/`protected_fifos`; no `O_TMPFILE`; no holes (`SEEK_DATA`/`SEEK_HOLE`,
  fallocate writes zeros; with tmpfs's size accounting a sparse file can fill an instance: LTP
  fallocate04, fallocate05, openat02); `RLIMIT_NOFILE`
  set by prlimit applies to the whole shared fd table; `MNT_EXPIRE` marks are never cleared by an
  access to the mount, and LTP umount2_02's mark-then-expire sequences fail; propagation is not
  emulated; `sendfile`, `copy_file_range` and `splice` into an `O_SYNC`/`O_DSYNC` file do not
  sync it (Linux's write path does); `O_NOTIFICATION_PIPE` is `EINVAL` from pipe2 (Linux 6.1
  creates a notification pipe or, without watch queues, fails with `ENOPKG`); vmsplice copies the
  data at the call where Linux queues references to the user pages, so changes the writer makes
  to the buffer before the reader reads are not seen.
- **splice, tee and vmsplice block by the wrong flags:** `SPLICE_F_NONBLOCK` and a pipe end's
  `O_NONBLOCK` only stop the wait for data in an input pipe; writing into an output pipe follows
  that pipe end's own `O_NONBLOCK`, reading a socket follows the socket's, and tee ignores
  `O_NONBLOCK` on both pipes, so a nonblocking splice or tee into a full pipe, or from an empty
  socket, sleeps (possibly forever) where Linux returns `EAGAIN`; vmsplice honours the pipe's
  `O_NONBLOCK` and, when reading, ignores `SPLICE_F_NONBLOCK`, the reverse of Linux. tee also
  takes the source data before waiting for room and returns 0 at source EOF without looking at the
  destination, so after sleeping on a full pipe it writes data a reader has since consumed, and a
  full or reader-less destination gives 0 instead of `EAGAIN`/`EPIPE`.
- **splice-family argument checks differ:** splice looks up its fds before returning 0 for a zero
  length (Linux returns 0 first, even for bad fds and flags) and an offset on a socket or other
  unseekable non-pipe is `ESPIPE` instead of `EINVAL`; vmsplice is `EBADF` on a non-pipe before an
  empty request returns 0 or a bad iovec is `EFAULT`, a zero-length vmsplice into a pipe without
  readers raises `SIGPIPE` and fails with `EPIPE` instead of returning 0, and on a FIFO opened
  read-write vmsplice reads where Linux writes; unknown splice, tee and vmsplice flags are accepted
  (Linux: `EINVAL`).
- **splice into a file stops quietly at `RLIMIT_FSIZE`:** a splice from a pipe into a regular file
  that the limit cuts short returns the short count without `SIGXFSZ`, where Linux's splice loop
  tries the next write at the limit and raises `SIGXFSZ` (fatal by default) while still returning
  the count; programs see the signal one call later.
- **Pipe I/O with faulting buffers ignores Linux's per-buffer rules:** a pipe read, readv or
  read-side vmsplice whose buffer faults consumes the data it took, where Linux leaves a pipe
  buffer whose copy came up short in the pipe; a writev whose later iovec faults still writes what
  precedes the fault, where Linux commits only whole copied pages (a sub-page writev fails with
  `EFAULT` and writes nothing); and a write or vmsplice whose buffer faults after its first page
  fails with `EFAULT` instead of returning the pages copied.
- **getdents64 copies all or nothing:** a buffer that runs into an unmapped page fails with
  `EFAULT` even when whole entries fit before it, where Linux returns the entries written before
  the fault.
- **open(2) does not validate `O_TMPFILE`:** openat skips the `O_TMPFILE` checks openat2 makes, so
  `O_TMPFILE` with `O_CREAT` is `EISDIR` instead of `EINVAL`, and a read-only `O_TMPFILE` open
  succeeds as a plain open of the directory instead of `EINVAL`.
- **`MFD_NOEXEC_SEAL` is accepted but not enforced:** memfd_create takes 6.3's
  `MFD_NOEXEC_SEAL`/`MFD_EXEC` and F_ADD_SEALS takes `F_SEAL_EXEC` (6.1 rejects all three with
  `EINVAL`), but chmod never checks the seal, so a memfd its creator sealed non-executable can be
  made executable again.
- **Alarm timerfds need no capability:** timerfd_create and timerfd_settime accept
  `CLOCK_REALTIME_ALARM`/`CLOCK_BOOTTIME_ALARM` without `CAP_WAKE_ALARM`, where Linux returns
  `EPERM`, so programs that probe alarm clocks to learn whether they may wake the system are told
  they can.
- **Remounting read-only ignores open writers:** a remount to read-only (bind or not) succeeds while
  files on the mount are open for writing, where Linux returns `EBUSY`, and those fds keep writing
  afterwards because only opens check the flag, so a mount made read-only (before shutdown, or a
  sandbox's remount-ro) can still change.
- **pivot_root resolves both paths before checking them:** a non-directory `new_root` with a missing
  `put_old` is `ENOENT` instead of `ENOTDIR`.
- **mountinfo loses the root of a bind whose source was removed:** the fourth column shows `/` once
  the bound directory is deleted, where Linux shows its old path followed by `//deleted`, so tools
  reading bind sources from mountinfo (findmnt, container runtimes) see a bind of the whole
  filesystem.

## Open questions

- Is losing renames in `/proc/<pid>/fd/N` (path frozen at open) acceptable, given Linux computes
  `d_path` at readlink?
- Is skipping search permission on synthetic directories intended (`/proc/<pid>/fd` is 0500 on
  Linux)?
- Should `protected_regular`, `protected_fifos` and `O_TMPFILE` be implemented?
- Should unused detached mounts, open_tree clones and tmpfs instances be reclaimed when the last fd
  closes rather than at the next `umount2` or task exit?

## To verify

- Whether reopening a deleted file through `/proc/<pid>/fd/N` (the open-file node) checks
  permission (reopening a pipe that way checks none).
- epoll's fdinfo device numbers for files on tmpfs and synthetic mounts (said to use the root
  filesystem's device, unlike fstat).
- Whether the store resolves the parent of an unlinked directory, which decides what getcwd and
  `/proc/<pid>/cwd` return for a deleted cwd (Linux shows ` (deleted)`).
- inotify's per-hook ordering against every Linux fsnotify call site (only spot-checked; the
  differential test `inotify` covers the common sequences).
- The Linux 6.1 AIO behaviour the AIO bugs compare against (completion from the poll wake-up,
  `aio_rw_done`'s `EINTR`, the ring head on a failed copy, `IOCB_APPEND` and the sync flags, `nr`
  charged to `aio-nr` since 4.12) comes from upstream knowledge: no Linux source is in the
  repository.
- The Linux 6.1 behaviour behind the items found by Starnix's syscall tests (splice's
  `SPLICE_F_NONBLOCK` conversion and per-page pipe copies, remount `EBUSY` with writers,
  `O_NOTIFICATION_PIPE`) was observed on a native 6.8 kernel and attributed to 6.1 from knowledge
  of the kernel.
