# kernel/net Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/net.md` for the paired decision history; it still holds the decisions on netlink sockets (`kernel/netlink`), raw ICMP (`kernel/rawicmp`) and the routing table (`kernel/route`) from before their splits.
>
> Verified: 2026-11-26 (full refresh: 5 research concerns; 18 claims checked by an independent verifier — 18 confirmed, 0 partial, 0 refuted, plus 6 map-audit refinements and 4 additional confirmed bugs folded in)

## Responsibilities

Internet sockets use nonblocking host fds. The socket layer handles the guest ABI, blocking, selected
options, DNS interception, endpoint translation, forwarder registration, traffic accounting and proc
output.

Under `core/kernel/src` this covers `portmap.rs`, `traffic.rs`, `origdst.rs`, non-netlink `net.rs` and
`syscall/net.rs` including legacy route ioctls. The latter excludes `kernel/netlink`’s `netlink_*`
helpers, netlink section and `Socket::Netlink` arms, and `kernel/rawicmp`’s `raw_recv` and `take_errors`.

## Key types

- `Socket::{Inet,Netlink,Unix}` lives in `FileKind::Socket` as a `SocketRef` (`Rc<RefCell<Socket>>`).
  Socket, open file and fd entry hold inode uid/gid, O_NONBLOCK and CLOEXEC respectively.
- `InetSocket` holds the host fd/type, family, guest protocol, raw flag and endpoint state. `net == None`
  denotes the initial namespace. `listener` is a binding id, including for accepted children. `peer`
  records a datagram peer and `guest_peer` the guest connect address.
- `Routed` carries one route’s host mapping, verdict and original destinations. Optional `Track` retains
  NAT, verdict, peer and mark state. `DeferredListen` stores backlog, guest reuse and private forwarding
  listener. `fwd` holds the registration.
- `SockOpts` holds common Unix/netlink options. `MsgHdr` represents imported iovecs and pointers.
  `Message` separates copied bytes from reported length and carries source, controls, flags and pending
  Unix rights.
- `PortMap` tracks live and released ports (`Lease`) and queues `ListenEvent`. `Traffic` stores `IfStats`,
  address cache and `Protocols`. `origdst::Table` indexes namespace-specific destinations by host tuple.

## Public entry points

- `socket`, `socketpair`, `bind`, `listen`, `connect`, `accept4` (also accept), `shutdown`, name/option
  calls, sendto/sendmsg/sendmmsg and recvfrom/recvmsg/recvmmsg. File operations use
  `syscall::net::{rw,send,recv,ioctl}`. Poll uses `readiness`, `wake_objects`, `arrivals`.
- `deliver_dns`, `Machine::take_listens`, `Traffic::{account,get}`, `traffic::outgoing_source`,
  `net::{proc_listing,sockstat,proc_snmp,proc_netstat,PROC_ARP}`.

## Data flow / lifecycle

### Creation and descriptors

Creation flags outside the type nibble, NONBLOCK and CLOEXEC give `EINVAL`. HAL sockets are nonblocking
and close-on-exec. Stream/datagram protocol zero defaults to TCP/UDP. Explicit MPTCP remains 262. Raw
protocols 1–255 require family-matching ICMP/ICMPv6 and `CAP_NET_RAW` over the netns owner, else `EPERM`.
Other raw numbers go to the host for validation. Raw ICMP uses a ping datagram, requests host options,
syncs receive budget, registers the namespace tap and signals the timer.

AF_UNIX sets the namespace datagram queue limit. AF_NETLINK accepts raw/datagram for ROUTE, GENERIC,
KOBJECT_UEVENT, NETFILTER and SOCK_DIAG (bad protocol `EPROTONOSUPPORT`, bad type `ESOCKTNOSUPPORT`).
Other families give `EAFNOSUPPORT`. Inodes take creator fsuid/fsgid. Inet retains euid for reuse-port.

Only Unix creates a pair. Inet `socketpair` checks types locally (≥11 `EINVAL`, other unsupported
`ESOCKTNOSUPPORT`). Streams admit 0/TCP/MPTCP, datagrams 0/UDP/ICMP/UDP-Lite, raw any nonzero protocol.
Mismatch gives `EPROTONOSUPPORT`, then raw checks namespace CAP_NET_RAW (`EPERM`), then inet gives
`EOPNOTSUPP`. Netlink gives `EOPNOTSUPP` directly. Unix second-install failure closes the first fd, pair
copyout failure closes both.

