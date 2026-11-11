# kernel/vfs Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/vfs.md` for the paired decision history (it still holds the decisions on mounts, inotify, AIO, loop devices, overlayfs, locks, pipes and xattrs made before they were split out).
>
> Verified: 2026-11-10 (full mapping: 6 research concerns; 36 claims checked by 3 independent verifiers — 26 confirmed and 10 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping: read-only write opens, splice out of a pipe, tmpfs recounts, host stdio readv, then mounts, inotify, AIO, timerfd, loop devices, overlayfs, locks, pipes and xattrs split out and wording condensed)

## Responsibilities

`kernel/vfs` is the layer between file-related syscalls and the filesystems behind them (the inode
store and the synthetic filesystems):

- path resolution into mount-aware nodes (Linux `struct path`): symlinks, `..` across mounts,
  search permission, `protected_symlinks`, magic links, openat2's `RESOLVE_*`;
- mount and superblock flag checks in file operations, and what removing or renaming a mount point
  does;
- tmpfs instances and their accounting;
- fd tables and open file descriptions, open flags, `fcntl`, data I/O on regular files (host
  `pread`/`pwrite` without the BKL), `sendfile`/`copy_file_range`;
- clones sharing the store's objects, and giving a shared file an object of its own before it
  changes;
- namespace and metadata syscalls;
- device nodes and the character devices;
- calling the inotify hooks where Linux's fsnotify does, and each open file's `Opened` record.

Root: `core/kernel/src/vfs/mod.rs` (except the mount table and the overlay methods), `vfs/dev.rs`,
`fd.rs` (its `EventFd` and `TimerFd` are described by `kernel/poll`), `syscall/fs.rs` (except its
overlay functions, `flock`, the fcntl lock commands and the pipe, FIFO and splice code), and in
`syscall/mount.rs` the tmpfs options.

Outside the module: the mount table, mount syscalls and mount namespaces (`kernel/mount`); overlayfs
mounts, lookups, copy-up and its removal and rename (`kernel/overlay`, branched to from here);
advisory locks and leases (`kernel/locks`); pipes, FIFOs and splice (`kernel/pipe`); extended
attributes (`kernel/xattr`); store semantics (`kernel/store`); FUSE connections, their node cache
and the file syscalls on FUSE nodes (`kernel/fuse`, dispatched to from here);
procfs/sysfs/cgroup2/devpts content (`kernel/procfs`); terminals (`kernel/tty`); sockets
(`kernel/net`, `kernel/unix`); poll/epoll, eventfd and timerfd (`kernel/poll`); inotify instances,
watches and event rules (`kernel/inotify`); native AIO (`kernel/aio`); loop devices (`kernel/loop`);
message queues (`kernel/ipc`); the BKL and the no-BKL fast path (`kernel/process`); namespaces
(`kernel/ns`); execve (`kernel/exec`); credentials and file capabilities (`kernel/cred`).

## Key types

- `Node` — `Store(MountId, Ino)`, `Pseudo(MountId, PseudoNode)`, `Overlay(MountId, OvlId)` or
  `Fuse(MountId, FuseId)`; the mount is part of its identity, so one directory mounted twice is
  two nodes. `Node::ino` is Some only for store nodes. `Dentry = (Node, name)` is where a result
  was last found.
