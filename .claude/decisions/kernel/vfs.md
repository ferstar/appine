# VFS (kernel/vfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/vfs"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930). D1–D14 were seeded by hand from the retired design
> doc §8.2–§8.3 and §11.1 and the round records in `docs/rounds/`, so their sources cite rounds;
> later entries cite commits.

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
- **Why**: OpenRC's `init.sh` and devfs service otherwise mount over Appine's directories; seeing them already mounted, they leave them alone. `/dev/shm` stays executable and unlimited so JIT double mappings keep working.
- **Tradeoffs**: Nodes created with mknod in `/dev` do not survive a reboot (as on Linux).
- **Watch out**: The store's own `/dev` is still populated with the same nodes, so unmounting `/dev` leaves a working directory underneath.
- **Source**: rounds 05 (`05-m3b`), 184 (`184-boot-tmpfs`)

### D5: Device nodes are dispatched by device number to in-process implementations
- **What**: Opening any character-device node, wherever it is, selects the implementation from its device number (null, zero, full, random, kmsg; tty, console, ptmx and pts routed to the terminal layer); unknown numbers are `ENXIO`.
- **Why**: This is Linux's static `/dev` plus devpts model: guests can copy or mknod nodes (for example into a chroot) and get the same behaviour, and deleted standard nodes come back at the next boot.
- **Tradeoffs**: Loop devices (D21) are the only block devices, and with no disk filesystems nothing can be mounted from them.
- **Watch out**: nodev mounts and the cgroup BPF device filter must be checked before dispatch.
- **Source**: rounds 04 (`04-m3a`), 05 (`05-m3b`), 184 (`184-boot-tmpfs`); a26c8b4

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

### D9: openat2's `RESOLVE_*` are implemented in Appine's own walker
- **What**: `BENEATH`, `IN_ROOT`, `NO_SYMLINKS`, `NO_MAGICLINKS` and `NO_XDEV` follow 6.1's `fs/namei.c` inside the normal path walk; `RESOLVE_CACHED` only rejects create, truncate and tmpfile opens.
- **Why**: New GNU tar with glibc ≥ 2.43 fails hard without openat2, and bubblewrap 0.12 uses it; Android's app seccomp does not allow openat2, so it cannot be passed to the host. Appine's resolution never blocks on a cache miss, so `RESOLVE_CACHED` has nothing to distinguish (as in gVisor).
- **Tradeoffs**: The "root already taken" rule for `NO_XDEV` while following dangling links on create is approximated.
- **Watch out**: `exe` is treated as a magic link here even though Appine follows it by content elsewhere.
- **Source**: round 144 (`144-openat2`)

### D10: Mounting over `/proc/<pid>/fd` entries is refused as in Linux 6.12
- **What**: A mount point reached through `/proc/<pid>/fd/<n>` to a deleted file or pipe, or that is an `fd`/`fdinfo` entry itself, is `ENOENT`.
- **Why**: Linux 6.12 blocked this ("proc: block mounting on top of /proc/<pid>/fd/*"), and LTP's mount08 tests it; 6.1 and native 6.8 allow it. Appine follows the newer rule as one of the deliberate exceptions to the 6.1 contract.
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
- **Source**: round 185 (`185-protected-links-random`); 12c4afa

### D15: Mounts, fds, file syscalls, pipes, locks, devices and inotify are one kernel/vfs module
- **What**: kernel/vfs covers path resolution, the mount table and namespace tables, tmpfs instances, fd tables and file I/O, metadata syscalls, xattrs, pipes and FIFOs, locks and leases, device nodes and loop devices, timerfd, inotify and native AIO, with eventfd in kernel/poll.
- **Why**: The retired design doc's §8.2–§8.3 covered mounts and inotify but not fd tables, file syscalls, pipes, locks or devices.
- **Tradeoffs**: One large map covers about 12k lines of source.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them `/proc/<pid>/fd` without an access check, unprotected `system.*` xattrs, file position races, umount ignoring mappings, pending AIO polls that can block io_getevents forever and splice, tee and vmsplice blocking by the wrong flags (three Starnix tests hang).
- **Source**: c5236a1 (scope since extended by 3f3db11; bug list since updated by 3f3db11 and 4e64e74)

### D16: timerfd and lease deadlines saturate at KTIME_MAX
- **What**: timerfd's parser uses the shared `timespec()` helper (kernel/process D15), and the lease-break deadline clamps the `lease-break-time` sysctl at KTIME_MAX.
- **Why**: timerfd_settime parsed timespecs on its own and lease breaking added the sysctl to now, so both could overflow `Instant` and panic.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 96cd8bd

