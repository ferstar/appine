# kernel/unix Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/unix.md` for the paired decision history.
>
> Verified: 2026-11-26 (full mapping: 15 adversarial claims checked by independent verifier — 15 confirmed, 0 partial, 0 refuted; 6 audit refinements and confirmed debt items folded in)

## Responsibilities

`core/kernel/src/unix.rs` owns in-process AF_UNIX endpoints, queues, names, readiness, procfs
rows and cycle collection. `core/kernel/src/syscall/unix.rs` implements stream, datagram and
seqpacket operations on them. Messages carry guest data, credentials and open file descriptions.
A filesystem, including FUSE, can supply the bound node. Data still travels through Unix
channels. The shared socket layer handles guest ABI copies, dispatch and fd installation.

## Key types

| Type | Role and ownership |
| --- | --- |
| `UnixSocket` | Endpoint inside `Socket::Unix`. Owns the local address, link, own receive channel, sender state, options, timeouts, peer credentials, filesystem uid/gid and optional network namespace. `connections` counts admissions and `embryo` identifies an unaccepted end. |
| `Link` | `None`, `Listening` (backlog, FIFO of server ends, last listen credentials), `Stream` (strong rx/tx channels for stream and seqpacket) or `Dgram` (weak destination channel). Connected links retain the peer address cell. |
| `AddrRef`, `SenderState` | A shared sockaddr byte vector, empty until bind. The sender state holds the address cell, accounting mode, outstanding charge `outq`, capacity `sndbuf` and write generation `taken`. It has a wake object and can outlive the socket. |
| `Channel` | One receiving direction. Holds packet FIFO, unread payload bytes, passcred, EOF, closed, reset and arrival/drain counters. Datagram channels also track a queue limit, weak peer identity and procfs `established` state. |
| `Packet`, `Received` | Queued payload with stream offset, files, credentials and optional charged sender state (otherwise a stored source address). A read returns data, original length, source, credentials and files. |
| `Ucred` | Global pid and kernel uid/gid, encoded as three little-endian words. `UNSTAMPED` has pid zero and invalid IDs. |
| `Name`, `Names` | `Kernel::unix` maps `Path(Ino)` or `Abstract(net_id, ty, bytes)` to weak sockets. Holds the shared autobind counter and weak embryo list. |
| `SendingRights`, `PendingRights` | Strong owners of imported files before enqueue and reserved (fd, file) pairs during receive copyout. The kernel registers these in `unix_sending` and `unix_receiving`, and packet channels in `unix_inflight`, all weakly. |

## Public entry points

- `syscall::unix::{socket, pair, max_dgram_qlen, bind, listen, connect, accept, sockname,
  shutdown, setsockopt, getsockopt}` are family handlers behind the shared socket syscalls.
- `send` accepts gathered data, address, control and flags. `recv_into` writes guest iovecs and
  returns the copied count and `net::Message` metadata. `recv` returns an owned payload for
  buffered callers such as splice.
- `UnixSocket::{readiness, wake_objects, arrivals, unread, outq}` supply poll/epoll and ioctls.
  `proc_listing` formats the supplied sockets. `try_inflight_files`, `inflight_files` and
  `inflight_sockets` expose files and endpoints in transit.
- `released`, `collect`, `defer_collect`, `take_deferred`, `collect_deferred` and `is_unix_file`
  connect file release, exit and cleanup sweeps to cycle collection.

## Data flow / lifecycle

### Creation and identity

After generic flag validation, `socket_type` checks protocol (0 or `AF_UNIX`, otherwise
`EPROTONOSUPPORT`) before base type. Stream, datagram and seqpacket are supported. Raw creates
a datagram, and other base types reaching this helper give `ESOCKTNOSUPPORT`. Endpoints record
the creating task's non-initial network namespace. Datagrams snapshot its
`unix/max_dgram_qlen`, with a fallback of 10.

