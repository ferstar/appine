# kernel/netlink Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/netlink.md` for the paired decision history; decisions from before the split are D3, D24, D26 and the netlink halves of D19 and D23 in `.agents/decisions/kernel/net.md`.
>
> Verified: 2026-11-26 (full refresh: 4 research concerns; 15 claims checked by an independent verifier — 15 confirmed, 0 partial, 0 refuted, plus 5 map-audit refinements and 2 additional confirmed bugs folded in)

## Responsibilities

The guest's `AF_NETLINK` transport runs in process. It assigns ports, dispatches kernel requests,
delivers peer datagrams and queues replies, resumable dumps and notifications.

In `core/kernel/src/net.rs` the module owns the netlink socket, rtnetlink notifications, generic
netlink, message encoding and socket diagnostics sections. In `core/kernel/src/syscall/net.rs` it owns
the `netlink_*` functions, the netlink section through `KernelHost` and the netlink arms of socket
operations. Shared syscall dispatch, ioctl implementation and the `net.rs` unit-test module belong to
`kernel/net`.

## Key types

- `NetlinkSocket` holds protocol/type, local and connected addresses, creation namespace, inode owner,
  memberships, options and timeouts. Its FIFO, `queued_bytes`, `arrivals` and `overrun` govern receive
  and readiness.
- `NetlinkItem` holds an atomic `Datagram(bytes, group)` or a resumable `Dump(NetlinkDump)`. The latter
  contains pre-encoded `DumpMessages` or `nft::SetElemDump`.
- `Kernel::rtnl_sockets` holds weak references to sockets of all five protocols. Port lookup, listener
  discovery and both multicast broadcasters share it.
- `KernelHost` implements `Host` with the socket namespace, requester credentials, PID namespace, fds,
  TGID and command. `Link` represents a base interface or a virtual device with an optional peer
  namespace ID.
- `Snapshot` stores encoded link, address and route bodies. `NotifyCause` carries request type, flags,
  sequence and sender port. `RtnlNote` carries group, message and echo status. `ConfigureResult` pairs
  a device-layer result with sender echoes. nftables' `HandleResult` separates sender items from
  committed `NftNote`s.

## Public entry points

- The socket syscalls call `netlink_bind`, `netlink_connect`, `netlink_setsockopt`,
  `netlink_getsockopt` and `netlink_send`. Receive calls `NetlinkSocket::dequeue`. Producers use
  `enqueue` for datagrams and `enqueue_item` for either item type.
- `NetlinkSocket::handle` dispatches kernel requests. `rtnl_notify` and `nft_notify` broadcast changes.
- `configure_host_link` checks changes to existing host links. `message` and `attr` encode 16-byte and
  four-byte headers respectively, with little-endian fields, lengths before padding and four-byte
  alignment.

## Data flow / lifecycle

### Creation, port ownership and names

`socket` accepts `SOCK_RAW` or `SOCK_DGRAM` with `NETLINK_ROUTE`, `NETLINK_GENERIC`,
`NETLINK_NETFILTER`, `NETLINK_SOCK_DIAG` or `NETLINK_KOBJECT_UEVENT`. Unknown type flags give `EINVAL`,
unsupported base types `ESOCKTNOSUPPORT`, and unsupported protocols on valid types `EPROTONOSUPPORT`.
Creation sets filesystem UID/GID and socket namespace (`None` for boot), prunes dead registry entries
and registers the new socket. Local port and peer start at zero. The netlink branches of listen,
accept4, shutdown and socketpair return `EOPNOTSUPP`. `FIONREAD` gives `ENOTTY`.

Bind requires a 12-byte or longer `AF_NETLINK` address. An explicit port already held in the same
network namespace and protocol gives `EADDRINUSE`. Rebinding requires the exact assigned port, even
when requesting zero (`EINVAL` on mismatch). First bind with port zero and `netlink_autobind` on
connect/send use `netlink_alloc_pid`. It tries the caller's nonzero TGID as seen in its PID namespace,
then a shared descending atomic counter starting at −4097. Each candidate must be free in the socket's
namespace and protocol. Lookup skips self, dead references and sockets it cannot borrow.

Bind replaces the low 32 membership bits, preserving upper groups. A nonzero NETFILTER mask and ROUTE
groups 30/31 require `CAP_NET_ADMIN` over the socket namespace's owner. A fresh zero mask leaves the
set unallocated. `getsockname` reports local port and low 32 bits.