- `Pseudo` trait — what a synthetic filesystem offers the walker: lookup, parent, name, kind,
  readlink, `follow`/`follow_dentry` (magic-link jumps), `magic`, `protected_symlinks`, `missing`
  (told of each name not found), `searched` (cgroup directories' search check); procfs's `View`
  implements it.
- `resolve` — openat2's `RESOLVE_*` bits; `SCOPED = BENEATH | IN_ROOT`.
- `Vfs` — the store handle, the mount table (`kernel/mount`'s `Mount`, `Mounts`, `Fs`, `MntNs`,
  `Index`, `GroupAlloc`), the no-BKL resolver state, the executable cache, the namespaces, the
  overlay and FUSE tables and the mount event counter.
- `TmpfsParams` / `Tmpfs` — options and usage (pages, inodes), counted lazily with a changed mark.
- `Cached` / `Pending` — the fast-path stat resolver without the BKL (reads the store's metadata
  cache under its read lock; 64-component stack), with the store inodes that are mount points.
- `FdTable` — slots (`FileRef` and `cloexec`), the soft `RLIMIT_NOFILE`, `holds_locks`.
- `OpenFile` (`FileRef = Rc<RefCell<OpenFile>>`) — kind, mode and status flags, position and
  `pos_busy` (0 free, 1 taken, 2 with waiters), display path, dentry, mount, `F_SETOWN*` owner
  with the setter's uids, poll bookkeeping (`kernel/poll`), the inotify `Opened` record, lock
  ownership, open_tree namespace (`tree`), `pseudo` (a memfd: no mount, not a writer; seals in
  `Kernel::seals`), `ovl` and `ovl_holds` (the overlay node behind a file opened through an
  overlay, and its `Hold`s), `retired` (host handles from before the file moved objects, kept
  until close).
- `FileKind` — Host, Regular, Directory, Path, Pipe, Device, Pty, Socket, Epoll, EventFd, TimerFd,
  SignalFd, Inotify, Pidfd, MessageQueue, SeccompNotify, Bpf, Proc, Fuse and FuseDev (a FUSE
  directory is a Directory with the daemon's handle). Regular carries `cow`: opened for writing
  while its file shared an object, so its handle only reads and its first change gives the file
  an object of its own.
- `IoPath` — the route of one I/O call, chosen so no borrow of the open file is held during I/O
  (`Local`: regular files through `unlocked_io`, other local files while borrowed).
- `Kernel::unsharing` — inodes being given an object of their own, which other changes wait for.
- `Device` — Null, Zero, Full, Random, Kmsg, LoopControl, Loop (`kernel/loop`'s; tty, ptmx, pts
  and `/dev/fuse` are routed before it).
- `fs::Attrs` — what chmod, chown and utimensat act on: Store, Overlay, Pts, Anon (inode-less
  files), Mq, MqDir, Fuse.

## Public entry points

- **Resolution**: `Vfs::resolve`, `walk`, `walk_resolve(…, follow, flags)`, `resolve_parent`,
  `resolve_parent_with`, `found_path`, `path_of`, `reach`; `Cached::stat` (fast path).
- **Instances**: `new_tmpfs`, `tmpfs_*`, `new_fuse`, `add_fuse_mount`, `fuse`/`fuse_mut`,
  `fuse_child`, `fuse_permission`. The mount table's methods and the mount syscalls are
  `kernel/mount`'s, the overlay methods `kernel/overlay`'s.
- **fds and I/O**: `openat`, `openat2`, `close`, `close_range`, `dup`/`dup3`, `fcntl`, `lseek`,
  `read`/`write` and the vector and positional forms, `preadv2`/`pwritev2`, `sendfile`,
  `copy_file_range`, `ftruncate`, `fallocate`, `memfd_create`; ioctls
  `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`/`FIONREAD`/`FICLONE`/`FICLONERANGE`, random ioctls,
  and the dispatch of other ioctls by kind.
- **Metadata**: `newfstatat`/`fstat`/`statx`, `statfs`/`fstatfs`, `fchmod*`, `fchown*`,
  `utimensat`, `truncate`, `faccessat`/`faccessat2`, `umask`, `mkdirat`, `mknodat`, `unlinkat`,
  `renameat2`, `linkat`, `symlinkat`, `readlinkat`, `getdents64`, `getcwd`, `chdir`/`fchdir`,
  `chroot`.
- **Devices**: `dev::ensure_dev_nodes` (store `/dev`, at import and every boot), `dev::populate`
  (boot `/dev` tmpfs), `Device::from_rdev`.
- **Kernel-internal**: `create_exclusive`, `unlink_path`, `write_at` (core dumps),
  `FdTable::with_host_stdio` (boot); `prepare_write`, `make_private` (before a regular file
  changes, also from mmap); `host_unshared` (from `kernel/inotify`'s `host_change`);
  `read_file`, `write_file`, `read_bytes`, `write_bytes`, `write_sync`, `vectored`, `fsync`,
  `has_fsync`, `seekable`, `append_offset` (`kernel/aio`, `kernel/loop`).

## Data flow / lifecycle

### Path resolution

- Start (`start_dir`): an empty path is `ENOENT` before the dirfd is examined (`AT_EMPTY_PATH`
  gives the fd's node, or the cwd for `AT_FDCWD`); absolute paths start at the process root,
  `AT_FDCWD` at the cwd; otherwise the fd must carry a node (`O_PATH` fds do; `EBADF`) and be a
  directory (`ENOTDIR`).
- `walk_resolve` runs `walk_in` in one store read snapshot. Empty components are dropped; a
  trailing `/` appends `.`, forcing a directory and following a trailing symlink.
- Per component: over 255 bytes `ENAMETOOLONG`; the node must be a directory (`ENOTDIR`); search
  permission on store directories, the store directory behind an overlay one, and cgroup
  directories (`Pseudo::searched`), skipped with `CAP_DAC_READ_SEARCH` or `CAP_DAC_OVERRIDE`; `.`
  is skipped; `..` follows `follow_dotdot` (stays at the process root, climbs stacked mount points
  at a mount root, takes the parent, enters anything mounted there; a namespace-tree or detached
  root stays put); other names are looked up and lifted to the topmost mount stacked on the
  result. In a FUSE directory the lookup reads `kernel/fuse`'s cache or fails with the internal
  `FUSE_RETRY`, which makes the dispatcher ask the daemon and rerun the syscall (search permission
  there is `fuse_permission`'s). A name not found is first passed to `Pseudo::missing`, so procfs
  counts names Linux would have under `/proc`, `/sys` and the boot `/dev` (`kernel/signal`'s
  unimplemented paths).
- Symlinks (intermediate, or last with `follow`): over 40 `ELOOP`; `protected_symlinks` (default
  on) applies to the last component of store links only (`EACCES`); `NO_SYMLINKS` or nosymfollow
  `ELOOP`. Magic links (`/proc/<pid>/fd/*`, `cwd`, `root`, `exe`, `ns/*`) run the procfs access
  check first (`EACCES`; `fd/*` directories also refuse listing and lookup to readers failing
  it), then `NO_MAGICLINKS` (`ELOOP`), `NO_XDEV` across mounts and scoped resolution (`EXDEV`),
  and jump to the target node; a deleted regular file, pipe, FIFO, socket, FUSE file, host fd,
  anonymous-inode file or message queue jumps to the open file itself, and reopening one is `ENXIO`
  unless it is a regular file, pipe or host fd. Ordinary absolute targets restart at the root
  (`EXDEV` under `BENEATH`, or under `NO_XDEV` when the root was not yet taken or the link is on
  another mount); relative ones continue from the link's directory.
- openat2: `open_how` size and flags validated per 6.1's `build_open_flags`; `BENEATH|IN_ROOT`
  `EINVAL`; scoped resolution starts at the dirfd, `IN_ROOT` absolute paths too. `RESOLVE_CACHED`
  is only a pre-check (`EAGAIN` for create, truncate, `O_TMPFILE`, and through 6.1's macro
  comparison `O_DIRECTORY`), then a normal walk, which still asks a FUSE daemon on a miss.
- Creation (`resolve_parent_with`): trailing slashes trimmed, the prefix walked with follow; a
  slash-free name returns `(cwd, name)` without a search check (callers check); a last `.`/`..` is
  left to the caller.
- `O_CREAT`: `O_EXCL` turns follow off and any existing name is `EEXIST`; on `ENOENT` a loop runs:
  a trailing `/` or last `.`/`..` `EISDIR`, the mqueue root creates a queue, a cgroup directory
  `EACCES`, dangling store, overlay and FUSE symlinks are followed to create their targets (same
  cap and restrictions; `NO_XDEV`'s root-taken rule approximated), a FUSE directory asks the
  daemon.
- Back to paths: `reach` climbs through mount points (or the recorded dentry for mounts on files)
  and stops as unreachable at a namespace-tree root, detached root, tmpfs root, the store root
  inside a bind, a parentless synthetic node, another mount's overlay or FUSE root, or a forgotten
  FUSE node; a deleted directory (a store one keeps its row without a name, an overlay one once
  its name is removed) is `ENOENT` for getcwd and `/proc/<pid>/cwd`. mountinfo's `mounts_from`
  does the same for every mount through an index built once. `getcwd` prefixes `(unreachable)`;
  `/proc/<pid>/{cwd,root}` are computed live against the reader's root; an fd's display path is
  fixed at open (a magic-link reopen of a pipe, memfd or host fd freezes the path it was opened
  by, such as `/proc/self/fd/3`), with ` (deleted)` appended when a regular file's nlink is 0.
- `Cached` (no-BKL newfstatat and statx) runs only when the root is in the root filesystem's
  mount, the credentials search everything, that mount is attached and not nosymfollow, and the
  path is absolute or `AT_FDCWD`-relative with the cwd in that mount. It returns `None` on a stale
  generation, any cache miss, over 64 pending components, a store inode that is a mount point (the
  store root after `pivot_root` too) and trailing links `protected_symlinks` might refuse; it never
  enters synthetic filesystems or overlays. Its walk mirrors `walk_in` step for step. Mount table
  changes call `mounts_changed` (also bumping the mount event counter that mounts and mountinfo
  poll), and syscalls refresh threads with `fast_changed`.

### Mount flags and mount points in file operations

- This module reads `kernel/mount`'s table while resolving and checks its flags where Linux does:
  ro of the mount or the filesystem (`readonly` includes `sb_readonly`: the instance's flag, or an
  overlay without an upper layer) — `EROFS` in creation after `EEXIST` and before directory write
  permission, in unlink/rmdir/rename before lookup, in attribute changes, in write opens of
  regular files (`O_TRUNC` before permission); noexec (exec `EACCES`, `PROT_EXEC` mmap `EPERM`);
  nosuid (exec ignores set-id bits and file caps); nodev (device opens `EACCES`); nosymfollow
  (`ELOOP`). Atime flags and the sync, dirsync, mand and lazytime superblock flags are only
  recorded and reported.
- Removing or renaming a mount point in the caller's namespace is `EBUSY` (rename: either name);
  mounts on it in other namespaces are detached (rename: only on the replaced target). Removing
  the name a mount's root is (a bound directory, or the name a bound file was found at, even with
  links left) marks that mount in every namespace (`roots_removed`), and mountinfo then shows the
  former path plus `//deleted` (`dentry_path`). Rename and link across mounts are `EXDEV`, even
  between binds of one filesystem.

### tmpfs

- An instance is a detached store directory (`create_detached_dir`); files are ordinary objects.
  Options follow 6.1's `shmem_parse_one` (`size=` with suffixes or `%`, `nr_blocks=`, `nr_inodes=`,
  octal `mode=`, `uid=`, `gid=`, `inode32`/`inode64`; `huge=`/`mpol=` and generic words such as
  `ro` accepted and ignored; unknown `EINVAL`); `uid=`/`gid=` are kernel IDs, not mapped through
  the caller's user namespace (`u32::MAX` `EINVAL`); the root defaults to 1777 owned by the
  mounter's fsuid/fsgid. mountinfo shows `size=` and `nr_inodes=` when not default, `mode=` when
  not 1777, `uid=`/`gid=` when not 0, then `inode64` or `inode32` (`shmem_show_options`). A remount
  takes only the limits and `inode32|64`; turning an unlimited instance limited, or going below
  what is in use, is `EINVAL`.
- Accounting per instance: one inode per name plus the root; regular files by the pages their host
  objects take (holes take none: lengthening by truncation is free, punching gives pages back);
  symlinks of 128 bytes or more one page. Limits default to half of RAM in pages; 0 is unlimited.
  Creates need room (`ENOSPC`); a write needs room only for its pages holding no data yet (found
  with the host's `SEEK_DATA`/`SEEK_HOLE` when they might not fit, less pages reserved and
  unwritten, which the host reports as holes) and is clipped where room ends; reservations need
  room for the whole range or fail; truncation is never limited; the last unlink frees at once.
  statfs and remount recount the tree, as does a create, write or reservation about to be refused
  or cut when usage changed since the last count (`tmpfs_recount`, once, before looking again);
  boot instances start uncounted and charge nothing until the first recount. The mount's
  filesystem decides what is charged, so changes through an overlay never count against a tmpfs
  its upper layer is in.

### fd tables and open files

- Allocation takes the lowest free slot at or above the minimum and below the table's limit
  (`EMFILE`; `F_DUPFD` with min at or over the limit `EINVAL`; dup3 targets over it `EBADF`).
  `prlimit(RLIMIT_NOFILE)` sets the limit on the table, so it changes for every process sharing
  it through `CLONE_FILES` (Linux checks each process's own rlimit).
- dup and fork share one `OpenFile` (offset and status flags shared). Clone without `CLONE_FILES`
  copies the slots and resets `holds_locks`. Every thread holds two references to its table:
  `unshare(CLONE_FILES)`, execve (after the other threads are gone) and `CLOSE_RANGE_UNSHARE` copy
  only when there are more, that is when another thread or process shares it; a copy is a new
  owner of POSIX locks, so an unshared table keeps its locks. `unshare(CLONE_FILES)` is also
  `EMFILE` when the highest fd, rounded up to 64, passes `nr_open`. execve then applies
  close-on-exec.
- Closing (close, dup3 over an fd, close_range, close-on-exec) removes the slot, runs
  `locks::closed`, `mqueue_closed`, flushes a FUSE file (close returns the flush's error, the
  others ignore it) and `unix::released`; the last reference's `Drop` tells `vfs::released` its
  mount, wakes host-socket (not AF_UNIX) waiters, wakes lock waiters when the file held locks,
  and drops the `Opened` record (`IN_CLOSE_*`).
- `OpenFile` keeps only the access mode, `O_APPEND`, `O_NONBLOCK`, `O_PATH`, `O_DSYNC`,
  `__O_SYNC` and `O_LARGEFILE` from open, and with `O_PATH` only `O_PATH` (`build_open_how`), so
  `F_GETFL` never shows `O_DIRECT`, `O_NOATIME` or `O_ASYNC` given at open. An `O_PATH` fd takes
  only `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD` and `F_GETFL` (else `EBADF`,
  `check_fcntl_cmd`), and flock on it is `EBADF`. `F_SETFL` changes
  `O_APPEND|O_NONBLOCK|O_ASYNC|O_DIRECT|O_NOATIME` (registering fasync when `O_ASYNC` turns on for
  a kind that has it; no `O_NOATIME` ownership check). Access mode 3 needs read and write
  permission but makes the file neither readable nor writable (`OPEN_FMODE`).
- Owners: `F_SETOWN`/`F_GETOWN`, `F_SETOWN_EX`/`F_GETOWN_EX` and `FIOSETOWN`/`SIOCSPGRP`,
  `FIOGETOWN`/`SIOCGPGRP` translate pids through the caller's pid namespace (the dispatcher's
  `pid_args` for `F_SETOWN`'s argument, -1 included as process group 1, and `F_GETOWN`'s result;
  the others themselves). Owners are global ids with the setter's real and effective uid for the
  delivery check (`kernel/poll`); an unknown or invisible number is `ESRCH` (`find_vpid`), 0
  removes the owner, an owner outside the caller's namespace reads as 0, `F_SETOWN(INT_MIN)` is
  `EINVAL`. fcntl also handles `F_SETSIG`, pipe size and seals. memfd_create takes 6.3's
  `MFD_NOEXEC_SEAL` (mode 0666, sealed with `F_SEAL_EXEC`, sealing allowed) or `MFD_EXEC`, not
  both (`EINVAL`); `F_ADD_SEALS` with `F_SEAL_EXEC` on a memfd with an execute bit also adds the
  shrink, grow, write and future-write seals (`memfd_add_seals`).
- Boot: with host stdio, fds 0–2 wrap the host's fds (`FileKind::Host`: reads poll the host fd
  and read without the BKL; writes ignore guest `O_NONBLOCK`); with a console, the PTY slave.
- ioctl: `FIOCLEX`/`FIONCLEX`/`FIONBIO`/`FIOASYNC`, the clone and dedupe requests, socket owners
  and `FIONREAD` on regular inodes (store and tmpfs files, procfs, sysfs and nsfs files, message
  queues: the stat size less the position, as an int, negative past the end, `do_vfs_ioctl`; not
  FUSE files) come before dispatch by kind (nsfs, `/dev/fuse`, seccomp listeners, terminals,
  sockets, loop devices, pipe and inotify `FIONREAD`, the random device); `O_PATH` is `EBADF`
  before all; host fds pass only the termios and window-size requests and `FIONREAD` to the host.
  Anything else is `ENOTTY`, counted as an unimplemented path unless Linux refuses it there too.

### Opening

- `open_path_with` (openat, openat2, `create_exclusive`): resolve; on create, `may_create`
  (search, `EEXIST`, `EROFS`, write+search) and create with the mode `new_owner` makes of the
  requested one and the umask (`kernel/cred`: setgid stripped before the umask, setgid
  directories); a just-created file skips permission checks and truncation.
- Store nodes (`open_store`): symlink `ELOOP`; a directory with write access, `O_CREAT` or
  `O_TRUNC` `EISDIR`; `O_TRUNC` on ro `EROFS`, else it adds `MAY_WRITE`; devices on nodev
  `EACCES`; permission; the cgroup BPF device filter; `O_NOATIME` by a non-owner without
  `CAP_FOWNER` `EPERM`; write on ro `EROFS` (the mount's or instance's flag); the `O_DIRECT` rule;
  `EISDIR` for directories and `ENOTDIR` for `O_DIRECTORY` (after the checks above, where
  `do_open` checks before `may_open`); writing an executing file `ETXTBSY`; break leases; `O_TRUNC`
  removes set-id bits and truncates, with `O_RDONLY` too (through a writable handle of its own), a
  held file first getting an empty object of its own; any other write open of a held file gets a
  read-only handle on the shared object, marked `cow` (as a `/proc/<pid>/fd` reopen of one is).
  Then by type: regular files open the host object, character devices by device number, block
  devices as loop devices (major 7, `kernel/loop`; others `ENXIO`), FIFOs the shared pipe,
  sockets `ENXIO`.
- FUSE nodes are opened and created by `kernel/fuse` (the daemon's open and create); a file
  created that way is not checked again.
- Pseudo nodes: permission from the synthetic stat (`O_TRUNC` asks for write), ptrace checks for
  process files, nodev for PTYs (devpts nodes also pass the device filter), `EISDIR`/`EROFS`,
  nsfs writes `EPERM`, `EACCES` for writes the filesystem does not allow except read-only procfs
  files outside `/proc/sys`, which root opens and whose writes fail (`kernel/procfs`); content is
  generated at open, and the opener's credentials are kept for regeneration and id-map writes.
- Magic links to nameless targets — `/proc/<pid>/fd/N` of a deleted or memfd file, a pipe or a host
  fd (`OpenFd`), `/proc/<pid>/exe` of a deleted program (`OpenExe`) — skip the synthetic stat and
  go to `reopen`/`reopen_exe`: permission against the host object's stat (`O_TRUNC` asks for
  write), seals only for `O_TRUNC` with a write mode, `ETXTBSY` for a program, then `open_store`
  as if just created, so `EROFS`, `O_NOATIME`, lease breaks and (for fds) `ETXTBSY` are skipped;
  pipes and host fds reopen with no check. Linked targets resolve to store or overlay nodes and
  get the normal checks.
- Every non-`O_PATH` open of a regular file, device or directory (store, overlay, FUSE, cgroup2)
  or FUSE file records an inotify `Opened` (`kernel/inotify`), watches or not. `O_TMPFILE` is not
  implemented: invalid combinations are `EINVAL` as on Linux (with `O_CREAT`, or read-only), a
  valid one reaches the directory branch (`EISDIR`). `O_DIRECT` opens only regular files and loop
  devices on mounts other than tmpfs (6.1's `direct_IO` check, before the file's own open, decided
  by the mount's filesystem); directories, tmpfs files, character devices, FIFOs and synthetic
  files are `EINVAL`.

### Data I/O on regular files

- Without an explicit offset, `unlocked_io` emulates `f_pos_lock`: wait while `pos_busy` is set,
  take it, snapshot the position, run host `pread`/`pwrite` with the BKL released, then advance,
  in 64 KiB chunks (8 MiB for FUSE files and `/dev/fuse`), releasing `pos_busy` between chunks.
  Only `unlocked_io`, `copy_regular` and FUSE I/O take `pos_busy`; lseek, `O_APPEND` writes,
  sendfile's and copy_file_range's rollbacks and `read_file`'s fault fix-up move the position
  without it (directories never do unlocked I/O).
- `O_APPEND` writes run under the BKL at the current host length; `pwrite` on an `O_APPEND` file
  appends too unless `RWF_NOAPPEND` (6.9's flag, honoured only with an explicit offset).
  preadv2/pwritev2 refuse `RWF_NOWAIT` and `RWF_APPEND` (`EOPNOTSUPP`). After each write call
  `write_sync` syncs the object (BKL released) for `O_DSYNC` (fdatasync), `O_SYNC` (fsync) or
  `RWF_DSYNC`/`RWF_SYNC`, so writev syncs once per iovec; sendfile, copy_file_range and splice into
  such a file do not sync.
- A zero-length `read` or `pread` returns 0 before the readable, `ESPIPE` and directory checks
  (write checks first); `fallocate` checks offset, length and `EFBIG` before the fd.
- Holes are the host's: `SEEK_DATA`/`SEEK_HOLE` ask the host object (outside the file `ENXIO`; a
  host that cannot tell has data everywhere). `fallocate` passes mode 0, `KEEP_SIZE`,
  `PUNCH_HOLE` (with `KEEP_SIZE`) and, outside tmpfs, `ZERO_RANGE` to the host; a host without the
  mode falls back to writing zeros and lengthening. Other modes `EOPNOTSUPP`.
- `sendfile` and `copy_file_range` between distinct regular store files use host `sendfile`
  without the BKL (FUSE files count as regular for copy_file_range and are copied block by block,
  as 6.1 falls back without `FUSE_COPY_FILE_RANGE`), falling back to chunked copies for
  `O_APPEND` output, the same inode, a sealed or tmpfs output, or host
  `EINVAL`/`ENOSYS`/`EOPNOTSUPP`/`EXDEV`. `copy_file_range` checks fds, offsets, then flags
  (`EINVAL`), then in `generic_copy_file_checks` order per file (a non-regular input before a
  directory output): directories `EISDIR`, other non-regular files `EINVAL`, modes and `O_APPEND`
  `EBADF`, different devices `EXDEV` (the mounts' device numbers; a memfd counts as the root
  filesystem), wrapping ranges `EOVERFLOW`, then the length cut to the input's end,
  `RLIMIT_FSIZE` and the largest file size (`EFBIG`), overlap in one file `EINVAL`, negative
  positions `EINVAL`. `sendfile` rejects `O_APPEND` output other than a pipe (`EINVAL`,
  `do_splice_direct`).
- Writes, truncation and fallocate on tmpfs files go through the instance's accounting.
- Before a regular file changes (a write through `write_bytes` or the host copy; ftruncate,
  truncate, `O_TRUNC`; fallocate; a shared mapping from a writable descriptor), `prepare_write` or
  `make_private` gives a held file an object of its own (`kernel/store`'s unshare), before set-id
  bits are removed: the copy (or what a truncation keeps) is made with the BKL released while the
  file is in `Kernel::unsharing`, where other changes wait on the lock channel. Then `rehome` gives
  every description of the file (fd tables, AF_UNIX messages in flight, loop backing files) a
  handle on its current object, read-only and `cow` while still held, retires the old handles and
  forgets the cached executable. Shared mappings move to a new object too (`move_mappings`,
  through `kernel/mm`'s `file_moving`/`file_moved`), or all stay when some address space is busy.
- Clones: copy_file_range of a whole non-empty input from offset 0 into an output no longer than
  it, and FICLONE or FICLONERANGE doing the same, share the input's object (`try_share`) unless
  either is a memfd, opened through an overlay or on tmpfs, the input is deleted, open for writing
  in this kernel by a description not marked `cow`, or writably mapped shared, either is open for
  writing by the Files app (`host_writing`), or the output is mapped, sealed or being unshared;
  then they copy. The output's set-id bits go first and its descriptions move to the shared object
  (`rehome`); copy_file_range returns the whole length at once and advances the positions.
  FICLONE/FICLONERANGE check in 6.1's order: different mounts (memfds on their own) `EXDEV`,
  directories `EISDIR`, other non-regular files `EINVAL`, modes and `O_APPEND` `EBADF`, memfds,
  overlay and tmpfs files `EOPNOTSUPP` (no remap_file_range), negative or wrapping ranges
  `EINVAL`; length 0 reaches the input's end (nothing at the end, `EINVAL` past it); unaligned
  offsets, an input offset at or past the end, a length cut by the end or `RLIMIT_FSIZE` (at or
  past it `SIGXFSZ` and `EFBIG`) or unaligned short of the end, and overlapping ranges in one file
  are `EINVAL`. Other ranges are copied. `FIDEDUPERANGE` is `EOPNOTSUPP`.
- readv and writev follow Linux's `read_iter`/`write_iter` split: pipes, terminals and sockets
  (and eventfd reads) do one read or write over all iovecs (one read of up to 64 KiB scattered in
  order; writes in 64 KiB pieces across iovecs until one is short; sockets through `kernel/net`),
  so a pipe readv returns what was there and small pipe writes stay atomic; `/dev/fuse` does the
  same in pieces of up to 8 MiB, since a FUSE message moves whole. Host stdio fds that are not
  regular files on the host take the pipe path; a host regular file goes iovec by iovec.
  Everything else goes iovec by iovec and stops at an incomplete one, as Linux does for files with
  only `read`/`write`. A total of 0 returns 0 after the offset and mode checks.
- Copying to and from the guest buffer first grows a grows-down mapping below it (`kernel/mm`),
  except vmsplice's write side, which reads guest memory directly. A read whose buffer faults
  partway returns the bytes before the faulting page (`EFAULT` when none): a regular file's
  position moves only past what was copied, while other files' data (pipe, terminal) is already
  consumed. Writes copy the guest buffer 64 KiB at a time, so a fault fails with `EFAULT` only in
  the first piece (a pipe writev also writes the iovecs before the faulting one); earlier pieces
  stay written and count.

### Metadata and namespace syscalls

- Stat: asm-generic `struct stat`, uid/gid mapped into the caller's user namespace; `st_dev` from
  the mount; `st_blksize` 4096 (1024 for procfs and devpts nodes); `st_blocks` of regular store and
  tmpfs files (and host fds) as the host allocated them, of others from the size (sysfs files 0;
  symlinks under 60 bytes on the root filesystem or 128 elsewhere live in the inode); directories
  report size 4096 on the root filesystem. statx always reports `STATX_BASIC_STATS` only.
  newfstatat takes only `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` (`EINVAL` for
  `AT_NO_AUTOMOUNT` and the `AT_STATX_*` sync flags, which statx accepts).
- statfs: the store reports ext4's magic with the host's `statvfs` of the objects directory, tmpfs
  its magic and limits (recounted), an overlay overlayfs's magic with the store's capacity, FUSE
  what the daemon answers, synthetic filesystems their magic with zero sizes; `f_flags` from the
  mount plus the filesystem's read-only, sync and mand (`flags_by_sb`). fstatfs of a pipe reports
  pipefs, of a message queue mqueue, of other node-less fds the root filesystem's ext4 figures.
- access/faccessat: real IDs unless `AT_EACCESS` (capabilities only when the real uid is root in
  the caller's namespace); `F_OK` returns right after the lookup; `EROFS` for `W_OK` after the
  permission check, on regular files, directories and symlinks only; FUSE nodes ask the daemon.
- chmod drops `S_ISGID` when the caller is outside the file's group without `CAP_FSETID` over
  it; on a memfd sealed with `F_SEAL_EXEC` changing an execute bit is `EPERM` after the owner check
  (6.3's `shmem_setattr`). chown needs mapped IDs (`EINVAL`) and `CAP_CHOWN` over the file or an
  owner keeping the uid and choosing one of its groups (-1 always passes). On a store
  non-directory it then clears setuid, setgid when group-executable or when the caller is outside
  the file's group without `CAP_FSETID` (6.2's `setattr_should_drop_sgid`), and file caps; when a
  set-id bit would go the caller must be the owner or have `CAP_FOWNER` (`EPERM`, bits kept), so
  `chown(-1, -1)` by another user fails on `su` but succeeds on a file with only caps (which lose
  them, as on Linux). Writes and truncation call `remove_privs` (`CAP_FSETID` checked in the
  initial namespace).
- utimensat: explicit times need the owner or `CAP_FOWNER`; `UTIME_NOW`/`UTIME_OMIT` only (one of
  each included) also accept write permission; the both-`UTIME_OMIT` shortcut runs after
  resolution. On synthetic nodes other than devpts terminals and message queues, chmod, chown and
  utimensat are `EROFS` on a read-only mount and `EACCES` otherwise by path, `EBADF` by fd (cgroup2
  nodes included).
- truncate (6.1's `vfs_truncate` order): `EISDIR`, `EINVAL` for non-regular, `EROFS`, write
  permission (a synthetic regular file then succeeds unchanged), `ETXTBSY`, `RLIMIT_FSIZE`, then
  leases (6.1 breaks leases before `inode_newsize_ok`); ftruncate `EINVAL` unless writable and
  regular, `EBADF` on `O_PATH`.
- mknodat: regular, char, block, FIFO, socket (directory `EPERM`); devices need `CAP_MKNOD` and
  pass the device filter (whiteouts exempt). renameat2 takes `RENAME_NOREPLACE` and
  `RENAME_EXCHANGE` (`RENAME_WHITEOUT` `EINVAL`); `EXDEV` comes before the `.`/`..` checks (a
  source `.`/`..` `EBUSY`; a destination one `EEXIST` with `RENAME_NOREPLACE`, else `EBUSY`). In
  synthetic filesystems (`vfs_rename` order): read-only mount `EROFS`, missing source `ENOENT` (in
  procfs a missing destination too), `RENAME_NOREPLACE` onto an existing name `EEXIST`, a name onto
  itself 0; otherwise the mqueue root and cgroup2 `EPERM`, others `EACCES` without write and
  search permission on the source directory, `EPERM` with it.
- linkat: `AT_EMPTY_PATH` without `CAP_DAC_READ_SEARCH` is `ENOENT` (`do_linkat`); `EXDEV` for
  synthetic nodes or different mounts, then `protected_hardlinks` (judged on the database row),
  then directory write permission; a magic link followed with `AT_SYMLINK_FOLLOW` reaches the same
  store node, a deleted file's link the store node on the mount it was opened through, a memfd's
  nothing (`EXDEV`); a source with link count 0 is `ENOENT`; no `EMLINK` limit.
- getdents64 takes the listing (with `.` and `..`) on the first call and refreshes it only on a
  seek to 0; `d_off` is index+1. `O_PATH` `EBADF`, a removed directory `ENOENT`; while entries
  remain, a count above `INT_MAX` or a buffer too small for the next entry `EINVAL`; entries are
  copied one by one, so an unwritable buffer returns the whole entries before it (`EFAULT` when
  none). lseek on a directory takes `SEEK_SET` and `SEEK_CUR` everywhere; `SEEK_END`, `SEEK_DATA`
  and `SEEK_HOLE` count from the end as the filesystem's `llseek` does: store directories end at
  the largest position (ext4's hashed directories), procfs, sysfs and cgroup2 at 0, every other
  filesystem's (tmpfs, devpts, mqueue, overlay, FUSE, nsfs) refuse them (`EINVAL`).
- chdir/fchdir/chroot check search on store, overlay, FUSE and cgroup directories; chroot then
  needs `CAP_SYS_CHROOT` in the caller's user namespace. umask lives in the fs context.
- `protected_symlinks` and `protected_hardlinks` exist (default 1); `protected_regular` and
  `protected_fifos` do not.

### Devices

- `dev::populate` (boot `/dev` tmpfs) and `ensure_dev_nodes` (the store's `/dev`, after creating
  `/proc`, `/sys` and `/run` in the store root; nothing when `/dev` is not a directory) create the
  missing names, all root's: `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx`, `fuse`
  0666, `kmsg` 0644, `console` 0600, `loop-control` and `loop0`–`loop7` 0660 group `disk` (gid 6),
  the `fd`/`stdin`/`stdout`/`stderr` links into `/proc/self/fd`, and `pts` (0755), `shm` (1777) and
  `mqueue` (0755).
- Opening dispatches by device number for any node anywhere (nodev `EACCES`, then the cgroup BPF
  device filter): 5:0 and 5:1 the controlling terminal, 5:2 the ptmx of the devpts beside the node
  (`ENOENT` without a `pts` there, `ENODEV` when it is not devpts), major 136 a boot devpts slave,
  1:3/5/7/8/9/11 `Device`, 10:237 loop-control and block 7:N (`kernel/loop`), 10:229 `/dev/fuse`
  (`kernel/fuse`); anything else `ENXIO`, counted as an unimplemented path.
- `/dev/zero`, `/dev/full` and the random devices fill the whole buffer per read; randomness comes
  from the host's `getrandom(2)` and never blocks (entropy count fixed at 256; the ioctls follow
  `random_ioctl`); writes to the random devices are discarded, to `/dev/full` `ENOSPC`.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher.
- `kernel/process`: fd tables and fs contexts at clone; the fast-path `Cached` resolver.
- `kernel/ns`: fd-table and fs copies at unshare; `kernel/exec`: execve's fd-table handling and
  mount-flag checks.
- `kernel/mount`: walks; new tmpfs and FUSE instances and tmpfs reconfiguration; the fd tables its
  busy and writer checks scan.
- `kernel/overlay`: `open_store`, `prepare_write`, the tmpfs instances its layers are in;
  `kernel/locks`: fd tables, open files, `lock_ino`; `kernel/pipe`: file checks and the read and
  write paths of a splice's other end; `kernel/xattr`: resolution and mount flags.
- `kernel/mm`: mmap `PROT_EXEC` on noexec.
- `kernel/procfs`: fd links and fdinfo; sysctls for `protected_*`.
- `kernel/unix`: AF_UNIX bind creating store nodes.
- `kernel/poll`: poll/epoll reading open files; eventfd and timerfd reads through the read path.
- `kernel/inotify`: `host_unshared`, overlay keys, second walks; `kernel/aio` and `kernel/loop`:
  the read, write and fsync paths.
- `kernel/ipc`: queue fds. `kernel/fuse`: the walk and fd-table helpers. Core dumps. The machine
  at boot (`/dev` nodes, host stdio).

### Outbound

- `kernel/mount`: the mount table resolution lifts through and climbs out of; mount and
  superblock flags; detaching mount points at unlink and rename; `released` when an open file, cwd
  or root in a detached mount goes.
- `kernel/store`: lookups, snapshots, objects, namespace changes, detached directories, `fs_stats`,
  the metadata cache reader.
- `kernel/inotify`: the hooks, `opened`, `renamed`, `deleted`.
- `kernel/overlay`: overlay nodes in the walk, opening, creation, attribute changes, truncate,
  readdir, stat and statfs; the sweep at syscall end.
- `kernel/locks`: fcntl lock commands, `locks::closed` on every close, `break_leases` on opens and
  truncation.
- `kernel/pipe`: `pipe2`, FIFO opens, pipe reads and writes, splice, tee and vmsplice, the pipe-size
  fcntl commands.
- `kernel/loop`: open, I/O, lseek, fsync and ioctls of loop devices.
- `kernel/fuse`: everything on FUSE nodes and open FUSE files (the cache the walk reads, open and
  create, namespace and attribute changes, I/O, flushes, statfs, `/dev/fuse`).
- `kernel/procfs` (the `Pseudo` views, `pseudo::stat`, writability), `kernel/tty` (controlling
  terminal, ptmx, devpts), `kernel/ipc` (mqueue objects), `kernel/poll` (eventfd and timerfd
  reads), `kernel/unix` (files in flight, for clones).
- `kernel/mm` for clones and unsharing: `writably_maps`, `maps_file`, `file_moving`,
  `file_moved`.
- Credentials (`permission`, `owns`, `capable_wrt`, `new_owner`, `FileCaps`), `bpf::check_device`,
  `cgroup`, `pidns`/`nsproxy`; signals (`SIGPIPE`, `SIGIO`, `F_SETSIG`); `kernel/signal`'s `stub`
  for unimplemented fcntl commands, ioctls and devices.
- `appine_hal`: pread, pwrite, poll, lseek, fsync, `copy_file`, `statvfs`, `memory_info`,
  `fill_random`; the kernel's wake objects and `Cx::unlocked`.

## Invariants and gotchas

- A node's mount is part of its identity: `..`, root checks, `BENEATH`, `EXDEV` and busy checks
  all compare mounts, so binds of one filesystem are distinct.
- `Node` values are held without counting (cwds, roots, open files, mounts, inotify watches):
  `kernel/fuse` may forget a FUSE node between syscalls only when its scan finds none, so a new
  long-lived holder of FUSE nodes must join that scan.
- `mounted()` matches a whole node, so a mount on a file shows through every hard link to it in
  that mount; the process root itself is never lifted (a mount on `/` is not seen by absolute
  lookups).
- Synthetic directories are searchable by everyone except cgroup directories (their mkdir mode);
  procfs gates per-process data in readlink/follow.
- Every file operation keeps `kernel/overlay`'s rules for overlay nodes: no `Node::Store` is ever
  kept for an overlay file's store inode (changes go through a copy-up), every open file naming an
  overlay node holds it, and a syscall that waits while keeping a looked-up node stays out of
  `waits_on_files_only`.
- File operations added here must call `kernel/inotify`'s hooks where Linux's fsnotify does,
  passing an overlay node's key (`key_of`), never the store inode behind it; every open records an
  `Opened`, and every rename that moves open files scans them all, even with no watches.
- `kernel/aio`'s `has_iter` and `has_fsync` must follow new `FileKind`s and devices.
- A walk may fail with the internal `FUSE_RETRY`: code that swallows walk errors (`.ok()`,
  `has_child`) must not turn it into "not found" where that changes what is done, and nothing that
  changes state may come before a walk that can need it.
- `open_store`'s "just created" bypass is also how `reopen` and `reopen_exe` enter it, so a check
  added behind that bypass is skipped for `/proc/<pid>/fd` and `exe` reopens unless they repeat it.
- A held file's object is shared and must never be written in place: every path that changes a
  regular file calls `prepare_write` (or `make_private`) first, with no borrow of the open file
  held, and takes the host fd only afterwards. A `cow` description's handle is read-only, so a
  path that forgets fails with `EBADF` instead of writing other files' data.
- A description's host handle can be replaced (`rehome`, `follow_copy_up`) while I/O without the
  BKL still uses the old fd number: old handles are kept until the description closes.
- `rehome` and `opens_of` skip descriptions borrowed at that moment, and a skipped read-only one
  would keep reading the old object, so no code may hold a borrow of an open file across a BKL
  release or a wait. The one known exception is ftruncate's `if let` borrow kept across a
  lease-break wait, reachable only with a lease conflicting with a writable description.
- Unlocked I/O snapshots the position before releasing the BKL and writes it back afterwards, so
  other code that moves the position must take `pos_busy` or accept being overwritten.

## Confirmed bugs / technical debt

- **`/proc/<pid>/fd` reopens skip open checks:** a nameless target (deleted or memfd file, pipe,
  host fd) is checked only against the host object (pipes and host fds not at all, where Linux
  checks the pipe inode's 0600 mode and owner) — no `EROFS`, `O_NOATIME`, `ETXTBSY` or lease
  breaks; its display path is the reopening path (`/proc/self/fd/3`, ` (deleted)` for a memfd), not
  `pipe:[N]`, `/memfd:name` or the host path.
- **tmpfs charges drift for unlinked open files:** writes through a description still open after
  the last unlink are charged and never given back (nothing uncharges at close; recounts skip
  unlinked files), while truncating or punching one uncharges pages already given back, so writes
  can pass the limit until the next recount. Linux charges until eviction at the last close.
- **Writes keep a setgid bit chown would clear:** `remove_privs` clears setgid only with group
  exec (6.1), while chown follows 6.2; since 6.2 Linux's writes match its chown.
- **File position races:** lseek and `O_APPEND` writes move the position without `pos_busy`, so a
  concurrent unlocked read or write can overwrite it (Linux's lseek takes `f_pos_lock`).
- **No inode flags:** `FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` are unsupported, while 6.1's tmpfs has
  append-only, immutable, nodump and noatime (LTP unlink09, copy_file_range02).
- **atime ignores the guest:** a regular file's atime is its host object's, updated by the host's
  rules whatever `O_NOATIME`, `noatime`, `nodiratime` or `strictatime` say; directories never get
  an atime of their own (LTP open12, mount03).
- **Fidelity gaps**, one per line:
  - statx: no btime, `stx_mnt_id`, attributes or `STATX_DIOALIGN`;
  - no `protected_regular`, `protected_fifos` or `O_TMPFILE`;
  - a fault on a mapping of a full tmpfs never raises `SIGBUS`, and shared-mapping writes are
    counted only at a recount;
  - `RLIMIT_NOFILE` from prlimit applies to the whole shared fd table;
  - sendfile, copy_file_range and splice into an `O_SYNC`/`O_DSYNC` file do not sync it;
  - preadv2/pwritev2 refuse `RWF_NOWAIT` and `RWF_APPEND` (6.1 supports both);
  - FIONREAD on a FUSE regular file is `ENOTTY`;
  - newfstatat refuses `AT_NO_AUTOMOUNT` and the `AT_STATX_*` sync flags;
  - utimensat with one `UTIME_NOW` and one `UTIME_OMIT` passes on write permission (Linux wants
    the owner unless both are `UTIME_NOW`);
  - chmod and chown of cgroup2 nodes are refused, where kernfs lets a privileged caller chown them
    (systemd's cgroup delegation does);
  - readlink of `/proc/<pid>/cwd` for a removed cwd is `ENOENT` (Linux appends ` (deleted)`).
- **Error order in I/O calls:** a zero-length read or pread skips the readable (`EBADF`, `EINVAL`
  for epoll), `ESPIPE` and `EISDIR` checks; pread and pwrite look up the fd before refusing a
  negative offset; fallocate checks its arguments before the fd, and on a socket is `ESPIPE`
  where 6.1 says `ENODEV`.
- **writev to a synchronous file syncs per iovec** (slower, same durability), and a sync failing
  on a later iovec returns the earlier total, leaving out bytes already written.
- **Scoped openat2 never returns `EAGAIN`:** the walk is atomic under the BKL, so a `..` walked
  while another thread mounts or renames never asks the caller to retry. Safe.
- **Anonymous pipes and sockets have no owner:** fstat shows root (mapped into the caller's
  namespace); fchown is checked as if the caller owned them and changes nothing.
- **Private mappings do not follow a file to its own object:** a private mapping made while the
  file shared an object keeps the shared data in pages it has not copied after a write through
  another descriptor. Shared mappings follow, unless an address space was busy then.
- **Clone fidelity:** FICLONE and FICLONERANGE work on a store that reports ext4's magic (ext4
  says `EOPNOTSUPP`); partial ranges are copied, not shared; `FIDEDUPERANGE` is `EOPNOTSUPP`; a
  whole-file copy_file_range returns more than `MAX_RW_COUNT` in one call.

## Open questions

- Is losing renames in `/proc/<pid>/fd/N` (path frozen at open) acceptable, given Linux computes
  `d_path` at readlink?
- Is skipping search permission on synthetic directories other than cgroup ones intended (only
  `/proc/<pid>/fd` and `fdinfo` gate access, by the ptrace-read check)?
- Should `protected_regular`, `protected_fifos` and `O_TMPFILE` be implemented?

## To verify

- epoll's fdinfo device numbers for files on tmpfs and synthetic mounts (said to be the root
  filesystem's, unlike fstat).
- Whether anything else (such as `mm`'s `shmem_like`, which looks for a `/memfd:` mapping name)
  depends on the display path a `/proc/<pid>/fd` reopen freezes.
