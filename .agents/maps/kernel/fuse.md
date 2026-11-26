# kernel/fuse Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/fuse.md` for the paired decision history.
>
> Verified: 2026-11-26 (full mapping: source-verified rewrite; 18 adversarial claims checked by independent verifier — 13 confirmed, 5 partial, 0 refuted; all partial findings and audit corrections patched below)
>
> Maintained: 2026-11-26 (targeted verification of device read, readv and splice delivery)

## Responsibilities

A guest daemon reads requests from an attached `/dev/fuse` and writes replies or cache notifications. The
implementation targets the Linux 6.1 FUSE interface.

- `core/kernel/src/vfs/fuse.rs` owns the wire protocol, connection, device, node cache, retry queries,
  open/forget accounting and periodic mapped-page writeback.
- `core/kernel/src/syscall/fuse.rs` implements operations on FUSE nodes and handles. It orders pathname
  checks, mutations, I/O, mapping fills and eviction around waits that release the big kernel lock (BKL).

## Key types

- `Conn` owns instance options, namespaces, INIT state, device/open counts and queues. A `Req` has a wire
  message, Pending/Sent/Done state and independent background, forced and interrupted flags. `DevFile`
  records the device inode, opener namespace and optional connection.
- `Fuses` indexes connections by device and nodes by internal `FuseId` and (device, wire node ID).
  `Node::Fuse(mnt, id)` also identifies the mount. A cached node holds parent/name, attributes, children,
  link, lookup count, lock state and pins. `Entry` carries separate name and attribute deadlines.
- `Query` records a syscall miss. `Handle` counts an open, distinguishes daemon fh/open flags from file
  status flags, and supplies a borrow-free `Io` for waits. `Released` carries final-drop accounting.
- `Fill` pins an open description, `FillPages` holds shared mapped data, and `BgWriteback` owns an
  outstanding write's bytes and busy guard. `FuseExe` retains executed-node identity.
  `FileKind::FuseSnapshot` serves a reopened deleted image.

## Public entry points

| Caller | Principal entry points |
| --- | --- |
| Mount, daemon fd, poll | `Options::parse`, `send_init`, `dev_read`, `dev_write`, `dev_ioctl`, `DevFile::readiness`, `Conn::abort` |
| Walker, dispatcher | `cached_child`, `cached_attr_fh`, `new_syscall`, `need`, `serve`, `prefault`, `sweep` |
| File and namespace syscalls | `open`, `create`, `read`, `write`, `lseek`, `fill_dir`, `stat_fh`, `setattr`, `set_xattr`, `rename_at`, `bind_socket` |
| MM, exec, teardown | `map_fill`, `fill`, `sync_mapping`, `flush_dirty`, `fill_for_dump`, `program`, `file_caps`, `flush_at_exit`, `flush_released` |

## Data flow / lifecycle

### Establishing an instance

Opening misc device 10:229 (`/dev/fuse`, mode 0666) gives an unattached description. Reads/writes require
attachment (`EPERM`), seek gives `ESPIPE`, poll gives `POLLERR`. A duplicated fd shares the description.

`fuse_mount` parses options before examining the fd. `fd/user_id/group_id/max_read` are decimal, `rootmode`
is octal with a recognized type, and owners must map into kernel IDs. `allow_other/default_permissions` take
no value. The parser accepts `source=`, skips empty fields and bare VFS options (ro/rw, sync/async, dirsync,
mand/nomand, lazytime/nolazytime). Bad options or numbers, unmapped IDs and `blksize` give `EINVAL`.

A type suffix in `fuse.<subtype>` must be nonempty. A suffix plus `subtype=`, or duplicate subtype options,
give `EINVAL`. Plain `fuse` accepts a single empty `subtype=` value. Mountinfo appends a present subtype
after a dot. It shows IDs in the connection namespace, enabled permission options and the requested max_read
floored at 4096, omitting the `u32::MAX` sentinel.

- Missing/unopened fd gives `EINVAL`. An attached FUSE fd mounts the existing registered instance's root
  again, retaining its original options. This needs no new owner, rootmode or INIT. A collected instance
  gives `ENOTCONN`.
- A new instance requires rootmode, user_id, group_id and a FUSE device opened in the caller's user
  namespace. The VFS allocates a device/mount and sets the superblock owner. Root node ID 1 starts with the
  supplied mode, its own parent and no fresh attributes. Mount attaches the fd and queues background INIT
  without waiting.

