# kernel/vfs Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/vfs.md` for the paired decision history (it still holds the decisions on mounts, inotify, AIO, loop devices, timerfd, overlayfs, locks, pipes, xattrs, open files and file I/O made before they were split out; the last two are listed in `kernel/files`'s and `kernel/fileio`'s maps).
>
> Verified: 2026-11-26 (full refresh; 18 claims verified — 13 confirmed, 5 partial, 0 refuted; all fixes applied)

## Responsibilities

`kernel/vfs` handles path resolution, mount policy, tmpfs accounting, `/dev` population and
metadata/namespace/directory syscalls: `core/kernel/src/vfs/mod.rs` (excluding mount table and
overlay methods), `vfs/dev.rs`, matching sections of `syscall/fs.rs`, and tmpfs options in
`syscall/mount.rs`. Open/ioctl dispatch is `kernel/files`; I/O, size and seek are `kernel/fileio`;
locks, pipes, loops, mounts, overlay and FUSE are separate modules.

## Key types

- `Node::{Store,Pseudo,Overlay,Fuse}` pairs a mount with a store inode, `PseudoNode`, `OvlId` or
  `FuseId`; only `Store` yields `Node::ino`. `Dentry` holds parent/name; `WalkDentry` adds unlink state.
- `FsContext` holds root, cwd, umask and mount namespace. `Vfs` owns the store, mounts/namespaces,
  mount events, executable cache, overlay/FUSE tables, fast resolver, removed names, memfd sizes,
  tmpfs ranges and anonymous attrs.
- `Pseudo` supplies lookup, kind, parent/name, readlink, magic jumps/dentries, search attrs,
  protected-link policy and miss reporting (`View` live, `Offline` boot/host). `Cached` holds
  metadata, store mount points and usability; `Pending` has 64 slots.
- `Tmpfs` holds root, `TmpfsParams`, optional usage, a changed flag and
  `unlinked: HashMap<Ino, UnlinkedTmpfs>` (pages + open host file). `TmpfsFileRanges` tracks
  allocations/data; `TmpfsMapping` pins the object for faults.
- `Attrs::{Store,Overlay,Pts,Anon,Mq,MqDir,Cgroup,Fuse}` selects the metadata backend.
  `AnonTarget` identifies a pipe/socket; `Device` identifies generic devices and loops.

## Public entry points

- Resolution: `Vfs::resolve`, `walk`, `walk_resolve`, `resolve_mountpoint`, `resolve_parent`,
  `resolve_parent_with`, `found_path`, `reach`, `path_of`, `mounts_from`, `Cached::stat`,
  `start_dir`, `lookup_at`, `parent_at`, `create_parent_at`, `may_create`, `display_path`.
- Syscalls: `newfstatat`, `fstat`, `statx`, `statfs`/`fstatfs`, `faccessat`/`faccessat2`,
  chmod/chown variants, `utimensat`, `getdents64`, `readlinkat`, `getcwd`, `chdir`/`fchdir`,
  `chroot`, `umask`, `mkdirat`, `mknodat`, `symlinkat`, `linkat`, `unlinkat`, `renameat2`.
- Internal/boot: `file_stat`, `dev_ino` (fallback without store lookup), `file_inode_flags`,
  `file_accessed`, `readonly`, `new_tmpfs` + tmpfs hooks, `new_fuse`, `add_fuse_mount`,
  `fuse_child`, `fuse_permission`, `open_with_gpu`.

## Data flow / lifecycle

### Path lookup and search

`start_dir` rejects empty input before dirfd (`ENOENT`). Absolute paths use root, relative
paths use cwd for `AT_FDCWD` or the fd's directory (`EBADF` for bad fd, `ENOTDIR` for wrong
type/no node). `lookup_at_or_fd` with empty path and `AT_EMPTY_PATH` takes cwd or fd node
(`EBADF` if absent). Stat and anonymous attrs also have fd-specific routes.

`walk_resolve` runs `walk_in` in one store snapshot. A trailing slash forces final-link follow
and requires a searchable directory. Each name checks length (255 bytes), type and search,
including `.` and `..`. Dot preserves the dentry. Dotdot clears it, stops at process root, or
climbs stacked attachments, takes a parent and enters its top mount. A store parent outside
the bind subtree gives `ENOENT`. Child lookup also enters the top mount. Ordinary walks touch
mounts. `resolve_mountpoint` preserves expiry marks for umount and host stats.