| Observation | Identity captured |
| --- | --- |
| Socket fd stat | `AnonTarget::Socket` starts at socket type and mode 0777. Socket and socketpair capture the creator's fsuid/fsgid. Accept assigns the accepting task's fsuid/fsgid. |
| Bound pathname | Filesystem node created at bind, with its own owner, inode and permissions. |
| Peer credentials | Tgid and effective uid/gid at socketpair, listen or stream-like connect. |
| Automatic message credentials | Tgid and real uid/gid at enqueue when stamping is enabled. |

Stream and seqpacket pairs get two oppositely oriented channels. Datagram pairs install
reciprocal weak links through `set_dgram_link`. Pair creation records credentials on both
ends. The net wrapper installs both files and closes installed fds if publishing the pair fails.

### Addresses and pathname creation

Generic sockaddr copy rejects lengths above 128 bytes with `EINVAL` before copying. Unix `parse` requires 2–110 bytes and
`AF_UNIX`. A family alone is unnamed. A leading NUL makes every supplied `sun_path` byte
significant. Other paths stop at the first NUL or end of input. A bound pathname retains
its supplied spelling with a trailing NUL. An unbound socket name contains only the family.

Bind on an already bound socket returns `EINVAL` before parsing. A live abstract name
conflicts within its socket namespace and type (`EADDRINUSE`). Family-only bind advances a
single wrapping 20-bit counter and tries NUL plus five lowercase hex digits. Candidate lookup
is scoped to namespace and type. A full pass without a free name yields `ENOSPC`. Insert
prunes stale weak registrations.

#### Filesystem bind

- `bind_addr` first calls `syscall::fuse::bind_socket`. For a FUSE parent, search and existence
  precede read-only and write/search checks. A trailing slash looks up the child, and collisions
  map to `EADDRINUSE`. The helper issues `FUSE_MKNOD` with `S_IFSOCK`, device zero and umask.
  It masks 0777 locally unless `FUSE_DONT_MASK` was negotiated. The directory lock covers the
  request, reply-type check and cache insertion. Successful creation then emits `IN_CREATE`. A weak socket pin keeps
  the node from eviction while bound. The returned `fuse::key(FuseId)` has its top two bits set.
- For store/overlay, `create_parent_at` starts at `AT_FDCWD`. An overlay checks the merged
  name, trailing slash, directory permissions and tmpfs room before copying up the parent.
  Existing names and dot entries give `EADDRINUSE`, a missing child with trailing slash
  `ENOENT`. The store checks directory search, existence, read-only mount, immutable flag
  and write/search permission. `Cred::new_owner` uses binder fsuid, setgid-parent gid or binder
  fsgid, and requested mode 0777. Without a parent default ACL the umask limits mode. With one,
  `create_in` inherits access permissions and installs an extended access ACL when needed.
  `mknod` creates `S_IFSOCK` with device zero, inheriting parent `NODUMP` and `NOATIME` flags.
  The helpers account for a tmpfs inode, replace an overlay whiteout in the creation transaction
  and emit `IN_CREATE` to the real directory and any overlay directory.

#### Peer lookup and autobind

Both bind paths register `Name::Path`. `find` follows the final symlink, checks `MAY_WRITE`
before `S_IFSOCK`, and uses the store inode, overlay's real inode or tagged FUSE key. FUSE
uses `fuse_permission`. A pseudo node undergoes a permission check then gives `ECONNREFUSED`,
as does a writable non-socket or a socket inode with no live endpoint. A different-type live
pathname peer can give `EPROTOTYPE`. Abstract lookup already includes type. Paths can cross
network namespaces and follow hard links or renames to the same inode. Endpoint close leaves
the pathname for normal unlink.

Enabling `SO_PASSCRED` alone leaves the socket unnamed. Connect can autobind before lookup
even if the named peer is missing. Addressed or connected datagram sends and connected
seqpacket sends can also autobind. When unnamed with `SO_PASSCRED` enabled, the helper rejects a family-only peer address with `EINVAL`; otherwise `find` rejects an unnamed peer if reached. Datagram
`AF_UNSPEC` disconnect bypasses autobind.

### Connections, shutdown and close

#### Listen