A two-byte `AF_UNSPEC` disconnect clears the peer and leaves local state intact. Otherwise
`netlink_dst` checks length/family (`EINVAL`), selects the lowest group bit (one-based), and requires
namespace-owner `CAP_NET_ADMIN` for any nonzero port or group. Connect autobinds and records the pair.
Peer existence is checked at unicast send. `getpeername` reports the stored port and single-bit group
mask, including the initial or disconnected zero pair.

### Socket options

`SOL_SOCKET` reports type, protocol, domain, `SO_ACCEPTCONN` (0), `SO_PEERCRED` (0, −1, −1 even after
connect), `SO_PASSCRED` and timeouts. Reading `SO_ERROR` takes a pending overrun as positive `ENOBUFS`,
otherwise zero. Setting PASSCRED needs four bytes. Other names go through `SockOpts`, including its
length, privilege and read-only checks. Unrecognized setters succeed and unrecognized getters give
`ENOPROTOOPT`.

Old and new timeout names use the shared timeval parser. All-zero input or values at least `KTIME_MAX`
mean no deadline, whereas negative seconds with valid microseconds store a zero duration so an empty
receive expires immediately (both read back as zero). `SO_RCVTIMEO` bounds empty receives. `SO_SNDTIMEO`
is recorded. `SO_RCVBUF` defaults to 212,992 bytes. The ordinary setter caps the unsigned request at 212,992 before doubling and applying a
2,304-byte floor.

`SOL_NETLINK` setters use the first four bytes or zero for a shorter value. Unknown names and
unsupported levels give `ENOPROTOOPT`.

- `NETLINK_ADD_MEMBERSHIP` and `NETLINK_DROP_MEMBERSHIP` check protocol permission, allocate the set,
  then validate the group (1–36 for ROUTE, 1–32 otherwise). Both need namespace-owner NET_ADMIN on
  NETFILTER. Only ADD has the extra admin check for ROUTE groups 30/31.
- `NETLINK_LIST_MEMBERSHIPS` writes whole 32-bit words that fit. It reports zero bytes before
  allocation and the full allocated size thereafter (8 bytes for ROUTE, 4 otherwise), even when empty
  or with zero output room. An out-of-range group returns `EINVAL` after allocation.
- `NETLINK_PKTINFO`, `NETLINK_NO_ENOBUFS`, `NETLINK_CAP_ACK`, `NETLINK_EXT_ACK` and
  `NETLINK_GET_STRICT_CHK` control receive metadata, overruns, ACKs and rtnetlink dump checks.
  `NETLINK_BROADCAST_ERROR` and `NETLINK_LISTEN_ALL_NSID` are stored for readback. Setting or clearing
  the latter requires namespace-owner `CAP_NET_BROADCAST`. All seven flag getters need four bytes of
  room or return `EINVAL`.

### Send and destination selection

`netlink_send` validates a nonempty destination with `netlink_dst`, or reuses the connected pair
without another capability check. After autobind it selects the first applicable path.

- **Group nonzero.** Consume the send and report its length, even when a port is also specified.
- **Port nonzero.** Enqueue the original bytes as a group-zero datagram, directly for self-unicast or
  on the matching peer. A missing peer gives `ECONNREFUSED`. A full peer queue can drop the datagram
  while the sender still reports its length and requests a peer wake.
- **Port and group zero.** ROUTE, GENERIC and SOCK_DIAG walk messages on four-byte boundaries. A short
  header, length below 16 or length past the end stops the batch after any valid prefix. NETFILTER
  receives the entire datagram. KOBJECT_UEVENT silently consumes kernel-directed data. Its socket can
  still receive peer bytes and message-queue cookies.

Sendto/sendmsg import up to 1 MiB. `sendmsg` still copies up to 4,096 control bytes from guest memory
before dispatch (so an unreadable control buffer can fault), after which netlink ignores send flags and
ancillary data.

### Receive queue, dumps and readiness

`enqueue_item` charges **one 256-byte overhead per FIFO item**, plus datagram length or estimated
remaining dump bytes. An empty FIFO admits its first item even above `SO_RCVBUF`. Empty dumps are
skipped, zero-byte datagrams queued. On an occupied queue an over-budget item is dropped whole and sets
one overrun latch unless `NETLINK_NO_ENOBUFS` is enabled. Enabling the flag leaves earlier overruns
pending. Successful insertion increments `arrivals`.