`FUSE_INIT` offers 7.37, 128 KiB readahead and `ATOMIC_O_TRUNC`, `EXPORT_SUPPORT`, `BIG_WRITES`,
`DONT_MASK`, `AUTO_INVAL_DATA`, `NO_OPEN_SUPPORT`, `PARALLEL_DIROPS`, `HANDLE_KILLPRIV`, `MAX_PAGES`,
`NO_OPENDIR_SUPPORT`. It omits ASYNC_READ, WRITEBACK_CACHE, READDIRPLUS, remote locks and splice flags. An
error, missing major/minor pair or major other than 7 refuses new RPCs with `ECONNREFUSED` until abort
takes precedence. Otherwise the
kernel stores the minor verbatim, zero-pads missing tail fields and intersects flags with the offer. The
ceiling is 32 pages, or the returned max_pages clamped to 1–256 with MAX_PAGES. Both the reply's max_write
and the option's max_read are clamped to 4096 through ceiling × 4096 bytes.

### Request and device state

After INIT, `call` maps fsuid/fsgid through the connection user namespace (overflow IDs if unmapped) and the
caller's thread ID through its PID namespace (zero if invisible). `call_as` uses supplied credentials,
including real IDs for access without AT_EACCESS. Background request credentials are zero. The 40-byte
little-endian input header includes length, opcode, unique, node ID and credentials. Normal uniques start at
2 and advance by two. Enqueue appends to the pending FIFO and wakes readers. Callers release the BKL and
wait on a separate answer object.

During INIT a kill ends an ordinary wait with `EINTR`. Forced waits can continue. A signal marks an
interruptible request. For a sent request it queues `FUSE_INTERRUPT` if supported. For a pending one,
delivery queues the interrupt. Its header unique sets the original's low bit and its body names the
original. A killed caller retracts an unread ordinary request with `EINTR`. A sent or forced request stays
outstanding. A killed forced caller does not initiate a new interrupt. On shutdown, a killed waiter that
must abandon a sent or forced request marks it background and returns `EINTR`.

| Device operation | Contract |
| --- | --- |
| Read | Require `max(8192, 80 + max_write)` bytes before the abort check. Deliver live interrupts, eligible forgets, then the oldest pending request. Empty reads give `EAGAIN` when nonblocking or `ERESTARTSYS` when interrupted while blocking. |
| Forget | With no pending requests, drain forgets. With both queues continuously populated, 16 forget messages alternate with eight ordinary requests. One entry or minor below 16 uses `FUSE_FORGET`. Otherwise `FUSE_BATCH_FORGET` packs as many (node ID, nlookup) pairs as fit. Each message gets a unique and no reply slot. |
| Oversized request | Skip and continue reading. Foreground SETXATTR completes with `E2BIG`, other foreground requests with `EIO`. Oversized background requests are dropped. |
| Write | A full 16-byte output header and exact total length are mandatory (`EINVAL`). After the abort check, unique zero dispatches a notification. Error replies are header-only with errno from −511 to −1. Replies require a sent request (`ENOENT` otherwise). A header-only interrupt reply can disable generation (`ENOSYS`) or requeue (`EAGAIN`). The original remains sent. |
| Completion | A foreground waiter takes its Done result. Device writes consume background INIT, RELEASE and periodic WRITE completions. ENOSYS is remembered per OPEN/OPENDIR, FLUSH, ACCESS, CREATE, FSYNC/FSYNCDIR, each xattr opcode, LSEEK, FALLOCATE or RENAME2. Callers define their fallbacks. |
| Poll and clone | A borrowable live device is writable and becomes readable for pending requests, forgets or a live interrupt. Unattached/aborted gives `POLLERR`, a borrow conflict gives writable bits only. `FUSE_DEV_IOC_CLONE` reads a source fd and attaches an unattached destination to the source connection. Both device opener namespaces must match. Invalid source/destination gives `EINVAL`. Unknown device commands give `ENOTTY`. |

Scalar and vectored device I/O use an 8 MiB per-chunk staging limit (large scalar or vectored writes can
dispatch multiple independently validated chunks), and a zero-length scalar write or zero-total-length
`readv` returns 0 before attachment or framing checks.