Datagrams give `EOPNOTSUPP`. A stream-like socket must be bound and either
unconnected or already listening (`EINVAL` otherwise). Backlog is capped at 4096 and negative
input selects the cap. Every successful listen captures the caller's credentials. Increasing
an existing backlog wakes connectors on the listener's own channel.

#### Connect

After any autobind, listening clients give `EINVAL`, connected clients
`EISCONN`. Self-connect or a same-type target that is not listening gives `ECONNREFUSED`.
`pending.len() <= backlog` admits a connection, so backlog zero admits one. A full queue
waits on the listener channel with the client's send deadline, then retries lookup and
admission. The client receive channel copies its passcred setting. The server channel starts
with passcred enabled for pre-accept messages. The embryo copies the listener address bytes
and inherits the connector's network namespace. The client retains the listener's address
cell and listen credentials. The embryo retains the client's address cell and connect
credentials. Connect enqueues the embryo, increments `connections`, wakes listener and
client, and registers the embryo weakly.

#### Accept

`net::accept4` checks fd existence and flags, then reserves an output slot before
socket validation or dequeue. Reservation failure preserves the FIFO. The listener's file
status and receive timeout govern waiting. `unix::accept` pops the oldest embryo, clears
its flag, assigns acceptor ownership, applies the listener's current passcred setting and
removes the weak registration. Other options and timeouts retain the new endpoint's defaults.
Accept wakes connectors and returns the peer's live name. The wrapper writes the requested
address before filling the fd with the requested flags. Address failure releases the slot
and drops the dequeued endpoint.

#### Datagram relink

Connect rejects a datagram target connected elsewhere with `EPERM` and
permits self-connect. `set_dgram_link` updates the link and the receive channel's weak peer,
and marks both ends established. Reconnecting to the same peer preserves the queue. Leaving
a different old peer purges local packets and returns files to release after borrows end.
An unconnected old peer loses its established bit. A distinct, open old peer pointing back
gets reset and a wake if the purge found unread records. Relink preserves any saved pair
credentials. An unaddressed send to a dead connected peer gives `ECONNREFUSED`, disconnects
if the link still matches, and leaves subsequent unaddressed sends with `ENOTCONN`.

#### Shutdown and drop

Valid shutdown directions 0–2 succeed in any state. On a stream link,
read shutdown sets receive EOF and closed, write shutdown sets transmit EOF. Both preserve
queued packets. Other links record local read EOF and/or `shut_wr`. Shutdown wakes the
endpoint's wait objects. Stream-like drop sets transmit EOF, sets peer reset when an embryo
or closing with unread packets, then closes and drains its receive direction. Listener drop
releases pending embryos. Datagram drop discards its own queue.

### Send admission and accounting

`sendmsg` gathers up to 1 MiB and 4096 control bytes. Unix `send` validates control and tracks
rights before dispatch. An explicit credential stamp wins. Otherwise the sender's tgid and
real IDs are stamped when either end requests credentials, including a server awaiting accept.
Other packets carry `Ucred::UNSTAMPED`. The sender address resolves from `SenderState` on
receive. A later bind is visible, and sender close preserves the last address. An unbound
packet source can be empty.

| Type | Send behavior |
| --- | --- |
| Stream | A destination gives `EISCONN` on a connected socket or `EOPNOTSUPP` otherwise. Without a link or address, send gives `ENOTCONN`. EOF/closed gives `EPIPE` and raises `SIGPIPE` unless `MSG_NOSIGNAL` is set. Partial progress wins. Enqueue chunks up to `clamp((sndbuf >> 1) - 64, 1, 36544)` with saturating subtraction. The first chunk takes the files. A zero-byte send carrying files requires room and queues a charged packet. A zero-byte send without files returns zero after the link and shutdown checks. |
| Seqpacket | Ignores a supplied destination. Checks transmit EOF/closed (`EPIPE` without a signal) before record size (`EMSGSIZE`). Enqueues one whole record. Both stream-link types wait on transmit channel and sender state. |
| Datagram | After autobind, local write shutdown gives `EPIPE`. An explicit destination overrides the connected peer. Each path checks the peer restriction (`EPERM`) during destination selection. The size check (`EMSGSIZE`) precedes the queue loop's closure/read-shutdown checks. Read shutdown gives `EPIPE` without a signal. Waits on destination channel and sender state. |