`socket_of` pins the open file across waits and rejects O_PATH (`EBADF`) or a nonsocket (`ENOTSOCK`). New
files use O_RDWR plus requested NONBLOCK. Install/reserve chooses the lowest free slot below soft
`RLIMIT_NOFILE`. `nr_open` constrains table expansion. Already allocated capacity survives a lowered
limit.

### Bind

Sockaddrs above 128 bytes give `EINVAL`. `inet_bind_addr` requires 16 bytes for IPv4, 24 for IPv6. IPv4
accepts AF_INET, AF_UNSPEC wildcard or any raw family and writes AF_INET. IPv6 requires AF_INET6. Short
gives `EINVAL`, wrong family `EAFNOSUPPORT` (raw IPv6 `EINVAL`).

Connected raw bind gives `EINVAL` before translation. Otherwise clear the port and translate. For a
changed host IP, `bind_raw` configures/binds a replacement ping fd, drains the old source, replaces the
tap source, swaps fds, resyncs receive budget and closes the old fd. Same-address rebind reuses it. Option
copying compensates for buffer doubling, restores priority after TOS and filter lock last.

Other binds translate before checking nonzero ports below namespace `ip_unprivileged_port_start` (default
1024). These need `CAP_NET_BIND_SERVICE` over the owner (`EACCES`). In a new namespace a wildcard
port-zero datagram other than ICMPv6 binds the host wildcard (`wide`). A v6-only mapped bind or rebind of
an already bound nonraw socket with a port gives `EINVAL` there.

For a new-namespace explicit TCP/UDP port, `bind_conflict` checks guest address/port, protocol, v6-only,
reuse, creator euid and device before host bind (`EADDRINUSE`). `shared_host_port` selects a compatible
port or the host allocates one. `add_binding` records the endpoint, immediately listening for UDP.
`claim_ephemeral` accepts the assigned host port if free in the guest table, else searches the namespace
ephemeral range and then outside it. Initial-namespace low ports use `PortMap`. Other binds use the
translated host address.

### Listen and deferred queues

`listen` tries namespace implicit bind, then `listen_conflict` before a bound TCP socket first enters
listen (`EADDRINUSE`). Host listen or `defer_listen` success sets listening state, marks the namespace
binding, updates forwarding and checks the host address for a listen event.

Retained RET 0 reserves the port with host SO_REUSEADDR off. Unbound IP_BIND_ADDRESS_NO_PORT gives
`EOPNOTSUPP`. Repeated listen updates any private listener and backlog. Held accept waits on the object or
gives `EAGAIN`. Poll returns (0, no fd). SO_ACCEPTCONN, TCP_INFO and proc report listen.

Detach or a supported pass replacement enables host reuse for handover, shuts the private queue and
listens with the saved backlog. Failure tries to restore private listen, disables host reuse and keeps RET
0. Success restores guest reuse best-effort, clears the hold, refreshes forwarding and wakes waiters.

### Connect and names

Retained RET 0 rejects connect (`EOPNOTSUPP`). Stub connect returns early. `route` applies OUTPUT NAT and
host translation, then applicable bridge/endpoint PREROUTING and path filters for TCP/MPTCP or UDP. A NAT
block ends hook evaluation. Port-only bridge DNAT preserves the L2 path. The flow carries guest source
port, caller euid/egid, mark, original destinations and seen-peer UDP reply state. IPv4 path filtering
clears the mark across namespaces.

Isolated streams give `ECONNREFUSED`. Isolated UDP records a silently blocked peer and disconnects the
host, trying to keep the local port. Other isolated protocols give `ENETUNREACH`. A stream reject returns
its errno. A drop redirects to a full-backlog kernel blackhole listener or gives `ETIMEDOUT` if unavailable,
skipping normal source bind, destination port translation, counters and original-destination
registration (the shared success/EINPROGRESS path can still claim an eligible new-namespace ephemeral
source port). A new-namespace stream target without a matching listener gives `ECONNREFUSED`.

`ensure_bound` selects a namespace source or permits host autobind. A self-directed UDP source can
register a return port. Destination resolution uses the target namespace binding or initial port map. A
wide datagram socket connecting to a mapped namespace peer leaves the host disconnected and saves `vconn`.
AF_UNSPEC undoes that connection. UDP can retain the verdict and a weak target for later sends.

Host success or `EINPROGRESS` can claim an ephemeral port and register an original destination before any
blocking wait. Nonblocking returns the host result. Blocking `EINPROGRESS` and repeated `EALREADY` wait
for POLLOUT. SO_SNDTIMEO expiry returns the original progress errno and leaves connect running.
Interruption gives `ERESTARTSYS` without a timeout (`SA_RESTART` can re-enter) or `EINTR` with one. On
readiness the kernel reads SO_ERROR and calls host connect again on either outcome to consume host
connecting state. The guest receives the saved result.