`searches_all` skips store, overlay and synthetic search checks for initial-namespace
`CAP_DAC_OVERRIDE` or `CAP_DAC_READ_SEARCH`. Otherwise store and overlay use the visible
directory's permissions. `View::searched` supplies attrs for cgroup directories, the mqueue root
and live other-group proc `fd`, `fdinfo` and `ns` directories. Own-group search works when
non-dumpable. Dead proc tasks supply no search attrs, leaving lookup to report disappearance.
Procfs also gates lookup, listing and magic links. Final `faccessat` allows the own-group
exception for nonwrite masks on `fd`/`fdinfo` and search alone on `ns`. Chdir/fchdir/chroot
check the final directory via `searched_attrs` or FUSE `may_chdir`. Chroot then checks
`CAP_SYS_CHROOT` in the caller's namespace and preserves cwd.

FUSE always checks whether the connection allows the caller. `default_permissions` can
require cached attrs. On a miss, `lookup_walk` batches consecutive ordinary names up to dots,
the trailing-slash marker or an oversized component. Lookup, readlink or attrs can request
`FUSE_RETRY`. The dispatcher services the query and retries, with `EIO` beyond 16,384 retries.
Only component lookup `ENOENT` invokes `Pseudo::missing`, which records selected proc/sysfs
interfaces and names directly in the boot `/dev` root.

### Symlinks and scoped walks

- The 41st followed symlink gives `ELOOP`. For a trailing store link in a store directory
  (including `link/`), `protected_symlinks` gives `EACCES` when the directory is sticky and
  world-writable and neither caller fsuid nor directory owner equals link owner. Next come
  `NO_SYMLINKS`/mount `NOSYMFOLLOW` (`ELOOP`). Proc `follow` can deny access before magic-link
  restrictions. For magic links `NO_MAGICLINKS` gives `ELOOP`, cross-mount `NO_XDEV` and scoped
  jumps give `EXDEV`. Proc fd, ns, cwd, root and exe links are magic. `self`/`thread-self` are ordinary.
- Magic jumps reach process directories, namespace files, store/overlay/FUSE executables or
  retained `OpenExe`. `follow_fd` preserves overlay identity, linked store files/devices, directory,
  path and proc nodes, and a slave PTY's opened device or devpts node. Deleted regular files,
  memfds and many special fds use `OpenFd`. An open walk also selects it for a retained overlay
  regular file whose last upper link is gone. Reopen supports regular backing, pipes, queues,
  FUSE snapshots and host fds. Unsupported kinds can give `ENXIO`. A final jump can retain
  the opened dentry and unlink state.
- Relative symlinks continue from the parent. Absolute targets restart at the supplied root.
  `BENEATH` rejects absolute input/targets and `..` from the start. `IN_ROOT` interprets
  absolute paths from the dirfd. `NO_XDEV` checks child and parent mount transitions. An
  absolute symlink jump under `NO_XDEV` requires an already-taken root (absolute/scoped start,
  earlier `..` or root jump) on the link directory's mount.
- `openat2` checks `open_how` size (under 24 gives `EINVAL`, over 4096 or nonzero extension
  bytes `E2BIG`), flags, mode and open combinations. `BENEATH|IN_ROOT` gives `EINVAL`.
  `RESOLVE_CACHED` rejects create, truncate, tmpfile or `O_DIRECTORY` with `EAGAIN` before
  pathname copy. Other requests use the ordinary walk. Scoped `..` under the BKL has no
  separate rename-race retry.
- `resolve_parent_with` handles slash-containing prefixes with the final separator included
  (checking parent search during the walk), uses the start directly for slash-free basenames, and
  takes a separate shortcut for all-slash input (`EXDEV` under `BENEATH`). It checks basename
  length and parent type, leaving final dots and access to callers. `O_CREAT|O_EXCL` rejects
  existing names. After `ENOENT`, create rejects a trailing slash/final dot with `EISDIR`,
  handles mqueue creation, rejects regular creation in writable cgroups with `EACCES` and
  delegates FUSE creation. Dangling store, overlay and FUSE links lead to their targets with
  a fresh 40-link count and approximate `NO_XDEV` root history.

### Path reconstruction

`reach` climbs parents and attachments toward the supplied root using the recorded dentry for
file mounts. Detached/disconnected roots, parentless pseudo nodes or forgotten FUSE nodes can
end as unreachable. `getcwd` prefixes `(unreachable)`, counts the NUL and gives `ERANGE` for
short buffers. Removed directories give `ENOENT`. `path_of` permits them and appends ` (deleted)`.
Store rows retain parents. `removed_dirs` saves names for cwd, root, directory/path fds and
tracked ancestors. Overlay linkage and FUSE's `unlinked` serve the same purpose. `mounts_from`
returns optional paths. Procfs skips unreachable mounts and shows cwd/root against the reader's
root. Fd links track renamed dentries, with saved paths/descriptions as fallback and deleted
names marked. `display_path` falls back from `found_path` to lexical normalization.
`readlinkat` checks positive size first and returns raw bytes truncated to fit. Empty input
reads the fd without flags (`ENOENT` for node-less fd/nonlink, `EINVAL` for a nonempty path to a nonlink).