| Front item | What one receive selects |
| --- | --- |
| `Datagram` | The entire payload and stored group. Peek clones it. Consumption removes its complete charge. |
| `DumpMessages` | Consecutive complete messages through the current section end, targeting `min(max, 32768)` bytes. Always include the first message even if it exceeds the target. |
| `nft::SetElemDump` | One multipart `NFT_MSG_NEWSETELEM` targeting `min(max, 3776)`, with at least one remaining element. After the elements, the empty `NEWSETELEM` and `NLMSG_DONE` share a receive if both fit. |

Dump reads return group zero. Partial consumption releases the decrease in remaining bytes and
increments `arrivals` for the next `EPOLLET` read edge. Finishing releases the last bytes and overhead.
Peek preserves cursor, charge and arrivals. nftables counts remaining aligned element attributes, empty
tail and 20-byte DONE. Chunk wrappers are built on receive. The query captures static and live dynamic
elements.

Receive takes pending `ENOBUFS` before data, including on peek. It copies what fits and sets output
`MSG_TRUNC` when the selected datagram exceeds the buffer. Input `MSG_TRUNC` requests the full selected
length as the syscall result. Otherwise the result is the copied length. A zero-byte
`MSG_PEEK | MSG_TRUNC` sizes the first atomic unit without consuming it. A later, larger receive can
pack more dump entries. Consuming a truncated unit preserves subsequent entries.

The source address always has PID zero, plus a group mask for groups 1–32. `NETLINK_PKTINFO` supplies
the group number first. `SO_PASSCRED` then supplies `SCM_CREDENTIALS` with PID zero and kernel UID/GID
zero mapped into the receiver's user namespace. Peer unicast uses the same metadata. `put_cmsg` omits
or shortens control messages when space runs out and sets `MSG_CTRUNC`.

An empty queue gives `EAGAIN` on a nonblocking fd or with `MSG_DONTWAIT` or `MSG_ERRQUEUE`. The latter
still reads the ordinary FIFO. A blocking receive fixes the deadline on its first empty read, checks
expiry (`EAGAIN`) and interruption (`ERESTARTSYS`), then waits on the socket wake object and retries.
Readiness includes `POLLOUT` always, `POLLIN` with queued items and `POLLERR` with an overrun. Poll
sees no host fd and reads the edge count as `[s.arrivals, 0]`.

### Namespace views and configuration

`KernelHost::links` combines `netdev::base` and virtual `views`, sorted by interface index. Enumerating
a virtual link with a peer in another namespace calls `nsid_alloc` for the lowest free ID if needed.
Single lookups and snapshots can allocate an ID this way.

| Interface | Sources for the encoded link |
| --- | --- |
| Boot namespace, host interface | HAL `getifaddrs` provides names, flags and addresses, `if_nametoindex` the index, six-byte `AF_PACKET` addresses the MAC (zero fallback). MTU is fixed at 65,536 for loopback or 1,500 otherwise. Link statistics use guest traffic counters. Host sysfs supplies transmit queue length with a 1,000 fallback. |
| New namespace, base loopback | `netns::lo_interface` supplies flags, addresses and configured MTU (default 65,536). MAC and statistics are zero. Transmit queue length is 1,000. |
| Virtual device in either namespace | `netdev::link_message` encodes the view's MAC, MTU, configurable transmit queue length and optional peer namespace ID, with zero statistics. `netdev::addr_messages` supplies addresses. |

`netdev::tables` merges virtual-device routes with boot host routes (kernel-supplied, HAL or
synthesized) or private loopback routes.

### NETLINK_ROUTE requests and acknowledgements

ROUTE skips messages without `NLM_F_REQUEST` and types below `NLMSG_MIN_TYPE`, ACKing success only if
asked. For requests, `KernelHost::configure` calls `netdev::request` first. It recognizes
NEW/DEL/SETLINK, NEW/DELADDR and NEW/DELROUTE and checks namespace-owner `CAP_NET_ADMIN` before
dispatch. On an existing host link, `configure_host_link` accepts supported no-op name, MTU and flag
settings. Actual host changes are unsupported. Private loopback and virtual devices have separate
change handlers. An unimplemented non-GET type in the 6.1 RTM range (16–123) gives `EPERM` without
admin. Other unsupported cases give `EOPNOTSUPP`.