Datagram and seqpacket maximum record payload is `sndbuf.saturating_sub(32)`. Datagram
`holds_back` tests **existing** packet count strictly greater than `max_qlen` and exempts a
receiver connected back to the sender. A limit of ten can therefore admit eleven before a
further send waits. That exemption only affects the record-count test. The byte charge belongs
to each sender across all destinations. Receiver `SO_RCVBUF` does not bound these queues.

`SenderState` charges by modeled allocation size (truesize). The linear charge is 256 plus the
next power of two at least as large as the head payload plus 320. Datagram and seqpacket use
a linear head through 16,064 payload bytes. Above that, they charge a 4,352-byte head plus
the excess over 3,776 rounded up to 4 KiB. For stream chunks, round the excess over 3,776 up
to 4 KiB first, subtract that paged portion from the payload with saturation, then charge the
remaining linear head plus the paged portion. Thus a zero-byte packet costs 768 bytes and a
3,777-byte payload costs 4,864 as a stream chunk or 8,448 as a record.

Allocation tests `outq < sndbuf` **before** charging the next packet. The final allocation
may take `outq` above capacity. Charged packets keep their original charge through peeks and
partial stream reads. On record removal `read_packet` explicitly uncharges before moving the
payload out. On a stream pop or discard, `Packet::drop` releases the charge. Taking its
optional sender state makes this a one-time operation.

### Receive, record boundaries and waits

`recvfrom`, `recvmsg` and socket reads use the iovec path. `recv_into` caps the aggregate
payload at 1 MiB. `rx_channel` rejects a listener with `EINVAL` and an unconnected
stream-like socket with `ENOTCONN`. Stream splice into a pipe uses buffered `recv` with no
control space. Its nonblocking decision includes the input file status and the splice/output
pipe nonblocking condition. Generic stream writes can iterate over 1 MiB pieces.

#### Stream

A non-peek receive targets the full capped request under `MSG_WAITALL`, otherwise
`min(SO_RCVLOWAT, request)`. A peek can return immediately on available data below either
threshold. On an empty queue it still observes reset, EOF and ordinary blocking rules.
`read_stream` merges packets in order, uses the first packet's source and stops after
encountering files, including when only part of that packet's payload fits. With credential
checking active it also stops before a different stamp. A waiting read can begin checking
when passcred turns on. Once it has taken a stamp it preserves that boundary across waits
and subsequent option changes. The receive channel's setting at finish controls whether
the credential cmsg is emitted.

#### Copy and consume

Stream receive previews the data and files, then `scatter_partial`
writes to the guest. The consuming pass uses the copied prefix on a short copy. A copy
error before progress returns the error and leaves the data queued. A short copy, rights,
a credential boundary, reset, EOF or interrupted/unsatisfied wait can finish a read below
its target. A partial result leaves a pending reset for a later read. Once queued stream
data is exhausted, reset precedes EOF. Stream `MSG_TRUNC` adds no discarded length.

#### Zero length and peek

A zero-length consuming stream receive on a nonempty queue can
detach the head's files while retaining its unread payload and charge. A rights-only head
is popped even when zero bytes were copied, waking the channel and uncharging the sender.
Peeking clones the files, preserving the packet and its charge. Scalar zero-length read
and zero-total-length readv return before Unix receive and leave head files attached.

#### Records

Datagram and seqpacket receive check and consume reset before looking at the
head. Otherwise they preview one record, including an empty one. A successful short receive
sets output `MSG_TRUNC` and consumes the whole record unless peeking. Input `MSG_TRUNC`
selects its original length as the syscall return value. A non-peek guest payload-copy fault
drops that record and releases its files, except that `FUSE_RETRY` leaves it queued. A
faulting peek also leaves it queued. Neither record type waits for `SO_RCVLOWAT`.