### D17: Open-time `O_SYNC`/`O_DSYNC` are kept, and `CLOSE_RANGE_UNSHARE` copies only a shared table
- **What**: `CLOSE_RANGE_UNSHARE` copies the fd table only above two references, like `unshare(CLONE_FILES)`, and open keeps `O_DSYNC` and `__O_SYNC` (`KEPT_AT_OPEN`), so `F_GETFL` shows them, `F_SETFL` cannot clear them and `write_sync` fdatasyncs or fsyncs after each write.
- **Why**: The unshare test ignored that each thread holds two references to its fd table, and open's status-flag mask lacked the sync flags although `write_sync` already acted on them.
- **Tradeoffs**: sendfile, copy_file_range and splice into an `O_SYNC` file still do not sync (recorded in the map), and synchronous writes now pay a host fsync each, as they should.
- **Watch out**: Programs that open files with `O_SYNC` by habit get slower writes.
- **Source**: 68cafce

### D18: A chown that would clear set-id bits needs ownership, after Linux 6.2
- **What**: chown computes `keep_sgid` (in the file's group or `CAP_FSETID`) and, for a store non-directory with setuid or with a setgid bit that is group-executable or not kept, requires the owner or `CAP_FOWNER` and otherwise returns `EPERM` before changing anything, following 6.2's `setattr_should_drop_sgid`.
- **Why**: Anyone could strip the set-id bits of root's programs on a writable mount with `chown(-1, -1)`, a denial of service against su and its kind.
- **Tradeoffs**: It is one more exception to the 6.1 contract, which neither refused nor cleared a non-executable setgid bit on chown; native kernels (6.8, CI's) show the 6.2 rule.
- **Watch out**: Writes (`remove_privs`) still clear setgid only with group exec, as 6.1 did, so chown and write differ for a writer outside the file's group (recorded in the map).
- **Source**: ce7d0d8

### D19: `/proc/<tid>` resolves to its thread group in the VFS too
- **What**: `exe_file` and `open_pseudo`'s process reference find the process with `pseudo::group_of`, so a non-leader thread id works like its group's.
- **Why**: `/proc/<tid>/exe` and opening `/proc/<tid>` as a directory looked the id up as a thread group only.
- **Tradeoffs**: None.
- **Watch out**: Code that treats a pid node's id as a thread group id misses threads (kernel/procfs D12).
- **Source**: 173da76

### D20: VFS files read in the order their code runs
- **What**: syscall/fs.rs runs opening, reading and writing, directories, syncing, metadata, namespace changes, ownership and times, truncation, then paths, tmpfs accounting and helpers; `impl Vfs` resolves paths top-down; mount.rs, inotify.rs, fd.rs and the smaller files follow their calls' order.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; syscall/fs.rs kept reading and writing at the end and syncing among the namespace calls.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `random_ioctl` stays above `ioctl`, sharing a paragraph with the ioctl constants.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 866b1cf

### D21: Loop devices forward I/O to their backing open file
- **What**: `syscall/loopdev.rs` keeps loop devices in `Kernel::loops` (eight at boot, more on demand with nodes in the boot `/dev`), block nodes dispatch by major 7, I/O goes through the backing open file's own read and write paths at the device offset (regular files without the BKL), and the loop and common block ioctls follow 6.1's errors and order.
- **Why**: LTP takes its test device from loop-control, so 151 tests broke before reaching Appine, and losetup and image tools did not work.
- **Tradeoffs**: There is no page cache, no partitions, discard, fallocate, direct I/O or mmap of the device, `BLKGETDISKSEQ` is approximate, and nothing can be mounted from a device.
- **Watch out**: Native kernels after 6.1 unbind an autoclear device some time after the last close, so differential cases must wait, and a loop over a loop keeps the lower device open until the upper one is unbound.
- **Source**: a26c8b4

### D22: Vectored I/O on pipes, terminals and sockets is one operation
- **What**: Following Linux's split between `read_iter`/`write_iter` and plain operations (checked against 6.1), sockets go to kernel/net's `rw` (D13 there), pipe, PTY and eventfd reads do one 64 KiB read scattered over the iovecs, pipe and PTY writes go in 64 KiB pieces across iovecs until one is short, and positional files keep the per-iovec loop.
- **Why**: readv and writev called the file's read or write once per iovec, so message boundaries, pipe readv waits and pipe write atomicity differed from Linux.
- **Tradeoffs**: A pipe or terminal readv still reads at most 64 KiB per call, and host stdio fds stay per-iovec.
- **Watch out**: Borrows of the open file must end before any socket or pipe call that can wait, and a file kind whose Linux operations change between plain and iter needs its arm moved.
- **Source**: b295644

### D23: timerfds keep their clock and honour `TFD_TIMER_CANCEL_ON_SET`
- **What**: A timerfd keeps an `Expiry` (kernel/process D19) and, for absolute realtime timers with `CANCEL_ON_SET`, the clock-jump count at settime, so a later jump counts as one expiration for poll and epoll and the next read returns `ECANCELED` once while the timer stays armed.
- **Why**: timerfds converted absolute times once at settime, and `CANCEL_ON_SET` was accepted but never cancelled, so systemd's clock-change detection could not work.
- **Tradeoffs**: A jump nobody looks at is noticed at the next look, with the same observable result.
- **Watch out**: None known.
- **Source**: 63b9e59

### D24: Reads into partly faulting buffers return what was copied
- **What**: read_file and readv copy out through `Cx::write_user_partial`, growing a stack below the buffer first, and return the bytes copied, rewinding a regular file's position past the uncopied part, while writes read their buffer through `Cx::read_user`.
- **Why**: A read into a buffer faulting partway returned `EFAULT` and lost the data, and a buffer below a grows-down mapping never grew it, where Linux returns the bytes copied and grows.
- **Tradeoffs**: Pipe and terminal data past the fault is still consumed, where Linux leaves it unread.
- **Watch out**: Callers that expected `EFAULT` for partly valid buffers now get a short count, as on Linux.
- **Source**: e03c0cc

### D25: F_SETOWN and lock ranges take arguments at the edges of their types as Linux does
- **What**: `set_owner`/`get_owner` follow Linux's `f_setown` (`INT_MIN` is `EINVAL`) on global numbers, with F_SETOWN/F_GETOWN translated once by the dispatcher's `pid_args` (which maps F_SETOWN(-1) to process group 1) and the socket owner ioctls translating their own, and record-lock ranges are computed with an inclusive last byte in `flock64_to_posix_lock`'s order.
- **Why**: F_SETOWN(`INT_MIN`) overflowed its negation, owners ignored pid namespaces, lock ranges added a negative `l_len` before checking the start and could not end at `OFFSET_MAX`, and the first fix translated owners a second time in child pid namespaces.
- **Tradeoffs**: An unknown pid is still accepted where Linux returns `ESRCH`, and F_SETOWN's translation stays split between the dispatcher and fcntl.
- **Watch out**: Syscalls listed in `pid_args` must not translate again in their handlers.
- **Source**: 09188ec, 5656ada

### D26: The tty ioctls learn whether their fd can read
- **What**: The vfs ioctl dispatch passes `file.readable()` to `tty::ioctl`, so TIOCSCTTY can require read access (kernel/tty D14).
- **Why**: Taking a terminal through an fd without read access needs `CAP_SYS_ADMIN` on Linux, and only the vfs knows the open file's mode.
- **Tradeoffs**: Every tty ioctl carries a flag only TIOCSCTTY reads.
- **Watch out**: None known.
- **Source**: 838b5b4

### D27: memfd_create gives the store its caller's filesystem ids
- **What**: memfd_create passes the caller's fsuid and fsgid (kernel ids) to the store's `create_anonymous` (kernel/store D14).
- **Why**: memfd_create is the store's only caller of `create_anonymous` and the one that knows the caller.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 907b82b

### D28: linkat follows 6.1's checks for deleted and fd-named files
- **What**: `AT_EMPTY_PATH` without `CAP_DAC_READ_SEARCH` is `ENOENT`, a deleted regular file reached through `/proc/<pid>/fd/N` becomes its store node on the mount it was opened through (memfds stay `EXDEV`), `protected_hardlinks` reads the database row, and the store refuses a new name for an nlink-0 inode (kernel/store D15).
- **Why**: linkat's other differences let a link to an unlinked file through or reported it wrongly, and the hardlink check read the host object of a deleted file.
- **Tradeoffs**: Following 6.1, unprivileged `AT_EMPTY_PATH` links fail where 6.10 and later allow them for the opener.
- **Watch out**: Non-root programs that link O_TMPFILE-style files must use the `/proc/self/fd` fallback.
- **Source**: c108be0

### D29: sendfile refuses an `O_APPEND` output that is not a pipe
- **What**: sendfile returns `EINVAL` for an `O_APPEND` output other than a pipe, after the input checks.
- **Why**: Linux sends non-pipe outputs through `do_splice_direct`, which rejects `O_APPEND`.
- **Tradeoffs**: None.
- **Watch out**: Programs that relied on Appine appending now get `EINVAL`, as on Linux.
- **Source**: 298cadb

## Superseded

- ~~There are no block devices or loop devices (D5's tradeoff, rounds 04, 05, 184)~~ → replaced by **D21** in a26c8b4 (2026-11-06)
- ~~F_SETOWN/F_GETOWN owners translated by the owner helpers (09188ec)~~ → replaced by **D25** in 5656ada (2026-11-07)