Scalar and vectored reads commit delivery after copying the entire message to daemon memory.
Device-to-pipe splice uses the device's O_NONBLOCK and commits after inserting the whole message into
free pipe slots. At commit, an ordinary request becomes `Sent` and queues its interrupt if needed.
An interrupt or forget leaves its queue, and a forget message consumes one unique. A user-copy fault
returns `EFAULT` even if a prefix was copied. Insufficient pipe room returns `EIO`. Both errors preserve
queue order and the forget interleaving counter.

Pipe-to-device splice reserves the pipe, snapshots available bytes up to the requested length including
live vmsplice contents, then writes. A header-length mismatch returns `EINVAL` before consumption.

### Notifications

- INVAL_INODE expires attributes on a known node (`ENOENT` if absent). Negative offset stops there.
  Otherwise revoke overlapping mapped pages in every mm, through EOF for a nonpositive length.
- INVAL_ENTRY and DELETE validate name length (≤1024) and terminating NUL. Both forget the name and stale a
  known parent's attributes. An unknown parent succeeds. DELETE consumes the child ID but acts on
  parent/name.
- STORE validates payload length, offset addition and node identity, extending cached size as needed. With
  live backing and nonzero payload, full cold pages and complete EOF tails become filled, partial
  materialized pages accept the slice and update tracked clean bytes, and partial cold pages stay cold;
  nonempty STORE advances the backing generation and invalidates overlapping Tier 1 executable translations
  when it touches materialized pages. Zero-byte STORE skips updating live backing even when it extends
  `attr.size`.
- POLL succeeds without a wake action. RETRIEVE gives `ENOSYS`, unknown codes `EINVAL`. Guest inotify events
  originate separately at syscall level.

### Walks and retries

`new_syscall` assigns this thread a globally fresh token and clears lookup errors, program copies and
capability results. Cached names and attributes survive to their individual deadlines or through their
fetching syscall, even at zero TTL. Symlink targets last only for that token. The VFS reads cached type,
parent (including `..`) and target while resolving paths.

A miss records a `Query` with `need` and returns internal `FUSE_RETRY` (600). A component over 1024 bytes
gives `ENAMETOOLONG`. At a plain-name miss, `lookup_walk` gathers consecutive plain components, stopping
before dot, dot-dot, empty/oversized names or other steps. A single name uses `Query::Lookup`.
`Query::LookupWalk` reuses later cached children and sends one LOOKUP per miss in one service pass. Before
continuing it checks kill, mount crossings, directory type and search permission, fetching attributes
directly if necessary. The ordinary walk resumes at a symlink, crossing, error or batch boundary.

LOOKUP errno `ENOENT` caches absence for the token. A successful node ID zero supplies a negative-entry
deadline. Other errors except `EINTR` enter per-thread `LOOKUP_ERRORS` by (parent, name) for the replaying
handler. Fresh child entries take precedence. Entering or forgetting a name clears its recorded error.
READLINK rejects empty or NUL-containing targets with `EIO`.

On retry the dispatcher first services any wanted mapping page, converting a fill failure to `EFAULT` or
`EINTR` to `ERESTARTSYS`. Otherwise `serve` takes the recorded query. Success replays the syscall under the
same token. More than 16,384 retries or a missing query gives `EIO`.

### Node identity and eviction

`enter` rejects root ID 1 as a child (`EIO`). A known same-type ID gains a lookup count. A different type
gets a new internal ID. A known directory entered beneath itself gives `ELOOP` after incrementing nlookup.
Success records parent/name, attributes, recency and token and clears cached link/unlink state. A new name
can rescue a doomed wire ID. `moved` updates nonroot files and directories unless that would loop. A node
can have several names but only one recorded parent/name.

The key `3 << 62 | id` names the node to locks, executable write exclusion, inotify and AF_UNIX. `id_of`
decodes it. The daemon's stat inode can use all 64 bits. Eviction returns the node's accumulated nlookup.

`sweep` drains removed mapping ranges before checking `SWEEP`. Its `in_use` scan keeps task roots/cwds, fd
nodes including O_PATH, distinct mms' executable and mapped image keys, live groups' FuseExe nodes, mount
roots/points and recorded dentry parents, and inotify watch keys. A holder borrow conflict in `in_use`
re-arms SWEEP and any full-drop request; a connection borrow conflict inside `evict` returns 0 without
re-arming sweep flags. `evict` adds instance roots, counted opens, live path/socket pins, I/O or directory
locks/waiters and requests still in the table (including rename's other directory and link's source), then
keeps their ancestors.

