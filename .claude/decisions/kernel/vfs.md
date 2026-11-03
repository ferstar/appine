# VFS (kernel/vfs) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/vfs"`
> Last distilled: 2026-11-03 (HEAD = 9e5be2b) — seeded by hand from the retired design doc §8.2–§8.3
> and the round records in `docs/rounds/`; no `MODULE: kernel/vfs` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: Nodes carry the mount they were reached through
- **What**: `Node::Store(mount, ino)` and `Node::Pseudo(mount, node)` play the role of Linux's `struct path`; one directory mounted in two places is two nodes, and `..`, getcwd and `/proc/<pid>/{cwd,root,fd}` climb back through their own mount points.
- **Why**: Chrooting from Alpine into Debian left `/proc` and `/sys` empty because mount was `ENOSYS`; mounting synthetic filesystems anywhere needed pseudo nodes to know their mount (round 105), and bubblewrap's binds of store directories needed the same for store nodes (round 116).
- **Tradeoffs**: Every path, open file, cwd and mount-point check carries a mount id; the no-BKL resolver only walks the root filesystem's mount.
- **Watch out**: Mount ids are never reused, so nodes in lazily unmounted mounts cannot be mistaken for nodes of a new mount.
- **Source**: rounds 105 (`105-mount-subset`), 116 (`116-store-binds`)

### D2: One global mount table, with the mount namespace kept in the fs context
- **What**: Mount records are stored globally by id and each records its namespace; a task's mount namespace lives in `FsContext` next to root, cwd and umask rather than in nsproxy.
- **Why**: Mount ids are global anyway, and path resolution needed no change because copied mounts have their own ids. `CLONE_NEWNS` cannot be combined with `CLONE_FS`, unshare implies it and setns requires an unshared fs, so tasks sharing an fs are always in the same mount namespace; this also avoided building all of nsproxy first.
- **Tradeoffs**: Unused namespaces, detached mounts and open_tree clones are only collected at `umount2` and task exit.
- **Watch out**: Operations on mounts must check that they belong to the caller's namespace (Linux's `check_mnt`).
- **Source**: round 118 (`118-mount-namespaces`)

### D3: tmpfs is a detached tree in the store, accounted by file size
- **What**: Each tmpfs mount is a new instance backed by a store directory outside the root tree, with its own device number; usage is counted per instance in pages and inodes, treating files as having no holes and freeing at the last unlink.
- **Why**: bubblewrap builds its new root on a tmpfs. A separate in-memory filesystem would need every path and fd syscall, mmap, file locks, inotify and AF_UNIX paths to handle another node kind, and its contents would consume the app's memory (the reason `/tmp` was put in the store).
- **Tradeoffs**: tmpfs is as slow as the store and writes flash; sparse files and `FALLOC_FL_KEEP_SIZE` are counted differently from Linux, so `ENOSPC` can come earlier; concurrent unlocked writes can miscount until the next recount.
- **Watch out**: statfs and remount recount the whole tree, which also corrects drift from shared mappings.
- **Source**: rounds 117 (`117-tmpfs`), 131 (`131-tmpfs-usage`)

### D4: Boot mounts match systemd and container runtimes; `/tmp` stays in the store
- **What**: `/dev` (tmpfs with standard nodes, like devtmpfs), `/dev/pts`, `/dev/shm`, `/dev/mqueue` and `/run` are mounted at boot with systemd's options; `/tmp` is a store directory.
- **Why**: OpenRC's `init.sh` and devfs service otherwise mount over appine's directories; seeing them already mounted, they leave them alone. `/dev/shm` stays executable and unlimited so JIT double mappings keep working.
- **Tradeoffs**: Nodes created with mknod in `/dev` do not survive a reboot (as on Linux).
- **Watch out**: The store's own `/dev` is still populated with the same nodes, so unmounting `/dev` leaves a working directory underneath.
- **Source**: rounds 05 (`05-m3b`), 184 (`184-boot-tmpfs`)