### No-BKL stat

`Fast::new` snapshots a root in attached `ROOT_MOUNT` (including chroot within it) without
`NOSYMFOLLOW` and with `searches_all` credentials. Absolute input permits any cwd. Relative input
needs `AT_FDCWD` and a cwd in that mount. Stat needs a current epoch, valid flags and readable
nonempty path fitting `PATH_MAX` including NUL. Global tracing, syscall tracing, stepping or
pending instruction-cache invalidation disables fast dispatch. Seccomp can admit a syscall
number cached as always allowed.

`Cached::stat` uses a store metadata reader. A cached negative returns `ENOENT` directly.
An unavailable/stale reader, uncached entry, host stat failure, over 64 pending steps or an
encountered store mount point falls back to the BKL. The mount-point list includes the store
root when the boot mount is no longer topmost. Length/type/loop errors can be final. A followed
trailing link owned by someone other than its sticky world-writable directory's owner falls back.
Final immutable, append or nodump forces fallback for both stat calls. Noatime alone does not.
Fast statx uses `ROOT_MOUNT` and advertises the supported attribute mask. `mounts_changed_ns`
always rebuilds cached mount state and increments/wakes once per affected namespace that still
exists and is nonanonymous. Mount, fs-context and credential changes invalidate snapshots with
`fast_changed`; `Fast::refresh` rebuilds on epoch change under the BKL.

### Mount flags and atime

- `readonly` combines mount `RDONLY` and superblock state shared by device number, including a
  lower-only overlay. Store create checks existence before `EROFS` and write permission after.
  Unlink/rmdir check writability after special-dot handling, rename after mount IDs and dots.
  Attribute changes generally check it before ownership, truncate after type checks.
- On existing store regular files, `O_TRUNC` on a read-only mount or a write open on a
  read-only superblock gives `EROFS` before DAC. On a read-only bind, an ordinary write open of a writable instance checks
  DAC and `O_NOATIME` first. Overlay write opens check read-only early. Memfd open and SETFLAGS
  exempt memfds from their read-only tests. Fd metadata mutation can still consult their mount.
- `NODEV` blocks device opens (`EACCES`), including overlay, FUSE and devpts. FUSE also refuses
  device opens on connections owned by a noninitial user namespace. `mknod` uses separate
  capability/device-filter checks. `NOEXEC` denies exec after execute permission and denies
  `X_OK` on regular files. Memfd descriptor exec omits mount flags and access exempts identified
  memfds from noexec. Executable regular mmap checks the fd node's mount (root for memfd), giving
  `EPERM` now or `EACCES` on a later mprotect upgrade. `NOSUID` suppresses set-id and file caps.
  FUSE also suppresses them outside the connection user namespace and its descendants, or for
  a retained executable snapshot whose connection is absent.
- Regular reads and successful regular mmap installation use `file_accessed`. A nonempty read
  can touch atime at EOF. Store `getdents64` checks atime before buffer validation. Exec header
  reads can call `Vfs::accessed` even for cached store/overlay programs. For fd access
  `O_NOATIME` suppresses the call. Mount/superblock read-only, mount `NOATIME` and inode
  `FS_NOATIME_FL` suppress updates. `NODIRATIME` applies only to directories. Relatime is due
  when atime ≤ mtime or ctime, or is at least 24 hours old. Ordinary access changes only atime.
  Mountinfo reports sync, dirsync, mand and lazytime. Statfs encodes sync and mand.

### Tmpfs creation and remount

`new_tmpfs` allocates a detached store directory and device number (initial usage: 0 pages, 1
inode); binds share the instance and overlay layer pins keep it alive. Boot instances start
uncounted (dropping deltas until first count) with relatime: `/dev` 0755 nosuid, `/dev/shm` 1777
nosuid,nodev, `/run` 0755 nosuid,nodev (20% RAM, `800 << 10` inodes).