- Last unlink, rmdir and non-exchange overwrite of a last link mark the nonroot node unlinked and `doomed`
  and request a sweep. The check uses cached nlink, decremented for files, zeroed for directories. A doomed
  node's request completion with no opens or a final synthetic close can trigger another sweep.
- Unused doomed nodes and unused descendants go leaves-first (from a hash set, without age sorting) even
  below the bulk threshold. A new node above `sweep_at` schedules work. After the doomed pass, a count over
  16,384 triggers bulk trimming to 12,288 where possible. `vm.drop_caches` 2 or 3 targets all unused nodes.
  Bulk trimming removes the oldest-entered eligible leaves first.
- Bulk work sets `sweep_at = max(16384, live + max(live / 4, 4096))`. The trigger can fall as nodes go. On
  full drop a node held only by RELEASE/RELEASEDIR loses aliases in its recorded parent and is doomed
  pending the reply. Independent pins can keep an unlinked node until a later sweep.
- Eviction removes the node, mapping registration and any index still pointing to it. It prunes names of
  gone nodes and expired negative entries, queues counts for non-aborted connections and wakes readers.
  Whole-instance collection removes its tables directly.

### Permissions and metadata reads

`Conn::allows` applies even to root. With allow_other the caller must be in the connection's user namespace
or a descendant. Otherwise real, effective and saved UIDs must all match the configured user_id and all
three GIDs the group_id. These are kernel-ID comparisons. With default_permissions, `Vfs::fuse_permission`
checks cached attributes, requesting stale ones on retry. A caller able to search all directories can skip
fetching attributes for directory search. Without default_permissions, executing a regular file still
requires some execute bit. Access and chdir/fchdir/chroot use ACCESS where the daemon decides permission.
Unsupported ACCESS succeeds.

Stat denial gives `EACCES`. Statfs denial returns only `FUSE_SUPER_MAGIC` with the other words zero.
Successful stat uses the instance device, daemon inode, mapped owners and daemon block size when nonzero.
STATFS supplies capacity/file counts and sizes. Attr parsing rejects unknown file types. GETATTR and SETATTR
reject type changes with `EIO`.

For a stale regular file opened by the daemon, `Handle::fh()` supplies `FUSE_GETATTR_FH`, even for numeric
fh zero. This reaches fstat, empty-path fd stats, proc-fd stats, copy_file_range size checks, first mmap
backing creation and fd-based attribute changes. Fresh attributes need no request. Directory, O_PATH and
synthetic opens supply no regular fh.

### Open and create

- Opening a symlink gives `ELOOP`, and opening a directory with write/CREAT/TRUNC gives `EISDIR`. A regular
  truncating open needs a writable mount/superblock (`EROFS`) and adds write permission to the mask. After
  permission checks, O_NOATIME needs ownership (`EPERM`), regular write access needs a writable mount,
  writing/truncating an executing image gives `ETXTBSY`, and O_DIRECTORY needs a directory. O_PATH opens a
  path description.
- Standalone OPEN/OPENDIR strips O_CREAT, O_EXCL and O_NOCTTY. It strips O_TRUNC unless INIT accepted
  ATOMIC_O_TRUNC. ENOSYS for either opcode yields a counted synthetic handle with fh zero, KEEP_CACHE and no
  release, reused as the policy for later opens of that kind. A non-atomic truncating open follows OPEN with
  size SETATTR. An atomic one holds the I/O lock across OPEN and backing resize to zero.
- CREATE requires a writable mount and parent write/search permission. It sends flags less O_NOCTTY, mode
  and umask. DONT_MASK leaves permission bits for the daemon to mask. Success requires a regular entry plus
  open result. ENOSYS falls back to MKNOD under the parent lock, then OPEN without O_TRUNC after unlocking.
  EEXIST without O_EXCL in either create path invalidates the cached absence and retries lookup. Successful
  fallback reports one creation.
- OPEN and CREATE share `finish_open`. For regular handles without retained O_TRUNC, absence of
  `FOPEN_KEEP_CACHE` invalidates the inode's mapped pages across aliases. Retained truncation resizes
  backing even with KEEP_CACHE, including CREATE returning an already mapped inode.