### D5: Device nodes are dispatched by device number to in-process implementations
- **What**: Opening any character-device node, wherever it is, selects the implementation from its device number (null, zero, full, random, kmsg; tty, console, ptmx and pts routed to the terminal layer); unknown numbers are `ENXIO`.
- **Why**: This is Linux's static `/dev` plus devpts model: guests can copy or mknod nodes (for example into a chroot) and get the same behaviour, and deleted standard nodes come back at the next boot.
- **Tradeoffs**: There are no block devices or loop devices, so mounts of disk filesystems are impossible.
- **Watch out**: nodev mounts and the cgroup BPF device filter must be checked before dispatch.
- **Source**: rounds 04 (`04-m3a`), 05 (`05-m3b`), 184 (`184-boot-tmpfs`)

### D6: Mount propagation is accepted but not emulated; remount changes only one mount
- **What**: shared, slave, private and unbindable are validated and then ignored, so every mount behaves as private; remount changes the flags of the one mount (tmpfs size options excepted, which belong to the instance).
- **Why**: bubblewrap, crun, podman and util-linux only need the calls to succeed; emulating peer groups and per-superblock flags was not needed by any workload.
- **Tradeoffs**: No `shared:N` in mountinfo, `MOVE_MOUNT_SET_GROUP` is always `EINVAL`, LTP's mount_setattr02 fails; remounting a filesystem read-only does not affect its other binds.
- **Watch out**: Programs that rely on mount events propagating into child namespaces will not see them.
- **Source**: rounds 105 (`105-mount-subset`), 116 (`116-store-binds`), 168 (`168-open-tree`)

### D7: Only open_tree, move_mount and mount_setattr from the new mount API
- **What**: The three are implemented per 6.1's `fs/namespace.c`; `fsopen`, `fsconfig`, `fsmount` and `fspick` stay `ENOSYS`; idmapped mounts are `EINVAL`.
- **Why**: crun opens binds with `open_tree(OPEN_TREE_CLONE)` outside a user namespace and moves them in, which containers with `--uidmap` require; once open_tree works crun sends every bind through these three, so all of them had to be right. crun and util-linux fall back to mount(2) on `ENOSYS` from the fsopen family.
- **Tradeoffs**: podman's and crun's `idmap` volume option is unavailable.
- **Watch out**: An unattached clone belongs to an anonymous namespace held by its fd and is only torn down at the next collection.
- **Source**: round 168 (`168-open-tree`)