- Options: `size=` (bytes rounded up to 4096-byte pages or `%` RAM), `nr_blocks=`, `nr_inodes=`
  (base prefixes, binary K/M/G/T/P/E suffixes), octal `mode=` (masked to 07777), base-detected
  `uid=`/`gid=` mapped in caller's user namespace (`EINVAL` if unmapped or `u32::MAX`),
  `inode32`/`inode64` (reporting only), `huge=` (never/always/within_size/advise/deny/force),
  `mpol=` and recognized generic superblock words ignored; unknown options `EINVAL`.
- Defaults: 1777, mounter fsuid/fsgid, inode64, half of RAM in pages and inodes (0 = unlimited).
  Mountinfo shows nondefault limits/mode, nonzero IDs and inode width. Non-bind remount validates
  options/IDs before `CAP_SYS_ADMIN` and RO writer checks, then evicts and recounts before applying
  limits and inode width (mode/owner persist; unlimited-to-finite or below-use is `EINVAL` with no
  parameter or flag changes).

### Tmpfs accounting and lifetime

- A count charges one inode for the root and every name (including hard links), pages once per
  distinct regular file or symlink, then adds unlinked records. A symlink of at least 128 bytes
  costs one page. `statfs` recounts and reports zero total/free for unlimited resources.
- `TmpfsFileRanges::allocated` includes reservations and instantiated pages. `data` tracks
  mapped-instantiated pages. Allocation rounds outward. `tmpfs_missing` combines guest runs
  with host extents (FIEMAP, with a seek fallback). Without guest runs, total allocation uses
  FIEMAP or rounded host block counts. Tracked stats combine ranges and host allocation in
  512-byte sectors. `SEEK_DATA`/`SEEK_HOLE` merge host results with `data`, so an untouched
  logical reservation can consume quota without becoming data or extending EOF.
- Check room before creation, then charge after success, including AF_UNIX bind. Overlay
  copy-up checks ancestors and data against the tmpfs upper. For a tracked lower inode the
  copy includes mapped data within EOF and omits untouched reservations. Replacing a whiteout
  needs no extra inode for that name. Mapping quota follows the layer supplying the mapped
  inode, even for a held unlinked object.
- `tmpfs_write_room` stops before the first unfittable page (`ENOSPC` if no prefix fits).
  Final room check, write and charge hold the BKL. Fallocate needs the entire missing range.
  Before refusal or shortening, changed usage triggers one fresh count. `tmpfs_resized`
  charges the measured delta and updates unlinked records. Sparse truncate growth costs no
  pages. Shrink/punch forget whole guest pages. An in-file hole fault can instantiate a page
  even for a private read, without double-charging a reservation. Read/private faults preserve
  backing mtime/ctime. Shared write faults publish mtime before granting write.
- `tmpfs_victim` captures pages and the last-linked regular file's object before removal or
  replacement. `tmpfs_removed` refunds the name and any last-link pages, or puts a regular file
  found by `file_in_use` in `Tmpfs::unlinked` with **one inode and its pages still charged**. Its
  host pin allows stat via the last `O_PATH` fd after store reaping.
- `evict_unlinked_tmpfs` scans fd tables (including path fds), AF_UNIX inflight files, loop
  backings, distinct address spaces and running executables. Borrow conflicts defer the scan.
  Live pins refresh page counts. Dead entries lose their ranges, pages and inode. Hooks run
  at post-syscall collection, thread exit, exec close-on-exec, tmpfs statfs and non-bind
  remount. The sweep also handles deferred shared-object releases, linkable tmpfiles and
  closed memfd size records with separate liveness checks. Memfds bypass ordinary tmpfs quota.
- `open_tmpfile` checks directory type, writability and permission. Store/tmpfs unnamed files
  inherit default ACL and inheritable flags. Tmpfs immediately charges one inode and pins it
  as unlinked. Without `O_EXCL` the inode is linkable. The first successful `Store::link` clears
  eligibility and `linkat` removes the unlinked record without another charge, even at full
  inode quota. Later hard links each need fresh room.

### Stat and filesystem identity

`stat_at` resolves and reads pathname metadata in one store snapshot. `AT_EMPTY_PATH` on a
real fd uses `file_stat_attrs` and `OpenFile::mnt_id`. A followed `OpenFd` link uses the target
descriptor. Owner IDs are mapped into the caller's namespace (overflow 65534).

- Store stat uses mount device and inode, overlay its visible inode/device following copy-up.
  Unlinked files/executables can stat the retained host object or FUSE executable snapshot.
  Namespace files use nsfs, queues their IPC namespace's mqueue device, anonymous pipes,
  sockets and anon-inodes fixed devices.