`connected_peer` maps IPv4 on IPv6, resolves an IPv4 wildcard to bound IPv4 or loopback, and an IPv6
wildcard to ::1 or its mapped local counterpart. It clears IPv6 flow info and retains scope only for
link-local. Name queries account for the stub, tapped/blocked UDP and virtual peers. `inet_name` restores
local ports and namespace addresses, uses cached guest/outside peer after successful host peer query, and
can choose a virtual local endpoint toward the peer. A raw socket reports its protocol as its local port.

### Accept, shutdown and release

`accept4` checks fd existence, validates NONBLOCK/CLOEXEC, reserves a slot, then resolves the listener.
Missing fd `EBADF` precedes bad flags `EINVAL`, and `EMFILE` can precede `ENOTSOCK`. Listener nonblocking
and receive timeout govern the wait. The call's flags govern the child. On `EAGAIN`, wait on POLLIN and
the socket, or only the socket for deferred listen. Expiry gives `EAGAIN`, interruption `ERESTARTSYS`
without a timeout or `EINTR` with one.

`accept_host` snapshots family/type/protocol, both timeouts, namespace, local map and virtual address,
mark, v6-only, creator euid and device before waiting. It clones the retained filter after host accept.
The child takes inode fsuid/fsgid from the accepting task, the listener's local port mapping without a
lease, and any original destination claimed in its namespace. A translated outside peer can be cached. A
mappable new-namespace child registers its own accepted, non-listening binding to preserve bind conflicts
after listener close. Passive opens count before copyout for initial TCP/MPTCP.

Accept/recvfrom ignore addrlen for null addr. Otherwise invalid addrlen gives `EFAULT`, negative room
`EINVAL`. Copy the fitting prefix and return full length. FUSE fill retains connection and slot. Success
installs the child, error releases both.

Deferred SHUT_WR preserves the listener. SHUT_RD/RDWR restores reuse, drops private forwarding, clears
listening, sets `rd_shut` and wakes waiters. Otherwise inet calls host shutdown. Read shutdown records the
flag, clears listening and wakes even on host `ENOTCONN`, returning the host result. Final drop closes the
host fd, removes its self-UDP port when loopback can be borrowed and releases its namespace binding, lease
and forwarding handle.

### Send and receive

`read_msghdr` imports 56 bytes. More than 1024 iovecs gives `EMSGSIZE`. It rejects signed-negative lengths
(`EINVAL`) during import, then range-checks each original extent against 2^48 (`EFAULT`) before shortening
the total to `MAX_RW_COUNT` (0x7fff_f000). `sendto`/`sendmsg` gather up to 1 MiB (`MSG_MAX`). Guest
ancillary space is capped at 4096 bytes each way, zero for a null receive control pointer.

After Unix/netlink dispatch, `inet_send_addr` validates explicit nonstream and FASTOPEN destinations
before DNS or routing. Ordinary streams ignore the destination. IPv4 needs 16 bytes, accepts AF_INET and
rewrites AF_UNSPEC for datagrams except nonraw ICMP. Short input gives `EINVAL`, wrong family
`EAFNOSUPPORT`. IPv6 nonraw datagrams except ICMPv6 accept AF_INET6 from 24 bytes, AF_INET from 16 unless
v6-only (`ENETUNREACH`), and AF_UNSPEC as no destination. Other invalid input gives `EINVAL` there.
Remaining IPv6 sends need 24 bytes and AF_INET6, with a raw AF_UNSPEC rewrite. FASTOPEN under retained RET
0 gives `EOPNOTSUPP`.

Address-less UDP honors its saved verdict and re-resolves any namespace target. A missing listener drops
the send with success. A changed endpoint updates `vconn` or attempts host reconnect. For addressed
datagrams blocked OUTPUT gives `EPERM`, later drops/rejects return the byte count. Isolated UDP and
missing namespace listeners drop silently. Reverse DNAT history holds 16 pairs. Wide/initial wildcard
sources can append IP_PKTINFO/IPV6_PKTINFO unless supplied. Raw and tapped UDP call `before_io`, drain tap
errors and may retry a host ICMP failure once. Raw uses `RawIcmp::send` per attempt.

Host send strips guest DONTWAIT/NOSIGNAL and HAL adds MSG_NOSIGNAL. Blocking stream sends loop. On host
`EAGAIN`, `wait_host` tests nonblocking, deadline, then interruption (`EAGAIN`, `EAGAIN`, `ERESTARTSYS`)
before waiting on POLLOUT and the socket. Errors after progress return a partial count. Initial host
`EPIPE` raises guest SIGPIPE unless suppressed. A successful datagram send can claim an autobound port.