- S_IFIFO uses the FIFO table keyed by `key(id)`, S_IFCHR uses `open_char_device` by rdev, S_IFBLK uses
  `loopdev::open`. Char/block return `EACCES` for MS_NODEV or a non-initial superblock user namespace
  (SB_I_NODEV). After mount permission, nonzero rdev goes through cgroup device BPF. FIFO/char take a
  temporary pin across dispatch. Resulting descriptions exposing a FUSE node can pin it through
  `OpenFile::fuse_path`. O_DIRECT on FIFO/char gives `EINVAL`, S_IFSOCK open gives `ENXIO`. Special opens
  send no FUSE_OPEN.

### File and directory I/O

Regular READ/WRITE split at negotiated limits and stop on a short reply. Scalar staging is capped at 8 MiB.
A daemon error after a completed piece can return a partial count. READ clips excess data, WRITE counts
beyond the submitted piece give `EIO`. Reads overlay dirty mapped bytes on returned data. Writes update
materialized backing and clean baselines for the accepted bytes and expire attributes.

`PosGuard` serializes implicit regular-file position changes and seek per description (`getdents64` uses a
separate path). Explicit offsets leave its position unchanged. Read copy-out accounts for bytes delivered.
`FOPEN_STREAM` uses zero for the ordinary start and leaves the position alone. STREAM and NONSEEKABLE reject
explicit-offset `pread`/`pwrite` and seek with `ESPIPE`; native AIO ignores the offset on these handles.
Current `O_APPEND` or `RWF_APPEND` selects EOF via GETATTR under the same node's `io_busy` lock through
WRITE, even across independent opens. RLIMIT_FSIZE cuts writes or gives SIGXFSZ and `EFBIG` at the limit.

`FIONREAD` on a regular live FUSE file or FuseSnapshot uses fd stat. It returns wrapping size minus position
narrowed to signed 32 bits, possibly negative past EOF. File SEEK_END issues GETATTR. DATA/HOLE tries LSEEK
unless it is already known unsupported, then assumes data to EOF and a hole at EOF. Fallback offsets below
zero or at/past EOF give `ENXIO`. Invalid whence, negative result or CUR/END overflow gives `EINVAL`.

READDIR requests one page at a time from offset zero, following the last parsed entry's daemon offset until
an empty reply. It preserves inode, type, names and supplied dot entries. Empty, slash-containing, truncated
or over-1024-byte names give `EIO`. The description caches the full vector. Guest getdents64 uses the next
vector index as d_off. Directory SET/CUR/END/DATA/HOLE uses cached size or handle-free GETATTR when size is
needed. It never sends LSEEK. A resulting position of zero clears the listing.

`write_sync` invokes `FUSE_FSYNC` for synchronous regular FUSE output, including sendfile, splice and
copy_file_range, unless FSYNC is already known unsupported. O_SYNC/RWF_SYNC requests full sync,
O_DSYNC/RWF_DSYNC data-only. File sync writes back the node before FSYNC. Directory sync uses FSYNCDIR.
Unsupported sync succeeds, other errors can reach the writer after bytes moved. Range copies use the
kernel's read/write loops.

### Directory changes and Unix sockets

Mutations hold `dir_busy` across the RPC and child-entry update (`LINK` stales the source and socket bind
installs `sock_pin` after the creation helper returns). `dir_waiters` pins waiting directories.
Operations on distinct directories can proceed independently. Cross-directory rename pins both parents
before waiting, takes `rename_busy` first for a directory move (including exchange), then locks in
increasing FuseId order and rechecks ancestry. Waits observe kill, shutdown and connection abort. Guard
drops unlock and wake waiters.

- MKDIR, MKNOD and SYMLINK check dot names, existence, read-only state and parent write/search permission.
  MKDIR keeps permission/sticky bits. Device MKNOD checks CAP_MKNOD then cgroup ACC_MKNOD, except for
  character whiteout 0:0. Type-specific creation replies must match the requested type. Success enters the
  child and stales the parent.
- LINK checks destination existence and read-only state before same-mount identity (`EXDEV`). Protected
  hardlinks use cached attributes, ownership and mount read/write permission. Parent write/search precedes
  `EPERM` for a directory source. Success stales the source.
- UNLINK/RMDIR apply dot-name, trailing-slash, read-only, parent write/search, sticky, type and mount-point
  checks. The sticky rule applies with default_permissions. Mount points give `EBUSY`. The daemon decides emptiness. Success drops the name, stales parent and
  child, and accounts for the lost link.