### D8: Mount flags and cross-mount rules follow Linux 6.1's check order
- **What**: ro, noexec, nosuid, nodev and nosymfollow are checked per mount at the same points and in the same order as Linux; rename and link across mounts are `EXDEV` even between binds of one filesystem; removing or renaming a mount point is `EBUSY` only in the caller's namespace, and mounts on it elsewhere are detached.
- **Why**: Sandboxes (bubblewrap's `--ro-bind`, `--remount-ro`) and LTP's read-only filesystem tests depend on exact errors and ordering (for example `mkdir -p` must still see `EEXIST` on a read-only mount). Treating mount points in other namespaces as busy broke `podman run --rm`.
- **Tradeoffs**: Atime flags are recorded and reported only.
- **Watch out**: New syscalls that create or modify files must call the read-only check at the matching point.
- **Source**: rounds 116 (`116-store-binds`), 168 (`168-open-tree`)

### D9: openat2's `RESOLVE_*` are implemented in appine's own walker
- **What**: `BENEATH`, `IN_ROOT`, `NO_SYMLINKS`, `NO_MAGICLINKS` and `NO_XDEV` follow 6.1's `fs/namei.c` inside the normal path walk; `RESOLVE_CACHED` only rejects create, truncate and tmpfile opens.
- **Why**: New GNU tar with glibc ≥ 2.43 fails hard without openat2, and bubblewrap 0.12 uses it; Android's app seccomp does not allow openat2, so it cannot be passed to the host. appine's resolution never blocks on a cache miss, so `RESOLVE_CACHED` has nothing to distinguish (as in gVisor).
- **Tradeoffs**: The "root already taken" rule for `NO_XDEV` while following dangling links on create is approximated.
- **Watch out**: `exe` is treated as a magic link here even though appine follows it by content elsewhere.
- **Source**: round 144 (`144-openat2`)

### D10: Mounting over `/proc/<pid>/fd` entries is refused as in Linux 6.12
- **What**: A mount point reached through `/proc/<pid>/fd/<n>` to a deleted file or pipe, or that is an `fd`/`fdinfo` entry itself, is `ENOENT`.
- **Why**: Linux 6.12 blocked this ("proc: block mounting on top of /proc/<pid>/fd/*"), and LTP's mount08 tests it; 6.1 and native 6.8 allow it. appine follows the newer rule as one of the deliberate exceptions to the 6.1 contract.
- **Tradeoffs**: Differs from the reported kernel version.
- **Watch out**: Count it among the 6.1 contract exceptions when deciding on a contract upgrade.
- **Source**: round 105 (`105-mount-subset`)

### D11: Regular-file I/O runs without the BKL behind an `f_pos_lock` emulation
- **What**: Reads and writes take the host fd and position, release the BKL for host `pread`/`pwrite`, and allow only one position-based operation per open file at a time; `O_APPEND` writes stay under the BKL.
- **Why**: With the BKL held, a cold read waiting on flash stalled every process's syscalls (an `apk add` in one tab froze the shell in another); serialising per open file keeps offsets correct, and holding the BKL for appends keeps concurrent appenders from overwriting each other.
- **Tradeoffs**: Appending writers still serialise on the BKL; large transfers release the position lock between 64 KiB chunks.
- **Watch out**: Waiters for the position lock listen on the lock wake channel only.
- **Source**: rounds 24 (`24-shared-translation`), 53 (`53-locks-host-sockets`), 89 (`89-release-borrow-check`)

### D12: Locks and leases are in-process and keyed by store inode
- **What**: `flock`, POSIX record locks (owned by the fd table, released by any close of the file) and OFD locks (owned by the open file) split and merge ranges and detect POSIX deadlocks; leases follow Linux's rules; nothing reaches host locks.
- **Why**: All guest processes run in one kernel layer, so in-kernel tables give exact Linux semantics; wake-ups go to a dedicated lock channel so unlocking does not wake every poller.
- **Tradeoffs**: Locks are not visible to another kernel or to the Files app sharing the store.
- **Watch out**: Closing a file with locks or leases must request the lock channel.
- **Source**: rounds 08 (`08-m6`), 19 (`19-deferred`), 53 (`53-locks-host-sockets`)

### D13: inotify is implemented entirely in the kernel layer
- **What**: Syscalls report events after changing the store, at the positions and in the order of Linux's fsnotify hooks; watches are keyed by store inode; per-fd events use the directory and name the file was opened through; Files app changes are replayed under the target kernel's BKL.
- **Why**: Every filesystem change passes through the kernel layer, so no host notification is needed; Java's WatchService and Node.js's `fs.watch` needed inotify. The early approximations (files opened before a watch, renamed open files, immediate `IN_DELETE_SELF`, unenforced limits) were removed to match Linux.
- **Tradeoffs**: Synthetic filesystems accept watches but never report; no fanotify; the provider only reports creates, removes, moves, write opens and writes.
- **Watch out**: Keep the no-watch cost at one emptiness check per event.
- **Source**: rounds 11 (`11-m8-jvm`), 19 (`19-deferred`), 201 (`201-documents-notify`)

### D14: `protected_symlinks` and `protected_hardlinks` are on by default and enforced
- **What**: Both sysctls default to 1 and follow Linux 6.1's `may_follow_link` (trailing links in sticky, world-writable directories are followed only by their owner or when owned by the directory's owner, root included) and `may_linkat` (non-owners without `CAP_FOWNER` may only hard-link readable and writable regular files that are not setuid or executable setgid); the no-BKL resolver hands such links to the normal walk.
- **Why**: Alpine's sysctl service writes them at boot and failed when they were missing; accepting the writes without enforcing them would misreport a security setting. systemd, Android's init and Alpine all set them to 1, as native VMs show.
- **Tradeoffs**: `protected_regular` and `protected_fifos` are not implemented.
- **Watch out**: The no-BKL `Cached` walk knows no credentials, so it must keep bailing out on links that the check might refuse.
- **Source**: round 185 (`185-protected-links-random`)
