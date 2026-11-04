# kernel/vfs Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/vfs.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 22 claims checked by 3 independent verifiers — 18 confirmed and 4 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: open keeps `O_SYNC`/`O_DSYNC` and writes sync accordingly; execve and `CLOSE_RANGE_UNSHARE` copy the fd table only when shared, keeping POSIX locks; removed both bugs)

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
- in-memory pipes and FIFOs, advisory locks (`flock`, POSIX, OFD) and leases, character-device
  nodes and their in-process implementations, eventfd/timerfd objects;
- inotify, including replaying changes made by the Android Files app.

Root: `core/kernel/src/vfs/mod.rs`, `vfs/dev.rs`, `fd.rs`, `pipe.rs`, `locks.rs`,
`syscall/fs.rs`, `syscall/mount.rs`, `syscall/xattr.rs`, `syscall/lease.rs`,
`syscall/inotify.rs`, `syscall/timerfd.rs`.

Outside the module: the store's own semantics (`kernel/store`), procfs/sysfs/cgroup2/devpts
content (`kernel/procfs`), terminals (`kernel/tty`), sockets, poll/epoll and the eventfd syscalls
(`kernel/net`), the BKL, the no-BKL fast path and the namespace mechanism
(`kernel/process`), credentials and file capabilities at exec (credentials module, not yet
mapped).

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
- `Device` — Null, Zero, Full, Random, Kmsg (tty, ptmx and pts are routed separately).
- `EventFd`, `TimerFd`.
- `Inotify`, `Event`, `Registry` (one per kernel, watches keyed by store inode), `Opened`.

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
  `timerfd_*`; ioctls `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`/`FIONREAD`, random ioctls.
- **Metadata**: `newfstatat`/`fstat`/`statx`, `statfs`/`fstatfs`, `fchmod*`, `fchown*`,
  `utimensat`, `truncate`, `faccessat`/`faccessat2`, `umask`, `mkdirat`, `mknodat`, `unlinkat`,
  `renameat2`, `linkat`, `symlinkat`, `readlinkat`, `getdents64`, `getcwd`, `chdir`/`fchdir`,
  xattr calls.
- **Locks**: `flock`, fcntl lock commands, `F_SETLEASE`/`F_GETLEASE`; `locks::closed`,
  `lease::break_leases`.
- **Devices**: `dev::ensure_dev_nodes` (store `/dev`), `dev::populate` (boot `/dev` tmpfs).
- **inotify**: `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`, read; hooks `active`,
  `path_event`, `file_event`, `dir_event`, `cookie`, `opened`, `renamed`, `deleted`,
  `watched_parent`; `host_change`.
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
- `MS_MOVE`, `pivot_root` (6.1 order, `chroot_fs_refs` for tasks whose root or cwd was the old
  root in the same namespace), `chroot` (`CAP_SYS_CHROOT` in the caller's user namespace and
  search permission).
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
  when `O_ASYNC` turns on, with no `O_NOATIME` ownership check). `F_SETOWN_EX`/`F_GETOWN_EX`
  translate pids through the caller's pid namespace (owners are stored as global ids); plain
  `F_SETOWN`/`F_GETOWN` do not. `F_SETSIG`, pipe size and seals are also handled in fcntl.
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
  devices by device number (section 11), FIFOs the shared pipe, block devices and sockets `ENXIO`.
- Pseudo nodes: permission from the synthetic stat, ptrace checks for process files, nodev for
  PTYs, `EISDIR`/`EROFS`/`EACCES` for writes the filesystem does not allow; content is generated
  at open.
- Every non-`O_PATH` store open records an inotify `Opened` (directory, name, close mask), whether
  or not watches exist; `O_TMPFILE` is not implemented (it reaches the directory branch and gets
  `EISDIR`).

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
  the uid and choosing one of its groups; an argument of -1 always passes, so `chown(-1, -1)` is
  allowed for anyone. The store then clears setuid, setgid (with group exec) and file caps on
  non-directories, for every caller. utimensat: explicit times need the owner or `CAP_FOWNER`,
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
  open file; the two kinds share one table and conflict with each other. Ranges split and merge;
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
  `null`, `zero`, `full`, `random`, `urandom`, `kmsg`, `tty`, `console`, `ptmx`, the `fd`/`stdin`/
  `stdout`/`stderr` links and the `pts`/`shm`/`mqueue` directories.