- Rename checks mount identity, dot names, read-only state, source/destination, NOREPLACE/EXCHANGE
  conditions, parent permissions and sticky rules, types, mount points and ancestry. A cycle gives `EINVAL`.
  Flags select RENAME2 (unsupported gives `EINVAL`), otherwise RENAME. Success forgets both names, expires
  affected attributes and moves the source. Exchange moves the other node too, while replacement accounts
  for its lost link.
- AF_UNIX bind issues MKNOD with S_IFSOCK and 0777 through the same umask policy. Dot names/collisions give
  `EADDRINUSE`. A trailing slash on an existing name gives `EADDRINUSE`, on a missing name `ENOENT`. While
  the bound socket lives, its weak `sock_pin` protects the node from eviction. Connect checks write
  permission and socket type before resolving the binding by FUSE key.

### Attributes, xattrs and privilege clearing

Chmod, chown, timestamp changes and truncation select SETATTR fields. Times distinguish omit, now and
explicit values. The request maps IDs up into the connection namespace and carries FH when supplied.
Attribute handlers enforce read-only state, mount access and, with default_permissions, local
owner/group/capability rules. Write permission can suffice to set both times to now. SETATTR under io_busy
caches the reply and uses its returned size for a size change.

Fallocate accepts zero, KEEP_SIZE, or KEEP_SIZE|PUNCH_HOLE. Unsupported mode or daemon ENOSYS gives
`EOPNOTSUPP`. A potentially growing operation checks RLIMIT_FSIZE. Success expires attributes and grows or
punches mapped backing as appropriate. Truncate/ftruncate apply the size limit through their path/fd routes.

Trusted xattrs require CAP_SYS_ADMIN (unauthorized read `ENODATA`, write `EPERM`). User xattrs need regular
file or directory type and mount read/write permission. Sticky directory writes also require `Cred::owns`.
Changing security.capability needs CAP_SETFCAP over the file, other security writes CAP_SYS_ADMIN. Size-zero
GET/LIST returns a length, an oversized reply to a nonzero buffer gives `ERANGE`. Values and daemon lists
pass through. Unsupported GET/LIST/SET/REMOVE each map to `EOPNOTSUPP`.

HANDLE_KILLPRIV delegates privilege clearing. Otherwise nonempty descriptor writes apply any needed mode
SETATTR and remove capabilities before WRITE. Non-atomic truncation combines size and any mode change in
SETATTR, then strips capabilities. Chown applies its mode rule and considers capability removal even for an
otherwise empty change. Write/truncate clear setuid and group-executable setgid without the relevant
CAP_FSETID. Non-directory chown clears setuid, and clears setgid for group execute or when neither group
membership nor capability preserves it. On regular files `remove_file_caps` probes capability length before
REMOVE when supported. `Node::no_sec` suppresses repeated killpriv probes after absence/removal. Explicit
capability set/remove updates the hint.

### Mappings and executable images

#### Demand paging and invalidation

Each live-file mapping's `Fill` retains its readable source description. The first creates a sparse memfd at
cached inode size (fh-aware GETATTR if needed). Opens, hard links and bind mounts of that node share
`FillPages` by file offset. Shared write permission, including mprotect upgrades, requires a writable fd.
Direct-I/O handles refuse shared mmap with `ENODEV`. Mount noexec restricts execution, and private stores
use host copy-on-write.

A fault takes io_busy and refreshes stale size with generation-checked GETATTR without recursive
invalidation. The default read window is up to 32 pages, sequential readahead up to 512. Short read-around
stores complete pages. If the faulted page is missing, `window_from` restarts there. Forward reads collect
the first page until complete, known EOF or an empty reply. Confirmed early EOF can fill the partial/empty
page without marking the whole window valid. A generation change discards an overtaken reply. Refill merges
dirty local bytes against their prior baseline.

Invalidation revokes overlapping translations in each distinct mm and expires filled ranges without waiting
for io_busy. Materialized bytes and local changes survive for reconciliation. With FUSE_AUTO_INVAL_DATA a
same-type LOOKUP or GETATTR reporting changed size or mtime after an earlier attribute reply invalidates
existing backing. Notifications and open policy can also invalidate. Shrink revokes translations before
resize. Hole punching clears affected resident bytes.

