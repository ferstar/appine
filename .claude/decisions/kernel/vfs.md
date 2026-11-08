# VFS (kernel/vfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/vfs"`
>
> Last distilled: 2026-11-08 (HEAD = fffe1c1). D1–D14 were seeded by hand from the retired design
> doc §8.2–§8.3 and §11.1 and the round records in `docs/rounds/`, so their sources cite rounds;
> later entries cite commits.

## Constraint index (compressed active decisions)

> Full text of D1, D2, D4–D14, D16–D24 and D26–D29: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/vfs.md`

- **D1** — Nodes carry the mount they were reached through (`Node::Store(mount, ino)` and `Node::Pseudo(mount, node)`, Linux's `struct path`), so one directory mounted twice is two nodes, `..`, getcwd and `/proc/<pid>/{cwd,root,fd}` climb back through their own mount points, and mount ids are never reused (rounds 105, 116)
- **D2** — Mount records live in one global table by id with their namespace recorded, a task's mount namespace sits in `FsContext` rather than nsproxy, unused namespaces and detached mounts are collected only at `umount2` and task exit, and every mount operation must check the caller's namespace (Linux's `check_mnt`) (round 118)
- **D4** — `/dev` (tmpfs with standard nodes), `/dev/pts`, `/dev/shm` (executable and unlimited for JIT double mappings), `/dev/mqueue` and `/run` are mounted at boot with systemd's options so OpenRC leaves them alone, `/tmp` stays a store directory, and the store's own `/dev` keeps the same nodes underneath (rounds 05, 184)
- **D5** — Opening any character-device node selects its in-process implementation by device number wherever the node is (unknown numbers are `ENXIO`), and nodev mounts and the cgroup BPF device filter must be checked before dispatch (rounds 04, 05, 184; a26c8b4)
- **D6** — Mount propagation types are validated and then ignored, so every mount behaves as private (no `shared:N`, `MOVE_MOUNT_SET_GROUP` is `EINVAL`), and remount changes only the one mount's flags except tmpfs size options, which belong to the instance (rounds 105, 116, 168)
- **D7** — Of the new mount API only open_tree, move_mount and mount_setattr exist, following 6.1's `fs/namespace.c` because crun sends every bind through them, while the fsopen family stays `ENOSYS` (callers fall back to mount(2)), idmapped mounts are `EINVAL` and an unattached clone's anonymous namespace is torn down only at the next collection (round 168)
- **D8** — ro, noexec, nosuid, nodev and nosymfollow are checked per mount at Linux 6.1's points and in its order, rename and link across mounts are `EXDEV` even between binds, a mount point is `EBUSY` only in the caller's namespace, and any new syscall that creates or modifies files must call the read-only check at the matching point (rounds 116, 168)
- **D9** — openat2's `RESOLVE_*` flags follow 6.1's `fs/namei.c` inside Appine's own walker because Android's app seccomp blocks the host's openat2, `RESOLVE_CACHED` only rejects create, truncate and tmpfile opens, and `exe` counts as a magic link here although it is followed by content elsewhere (round 144)
- **D10** — Mounting over `/proc/<pid>/fd` entries is `ENOENT` as in Linux 6.12, one of the deliberate exceptions to the 6.1 contract to count when deciding on a contract upgrade (round 105)
- **D11** — Regular-file reads and writes release the BKL for host `pread`/`pwrite` behind an `f_pos_lock` emulation (one position-based operation per open file, released between 64 KiB chunks), `O_APPEND` writes stay under the BKL, and position-lock waiters listen on the lock wake channel only (rounds 24, 53, 89)
- **D12** — flock, POSIX and OFD locks and leases are in-process tables keyed by store inode with Linux's ownership, splitting, merging and deadlock rules, invisible to other kernels and to the Files app, and closing a file with locks or leases must request the lock channel (rounds 08, 19, 53)
- **D13** — inotify lives entirely in the kernel layer, reporting at the positions and in the order of Linux's fsnotify hooks with watches keyed by store inode and Files app changes replayed under the target kernel's BKL; synthetic filesystems never report, there is no fanotify, and the no-watch cost must stay one emptiness check per event (rounds 11, 19, 201)
- **D14** — `protected_symlinks` and `protected_hardlinks` default to 1 and are enforced as 6.1's `may_follow_link` and `may_linkat` do (`protected_regular` and `protected_fifos` are not implemented), and the credential-less no-BKL `Cached` walk must keep handing such links to the normal walk (round 185; 12c4afa)
- **D16** — timerfd parses timespecs through the shared `timespec()` helper (kernel/process D15) and the lease-break deadline clamps at KTIME_MAX, so neither can overflow `Instant` (96cd8bd)
- **D17** — Open keeps `O_DSYNC` and `__O_SYNC` (`KEPT_AT_OPEN`), so `F_GETFL` shows them, `F_SETFL` cannot clear them and `write_sync` syncs after each write (sendfile, copy_file_range and splice still do not), and `CLOSE_RANGE_UNSHARE` copies the fd table only above two references, like `unshare(CLONE_FILES)` (68cafce)
- **D18** — A chown that would clear the set-id bits of a store non-directory needs the owner or `CAP_FOWNER` (`EPERM` otherwise), following 6.2's `setattr_should_drop_sgid` as an exception to the 6.1 contract, while writes still clear setgid only with group exec as 6.1 does (ce7d0d8)
- **D19** — `exe_file` and `open_pseudo` find a pid node's process with `pseudo::group_of`, so `/proc/<tid>` works like its thread group's, and code treating a pid node's id as a thread-group id misses threads (kernel/procfs D12) (173da76)
- **D20** — The VFS's Rust files read in the order their code runs (syscall/fs.rs from opening to its helpers, `impl Vfs` top-down), and code appended at the end of a file drifts from that order (866b1cf)
- **D21** — Loop devices in `Kernel::loops` forward I/O through the backing open file at the device offset with 6.1's loop and block ioctl errors (no page cache, partitions, discard, mmap or mounting), differential cases must wait for native kernels after 6.1 to unbind an autoclear device, and a loop over a loop keeps the lower device open until the upper one is unbound (a26c8b4)
- **D22** — readv and writev on pipes, terminals and sockets are one operation, as Linux's `read_iter`/`write_iter` split has it (sockets through kernel/net's `rw`, D13 there; one 64 KiB read for pipes, PTYs and eventfds; pipe and PTY writes in 64 KiB pieces), positional files keep the per-iovec loop, and open-file borrows must end before any socket or pipe call that can wait (b295644)
- **D23** — A timerfd keeps an `Expiry` on its clock (kernel/process D19), and with `TFD_TIMER_CANCEL_ON_SET` a later realtime clock jump counts as one expiration and makes the next read return `ECANCELED` once while the timer stays armed (63b9e59)
- **D24** — read_file and readv copy out through `Cx::write_user_partial`, growing a stack below the buffer first, and return the bytes copied (rewinding a regular file's position past the uncopied part) instead of `EFAULT`, though pipe and terminal data past the fault is still consumed (e03c0cc)
- **D26** — The vfs ioctl dispatch passes `file.readable()` to `tty::ioctl` so TIOCSCTTY can require read access (kernel/tty D14) (838b5b4)
- **D27** — memfd_create passes the caller's fsuid and fsgid (kernel ids) to the store's `create_anonymous` (kernel/store D14) (907b82b)
- **D28** — linkat follows 6.1: `AT_EMPTY_PATH` without `CAP_DAC_READ_SEARCH` is `ENOENT` (6.10's exception for the opener is not followed), a deleted file reached through `/proc/<pid>/fd/N` links as its store node (memfds stay `EXDEV`), `protected_hardlinks` reads the database row, and the store refuses a new name for an nlink-0 inode (kernel/store D15) (c108be0)
- **D29** — sendfile returns `EINVAL` for an `O_APPEND` output that is not a pipe, after the input checks, as Linux's `do_splice_direct` does (298cadb)

## Active (full detail)

### D3: tmpfs is a detached tree in the store, accounted by the pages its objects take
- **What**: Each tmpfs mount is a new instance backed by a store directory outside the root tree, with its own device number; usage is counted per instance in inodes and in the pages its host objects take, a write or reservation checks room only for the pages it would newly allocate, and space is freed at the last unlink.
- **Why**: bubblewrap builds its new root on a tmpfs. A separate in-memory filesystem would need every path and fd syscall, mmap, file locks, inotify and AF_UNIX paths to handle another node kind, and its contents would consume the app's memory (the reason `/tmp` was put in the store); counting file sizes as if files had no holes made sparse files reach `ENOSPC` early.
- **Tradeoffs**: tmpfs is as slow as the store and writes flash; room checks near a full instance cost host lseeks and count reserved-page slack per file rather than per range; concurrent unlocked writes and writes through shared mappings can miscount until the next recount.
- **Watch out**: statfs and remount recount the whole tree, which also corrects drift from shared mappings; a host whose `SEEK_DATA` treats reserved space differently could let a write past a full tmpfs or refuse one.
- **Source**: rounds 117 (`117-tmpfs`), 131 (`131-tmpfs-usage`); 5d7b936

### D15: Mounts, fds, file syscalls, pipes, locks, devices and inotify are one kernel/vfs module
- **What**: kernel/vfs covers path resolution, the mount table and namespace tables, tmpfs and overlay instances, fd tables and file I/O, metadata syscalls, xattrs, pipes and FIFOs, locks and leases, device nodes and loop devices, timerfd, inotify and native AIO, with eventfd in kernel/poll.
- **Why**: The retired design doc's §8.2–§8.3 covered mounts and inotify but not fd tables, file syscalls, pipes, locks or devices.
- **Tradeoffs**: One large map covers about 12k lines of source.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them unprotected `system.*` xattrs, file position races, umount ignoring mappings, no superblock flags (a read-only tmpfs turns writable through a bind remount), `IN_EXCL_UNLINK` filtering the wrong events, execve reporting no inotify events, mountinfo without propagation fields (dockerd leaves its data root bind-mounted until the next boot) and the overlay gaps.
- **Source**: c5236a1 (scope since extended by 3f3db11 and 2a244d2; bug list since updated by 3f3db11, 4e64e74, 70c1bc6, ce8a9ce and 2aca260)

### D25: F_SETOWN and lock ranges take arguments at the edges of their types as Linux does
- **What**: `set_owner`/`get_owner` follow Linux's `f_setown` on global numbers (`INT_MIN` is `EINVAL`, a number no task, thread group, process group or session has is `ESRCH`, 0 removes the owner, and the setter's uids are kept), a lease sets the owner only when none is set, F_SETOWN/F_GETOWN are translated once by the dispatcher's `pid_args` (which maps F_SETOWN(-1) to process group 1) with the socket owner ioctls translating their own, and record-lock ranges are computed with an inclusive last byte in `flock64_to_posix_lock`'s order.
- **Why**: F_SETOWN(`INT_MIN`) overflowed its negation, owners ignored pid namespaces, lock ranges added a negative `l_len` before checking the start and could not end at `OFFSET_MAX`, the first fix translated owners a second time in child pid namespaces, and F_SETOWN accepted any number with an owner carrying no rights, which a lease then replaced.
- **Tradeoffs**: F_SETOWN scans the thread groups to find a process group or session, and its translation stays split between the dispatcher and fcntl.
- **Watch out**: Syscalls listed in `pid_args` must not translate again in their handlers.
- **Source**: 09188ec, 5656ada, f6a82cc

### D30: Mounts are limited by `fs.mount-max`, and whole-table walks stay linear
- **What**: `Kernel::mount_max` refuses with `ENOSPC` a new mount, a bind (counting the copies it plans) or the attachment of a detached tree that would exceed it, and copying submounts, unmounting a tree and listing mounts index the table once per operation (`plan_copies`, `tree_of`, `mounts_from`) instead of scanning it for every mount.
- **Why**: Without a limit a recursive-bind loop grew the table without bound, and the per-mount scans turned into minutes of BKL time at Linux's own limit.
- **Tradeoffs**: Single-mount lookups (`reach` for getcwd, `root_point`) stay linear, so walks through many mount points in a huge table are slower than Linux; detached trees in anonymous namespaces are not counted until attached, as on Linux.
- **Watch out**: A loop over mounts that looks each one up in the list is quadratic again, and `reach` holds a shared borrow of the mount table while it calls the store and `Pseudo`, so a mutable borrow there would panic.
- **Source**: 0a4712e

### D31: splice, tee and vmsplice wait and refuse as 6.1's do
- **What**: The three calls follow 6.1's `do_splice`, `do_tee` and `vmsplice` in Linux's argument order: a zero length returns 0 and unknown flags are `EINVAL` first; pipe-to-pipe splices and tee go through `pipe_to_pipe` (Linux's `ipipe_prep` and `opipe_prep`) and are nonblocking with `SPLICE_F_NONBLOCK` or either end's `O_NONBLOCK`; a splice into a pipe waits for room (`EPIPE` with `SIGPIPE` without readers) unless the flag or the pipe's `O_NONBLOCK` says not to and pushes what it read whole; vmsplice waits only without `SPLICE_F_NONBLOCK`; room counts whole free pages.
- **Why**: The nonblocking flags were looked at only for an input pipe, so splice and tee into a full pipe or from an empty socket slept where Linux returns `EAGAIN`, vmsplice followed the pipe's `O_NONBLOCK` instead of the flag, and tee took data before waiting for room and so wrote data a reader had since consumed.
- **Tradeoffs**: Pipes hold bytes, so whole free pages only approximate Linux's free buffers, and data read for a splice into a pipe can briefly overfill it when another writer filled it while the BKL was released; splice's remaining argument-order differences are recorded in the map.
- **Watch out**: A pipe with less than a page free is full to splice, tee and vmsplice although a small write fits, and programs that relied on splice blocking on a nonblocking pipe now see `EAGAIN`.
- **Source**: 5530624

### D32: Inherited mounts are locked, and proc, sysfs and cgroup2 check their namespace's owner
- **What**: Copying a mount namespace into one owned by another user namespace locks every copy (`Mount::locks`: the mount itself and its ro, nodev, nosuid, noexec and atime flags), so umount2, moves and pivot_root refuse a locked mount (`EINVAL`), remount and mount_setattr refuse to clear a locked flag (`EPERM`), and a non-recursive bind or clone of a tree with locked mounts below is `EINVAL`; lazy unmounts and namespace collection keep a locked mount on a parent detached with it, as 6.1's `disconnect_mount` does; proc, sysfs and cgroup2 need `CAP_SYS_ADMIN` over the owner of their pid, network or cgroup namespace, and outside the initial user namespace proc and sysfs also need a fully visible instance, whose locked flags the new mount takes over.
- **Why**: Root of a nested user namespace inside a sandbox could unmount the mounts hiding paths, clear ro/nosuid/nodev/noexec with a bind remount, bind a tree without its submounts or mount a fresh unmasked proc or sysfs, and once mounts were locked a lazy unmount still dropped them from their parent, so an fd kept in the detached tree reached the covered directories.
- **Tradeoffs**: Mounts detached because their mount point was deleted in another namespace still disconnect unlocked children (Linux keeps them connected), every lookup also scans the detached list, and `/sys/fs/cgroup` is the only directory counted as an empty mount point.
- **Watch out**: A detached mount may still have a mount point; rootless container setups that mount sysfs depend on `/sys/fs/cgroup` counting as empty, and `unshare -Urm mount -t proc` without a new pid namespace is `EPERM`, as on Linux.
- **Source**: 6d05aa9, 135cb1e

### D33: Shrinking a file unmaps its dropped tail before the host file shrinks
- **What**: truncate, ftruncate and `O_TRUNC` call `truncating` before `set_len`, which, when the file gets shorter, removes the tail's page-table entries in every address space through `Mm::file_truncated` and syncs each one that changed, the caller's included (kernel/mm D5).
- **Why**: The host file shrank first and the caller's address space was synced only at syscall exit, so a concurrent access through a stale translation hit a host `SIGBUS`.
- **Tradeoffs**: The caller's address space is quiesced during the syscall rather than at its exit.
- **Watch out**: If the host truncation then fails (`EIO`), mappings take the file to end at the requested length, and accesses beyond it get `SIGBUS` until remapped.
- **Source**: 400817d

### D34: Open modes and open-time flags follow Linux's `build_open_how` and `OPEN_FMODE`
- **What**: An `O_PATH` fd keeps only `O_PATH` (`kept()`), reading and writing require the matching access mode so mode 3 does neither (a FIFO opened with it is `EINVAL`), `O_TRUNC` on a directory is `EISDIR` and on an `O_RDONLY` regular file truncates through a writable handle of its own, and a synthetic file opened with `O_TRUNC` needs write permission, with non-writable nodes going to `pseudo::open_unwritable`.
- **Why**: `O_PATH` fds kept other status flags, access mode 3 read and wrote, `O_RDONLY|O_TRUNC` neither truncated nor refused directories, and the pseudo open let `O_TRUNC` through without write permission.
- **Tradeoffs**: The `ETXTBSY` check still treats mode 3 as a write.
- **Watch out**: Programs that relied on mode 3 reading or writing now get `EBADF`, and a non-root `O_RDONLY|O_TRUNC` open of a synthetic file is `EACCES`, as on Linux.
- **Source**: 5d23297, f6a82cc

### D35: A read-only remount is `EBUSY` while files on it are open for writing
- **What**: `writers()` collects the mounts of regular and synthetic files open for writing in fd tables and of writable loop backing files; a bind remount or mount_setattr to read-only is `EBUSY` when that mount has one, and a remount without `MS_BIND` when any mount of the same device has one.
- **Why**: A read-only remount succeeded while files were open for writing, and those descriptors went on writing.
- **Tradeoffs**: Each read-only remount scans every fd table, and files held only by mappings or in-flight `SCM_RIGHTS` messages do not count.
- **Watch out**: "The same superblock" is approximated by the device number.
- **Source**: b43ff37

### D36: File syscalls follow 6.1's argument checks and error order
- **What**: getdents64 checks `O_PATH`, removed directories and an int count and copies out entry by entry; copy_file_range follows `generic_copy_file_checks` and `rw_verify_area` (`EXDEV` between devices); fcntl and flock refuse `O_PATH`; truncate checks the type before the mount and permission, and synthetic regular files succeed unchanged; `O_DIRECT` is `EINVAL` except on store regular files outside tmpfs and on loop devices; openat validates `O_TMPFILE`; alarm-clock timerfds need `CAP_WAKE_ALARM`; AIO maps restart codes to `EINTR`; renameat2 `NOREPLACE` onto `.` or `..` is `EEXIST` and synthetic filesystems follow `vfs_rename`'s order; directory `SEEK_END`, `SEEK_DATA` and `SEEK_HOLE` answer per filesystem (the store as ext4's hashed directories, procfs, sysfs and cgroup2 at 0, others `EINVAL`); utimensat reports an event for one time alone; procfs and devpts report an `st_blksize` of 1024.
- **Why**: These calls answered with other errors or in another order than 6.1, which Starnix's, gVisor's and LTP's tests check.
- **Tradeoffs**: `O_DIRECT` is checked before the file's own open, where Linux opens first (a FIFO waits for its peer before `EINVAL`), and tmpfs refuses it as 6.1 does although 6.6 and later accept it.
- **Watch out**: Opens of character devices or tmpfs files with `O_DIRECT` now fail, and copies between tmpfs and the store are `EXDEV`, which callers handle by falling back.
- **Source**: dcacfb6, 5d7b936

### D37: Holes and preallocation are the host object's
- **What**: fallocate (mode 0, `KEEP_SIZE`, `PUNCH_HOLE`, and `ZERO_RANGE` outside tmpfs) and `SEEK_DATA`/`SEEK_HOLE` go to the host object, and `st_blocks` comes from the host object's allocation (kernel/store D16).
- **Why**: Holes were emulated by writing zeros, so no storage was freed, cp, tar and rsync wrote sparse files out in full, and LTP's fallocate04 and fallocate05 failed.
- **Tradeoffs**: A host filesystem without a mode still falls back to writing zeros.
- **Watch out**: Host fallocate with mode 0 now really reserves storage on the device.
- **Source**: 5d7b936

### D38: `security.capability` is checked and converted over the file's user namespace
- **What**: setxattr and removexattr of `security.capability` need `CAP_SETFCAP` over the file (its owner and group mapped, as `cap_convert_nscap` and `cap_inode_removexattr` check), setxattr stores the converted value, and getxattr returns the reader's view (kernel/cred D12).
- **Why**: The writes needed `CAP_SETFCAP` in the initial namespace, so root of a user namespace could not set file capabilities on files it owns, which rootless podman and docker build do.
- **Tradeoffs**: None.
- **Watch out**: Files whose owner is unmapped in the caller's namespace refuse capability changes there (`EPERM`), as on Linux.
- **Source**: 2aca260

### D39: overlayfs is a third node kind with instances of its own
- **What**: `Node::Overlay(MountId, OvlId)` and `Fs::Overlay` in `vfs/overlay.rs` keep overlay instances and one node per directory and name (Linux's overlay dentry); reads go to the topmost store inode, open files record their overlay node in `OpenFile::ovl` so `node()`, fd-based changes and `/proc/<pid>/fd` never reach a layer's inode, and an overlay without an upper layer is read-only as a superblock (`sb_readonly`).
- **Why**: Container engines on Appine ran on the vfs driver and native snapshotter, which copy every layer and every container's tree, while on Linux they use overlayfs, whose base is read-only multi-layer mounts.
- **Tradeoffs**: Every exhaustive match on `Node` handles a third variant; overlay nodes are kept until the instance's last mount is collected; overlays cannot be layers, inotify on overlay nodes never reports, and `/proc/<pid>/exe` of a program in an overlay is followed by path with no `ETXTBSY` for its writers.
- **Watch out**: `let Node::Store(..) = .. else` sites treat overlay nodes like synthetic ones and may return another errno than Linux; overlay lock keys set the top bit and must never collide with store inode numbers; code that turns an open file's store inode back into a `Node::Store` writes into a layer behind the overlay's back.
- **Source**: 2a244d2

### D40: Overlay copy-up runs in one store transaction per node
- **What**: A change copies its node up in one store transaction (`Overlays::upper_of`), parents first, keeping mode, owner, attributes, times and holes; creations replace a whiteout in the same transaction and make a directory there opaque; `unlink_overlay` and `rename_overlay` follow Linux's check order (merged-directory renames are `EXDEV`); permission is checked on what the overlay shows before copying up; `store_dir` refuses overlay directories so no store path can change a layer; read-only fds move to the upper object on their next read; an instance pins the tmpfs instances its layers are in; origins are kept in memory only.
- **Why**: Container engines need writable overlays (copy-up on change, whiteouts on removal, opaque directories, `EXDEV` for merged-directory renames), and podman's probe (a premade whiteout, then `rm -rf` through the overlay) must pass.
- **Tradeoffs**: The work directory is created but never used; copy-up copies data with the BKL held; copied-up files change inode number at the next mount, and links made through the overlay show the upper inode's number; writes through an overlay are not counted against a tmpfs upper layer's limits; parents are copied up before a creation's permission check.
- **Watch out**: Code that builds a `Node::Store` from an overlay file's store inode or calls store mutations on a layer bypasses copy-up and whiteouts, large copy-ups stall every process, and memory grows with the overlay nodes looked up while mounted.
- **Source**: 4b4af75

### D41: timerfds are opened `O_RDWR`, as `anon_inode_getfd` does
- **What**: timerfds open `O_RDWR`, and the write path answers `EINVAL` for them; signalfds do the same (kernel/signal D22).
- **Why**: timerfds were `O_RDONLY`, so `F_GETFL` showed the wrong mode and write failed with `EBADF` instead of `EINVAL`.
- **Tradeoffs**: None.
- **Watch out**: Writes are kept away from a timerfd by the write path's `EINVAL`, not by `writable()`.
- **Source**: e6bfdf8

### D42: Pending AIO polls complete outside io_getevents, and io_submit follows 6.1
- **What**: io_getevents waits on its pending polls' files as ppoll would, plus the context's wake object (kernel/poll D16); io_cancel first completes ready polls; the timer thread watches the contexts with eventfd-signalling polls (`Kernel::aio_polls`, kernel/process D11); the resfd eventfd is taken at submission, and releasing a context signals its cancelled polls' eventfds; writes take `append_offset` and `write_sync` as pwritev2 does; io_submit follows `io_submit_one` and the per-command checks, with a `read_iter`/`write_iter` table (`has_iter`) and `has_fsync`; events whose copy faults go back to the queue.
- **Why**: Pending `IOCB_CMD_POLL`s completed only inside io_getevents on their context, so a resfd eventfd watched with epoll never fired and polls on host sockets or timerfds were seen late or never; AIO writes also ignored `O_APPEND` and the `RWF_*` sync flags, a faulting copy lost events, and io_submit queued requests 6.1 refuses.
- **Tradeoffs**: An eventfd-signalling poll completes when the timer thread next runs rather than inside the readying write, and synthetic files are all taken to have `read_iter` and `write_iter`, which `/proc/<pid>/mem` and pagemap do not.
- **Watch out**: io_getevents sleeps on the context object, so a path that queues an event without requesting it leaves waiters asleep; `has_iter` must follow new file kinds, and its `/dev/kmsg`, loop-control, seccomp and bpf entries were not checked natively.
- **Source**: fffe1c1

## Superseded

- ~~There are no block devices or loop devices (D5's tradeoff, rounds 04, 05, 184)~~ → replaced by **D21** in a26c8b4 (2026-11-06)
- ~~F_SETOWN/F_GETOWN owners translated by the owner helpers (09188ec)~~ → replaced by **D25** in 5656ada (2026-11-07)
- ~~An unknown F_SETOWN pid is accepted where Linux returns `ESRCH` (D25's tradeoff, 09188ec, 5656ada)~~ → replaced by **D25** in f6a82cc (2026-11-08)
- ~~tmpfs counts usage by file size, treating files as having no holes (D3's What and tradeoff, rounds 117, 131)~~ → replaced by **D3** in 5d7b936 (2026-11-08)