Unix, netlink and raw ICMP have family-specific receive paths. Ordinary inet checks the stub FIFO, a saved
error for a non-ERRQUEUE read, then UDP tap errors. ERRQUEUE implies nonblocking. Host receive strips
DONTWAIT/WAITALL. `vconn` discards foreign datagrams even on peek. Source translation undoes
port/namespace mapping and DNAT. Up to 16 observed datagram peers support reply classification. Host
ancillary bytes and flags accompany the result. On host `EAGAIN`, recheck stub answers. With `rd_shut` and
an empty datagram queue, blocking recv returns zero before timeout or signal, nonblocking gives `EAGAIN`.
Other waits use POLLIN and the same ordering as send.

Inet stream `MSG_WAITALL` accumulates until full, EOF, error or unsuccessful wait. Partial progress
survives interruption even under SA_RESTART. A consumed host error after partial progress goes into
`pending_err`. Ordinary receive, stream send or SO_ERROR can consume it once. Poll reports POLLIN|POLLERR
and the host fd without clearing it. Stream MSG_TRUNC returns a length without copying payload. Datagram
MSG_TRUNC returns the backend's message length, which can exceed the copied prefix.

`MSG_PEEK|MSG_WAITALL` rereads the queue head. `wait_peek` temporarily sets host SO_RCVLOWAT to the
requested length and attempts to restore it afterward. No growth after a ready poll returns the prefix.
For non-peeking stream WAITALL beyond 1 MiB, recvfrom/recvmsg use `recv_rest` to scatter at successive
offsets until a short piece, without further control data. Each receive starts its own timeout deadline. A
later hard receive error is saved.

### File I/O, batches and copy faults

`rw` reads once across iovecs without flags, address or control. Empty reads consume nothing.
Datagram/seqpacket writes gather one message up to 1 MiB. An empty scalar write can send an empty
datagram. All-zero writev returns in file I/O. Stream writes continue in 1 MiB pieces, sending an already
gathered prefix on a later gather fault. A record gather fault fails the message. After a successful
piece, later sends use NOSIGNAL and errors return the existing count. File byte helpers and splice paths
use `send`/`recv` with flags zero.

Both batch calls handle at most 1024 records of 64 bytes. `sendmmsg` calls sendmsg per numeric fd and
returns completed records on a later send error. Its plain msg_len store propagates faults. `recvmmsg`
reads the deadline, handles zero elements, then pins the file and socket. It rereads nonblocking status on
each record attempt. WAITFORONE adds DONTWAIT after the first success. The batch timeout is checked after
each completed record. Later failures return the fully reported record count.

After consumption, `scatter_received`/`copy_result` retain data across FUSE page fills for individual
iovecs, address bytes/lengths, controls and flags. Recvmmsg retries a pre-consumption input fault within
the current record, and retries its final msg_len separately. Unix copies payload before consumption
(non-peeking datagram/seqpacket survives FUSE retry, while a hard scatter error still dequeues it). A
failed ancillary copy drops pending rights and reports MSG_CTRUNC and zero control length. SCM_RIGHTS fds
install only after output header writes succeed, honoring MSG_CMSG_CLOEXEC.

### Socket options

Setters resolve the fd and copy up to 256 option bytes. Getters reject negative room (`EINVAL`) and cap
ordinary host reads at 256. SO_RCVTIMEO/SO_SNDTIMEO and SO_RCVTIMEO_NEW/SO_SNDTIMEO_NEW use the same two
signed 64-bit fields for all families. Short timeval gives `EINVAL`, invalid microseconds outside 0–999999
give `EDOM` before the seconds check. Negative seconds mean immediate expiry. Zero duration and values ≥
KTIME_MAX mean unlimited. Both read back as zero.

Unix/netlink `SockOpts` defaults both buffers to 212992. Plain requests compare unsigned, cap there and
double (minima 4608 send/2304 receive). FORCE requires initial-user-namespace CAP_NET_ADMIN (`EPERM`),
clamps to 0..INT_MAX/2 then doubles with the same floors. DEBUG enable needs that capability (`EACCES`).
Priority outside 0–6 needs CAP_NET_RAW or NET_ADMIN over netns owner. Generic setters need an int, linger
eight bytes, retaining seconds on disable. Negative receive low-water becomes INT_MAX, zero becomes one.
Read-only type/error/domain/protocol give `ENOPROTOOPT` after size check. Unknown generic SOL_SOCKET sets
succeed without effect. Netlink enforces receive capacity. Unix enforces send capacity and stream receive
low-water but only records receive capacity.