`prefault` services selected user buffers before a syscall consumes data, advances offsets or installs fds.
These include scalar/vector I/O, poll arrays, copy/splice offsets, fd pairs and socket buffers. It leaves
errors for the syscall to surface. Stateful socket and remote-memory copies can fill locally.
MAP_POPULATE/MAP_LOCKED, mlock, mlock2, mlockall, remap_file_pages, MADV_WILLNEED and MADV_POPULATE use
operation-specific range walks. Core dumps force fills for ELF-header checks and selected data pages. Cold
pages are unavailable when the dumping task holds a serving device and no non-killed task holds one for that
connection.

#### Writeback

Foreground writeback compares tracked pages in range against clean bytes, clips contiguous changed runs at
current EOF and sends forced WRITE with FUSE_WRITE_CACHE through a retained shared writer. Only acknowledged
bytes from the submitted snapshot become clean. A short foreground mapped writeback gives `EIO`, leaving
unaccepted bytes dirty. Guest stores during an RPC can differ from the acknowledged snapshot.

MS_SYNC writes changed runs in shared ranges and waits for data-only FSYNC when supported. Regular fsync and
close write back the whole inode first. Unmap/fixed replacement records removed shared ranges with write
capability for syscall-end flushing. Exec and exit flush outgoing shared ranges. Failed teardown retains a
writable Fill and dirty backing for retry.

The machine timer calls `flush_dirty` under the BKL. `vm.dirty_writeback_centisecs` defaults to 500 (5 s),
disabled at ≤0. When due, an idle ready node with a writer, tracked bytes and known size can queue one
bounded dirty run. Its `BgWriteback` holds `bg_busy` against foreground `with_io` operations (exec snapshot
reads directly). A valid reply acknowledges its prefix. A full reply can chain another run. Short or failed
replies leave data for later. Dropping the guard on completion, error, oversized removal or abort clears the
bit and wakes waiters.

#### Exec, procfs and inotify

`process::open_fuse` requires a regular executable, mount execute permission and no MS_NOEXEC. Its internal
image inode is the FUSE key. `Query::Program` opens read-only, reports the logical open, fetches size and
copies up to it into a host memfd. A short ordinary READ ends the snapshot at the actual byte count. The
temporary handle drops. The syscall caches the copy by node ID for replay, including interpreter resolution.

The `mnt_may_suid` gate requires a mount without MS_NOSUID and a caller in the connection namespace or a
descendant. Exec also checks ID mappings, group execute for setgid and no_new_privs. For eligible images,
`Query::FileCaps` fetches up to 64 bytes of security.capability. Known unsupported GETXATTR skips the
request. ENODATA/EOPNOTSUPP records absence, ERANGE becomes EINVAL, EINTR propagates. Results/errors are
cached for replay. Exec parses capability bytes in the connection user namespace. Re-exec through a matching
recorded executable can fall back to saved bytes on ENOTCONN/ENOENT/ESTALE. Parsing still needs that
namespace.

`ExeFile::fuse` records the mount, node pin, stat, block size and capability bytes alongside `ExeFile.host`.
A linked `/proc/<pid>/exe` magic link follows the node across rename/replacement. Last unlink or a missing
node with a saved host file selects OpenExe. Readlink resolves the node's path when possible and otherwise
marks the recorded path deleted. Reopen produces a FuseSnapshot with zero links. Reads/mmap use saved bytes,
stat preserves FUSE identity, and re-exec rechecks execute permission, noexec and nosuid. Write-open while
executing gives ETXTBSY, otherwise the snapshot route rejects it with ENOENT after earlier checks.

Ordinary live FUSE mmap labels carry full-width daemon inodes. Program, interpreter and reopened
`FuseSnapshot` mappings carry internal keys. Maps/smaps distinguish ordinary mappings by the source path
allocation, preserving even high-bit daemon inode numbers. They resolve image keys to cached daemon inodes
(zero if missing) and skip store deletion marking in both FUSE cases.

Inotify watches use the node key. Descriptions track parent/name and unlink state for access, modify and
close events. Namespace changes report create, link count, deletion, paired rename cookies and move-self as
applicable. Unlink/replacement updates descriptions for IN_EXCL_UNLINK. Truncate/fallocate report
modification, chmod/chown and xattr changes report attribute events, and timestamp events depend on which
times changed. `Query::Program` reports the logical open before its GETATTR and data copy (outer exec can
fetch stale attributes earlier), so a later read failure can still deliver open and close. ExecOpens reuses
matching logical opens and avoids duplicate access events across retries.