`block_on` checks nonblocking mode or an elapsed deadline first (`EAGAIN`). A pending signal
then yields `EINTR` with a socket deadline or `ERESTARTSYS` without one. Otherwise it waits
via `Cx::wait_objects`. Accept/receive use the receive timeout, connect/send the send timeout.
The operation keeps its deadline over wakeups. A stream operation that already moved data can
return that count on interruption or timeout.

### Ancillary messages and descriptor delivery

`rights` walks complete 16-byte cmsg headers at eight-byte aligned offsets. It validates each
length before skipping a non-socket level, ignores a short trailing header and rejects an
unknown `SOL_SOCKET` type with `EINVAL`. For `SCM_RIGHTS`, complete four-byte numbers across
all headers count toward a maximum of 253. Exceeding that count gives `EINVAL`. An invalid descriptor
gives `EBADF`. Multiple occurrences retain multiple file references.

`SCM_CREDENTIALS` requires a 28-byte cmsg. Unmapped uid/gid give `EINVAL`. The declared pid
must be the caller's visible tgid or authorized by `CAP_SYS_ADMIN` over the pid namespace's
owner. Real, effective or saved uid/gid match directly, otherwise `CAP_SETUID` or `CAP_SETGID`
is required in the sender's user namespace. Failed authorization gives `EPERM`. The subsequent
positive-pid and task lookup can give `ESRCH`. The last valid credential declaration supplies
the stamp.

1. `SendingRights::track` registers a nonempty import weakly. The send keeps the strong owner
   while blocked. At enqueue `into_packet` registers the destination channel once and moves
   the file vector into the packet. A failed send that still owns files schedules deferred GC.
2. On receive, `deliver` encodes credentials first if requested, using the receiving
   namespaces. An unstamped packet then reports pid zero and overflow uid/gid (65534). `put_cmsg`
   requires room for a header and can shorten a credential payload with `MSG_CTRUNC`.
   Remaining room after a rights header determines how many whole fd numbers fit. NULL
   control means zero room regardless of `msg_controllen`. The receiver reserves the lowest
   available numbers until it runs out of room or a reservation fails. It returns the prefix,
   releases the excess files and sets `MSG_CTRUNC`. Payload can succeed even with no free fds.
3. `PendingRights` holds the reserved numbers and files. The net wrapper copies the requested
   source address and control, writes back lengths and flags, then calls `install` with
   `MSG_CMSG_CLOEXEC`. Result-copy retries during FUSE fills keep the reservations and do not
   repeat a completed receive. A control write failure drops the reservations and reports
   zero control length and `MSG_CTRUNC` if the header writes succeed. An address or header
   error also unwinds pending delivery. On cancellation, `Drop` unreserves slots in the
   original table if it still exists and defers collection when holding a Unix socket.

Both `accept4` and ancillary delivery use `Cx::reserve_fd`. The search range ends at the
allocating process's soft `RLIMIT_NOFILE`, including when its table is shared. Occupied and
reserved slots are skipped. `FdTable::grow_for` checks `nr_open` when the selected fd requires
expansion. Already allocated table capacity remains usable after `nr_open` is lowered. Growth
uses powers of two bounded by the 64-slot-rounded ceiling. Filling a reservation does not
recheck the limits. Accept propagates a reservation error before dequeue, while ancillary
delivery converts it into truncation.

### Socket options, readiness and observation

Unix handlers accept `SOL_SOCKET` and reject other levels with `EOPNOTSUPP`. Net normalizes
the new timeout names. Unix uses the common timeval parser. `SO_PASSCRED` lives on the receive
channel and `SO_ERROR` consumes its reset bit once. `SO_ACCEPTCONN`, `SO_TYPE`, `SO_PROTOCOL`
(zero) and `SO_DOMAIN` describe the endpoint. `SO_PEERCRED` translates the most recent listen
credentials or the saved peer credentials for the querying task. Without a saved peer it
returns pid zero and both IDs all ones. `getpeername` returns `ENOTCONN` on unconnected and listening endpoints.