- A devpts-pinned master stats `PtsPtmx` with 1024-byte blocks. Store/overlay ptmx masters
  report the opened node. Node-less unpinned masters report inode 2, `ROOT_DEV`, 1024-byte
  blocks. Slaves use their opened device inode or terminal attrs and devpts instance/index.
- Regular objects and host fds supply host block counts, with tracked tmpfs adjustments.
  Default block size is 4096, procfs/devpts synthetic nodes use 1024, FUSE can supply its own.
  Missing block counts derive from size rounded to pages (eight sectors each). Symlinks under
  60 bytes on `ROOT_DEV` or 128 elsewhere use none. Directories on `ROOT_DEV` report size 4096.
- `newfstatat`/`statx` accept `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH`, `AT_NO_AUTOMOUNT`
  and statx sync flags. `newfstatat` permits both sync bits together. `statx` rejects that combination or a reserved
  mask bit with `EINVAL` before pathname copy. It reports `STATX_BASIC_STATS | STATX_MNT_ID` regardless
  of requested fields. `stx_mnt_id` comes from the node or fd (successful empty-path `statx` on a
  node-less fd reports `ANON_MOUNT`, matching fdinfo; GPU fence fds return `EBADF`). Store/overlay
  advertise immutable, append and nodump even when clear; `stx_attributes` reports set bits and
  noatime is outside `stx_attributes_mask`.
- `statfs_of` returns ext4 magic and host object-directory capacity for store, tmpfs limits
  for tmpfs, overlayfs magic with store capacity, daemon figures for FUSE (when excluded,
  `FUSE_SUPER_MAGIC` with zero capacity and zero `f_flags`), or the synthetic
  filesystem's magic and zero capacity. Supported `f_flags` include `ST_VALID` plus mount flags
  and superblock read-only/sync/mand. Nsfs reports `ST_VALID` alone. `fstatfs` uses pipefs for
  anonymous pipes, the underlying filesystem for named FIFOs, mqueue for queue fds, root
  figures for other node-less fds.

### Access and inode flags

`faccessat` validates mask/flags before pathname copy. Without `AT_EACCESS`, `for_access` uses
real IDs. Unless `SECBIT_NO_SETUID_FIXUP` is set it uses permitted caps for namespace root and
clears effective caps otherwise. FUSE uses its access handler even for `F_OK`, which otherwise
succeeds after lookup. For non-FUSE targets, regular-file noexec (`EACCES`) and immutable for
`W_OK` (`EPERM`) precede final DAC. Write access to a regular file, directory or symlink checks
read-only (`EROFS`) after permission.

`FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` transfer four bytes for store/overlay regular or directory
descriptions, including tmpfs and memfds. GET reads supported flags on the visible inode.
SET reads the argument, checks read-only (except memfd), owner or `CAP_FOWNER`, then initial-user-namespace
`CAP_LINUX_IMMUTABLE` for immutable/append changes. Unsupported bits give `EOPNOTSUPP` before
copy-up. Success persists flags and updates ctime. Legacy rows default to zero. Named children
and tmpfiles inherit only nodump/noatime.

Immutable blocks write open. Append-only requires append on write open and refuses truncate.
Writes recheck current flags and force EOF even for positional writes on append-only files.
`F_SETFL` cannot toggle their `O_APPEND`. Range copies check destination flags. Both protected
flags restrict unlink/replacement, hard-link source, chmod/chown, explicit times and xattrs/ACLs.
Append-only directories can gain names. After fd access and noexec checks, append on a
readable/writable regular fd rejects any `MAP_SHARED` with `EACCES` (even read-only); if that
passes, immutable rejects shared `PROT_WRITE` with `EPERM`. Permitted protected shared mappings
cannot gain write later.

### Mutable metadata

- `Attrs::of` checks writability for store/overlay, devpts slaves, queues, mqueue roots,
  cgroup and FUSE. For named cgroup files `pseudo::gone` matches name **and generation**.
  A stale handle gives `ENODEV` before mutation. FUSE can pass an open `fh` to `SETATTR`
  and delegate permissions. Unsupported pseudo paths give `EROFS` if read-only, else `EACCES`.
- `Attrs::of_fd` selects anonymous objects, queues and node-less devpts slaves directly without
  checking mount writability. Ordinary `O_PATH` and unsupported pseudo fds (including the mqueue
  root) give `EBADF` before mount checks. `Attrs::at` handles `AT_EMPTY_PATH` for `fchownat` /
  `utimensat` and followed proc links to anonymous objects. `fchmodat` takes no flags.