- Opening dispatches by device number for any node anywhere (unless nodev): 5:0 and 5:1 the
  caller's controlling terminal, 5:2 the ptmx of the devpts next to the node, major 136 a boot
  devpts slave, 1:3/5/7/8/9/11 `Device`, anything else `ENXIO`.
- `/dev/zero`, `/dev/full` and the random devices fill the whole buffer per read; randomness
  comes from the host `/dev/urandom` and never blocks (entropy count fixed at 256; the random
  ioctls follow 6.1's `random_ioctl`).

### eventfd and timerfd

- eventfd: counter or semaphore mode; writing `u64::MAX` is `EINVAL`; overflow blocks; every
  read/write wakes the object (the syscalls live in `kernel/net`'s poll code).
- timerfd: realtime, monotonic, boottime and the alarm clocks; absolute times are converted once
  at settime; settime resets the expiration count; `TFD_TIMER_CANCEL_ON_SET` is accepted but never
  yields `ECANCELED`.

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

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher; `kernel/process` (clone/unshare/setns, execve's fd-table handling and
  mount-flag checks, task exit's `collect`, the fast-path `Cached` resolver); `kernel/mm` (mmap
  `PROT_EXEC` on noexec); `kernel/procfs` (mountinfo, fd links, fdinfo, sysctls for pipes, leases,
  inotify and `protected_*`); `kernel/net` (AF_UNIX bind creating store nodes, poll/epoll reading
  open files, eventfd syscalls); the machine (boot, `store_changed` for inotify); core dumps.

### Outbound

- `kernel/store` (lookups, snapshots, objects, namespace changes, detached directories, xattrs,
  `fs_stats`, the metadata cache reader); `kernel/procfs` (the `Pseudo` views, `pseudo::stat`,
  writability); `kernel/tty` (controlling terminal, ptmx, devpts instances); credentials
  (`permission`, `owns`, `capable_wrt`, `new_owner`, `FileCaps`); `appine_hal` (pread, pwrite,
  poll, lseek, fsync, `copy_file`, `statvfs`, `memory_info`, `fill_random`); the kernel's wake
  objects and `Cx::unlocked`; signals (`SIGPIPE`, `SIGIO`, `F_SETSIG`); `bpf::check_device`;
  `mqueue`; `cgroup`; `pidns`/`nsproxy`.

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

## Confirmed bugs / technical debt

- **Anyone can strip set-id bits and file caps:** `chown(path, -1, -1)` passes the permission
  check for every caller, and the store clears setuid, setgid (with group exec) and
  `security.capability` on any non-directory. An unprivileged user can thus disable `su`,
  `passwd`, `ping` and similar programs owned by root on a writable mount (Linux refuses with
  `EPERM` for set-id files because the implied mode change needs the owner or `CAP_FOWNER`; a
  `CAP_CHOWN` holder without `CAP_FOWNER` is refused the same way).
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
- **`F_SETOWN`/`F_GETOWN` ignore pid namespaces:** pids are stored and returned untranslated, so
  inside a nested pid namespace `SIGIO` goes to the wrong process; `F_SETOWN` with `INT_MIN`
  overflows the negation (a panic in debug builds) instead of `EINVAL`.
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
- **Fidelity gaps:** statx never reports btime, `stx_mnt_id` or attributes; no
  `protected_regular`/`protected_fifos`; no `O_TMPFILE`; no holes (`SEEK_DATA`/`SEEK_HOLE`,
  fallocate writes zeros); `TFD_TIMER_CANCEL_ON_SET` never cancels; `RLIMIT_NOFILE` set by prlimit
  applies to the whole shared fd table; `MNT_EXPIRE` marks are never cleared; propagation is not
  emulated; `sendfile`, `copy_file_range` and `splice` into an `O_SYNC`/`O_DSYNC` file do not
  sync it (Linux's write path does).

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