Inet SO_SNDBUFFORCE/SO_RCVBUFFORCE need initial-user-namespace CAP_NET_ADMIN, then use ordinary host
options. Raw ICMP options and UDP TTL/RECVERR tap handling precede BPF. A successful host receive-buffer
update resyncs raw receive accounting.

- SO_ATTACH_BPF and SO_ATTACH_REUSEPORT_EBPF check optlen <4 (`EINVAL`), then only plain TCP ATTACH_BPF
  checks namespace-owner CAP_NET_ADMIN (`EPERM`), then exact size four (`EINVAL`). Invalid guest fd gives
  `EBADF`, valid fd `EINVAL`. No guest fd reaches the host.
- SO_ATTACH_FILTER and SO_ATTACH_REUSEPORT_CBPF check four-byte minimum, plain TCP ATTACH_FILTER
  capability, exact 16-byte fprog, host filter lock (`EPERM`), then nonnull program and 1–4096
  instructions (`EINVAL`). An unreadable lock is treated as unlocked. Copy eight bytes per instruction and
  substitute a host-local fprog pointer. Plain TCP here means stream protocol 6, excluding MPTCP.
- For plain TCP ATTACH_FILTER, host `EPERM`, test hook 14 or an existing deferred listen selects
  validation on a scratch UDP fd. Fallback accepts exact RET 0 only in host TCP_CLOSE with SO_REUSEPORT
  off, or RET UINT_MAX after only immediate/length loads. Other valid programs give `EOPNOTSUPP`. An
  existing host filter blocks fallback when no program is retained. Failure preserves the old filter.
  REUSEPORT_CBPF stays on the host path.
- SO_GET_FILTER takes and returns **instruction counts**. Zero asks for the count, short nonzero room
  gives `EINVAL`. Retained bytes answer locally, else HAL queries up to 4096 instructions. SO_LOCK_FILTER
  uses the host. SO_DETACH_FILTER on a retained program needs ≥4 bytes and an unlocked host filter. During
  deferred listen SO_REUSEADDR uses the saved guest value. Enabling reuse-port under retained RET 0 or
  rebinding the device while a private queue exists gives `EOPNOTSUPP`.
- SO_MARK needs four bytes then namespace-owner CAP_NET_RAW or NET_ADMIN. It feeds nftables and accepted
  children without setting a host mark. New-namespace AF_INET6 records IPV6_V6ONLY locally. Changing it
  once both bound and registered gives `EINVAL`.
- SO_BINDTODEVICE truncates to 15 bytes or first NUL. SO_BINDTOIFINDEX needs a nonnegative int. Empty
  name/zero clears. Namespace lookup (`ENODEV`) precedes CAP_NET_RAW on rebind/clear. First bind needs no
  guest capability. Real initial interfaces go to the host by name. Store the index and update any
  namespace binding. Unbound name readback has zero length. Otherwise room below 16 gives `EINVAL`,
  deleted device `ENODEV`, success a NUL-terminated name. Index readback returns the saved index.
- SO_ERROR drains the relevant ICMP tap, consumes `icmp_err` before `pending_err`, then uses raw/UDP
  handlers or host. A returned 4-byte ECONNREFUSED on a counted opening increments AttemptFails. SO_TYPE
  reconstructs SOCK_RAW for raw ping-backed sockets. Remaining inet options, including detach without a
  retained program, priority, IP_TRANSPARENT, SO_PEERCRED and ordinary TCP options, use host semantics.

### Socket ioctls

Common file ioctls run first. For netlink’s 15 `CHANGING_IOCTLS` and watched namespaces, take snapshots,
run the ioctl, diff, notify subscribers and return the original result.

Inet FIONREAD/SIOCINQ returns front stub-answer length or host input count. Inet SIOCATMARK queries host
before copyout. Unix FIONREAD uses unread count (`EINVAL` if unavailable), SIOCOUTQ outgoing bytes.
Netlink FIONREAD gives uncounted `ENOTTY`. Interface ioctls try virtual devices first, then namespace
loopback or initial host interfaces. Host SIOCGIF* handles name, flags, address, destination, broadcast,
mask, MTU, HWADDR, index and tx queue length. Zero HWADDR family is reconstructed from loopback flags.
Recognized host setters read 40 bytes before `EPERM`. Unix/netlink borrow a temporary host datagram fd.