- `chmod` masks to 07777, checks protected flags and ownership, and drops setgid outside the
  file's group without `CAP_FSETID` over it. On a memfd sealed with `F_SEAL_EXEC`, changing an
  execute bit gives `EPERM` after ownership. `chown` maps IDs (`EINVAL` if unmapped), with −1
  unspecified. Without `CAP_CHOWN`, supplying a uid requires the owner keeping that uid, and a
  gid requires the owner keeping the existing gid or choosing one of their groups.
- On store/overlay non-directories chown clears setuid, file caps and setgid when group-executable
  or when the caller lacks both group membership and `CAP_FSETID`. Clearing set-id needs owner
  or `CAP_FOWNER`. Even `(-1,-1)` can fail on a set-id file or clear caps alone. No supplied IDs
  and nothing to clear skips the change. Writes/truncates use `kernel/fileio`'s `remove_privs`.
- `utimensat` validates flags and resolves the target before reading times. A null pathname
  uses the fd (`EFAULT` with `AT_FDCWD`, otherwise zero flags required). Two `UTIME_OMIT`
  succeed after target checks. Null times or two `UTIME_NOW` permit ownership or write access,
  including on append-only. Explicit/mixed updates need owner or `CAP_FOWNER` where locally
  checked. Immutable blocks both-now as well. Inotify (`notify_utimens`) runs only for store,
  overlay, cgroup and FUSE (`IN_ACCESS` for atime, `IN_MODIFY` for mtime, `IN_ATTRIB` for both);
  PTYs, anonymous objects, queues and queue roots emit none.
- With a VFS, `pipe2` initializes both ends' shared attributes to 0600, creator fsuid/fsgid
  and creation times (without a VFS, pipe fstat reports 0600, root and zero times). Positive
  ordinary pipe reads touch atime and writes mtime/ctime. A socket starts at 0777, creation/accept
  fsuid/fsgid and zero times; socket chown updates the socket and shared attrs when present.
  `anon_attrs` preserves mutations across aliases, pruning dead weak refs at insertion thresholds.
  Named FIFOs use their filesystem inode.

### Creating and removing names

- `resolve_create_parent` rejects final dots or existing synthetic children with `EEXIST`.
  For ordinary store creation, `may_create_name` checks search, existence and read-only, then
  `may_add_name` checks immutable and parent write/search. Non-directory creation with a
  trailing slash has an earlier existence test (`EEXIST` if present, `ENOENT` otherwise).
  Overlay checks merged existence, permissions and upper quota before copying up the parent.
  `create_parent_at` exposes this to AF_UNIX bind. `store_dir` rejects `Node::Overlay` with `EXDEV`
  and accepts any directly addressed `Node::Store` without checking whether it backs an overlay layer.
- `new_owner` applies setgid-directory inheritance and skips normal umask for a parent default
  ACL. `create_in` installs inherited ACLs in the creation transaction. The access ACL is
  intersected with the requested mode and stored only if extended. New directories also keep
  the full default ACL. Symlinks skip inheritance. Overlay creation can replace a whiteout and
  mark the new directory opaque in that transaction. Upper create events precede overlay events.
- For existing `O_CREAT` targets, `may_create_in_sticky` uses found parent and store/overlay/FUSE
  attrs. `protected_regular` defaults to 2, `protected_fifos` to 1. In sticky world-writable
  directories (also group-writable at level 2) a caller gets `EACCES` unless its fsuid or the
  directory owner equals the file owner, including root. Protected symlinks/hardlinks default on.
- Cgroup mkdir checks parent search, existing name/dots, read-only, then write/search.
  FUSE creation uses the daemon. Store/overlay `mknodat` validates type after resolving the
  parent and rejecting dots. Zero/regular, char, block, FIFO and socket are accepted, directory
  gives `EPERM` and other types `EINVAL`. Device creation needs initial-namespace `CAP_MKNOD`
  and device BPF permission after admission. A char whiteout (device zero) skips both checks
  and is refused on overlay. `symlinkat` rejects an empty target before reading the new path.
- `unlinkat` accepts only `AT_REMOVEDIR`. It tries mqueue, cgroup, overlay, then FUSE.
  Other synthetic parents give `EROFS`/`EACCES` before dot/victim checks. Store unlink of
  either dot gives `EISDIR`, rmdir of `.` gives `EINVAL` and of `..` gives `ENOTEMPTY`. Otherwise
  writability precedes normal lookup. Plain unlink with trailing slash gives `ENOENT` if
  missing, `EISDIR` for a directory, `ENOTDIR` otherwise, ahead of delete permission. Delete
  checks search, victim lookup, immutable parent, write/search and sticky policy, then protected
  flags on parent and victim.