### Close, abort and collection

Regular close records the synthetic-open and `FOPEN_NOFLUSH` state, attempts whole-inode writeback, then
uses the saved result and cached FLUSH support to decide whether to send forced FLUSH. When sent, FLUSH
carries a lock owner derived from the closing fd table. Close removes the fd and reports errors. Dup
replacement, close_range, close-on-exec and last-fd-table exit ignore flush errors. Directories have no
per-close FLUSH.

Final handle drop decrements the node's open count and queues background RELEASE/RELEASEDIR for a real
handle on a usable connection. A borrow conflict defers both actions in a thread-local `Released`.
`flush_released` retries under the BKL at syscall end and exit. The TLS destructor leaks residual records to
avoid dropping non-atomic references outside the BKL.

Last attached device close normally aborts. MNT_FORCE aborts before the busy check, so even EBUSY can leave
the mount aborted. Lazy detach preserves held mounts. Mount holders include cwd/root, fds and in-transit
descriptions, mapped mounts, loop backing and live groups' executables. `Vfs::collect` removes an instance
absent from both attached and retained detached mounts. Removal drops its cache and aborts if the connection
is borrowable, otherwise it wakes both sides.

Abort clears pending, interrupt and forget queues, drops background requests, preserves completed results
and ends unanswered foreground requests with ECONNABORTED. Readers, answer and lock waiters wake. New calls
get ENOTCONN, sufficiently large device reads ENODEV, well-framed writes ENOENT and poll POLLERR.

## Dependencies (inbound / outbound)

### Inbound

- VFS, mount and syscall dispatch own walks, mounts, retry and end-of-syscall cleanup. File, pipe, poll,
  xattr and AF_UNIX dispatch route operations by node and description kind.
- MM, guest faults, remote copies, core dumps and timers request fills or writeback. Exec consumes
  snapshots. Procfs consumes image keys and mount options and lists `nodev fuse`. Inotify consumes node keys
  and syscall events.

### Outbound

- Credential/namespace and cgroup code supplies ID conversion, permission and device checks. Mount tables
  supply flags, holders and read-only state. Request waits use kernel wake objects.
- File helpers supply position guards, size limits, write exclusion and special-device dispatch. The HAL
  supplies memfds. MM supplies backing and translation invalidation. Eviction reads task, group, fd, mount
  and watch registries.

## Invariants and gotchas

- Propagate FUSE_RETRY through existence and permission checks. Finish retryable checks before mutations, or
  fill locally to preserve completed work.
- Drop borrows before waiting. Another thread can sweep or unmount meanwhile. Across a wait, retain a
  handle, path pin, guard, request reference or scanned holder. Recheck node existence after an RPC before
  indexing.
- A path pin follows its description, including open_tree and O_PATH in transit over AF_UNIX. Node eviction
  and mount collection scan different holders. Ancestry checks, getcwd and proc-exe depend on acyclic
  parents.

## Confirmed bugs / technical debt

- POSIX, OFD and flock locks are local. The guest lacks fuseblk and fusectl. The client lacks protocol
  IOCTL, POLL, COPY_FILE_RANGE, TMPFILE, READDIRPLUS and notify RETRIEVE.
- First getdents64 reads the complete directory before returning entries. Memory and initial latency grow
  with the listing.
- READDIR parsing silently ignores an incomplete trailing header (<24 bytes), leaves the next offset
  unchanged on a nonempty reply with no complete record, and advances by padded record length without
  verifying trailing padding bytes exist.
- Zero-byte `STORE` with an offset past cached EOF extends `attr.size` while the `size > 0` guard skips
  resizing live mapped backing, leaving existing mappings at the old EOF.

## Open questions

- Can normal teardown reach the borrow conflicts where device drop skips its count update or instance
  removal wakes waiters without setting the abort flag?
- Would READDIRPLUS reduce lookup traffic enough to justify its lookup accounting?

## To verify

- Compare INIT refusal, forced/interrupt waits, forget interleaving and insufficient splice capacity against
  a native Linux 6.1 daemon trace.
- Compare shared direct-I/O mapping refusal, short fault reads and partial cold-page STORE on Linux 6.1 with
  identical negotiated flags.