Single-query data and successful change echoes precede an optional, separate success ACK. `NLMSG_ERROR`
carries zero or negative errno, request sequence and socket port. Success or `NETLINK_CAP_ACK` sets
`NLM_F_CAPPED` and attaches only the 16-byte request header. Other errors include the whole request.
EXT_ACK adds `NLM_F_ACK_TLVS` only when an explanation or offset is available. Supported rtnetlink dump
errors travel in multipart `NLMSG_DONE` with negative errno and optional `NLMSGERR_ATTR_MSG`, without a
copy of the request.

### Rtnetlink queries and strict checks

Either bit of `NLM_F_DUMP` selects a supported ROUTE dump. Links and rules each occupy one data
section. Addresses and routes use one section per selected IP family, IPv4 first. DONE has its own
section. Single queries have their own validators. The permissive attribute walker stops at an invalid
length. The strict walker rejects invalid lengths and leftover bytes.

- **Links.** Dumps filter by nonzero master and nested link kind. Strict mode needs a 16-byte
  `ifinfomsg` with bytes after family zero. It allows `IFLA_MASTER`, `IFLA_LINKINFO`, `IFLA_EXT_MASK`
  and `IFLA_TARGET_NETNSID`. The last two validate u32 size only. Master/kind filters mark data
  `NLM_F_DUMP_FILTERED` and leave DONE unmarked. Single GETLINK validates the full header and
  recognized attribute sizes. Positive index precedes name. Missing selector gives `EINVAL`, bad
  attribute size `ERANGE`, absent device `ENODEV`.
- **Addresses.** Strict dumps need an eight-byte `ifaddrmsg` with zero prefix length, flags and scope.
  Only a four-byte `IFA_TARGET_NETNSID` attribute is accepted. A nonzero index marks data and DONE
  filtered, with `-ENODEV` on DONE for an unknown index. Single GETADDR gives `EOPNOTSUPP` outside
  IPv6. It needs the header and at least 16 bytes per supplied address. `IFA_LOCAL` precedes
  `IFA_ADDRESS`. A nonzero index scopes link-local lookup. Missing address gives `EINVAL`, short
  address `ERANGE`, no match `EADDRNOTAVAIL`.
- **Routes.** A strict dump needs a 12-byte `rtmsg` with zero destination/source lengths, TOS and
  scope. Table, protocol, type and `RTA_OIF` filter entries. `RTA_TABLE` overrides the header table.
  Both attributes need four bytes. The only accepted flag, `RTM_F_CLONED`, selects an empty result. An
  unknown OIF ends with unfiltered `-ENODEV`. A missing table for a specified family ends with filtered
  `-ENOENT` (main/local always exist for this check). All strict route data records carry
  `NLM_F_DUMP_FILTERED`. Successful DONE carries it only for an active filter.
- **Single route.** `route::get` uses namespace tables and interfaces, real UID, boot-namespace status,
  and input-interface forwarding and `route_localnet` settings. IPv4/IPv6 need a 12-byte route header.
  Long source/destination attributes use their leading bytes. Short IPv4 `RTA_DST`/`RTA_SRC` give
  `ERANGE` and short IPv6 `RTA_DST`/`RTA_SRC` give `EINVAL`, while short `RTA_GATEWAY`/`RTA_PREFSRC`
  give `ERANGE` in both families. Short `RTA_IIF` gives `ERANGE` in either family, short `RTA_FLOW` in
  IPv4.
  Nonzero IIF selects input lookup.
- **Rules.** Strict mode requires a 12-byte `fib_rule_hdr`, all bytes after family zero, and no
  attributes. `route::rule_messages` supplies default rules. A family without rules ends with
  `-EAFNOSUPPORT` on DONE.
- **Namespace IDs.** GETNSID reads four leading bytes per recognized `NETNSA_*` attribute (`ERANGE` if
  short). PID takes priority over FD. `KernelHost::net_of` resolves a process in the caller's PID
  namespace or a network-namespace fd. `netdev::nsid` reads the assigned ID. An unassigned peer is
  reported as −1. Lookup by number alone gives `ENOENT`, missing reference `EINVAL`, and TARGET_NSID
  after successful resolution `EINVAL`.
- **Neighbours.** A valid dump returns only DONE. Strict mode checks a 12-byte `ndmsg`, zero fields
  after family except `NTF_PROXY`, and four-byte `NDA_IFINDEX`, `NDA_MASTER` or `NDA_FLAGS_EXT`
  attributes. Single GETNEIGH checks full header, zero state/type, presence of `NDA_DST`, then family
  and exact address length (4 or 16). An unsupported family gives `EAFNOSUPPORT`, negative or unknown
  index `ENODEV`, zero index without proxy `EINVAL`. A valid lookup gives `ENOENT`.