- `linkat` validates flags, then requires initial-namespace `CAP_DAC_READ_SEARCH` whenever
  `AT_EMPTY_PATH` is set (`ENOENT` before reading paths). It resolves source before destination.
  Existing pseudo destination gives `EEXIST`. Overlay pairs and FUSE destinations get separate
  handlers, other mixed nodes `EXDEV`. For store nodes, dots and trailing-slash checks precede
  parent search, `EEXIST`, `EROFS` and cross-mount `EXDEV`, even between binds. Protected
  hardlinks require ownership/capability or a readable, writable regular source without setuid
  or group-executable setgid. Next come directory admission, source flags, tmpfs room and
  `Store::link` (directory `EPERM`, zero links `ENOENT` except linkable tmpfiles, no `EMLINK` limit).
  A deleted regular file reached via proc fd keeps its opened mount. A followed memfd stays
  synthetic (`EXDEV`), while its direct empty-path fd has a store node on the root mount.

### Rename and mount points

`renameat2` rejects unknown flags (including `RENAME_WHITEOUT`) and `NOREPLACE|EXCHANGE` before
copying paths. It resolves source then destination parent and checks final store parent search.
Different mount IDs give `EXDEV` before dots. A source dot gives `EBUSY`, a destination dot
`EEXIST` with `NOREPLACE` and `EBUSY` otherwise.

- Store writability precedes source lookup. One transaction looks up both entries, tests
  `NOREPLACE` (`EEXIST`) and missing exchange target (`ENOENT`), then trailing slashes.
  Destination slash requires a directory source for ordinary rename, directory destination for
  exchange. Moving into the source subtree gives `EINVAL`. Replacing an ancestor gives
  `ENOTEMPTY`, or `EINVAL` on exchange. Only then does the same-inode no-op succeed.
- `may_rename` checks source removal then destination removal/addition, including write,
  sticky and inode flags. Nonexchange type mismatch precedes write permission on reparented
  directories (both sides of exchange), then local mount-point checks. Replacement quota
  and open-name maintenance follow transaction success.
- Synthetic rename checks read-only, source lookup and, on procfs, destination lookup errors.
  `NOREPLACE` onto an existing name gives `EEXIST`, then identical nodes succeed. Otherwise
  mqueue/cgroup give `EPERM`. Other pseudo parents check write/search (`EACCES`), then give
  `EPERM`. Same-mount overlay and FUSE rename use their own handlers.
- `not_mount_point` rejects attached mount points in the caller's namespace (`EBUSY`). For
  store files it compares inode and recorded parent/name when available. After removal or
  nonexchange replacement, `detach_points` detaches mounts elsewhere and marks removed names
  used as attached/detached mount roots. Mountinfo can show `//deleted` even when a bound
  file has other hard links. A held last-unlinked store object can be released once no fd,
  mapping, executable, unshare or inotify open hold needs it.
- Moved open names follow rename even without active watches. With watches, move pairs share
  a cookie, followed by self/overwrite events. Exchange also moves the second side. Visible
  overlay events use overlay keys independently of backing-layer events.

### Directory iteration

`getdents64` rejects `O_PATH` with `EBADF`, non-directories with `ENOTDIR`, and removed store
directories with `ENOENT`. The open description caches its listing until a directory seek lands on zero.
Store, pseudo and overlay prepend `.` and `..`. FUSE supplies its own list.
Records align to eight bytes and `d_off` is the next index. Count narrows to 32 bits. While
entries remain, count above `INT_MAX` or room for no complete next entry gives `EINVAL`.
Partial usercopy returns bytes through the last complete record and advances only that far
(`EFAULT` if none). Successful EOF can still report `IN_ACCESS`.

### Boot devices and host configuration

Import runs `ensure_dev_nodes` transactionally (propagating errors); boot logs persistent setup
failures. It creates missing `/proc`, `/sys` (0555), `/run`, `/dev` (0755), then populates `/dev`
if a directory. `populate` adds missing root-owned entries to the store and boot tmpfs without
overwriting existing types/modes:

- Char 0666: `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx`, `fuse`; char `kmsg` 0644,
  `console` 0600.
- Group `disk` (gid 6), 0660: char `loop-control` (10:237) and block `loop0`–`loop7` (7:0–7).
- Symlinks: `fd`, `stdin`, `stdout`, `stderr` → `/proc/self/fd` (+ `/0`, `/1`, `/2`).
- Dirs: `pts` (0755), `shm` (1777), `mqueue` (0755).