SIOCGIFCONF reports size for a null buffer. Initial-namespace output appends whole 40-byte virtual entries
after host entries (64 KiB room cap). A new namespace supplies loopback plus virtual entries. Unhandled
requests including inet SIOCOUTQ and timestamp queries reach `stub::ioctl` and return `ENOTTY`. Expected
terminal probes are filtered from counts.

Inet SIOCADDRT/SIOCDELRT read arm64 rtentry (IPv4, 120 bytes) or in6_rtmsg (IPv6, 80) before
namespace-owner CAP_NET_ADMIN (`EPERM`). IPv4 checks families (`EAFNOSUPPORT`), contiguous mask/alignment
(`EINVAL`), or uses /32 for RTF_HOST. Device labels resolve index/preferred source (`ENODEV`). Nonzero metric sets RTA_PRIORITY to metric minus
one. RTF_GATEWAY adds require a gateway, with global scope for unicast. IPv6
retains prefix, optional gateway/index and metric, refuses source prefixes (`EOPNOTSUPP`). Adds use
RTPROT_BOOT. RTM_NEWROUTE/DELROUTE goes to `netdev::request`. Only virtual routes change. Unreachable adds
give `EOPNOTSUPP`. MTU/window/initial RTT are ignored.

### DNS stub

During VFS setup, `ensure_resolv_conf` writes `nameserver 127.0.0.53` plus newline (0644) only if /etc
exists and lacks a resolv.conf entry. Failure only logs. Android Debian import writes its own resolver
entry.

`stub_capable` requires initial-namespace SOCK_DGRAM, UDP, no raw flag or ICMP state. `is_stub` recognizes
127.0.0.53:53 and its IPv4-mapped form. Connect sets `stub_peer` and attempts host connect for local
address/port. Send to the endpoint or address-less send on a stub peer attempts wildcard autobind if
needed, then bypasses host payload I/O, nft and traffic counters.

At 128 registered queries kernel-wide (`DNS_MAX_INFLIGHT`), send drops the query and returns its length.
Otherwise `Kernel::dns_queries` takes an id and weak socket. Spawn failure removes it (`EAGAIN`). A
`dns-stub` thread calls `hal::net::dns_query`, appends (id, answer) to `Shared::dns_answers` and signals
the timer without holding the kernel.

HAL rejects queries below 12 bytes. Android calls `android_res_nsend` on default network zero, with a 30 s
poll deadline and cancellation on unsuccessful poll. Non-Android tries up to three resolv.conf servers.
UDP checks header/ID within 3 s per server. Truncation retries over length-framed TCP with a fresh 3 s. No
servers gives `EHOSTUNREACH`. On error `servfail` copies the query, setting QR, RA and rcode 2 if four
bytes exist.

`deliver_dns` removes completed ids and drops dead-socket replies. It compares bytes **already queued**
with host SO_RCVBUF (fallback 212992). Only a sum strictly above the limit drops the new reply. Enqueue
increments `answered` and wakes the socket. Receive serves this FIFO before host data with the stub
source, peek and truncation semantics. FIONREAD gives front length. Queued answers supply POLLIN|POLLOUT,
and tapped UDP additionally supplies host fd and any ICMP error bit. Read generations use `answered`.

### Low ports and inbound forwarding

Initial `PortMap` covers 1–1023 with nonzero offset (default 10000). Nonraw TCP/UDP qualify, MPTCP shares
TCP's key. All addresses/families share (protocol, guest port). A successful bind holds a lease even at
the original port. A live key or one released within 120 s fixes the host port. Its error is final. A
fresh key tries the original unless `always_map`, the other protocol's live host port, guest port plus 1–3
offsets (distinct, in range), then zero. Only nonzero-candidate `EADDRINUSE` or original-port `EACCES`
continues. Ephemeral bind reads back the host port. Newly changed mappings are logged. Last lease removes
the live lookup immediately and permits reverse translation for less than 10 s.

`map_dest` rewrites a live mapped port only on loopback, wildcard or a host interface address.
`unmap_addr` applies the socket's matching local pair. `translate_peer` passes the intact endpoint to
`netns::from_host_peer`. A matching saved peer pair precedes a namespace binding or local initial reverse
lookup. Accept, peer names, recv and proc share it. Non-loopback host listens enqueue comm and guest/host
ports, even when equal. Only queued port pairs are deduplicated for the instance lifetime. A pair dropped
at the 32-event cap remains eligible. `Machine::take_listens` drains the queue.