### Rtnetlink notifications

On a kernel-directed ROUTE send, `rtnl_watched` deduplicates namespaces with ROUTE members of groups 1
(link), 5 (IPv4 address), 7 (IPv4 route), 9 (IPv6 address) or 11 (IPv6 route). It checks the borrowed
sender directly and skips other sockets it cannot borrow. `KernelHost::configure` calls
`Snapshot::take` via `snapshots` and `changed_since` around each supported change in `netdev::request`,
capturing partial changes on error and effects in peer namespaces. An address or route request with
`NLM_F_ECHO` adds the socket's namespace even without a subscriber.

`Snapshot::changes` emits new/changed links, removed then new addresses, removed then new routes, and
finally removed links. It matches links by ifindex, ignoring both statistics attributes. Address
identity uses family, prefix, index, address and local address. Route identity uses family, destination
length, TOS, header table and destination/priority/table attributes. A changed body with surviving
identity emits NEW alone.

| Notification | Linux 6.1 header and echo model |
| --- | --- |
| Link, IPv6 address or indirect change (e.g. routes added by NEWADDR) | Zero sequence and port, no echo. |
| IPv4 address or either-family route whose NEW/DEL type matches the request | Request sequence and sender socket port. ECHO marks the note. On success sender-namespace echoes retain their multicast group and precede the optional ACK, even without membership. |
| NEWROUTE flags | Direct `NLM_F_REPLACE` with a prior identity sets REPLACE. Otherwise direct `NLM_F_APPEND` sets CREATE\|APPEND. All other new routes carry `NLM_F_CREATE` with `NLM_F_EXCL`. Link, address and DELROUTE flags are zero. |

`rtnl_notify` broadcasts by namespace and membership, skipping the sender for notes marked echo.
Unmarked link events can still reach a subscribed sender. Two callers use a zeroed `NotifyCause`.

- The socket ioctl wrapper checks the 15 `CHANGING_IOCTLS` (interface settings, address changes, route
  add/delete). With listeners, it snapshots watched namespaces around `socket_ioctl` and emits
  differences before returning the result.
- `write_proc` matches the IPv6 `disable_ipv6` NetConf node. `rtnl_snapshot_ns` snapshots only its
  pinned namespace if it has a notified-group subscriber. After `pseudo::write`, before propagating its
  result, `rtnl_notify_ns` diffs that namespace. IPv6 address and route events therefore have zero
  sequence and port.

### nftables requests and notifications

`KernelHost::netfilter` calls `nft::handle_with_notes` with the namespace, credentials and `Peer`
(socket port, CAP/EXT ACK, PID-namespace TGID and process name). It returns sender items to
`NetlinkSocket::handle` and saves committed notes. `nft::handle` is the replies-only wrapper. Even
queries require namespace-owner `CAP_NET_ADMIN`. Without it only the first parseable message gets an
`EPERM` ACK.

nftables stages each batch against a copy. A missing batch end discards the unfinished batch. A failed
batch returns accumulated ACKs and no events. On commit with events, `Events::finish` appends
`NFT_MSG_NEWGEN` with generation and requester identity. Events use the low 16 generation bits for
`res_id`. NEWGEN echoes if the batch begin or any staged event requested echo. Sender echoes are
group-zero datagrams preceding the batch's ACKs. nftables encodes its own capped ACKs and optional
attribute offsets.

A boot-namespace ruleset generation change across the datagram calls `forward_all` to reconsider
boot-namespace internet sockets. After the sender borrow ends, `netlink_send` wakes the sender and
`nft_notify` broadcasts the saved events to same-namespace NETFILTER members of `NFNLGRP_NFTABLES`
(group 7). Notes marked echo skip the sender.

### Generic netlink and socket diagnostics

`generic_request` requires both dump bits for a dump. Its request/control gate matches ROUTE. An
unknown family type gives `ENOENT`, a short controller header `EINVAL`, and an unsupported command
`EOPNOTSUPP`. The Linux 6.8 controller model advertises ID 16, version 2, header size and maxattr zero,
two operations and the `notify` group.

- GETFAMILY dumps enumerate `nlctrl` without parsing selectors. Single GETFAMILY resolves name before
  ID. Dump-only GETPOLICY resolves ID before name and returns both operations' policies or a selected
  operation's policy. Missing selector gives `EINVAL`, unknown family or operation `ENOENT`. For an
  unknown operation EXT_ACK can supply `NLMSGERR_ATTR_OFFS` without text.