During rootfs-backed boot (`Vfs::open_with_gpu`), if tmpfs creation or base population fails, a
usable store `/dev` serves as fallback; renderer-enabled VFS startup requires the fresh
`DEV_MOUNT` or returns `ENODEV` (a machine without a rootfs skips `Vfs::open_with_gpu` and can
keep a supplied renderer without `/dev`). Only `DEV_MOUNT` receives `populate_gpu`'s root-owned
`dri` (0755) and `dri/renderD128` (char 226:128, 0666) in one transaction (errors propagate).
Char open dispatches by device number regardless of path; `Device::from_rdev` handles major 1
minors 3, 5, 7, 8, 9, 11 (both randoms → `Random`) and loop-control (terminal, render and FUSE
precede it; unknown → `ENXIO`); block opens go to `kernel/loop`.

Host config helpers validate the whole absolute path first (NUL/`..` `EINVAL`, long component
`ENAMETOOLONG`), skip empty/`.` components and walk store names without following symlinks.
`write_file_owned` creates root-owned 0755 parents and applies uid/gid to new final files; with
overwrite off, existing entries stay untouched; overwrite and `read_file` require a regular file
(overwrite keeps owner/mode). Boot seeds missing `/etc/resolv.conf` (`nameserver 127.0.0.53`,
0644) when `/etc` exists.

## Dependencies (inbound / outbound)

### Inbound

- Dispatcher, `kernel/files`, `kernel/fileio`, `kernel/exec`, `kernel/mm` (lookup, metadata,
  mount policy, accounting); `kernel/process` (`Cached`, fs-context invalidation); `kernel/mount`
  (walks, tmpfs); `kernel/overlay` (layer pins, upper charges); `kernel/xattr` (lookup, flags);
  `kernel/unix` (bind); procfs/epoll/locks (identities); boot/import/host wrappers (devices, config,
  offline lookup).

### Outbound

- `kernel/store` (snapshots, readers, metadata, transactions, objects); `kernel/mount` (topology,
  flags); `kernel/overlay`, `kernel/fuse`, `kernel/files` (fd identity/holds); eviction scans
  mappings, executables, sockets, loops and inotify open records.
- `kernel/procfs` (`View`, `kernel/sysfs`, `kernel/sysctl`), `kernel/ipc`, `kernel/tty`,
  `kernel/cgroup` (ops + device BPF), `kernel/cred`, `kernel/xattr` (ACLs), `kernel/inotify`,
  `appine_hal` (host file/fs/memory).

## Invariants and gotchas

- Preserve reached mount for scope, `..`, `EXDEV` and stat; absolute lookup and `..` at process
  root never lift it; store mount-point busy checks compare namespace and entry.
- A `Node` alone pins neither FUSE entries nor overlay orphans (FUSE path fds pin entries; cwds,
  mounts and watches join sweeps). Preserve overlay identity/holds on opens; waits retaining a
  looked-up node stay out of `waits_on_files_only`; end file borrows before waits or BKL release.
- Propagate `FUSE_RETRY` before irreversible state changes and never swallow it as absence. Report
  overlay events by visible key. Check quota before mutation, charge after success and transfer a
  tmpfile's charge on first link.

## Confirmed bugs / technical debt

- Final non-FUSE `faccessat` rebuilds `Stat` with only mode, uid and gid, discarding extended
  access ACLs; a non-owner can therefore see a different result than opening the same store/overlay file.
- `Attrs::of_fd` selects a node-less devpts slave or message queue directly before checking mount
  writability, so `fchmod` can succeed where pathname `chmod` returns `EROFS`.
- `file_in_use` and `live_store_inodes` omit files held solely by pending AIO polls
  (`PendingPoll`), and unlink-time `file_in_use` skips unborrowable descriptions, so a live
  unlinked tmpfs regular file can lose its quota charge early.
- GPU fence fds have no node or dedicated fd-stat arm, so `file_stat_attrs` (including empty-path
  `statx`) returns `EBADF` instead of reporting `ANON_MOUNT`.
- `RESOLVE_CACHED` can perform store I/O or request a FUSE lookup; `statx` lacks birth time and
  `STATX_DIOALIGN`.
- File-mount traversal matches a node (so same-mount hard links enter it), while busy checks can
  distinguish the recorded parent/name.
- Host punch `EOPNOTSUPP` can leave zero-filled extents charged after logical pages are forgotten,
  and dynamic boot `/dev` loop-node changes bypass quota/changed flags until a fresh count.
- Without a VFS, anonymous mode/time mutations lack shared storage (pipe fstat falls back to 0600,
  root and zero times; socket ownership persists on the socket).

## Open questions

None.