The initial send buffer is 212,992 bytes. Plain `SO_SNDBUF` caps the unsigned input at
212,992, doubles it and applies a 4,608-byte floor. `SO_SNDBUFFORCE` requires `CAP_NET_ADMIN`
in the initial user namespace, floors negative input at zero and bypasses the ordinary cap.
Both limit before doubling to avoid signed overflow and update live sender capacity on success.
Growth wakes writers. Shrinkage leaves queued charges intact. `SO_RCVLOWAT` maps zero to one
and negative values to the largest signed int. Receive-buffer size, keepalive and linger are
recorded settings. Read-only integer options check size (`EINVAL`) before `ENOPROTOOPT`.
Unknown socket-level options fall through on set and give `ENOPROTOOPT` on get.

- `FIONREAD` reports all unread payload for connected stream/seqpacket, the first datagram's
  size, or `EINVAL` on a listener. An empty record can be readable with zero bytes.
  `SIOCOUTQ` sums this sender's full outstanding packet charges.
- Listeners report readability only for pending accepts. Unconnected stream/seqpacket report
  writable plus hangup. A queued packet is readable regardless of receive low-water. EOF
  adds readability and `POLLRDHUP`, plus `POLLHUP` when the write direction is shut.
  Pending reset gives `POLLERR` until receive or `SO_ERROR` consumes it.
- Poll write readiness for connected stream/seqpacket and all datagrams requires
  `(outq + 1) * 4 <= sndbuf` (saturating arithmetic). Connected datagrams also check
  `holds_back` on a live peer. Every uncharge or capacity increase wakes the sender object.
  Sender `taken` advances on each such notification with a writable result, including when
  already writable. A writer can regain admission before `POLLOUT` returns.
- Wait objects are the socket's own channel and sender state, plus both stream-link channels or a live
  datagram destination. Listener generations are `[connections, 0]`. Other sockets use receive
  arrivals and sender `taken`. Connected datagrams add the live peer channel's `taken` to the
  write generation. The channel `taken` counter advances only on datagram channels (`max_qlen` set): per datagram dequeue and once per nonempty relink purge.

### In-flight snapshots and `/proc/net/unix`

`collect_inflight` visits live pending sends, pending receives and every packet in registered
channels. File snapshots preserve repeated occurrences. `try_inflight_files` returns `None`
for the whole snapshot when a channel cannot be borrowed. `inflight_files` expects those
borrows to succeed. `inflight_sockets` selects every socket-kind file in the transfer,
including other socket families, expects the same channel availability, then appends live
weak embryos. It can do this without borrowing any socket's contents.

`pseudo::sockets` walks distinct task fd tables, merges the snapshot, deduplicates socket
identities and sorts by synthetic `socket_ino`. The proc reader then filters by the network
namespace pinned on the proc node. `proc_listing` formats only AF_UNIX endpoints. A listener
has state 1 and flags 0x10000, an ordinary unconnected socket state 1, a stream link state 3,
and a datagram (`Link::Dgram` or `Link::None`, including a one-way target) state 3 when its
receive channel is marked established. A pending embryo is
a separate state-2 row with displayed inode zero and the listener's address, visible in the
connector's namespace even for a cross-namespace pathname connection. After accept its
ordinary row uses the socket inode. Abstract names display their leading and interior NULs
as `@`. The Unix formatter ignores the uid carried in its input tuple.

### Cycle collection

`collect_once` prunes dead channels and channels without queued rights. It counts queued
references per `OpenFile` and holds one temporary reference per distinct file. A Unix socket
file is a candidate if its strong count equals its queued occurrences plus one. Fd slots,
blocked syscalls and pending sends or deliveries holding the file keep it outside that set.

For each candidate, `inboxes` includes the socket's own channel and its connected receive
direction, or the receive directions of pending embryos for a listener. The collector subtracts
edges within the candidate set, including self-edges. A positive remaining
count supplies a root. It marks these roots and all candidates reachable through their inboxes,
then takes packets from the unmarked inboxes and zeros their byte counts. After releasing
borrows it drops the temporary file map, then the extracted packets. Collection repeats if
garbage was freed, another sweep was requested and registered channels remain.