- The parser takes the first two bytes of ID, first four of operation and up to 16 bytes of name
  through the first NUL. Short numeric fields and overlong names give `ERANGE`. Unknown attributes are
  ignored. Malformed lengths stop parsing. Successful controller dumps pack DONE in the same section as
  data and have no separate success ACK. Single GETFAMILY can have a separate ACK.
- SOCK_DIAG models Linux 6.1 without family handlers. `TCPDIAG_GETSOCK` and `DCCPDIAG_GETSOCK` give
  `EOPNOTSUPP`. `SOCK_DIAG_BY_FAMILY` and `SOCK_DESTROY` give `EINVAL` for fewer than two body bytes or
  family ≥46, otherwise `ENOENT`. Other requests give `EINVAL`. Nonrequests/control messages get a zero
  ACK only if asked. Errors honor CAP_ACK and supply no extack text or offset.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/net` dispatches socket operations, brackets changing ioctls and builds legacy route-ioctl
  requests with `message` and `attr`. `kernel/poll` calls socket `readiness`, `arrivals` and
  `wake_objects`.
- `kernel/fileio` calls the pinned-namespace hooks from `write_proc`. Sysfs uses `operstate` for base
  interfaces.
- `kernel/ipc` registers a weak netlink socket and 32-byte cookie for `mq_notify(SIGEV_THREAD)`.
  Delivery or removal sets the last byte to the event code, attempts to enqueue with group zero and
  requests a wake.
- `kernel/netns` uses `configure_host_link` and encodes device payloads with `attr`. `kernel/route`
  calls `attr` through `put` for route and rule bodies. Netlink wraps the bodies in `message`.
  `kernel/nft` uses both encoders and returns queue items.

### Outbound

- `kernel/net` supplies `SockOpts`, timeval handling, guest traffic counters and `forward_all`. Kernel
  services supply credentials (`ns_capable`, UID/GID mapping), PID/fd lookup and interruptible
  waits/wakes.
- `kernel/route` provides attribute parsers, host routes, `route_message`, `get` and `rule_messages`.
  `kernel/netns` provides `netdev::{request, base, views, tables, interfaces, nsid, nsid_alloc}`,
  virtual-device encoding and per-interface routing settings. `appine_hal::net` supplies host
  interfaces and transmit queue length.
- `kernel/nft` supplies `handle_with_notes`, the sender and multicast result types and `SetElemDump`.

## Invariants and gotchas

- Ports and multicast groups are protocol-local within a network namespace. ROUTE and NETFILTER group 7
  have different subscribers.
- A queued dump retains its send-time contents and can exceed `SO_RCVBUF`.
- A snapshot diff collapses intermediate steps of one request. Several changes in a datagram can each
  render every watched namespace twice.
- Sender echoes are queued during request handling. Multicast runs after the sender borrow ends.
  `notify_netlink` prunes dead entries and releases each recipient borrow before requesting a wake.
  `notify_proto` reports a membership match even if queue admission drops the message.

## Confirmed bugs / technical debt

- Authorized guest sends to a group report success without delivery, even with a peer port. Peer
  unicast delivers bytes with kernel source port and credentials, so a receiver cannot identify the
  sender from that metadata.
- Only the five ROUTE groups above and `NFNLGRP_NFTABLES` have unsolicited events. Neighbour, rule,
  NETCONF, NSID, conntrack and hotplug events have no producer. There is no independent host interface
  monitor. The IPv6 sysctl hook omits NETCONF and `IFLA_AF_SPEC` link events.
- Base link dumps omit `IFLA_AF_SPEC` and fix host MTUs. Strict link/address dumps validate target
  namespace attributes but use the socket namespace. A header-only single GETNEIGH returns an explicit
  `EINVAL` ACK, a compatibility exception pinned by the guest regression.
- `SO_ATTACH_FILTER` on netlink falls through `SockOpts`'s unrecognized-setter path and succeeds without
  installing or running any filter on queued items.
- `mq_notify(SIGEV_THREAD)` cookie delivery ignores queue-admission failure while still waking the
  socket, so a one-shot registration can be consumed without its cookie reaching the receiver.

## Open questions

None.

## To verify

Full link/address wire parity across target interfaces needs native captures. `tests/diff/compat.sh`
compares selected link attributes and excludes address creation/update times.