`forward_target` considers initial TCP/MPTCP listeners and bound UDP with PREROUTING NAT. It restores the
guest port before `external_nat`, retries a dual-stack wildcard as IPv4, translates, excludes
isolated/filter-denied targets and chooses the namespace binding or initial mapped port where available.
`forward_socket` registers fd and target with the forwarder on datagram bind or listen. `forward_all`
rescans after an initial NETFILTER generation change, and after a new-namespace explicit port registration
or TCP listen marking a binding if initial PREROUTING NAT exists.

Updating the UDP handle in place lets replies drain while withdrawal pauses intake. Removing an ordinary
TCP target drops its handle. Deferred listen gives the forwarder a private TCP listener, copying host
v6-only and device binding and temporarily enabling guest reuse-address to bind the same endpoint. Target
loss or failed registration discards that queue. `ForwardListener::drop` shuts it down even while the
worker holds a dup. The guest fd keeps the port.

### Traffic and proc files

`Traffic::account` counts initial-namespace payload. Datagrams count once even if empty. Nonempty streams
contribute ceil(bytes / max(MSS,1)). Host receive skips PEEK and ERRQUEUE. Raw ICMP feeds tap arrivals.
Stub and Unix traffic bypass interface counters. Bind/connect invalidate the cached socket route. A
concrete local IP maps to interface/MSS (MTU minus 52). Wildcard traffic selects loopback or default by
peer. The interface cache expires after 5 s and can refresh on address miss after 1 s. Default selection
tries host IPv4 then IPv6 source selection, an up non-loopback interface, then loopback.

`Protocols` counts IPv4 packets/octets including mapped IPv4, IPv4 UDP datagrams, ping echoes/replies and
estimated TCP/MPTCP segments across families. Initial TCP/MPTCP connect counts success or EINPROGRESS once
per `opening` flag. Immediate refusal/timeout can count an open and failure together. Refusal, timeout or
host/net unreachable on a counted attempt increments AttemptFails and clears opening. Host accept counts
passive opens. MPTCP accounting is per guest socket.

Procfs selects the named process's pinned network namespace and translates inode uid through the reader's
user namespace. Its inode-sorted inventory deduplicates sockets and includes sockets held only in Unix
rights. `proc_listing` renders tcp/udp/icmp/raw and v6 variants. Host SO_PROTOCOL identifies TCP including MPTCP, UDP and ping, and supplies the raw local port (0 on
query failure), while the guest raw flag selects the raw table. Raw/ping peer port is zero.
Addresses reverse host and port translation and can retain the outside peer. TCP state uses the first
TCP_INFO byte, default CLOSE, or LISTEN for a held listener. Queue, timer and retransmit fields are
placeholders.

`sockstat` uses host protocol for TCP/MPTCP. In-use requires requested family and non-CLOSE. IPv4 alloc
counts both families/all states. Bound or stub-connected UDP and raw count by family. IPv4 used counts all
supplied sockets. Orphan, TIME_WAIT and memory are zero. `proc_snmp` counts CurrEstab by stored TCP/MPTCP
protocol and exactly ESTABLISHED. Noninitial namespaces have zero cumulative counters but live namespace
socket counts. Procfs reports IP Forwarding 1 for ip_forward on, else 2. In netstat, TcpExt/MPTcpExt are
zero. IpExt reports payload InOctets/OutOctets and inbound packets as InNoECTPkts. `PROC_ARP` contains
only the neighbour-table header.

### Original destination

`origdst::Table::register` keys (host client local, host destination), mapping IPv4 into IPv6. On
TCP/MPTCP host success or EINPROGRESS with a usable tuple, connect registers pre-OUTPUT or pre-PREROUTING
originals for each rewriting namespace before waiting. Empty originals clear a stale tuple. Client close
leaves the record intact. Nonempty insertions prune at most once a second and evict the oldest for a new
key at 4096. At accept, `claim` uses (host peer, host local), removing the whole entry before checking its
120 s age and matching the listener namespace.