`released` drops the supplied file references before collecting on a Unix release or deferred
work. Socket drop defers GC for discarded rights or pending embryos. AIO completion can set
the same thread-local flag. Post-syscall sweeps and AIO poll checks drain it. After releasing
task resources, exit collects if registered channels remain and this was the last thread,
the fd table went away, or a deferred sweep was requested.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/net` dispatches socket syscalls, publishes accepted fds and installs received rights.
  `kernel/fileio` uses its scalar/vectored paths and `kernel/pipe` its buffered stream receive.
- `kernel/poll` consumes readiness, objects and generations. Unconnected Unix links and
  datagram links are dynamic epoll interests. `kernel/aio` holds poll files across waits and
  releases them on cancellation, completion or teardown.
- `kernel/files` releases references on close, range close, dup replacement and the seccomp
  addfd `close_replaced` path. Exec releases close-on-exec fds. Task exit and syscall-end
  sweeps service GC. `kernel/procfs` merges and formats socket snapshots.
- `fs::opens_of`, `file_in_use` and `live_store_inodes` include transferred files in open-file
  rehoming and inode lifetime checks. Mount `try_in_use` propagates a failed snapshot so
  `collect_now` can defer cleanup. `sb_busy` checks transferred unlinked files on read-only remount. `release_anon_ns`
  includes open-tree files in transit.

### Outbound

- VFS, store, overlay, FUSE and inotify supply path resolution, node creation, inherited
  metadata, cache pins, tmpfs accounting and events. Procfs provides pseudo-node stat and
  the synthetic socket inode.
- `kernel/net` supplies `Socket`, `SockOpts`, control encoding, timeval parsing, guest scatter
  and stream `epipe`. `kernel/netns` provides the namespace datagram queue setting.
- The fd table supplies file references, slots and reservation operations. Credentials and
  namespaces supply IDs and permission checks. `Cx` supplies fd limits, interruption and
  object waits. Channels and sender states request scheduler wakes.

## Invariants and gotchas

- The BKL serializes `Rc` and `RefCell` access. Waits and guest page fills can let other tasks
  run. `net::socket_of` pins the open file through a blocking syscall. Descriptor reservations
  survive result-copy retries.
- Namespace, serialized name, pathname inode and anonymous socket inode answer different
  lookups. Procfs even sorts embryos by socket identity while displaying inode zero.
- A partial read can reduce `FIONREAD` while `SIOCOUTQ` is unchanged. Detaching files and
  releasing a sender charge are separate operations.
- Extract queued packets before dropping their files. A nested drop can borrow another
  channel. `OpenFile::drop` skips the generic wake for a socket it can identify as Unix. The
  endpoint wakes its own objects. A stream inbox emptied by GC contributes no unread-data
  reset when its endpoint later drops.

## Confirmed bugs / technical debt

- The namespace's writable `net.core.somaxconn` value can differ from the backlog cap actually
  used by AF_UNIX. The syscall forwards its raw backlog and the Unix listener clamps to a
  fixed 4096.
- Initial-namespace writes update `net.core.wmem_default` and `net.core.wmem_max`, while AF_UNIX
  initial sender capacity and plain `SO_SNDBUF` capping still use fixed 212,992-byte constants.
- A blocked datagram send checks local write shutdown (`shut_wr`) and peer eligibility
  (`may_receive_from` / `EPERM`) only before its retry loop; after waking, it rechecks only
  receiver closure, receiver EOF, sender buffer room and `holds_back`, so concurrent local
  shutdown or target reconnection can slip past those checks.
- The Unix proc formatter prints a fixed `RefCount` of two on every row. The display cannot
  supply a live reference count.
- The `pseudo::sockets` doc comment excludes pending accepts. Its call to
  `inflight_sockets` includes the registered embryos.

## Open questions

None.

## To verify

- Exact correspondence to arm64 Linux 6.1/6.8 source remains open for the allocation-size
  model, send limits, poll and receive low-water behavior, receive-time addresses and seqpacket signals. The
  formulas and transitions above describe this implementation.