SOL_IP/SOL_IPV6 SO_ORIGINAL_DST handles TCP/MPTCP streams. UDP gives `ENOPROTOOPT`. Other protocols, raw
ICMP and SOL_IPV6 on IPv4 fall through. Room below 16/28 bytes gives `EINVAL` before lookup. A saved
initiating peer is returned when the namespace tracks its family. Accept-side lookup requires a host peer
and returns the claimed original or, if tracking, the translated local endpoint. Missing or wrong-family
result gives `ENOENT`. Mapped IPv4 uses SOL_IP. Success leaves optlen unchanged.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/process` dispatches syscalls and drains listens. `kernel/files` dispatches ioctls and updates
  socket ownership. `kernel/vfs` reads ownership for stat and sets up resolv.conf. `kernel/fileio` and
  `kernel/pipe` use socket transfers.
- `kernel/poll` combines ready bits, host fds, wake objects and arrivals. `kernel/time` delivers DNS
  completions. Unix/netlink use shared options and message helpers. Netlink reads link counters and
  triggers forwarding rescans. Raw ICMP uses the local-address translator and feeds tap arrivals to
  Traffic.
- `kernel/procfs` supplies socket inventory, reads interface counters for dev, renders if_inet6, and
  delegates route/ipv6_route to `kernel/route` and unix to `kernel/unix`. `kernel/sysfs` reads interface
  statistics. `kernel/route` calls `traffic::outgoing_source` for default interface selection.

### Outbound

- `appine_hal::net` supplies socket calls, interface enumeration, source selection and DNS.
  `kernel/process` supplies waits/BKL, `kernel/files` fd allocation, `kernel/fuse` page fills,
  `kernel/signal` restart, SIGPIPE, logs and unsupported-path counts.
- `kernel/netns` owns translation, bindings, virtual device/route mutations, forwarding workers and
  netconf. `kernel/sysctl` exposes the port threshold/range, bridge settings and fd-growth limit consumed
  on these paths. `kernel/nft` evaluates rules, returns verdicts and reports whether a family tracks
  connections (per-socket `Track` and `origdst::Table` belong to `kernel/net`).
- `kernel/netlink` owns family operations, message/attribute wrapping, snapshots and notifications
  (while `kernel/route` encodes route-entry bodies and `kernel/net` builds legacy ioctl route change
  requests). `kernel/rawicmp` owns ping emulation, taps, raw receive and error draining. `kernel/unix`
  owns local sockets and rights transfer. `kernel/bug` supplies TCP filter refusal injection.

## Invariants and gotchas

- Host sendmsg/recvmsg run nonblocking under the big kernel lock (BKL). Ordinary socket waits release it
  and register the socket wake object. A peek wait's temporary host low-water change is visible to
  concurrent pollers.
- Keep the original host peer port through namespace ownership lookup. Release the socket borrow before a
  raw tap drain that may borrow it again. Keep changing ioctls inside the watched snapshot/diff path and
  the private DNAT accept queue separate from guest accept.

## Confirmed bugs / technical debt

- In-process socket buffers retain fixed defaults/caps after writes to the guest core buffer sysctls.
- `sendmmsg` looks up the numeric fd anew per record. Despite best-effort dispatcher prefaulting, a
  post-send `msg_len` fault can hide earlier progress, and whole-syscall FUSE retry can repeat output.
  In `recvmmsg`, `SCM_RIGHTS` fds install before the outer `msg_len` store, so an unrecoverable `msg_len`
  fault can leak installed fds for an uncounted record.
- On inet streams, an unrecoverable scatter fault in a later WAITALL piece leaves that piece consumed but
  absent from the returned count, and continuing WAITALL after a short read drops earlier ancillary data
  and flags (the finishing piece's metadata wins; partial completion after failed wait/error returns empty
  controls and zero flags).
- `wait_peek` saves, raises and restores host `SO_RCVLOWAT` across an unlocked wait, which can overwrite a
  concurrent `SO_RCVLOWAT` update.
- Normal `connect` overwrites `guest_peer` before calling host connect, so on `EISCONN` subsequent
  `getpeername` verifies the host peer and returns the rejected destination.
- DNS stub connect ignores the host connect result, so synthetic peer success can disagree with the actual
  local endpoint. A receive with `MSG_ERRQUEUE` checks the ordinary stub FIFO and can consume a DNS reply.
- The external DNAT relay registers no original-destination tuple. Its accepting socket can return its
  translated local address when tracking or `ENOENT`, losing the pre-DNAT destination.
- Passthrough options are checked against host credentials. Even authorized guest FORCE uses capped plain
  host options. A guest-authorized real-device bind ignores host `EPERM`, and clearing a host binding
  ignores host errors, so the saved interface and host routing can diverge.
- A host-refused RET 0 listener can give peers an immediate refusal. A reuse-address conflict can surface
  at activation. Packet-dependent filters, dropping on active streams and reuse-port sharing exceed the
  fallback. The TCP drop blackhole retains its host fds past kernel teardown.

## Open questions

- Should UDP queries from a new namespace have a route to the stub, and should intercepted queries
  traverse nftables?
- Which creation errno should AF_PACKET expose to a caller without raw capability?

## To verify

- On target Android kernels, measure privileged passthrough option results, TCP filter acceptance and
  MPTCP TCP_INFO availability.
- On a phone, check reachability of guest loopback listeners from another app and default-network resolver
  behavior under Private DNS and VPN changes.
