# kernel/net Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/net.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 15 confirmed and 3 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping: timeouts; port map entries; DNS stub threads; socket I/O; listen events; test-suite findings; bind checks; dump size; UDP read shutdown; accept, connect; raw rebinds; netlink options, peers, socketpair last)

## Responsibilities

`kernel/net` implements the socket layer for internet and netlink sockets:

- TCP/UDP passthrough to host sockets, with blocking, timeouts, signals and a set of emulated
  options handled in the kernel layer;
- raw ICMP and ICMPv6 sockets emulated on unprivileged host ping sockets, including traceroute's
  ICMP errors;
- in-process netlink (`NETLINK_ROUTE`, `NETLINK_GENERIC`'s `nlctrl`, the `NETLINK_NETFILTER`
  entry point, `NETLINK_KOBJECT_UEVENT`) and the routing table;
- the DNS stub at `127.0.0.53:53`;
- low-port mapping on the host and hiding it from the guest;
- traffic counters for interfaces and protocols;
- `SO_ORIGINAL_DST` records and `SO_MARK`.

Root: `core/kernel/src/net.rs`, `syscall/net.rs`, `rawicmp.rs`, `route.rs`, `portmap.rs`,
`traffic.rs`, `origdst.rs`.

Outside the module: network namespaces, address translation, virtual devices, nftables, namespace
port registration and inbound forwarding (`kernel/netns`); AF_UNIX sockets (`kernel/unix`); poll,
epoll and readiness plumbing (`kernel/poll`); `/proc/net` and sysfs file structure
(`kernel/procfs`); host socket wrappers and the host resolver (`hal`); waiting and the BKL
(`kernel/process`); the app's network callbacks and listen notifications (`android`).

## Key types

- `Socket` — `Inet`, `Netlink` or `Unix`, shared as `SocketRef` inside `FileKind::Socket`.
- `InetSocket` — host fd (always nonblocking and close-on-exec on the host), family, host type,
  protocol, `raw` (the guest asked for `SOCK_RAW`), guest-visible peer, DNS stub state
  (`stub_peer`, queued `answers`, `answered`), `rcvtimeo`/`sndtimeo`, namespace state (owned by
  `kernel/netns`), `track` (nat state, claimed original destination, `SO_MARK`), raw ICMP and
  ICMP-tap state, portmap `lease`/`local_map`/`peer_map`, cached traffic route, `rd_shut` (the
  guest shut it down for reading). Guest `O_NONBLOCK` lives on the open file.
- `Message` (one recv result), `Routed` (connect/sendto routing result: namespace map, nat,
  filter verdict, original destinations).
- `NetlinkSocket` — protocol, type, `nl_pid`, datagram queue, arrival counter,
  `rcvtimeo`/`sndtimeo`, namespace, `ext_ack`/`cap_ack`, joined groups, the other recorded
  `SOL_NETLINK` flags, `passcred`, `SockOpts`; `Host` trait / `KernelHost` (interfaces, links, tables, routes, stats,
  configure, netfilter, all scoped to the socket's namespace); `Link` (host or virtual).
- `route::Entry` — one route (`host` marks entries whose source the host chooses).
- `RawIcmp` (per raw socket: last guest echo id, `ICMP_FILTER`, guest option flags, its `Tap`),
  `Tap` (per-namespace aggregation point on a host epoll: sources and receivers), `Source`,
  `Receiver` (data queue, arrivals, error queue), `Item`/`ErrItem`; `Kernel::icmp_taps` (weak).
- `PortMap` (kernel-wide: offset, `always_map`, entries keyed by `(namespace, protocol, guest
  port, IPv6-only)`, `by_host` (by host port), `released`, `reported`, listen events), `Lease`
  (RAII refcount on an entry), `ListenEvent`.
- `Traffic` (per-interface `IfStats`, address table, `Protocols` counters), `traffic::Route`.
- `origdst::Table` (host 4-tuple → original destinations per namespace, with age and a cap).
- `SockOpts` — the generic `SOL_SOCKET` options netlink and AF_UNIX sockets only record (debug,
  reuse, dontroute, broadcast, keepalive, oobinline, no_check, buffer sizes, priority, rcvlowat,
  linger), set and read back by Linux's `sk_setsockopt`/`sk_getsockopt` rules: an int needed
  (`EINVAL`), buffer sizes capped at 212992 unless the `FORCE` form (`CAP_NET_ADMIN`), then
  doubled with Linux's minimums, `SO_DEBUG` on needs `CAP_NET_ADMIN` (`EACCES`), priorities
  outside 0..6 `CAP_NET_RAW` or `CAP_NET_ADMIN` over the namespace owner, the read-only ones
  `ENOPROTOOPT`.

## Public entry points

- Syscalls: `socket`, `socketpair`, `bind`, `listen`, `accept4`, `connect`, `getsockname`,
  `getpeername`, `sendto`/`sendmsg`/`sendmmsg`, `recvfrom`/`recvmsg`/`recvmmsg`, `setsockopt`,
  `getsockopt`, `shutdown`; read/write on sockets (`net::recv`/`send` with flags 0); `net::ioctl`;
  `readiness`, `wake_objects`, `arrivals` for poll/epoll.
- Netlink: `NetlinkSocket::handle` (`route_request`, `generic_request`, `Host::netfilter`).
- Routes: `route::{tables, merge, get, rule_messages, proc_route, proc_ipv6_route, host_routes}`;
  `Machine::set_routes`.
- Raw ICMP: `tap_for`, `register`, `RawIcmp::{sent, setsockopt}`, `udp_setsockopt`, `before_io`,
  `untap`, `raw_recv`, `check_taps` (timer thread).
- Ports: `portmap::{bind, bind_any}`, `PortMap::listened`, `Machine::take_listens`, `map_dest`,
  `unmap_addr`.
- Traffic: `Traffic::account` and the views (`/proc/net/dev`, sysfs `statistics/`, netlink
  `IFLA_STATS*`, `/proc/net/snmp`, `/proc/net/netstat`).
- `origdst::{register, claim}` and `getsockopt(SO_ORIGINAL_DST)`.

## Data flow / lifecycle

### Creating sockets

- `socket`: flags other than the type, `SOCK_NONBLOCK` and `SOCK_CLOEXEC` are `EINVAL`.
  AF_INET/AF_INET6: `SOCK_RAW` with a protocol 1..255 is `EPERM` unless it is ICMP/ICMPv6 and the
  caller has `CAP_NET_RAW` over the network namespace's owner (raw ICMP becomes a host ping
  socket); every other type/protocol goes to the host, which validates it. AF_NETLINK
  `SOCK_RAW`/`SOCK_DGRAM` with ROUTE, NETFILTER, KOBJECT_UEVENT or GENERIC (others
  `EPROTONOSUPPORT`, other types `ESOCKTNOSUPPORT`). AF_UNIX goes to `kernel/unix`. Other
  families (AF_PACKET, AF_VSOCK, …) `EAFNOSUPPORT`.
- `socketpair` is AF_UNIX only: inet first creates the socket as Linux does (a type of
  `SOCK_MAX` or more `EINVAL`, one no inet protocol has `ESOCKTNOSUPPORT`, a protocol the type
  lacks `EPROTONOSUPPORT`, raw without `CAP_NET_RAW` `EPERM`), then `EOPNOTSUPP`; netlink
  `EOPNOTSUPP`; others `EAFNOSUPPORT`/`EPROTONOSUPPORT`.

### Binding, listening, connecting, accepting

- bind: sockaddr over 128 bytes `EINVAL`; inet sockets then check the length and family as Linux
  6.1 does first (shorter than the family's sockaddr `EINVAL`; IPv4 takes `AF_UNSPEC` with the
  wildcard address as `AF_INET` and refuses other families with `EAFNOSUPPORT`, raw IPv4 ignores
  the family; IPv6 takes only `AF_INET6`, raw IPv6 answers `EINVAL`), so the host only sees
  `AF_INET` and `AF_INET6`; raw sockets drop the port; the namespace translation runs; ports below `ip_unprivileged_port_start` need `CAP_NET_BIND_SERVICE` over the namespace
  owner (`EACCES`); then portmap (section 6) or a plain host bind; namespace registration and
  forwarding hooks follow.
- listen binds implicitly if needed, listens on the host, registers the listener in a new
  namespace and with the forwarder, and reports non-loopback listens to the app.
- connect: a UDP connect to the DNS stub only records `stub_peer`. Otherwise `route()` applies
  nat, namespace translation and the filter chains (`kernel/netns`); a TCP reject returns its
  errno, a drop connects to a blackhole listener, no listener in the target namespace is
  `ECONNREFUSED`; then implicit bind, portmap destination mapping and the host connect.
  Nonblocking returns the host's `EINPROGRESS`; blocking (also when the host answers `EALREADY`
  to a connect made again while one is in progress) waits for `POLLOUT` with the BKL released, as
  Linux's `__inet_stream_connect`: `SO_SNDTIMEO` bounds the wait (then `EINPROGRESS`, or `EALREADY`
  for the repeated connect, and the connection goes on), and a signal gives `ERESTARTSYS`
  (restarted under `SA_RESTART`, the restarted call waiting again) or `EINTR` when there is a
  timeout; then it reads `SO_ERROR` and connects again, so the host reports `EISCONN` after a
  success and, after a failure, has reset the socket so that the guest's next connect sends a new
  SYN. TCP connects register `SO_ORIGINAL_DST` records before the BKL is released. The peer
  getpeername reports is recorded at connect as Linux reports it: an IPv4 address given to an IPv6
  socket IPv4-mapped, the wildcard as the loopback (IPv4-mapped when the IPv6 socket's local
  address is), no IPv6 flow information, a scope only for link-local addresses.
- accept4 (Linux's `__sys_accept4_file` order): bad flags `EINVAL`; no free fd `EMFILE` before a
  connection is taken; host accept with waits bounded by the listener's `SO_RCVTIMEO` (`EAGAIN` on
  timeout); the new socket inherits protocol, local port map, namespace state, mark, the claimed
  original destination and the listener's `SO_RCVTIMEO`/`SO_SNDTIMEO`; the peer address is
  unmapped, translated and written back before the fd is installed, so a fault drops the
  connection and leaves no fd.

### Sending and receiving

- send (sendto, sendmsg, write; sendmmsg one message at a time): netlink goes to `handle`; DNS
  stub queries are answered in place (section 5); datagrams with an address go through `route()`
  (`EPERM` or a silent drop from the filter); TCP drops the guest address unless `MSG_FASTOPEN`;
  destinations are portmapped; an `IP_PKTINFO` may be appended; the host gets the guest flags
  minus `MSG_DONTWAIT`/`MSG_NOSIGNAL` (the host call always adds `MSG_NOSIGNAL`). Blocking stream
  sends loop until done; `EAGAIN` waits (bounded by `SO_SNDTIMEO`, then `EAGAIN` or the partial
  count); `EPIPE` raises `SIGPIPE` unless `MSG_NOSIGNAL`.
- recv: AF_UNIX and raw ICMP go to their own code; queued stub and netlink answers come first;
  `MSG_ERRQUEUE` never waits; the host gets the flags minus `MSG_DONTWAIT`/`MSG_WAITALL`; source
  addresses are unmapped, translated and nat-reversed; control data is returned verbatim;
  `EAGAIN` waits (`SO_RCVTIMEO`, `ERESTARTSYS` on signals). `MSG_WAITALL` on a stream reads until
  the length is complete, waiting between pieces; nonblocking, the timeout, a signal (even under
  `SA_RESTART`), end of file or a host error ends it with what was read, as Linux's
  `tcp_recvmsg` returns what it copied. `MSG_PEEK|MSG_WAITALL` peeks from the head each time and
  waits with the host socket's `SO_RCVLOWAT` raised to the length (restored afterwards); when the
  host fd was ready but the peek did not grow (end of file, a receive buffer that can never hold
  the length) it returns what is there instead of spinning. A datagram socket shut down for
  reading (shutdown with `SHUT_RD` or `SHUT_RDWR` records it, also when the host answers
  `ENOTCONN` for an unconnected socket, as Linux's `inet_shutdown` takes effect then too) returns
  0 from a blocking recv with nothing queued, before timeouts and signals, where the nonblocking
  host socket would answer `EAGAIN` while polling readable; queued datagrams come first, and a
  nonblocking recv stays `EAGAIN`.
- read, write, readv and writev on a socket are one recvmsg or sendmsg over all the iovecs with no
  address, control data or flags (Linux's `sock_read_iter`/`sock_write_iter`): a datagram or
  seqpacket record is one message whatever the iovecs or the length, a read of nothing returns 0
  at once, and a write of nothing sends an empty datagram (a writev of nothing does not reach the
  socket); a stream socket's write goes on in 1 MiB pieces until all is sent or one is short. A
  fault ends a stream write's data there and fails a message with `EFAULT`.
- Limits: at most 1 MiB per call, 1024 iovecs (`EMSGSIZE`), 4096 bytes of send control data;
  recvfrom and recvmsg with `MSG_WAITALL` (not `MSG_PEEK`) on an internet stream go on past 1 MiB
  piece by piece, writing each piece out at its offset until one comes up short; msghdr iovecs are
  read as Linux's `import_iovec` does (a negative length anywhere `EINVAL`, then an iovec reaching
  past 2^48 `EFAULT`, the total cut to `MAX_RW_COUNT`); setsockopt values are cut to
  256 bytes (longer ones then fail on the host) and getsockopt asks the host for at most 256.
- Host sendmsg/recvmsg run under the BKL (nonblocking); only waits release it.
  There is no BKL-free fast path for sockets.

### Options and ioctls

- `SO_RCVTIMEO_NEW`/`SO_SNDTIMEO_NEW` are taken as the old numbers for every socket kind (the same
  16-byte timeval on a 64-bit kernel).
- Emulated: `SO_RCVTIMEO`/`SO_SNDTIMEO` (Linux 6.1's `sock_set_timeout`: a short
  buffer `EINVAL`, microseconds outside `[0, 1000000)` `EDOM`, a negative time zero (no waiting,
  `EAGAIN` at once), 0 or at least `KTIME_MAX` no timeout, read back as 0 like Linux's
  `MAX_SCHEDULE_TIMEOUT`; AF_UNIX sockets parse them the same way), `SO_MARK` (needs `CAP_NET_RAW` or `CAP_NET_ADMIN` over the namespace
  owner; stored only, inherited by accepted sockets, fed to nftables, cleared when entering
  another namespace), `SO_TYPE` (`SOCK_RAW` for raw sockets), `SO_ERROR` (pending ICMP errors
  first),
  `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE` (need `CAP_NET_ADMIN`, then become the plain options),
  `SO_ATTACH_FILTER`/`SO_ATTACH_REUSEPORT_CBPF` (filter pointer rewritten, 1..4096 instructions),
  `IPV6_V6ONLY` in new namespaces, `IP_TTL`/`IP_RECVERR` on UDP (raw ICMP taps),
  `SO_ORIGINAL_DST` (section 7). Everything else (including `SO_PEERCRED`, `SO_BINDTODEVICE` —
  resolved against host interface names —, `SO_PRIORITY`, `IP_TRANSPARENT`, `TCP_*`, and
  `SO_ATTACH_BPF` with its guest fd number) goes to the host and is checked against the app's host
  credentials. Netlink and AF_UNIX sockets keep their options in process (`SockOpts`).
- ioctls: `FIONREAD`/`SIOCINQ` (stub answers emulated) and `SIOCATMARK` pass through;
  `SIOCGIF*` getters pass through (hardware address fixed up), setters `EPERM`; `SIOCGIFCONF`
  appends virtual devices; interface ioctls also work on AF_UNIX and netlink sockets through a
  temporary host socket; `SIOCOUTQ`, `SIOCGSTAMP*` and others `ENOTTY`.
- Loopback in the initial namespace goes straight to the host, so other apps on the device can
  reach guest services.

### DNS stub

- `Vfs::open` writes `nameserver 127.0.0.53` when `/etc/resolv.conf` is missing (existing files,
  empty files and symlinks are left alone); the Android app overwrites Debian's at import.
- Interception: `SOCK_DGRAM` sockets in the initial namespace sending to `127.0.0.53:53` (or its
  IPv4-mapped form), by address or after connecting to it; checked before routing, so nftables
  never sees these queries; TCP and other ports are not intercepted; sockets in new namespaces
  are not either.
- Each query is registered by number in `Kernel::dns_queries` (a weak reference to the socket)
  and handed to a new host thread (`dns-stub`); the send returns the query length at once, as a
  UDP send does. The thread resolves it — Android uses `android_res_nsend` (so Private DNS, VPNs
  and per-network routing apply; 30 s limit, then cancel); other hosts send to up to three servers
  from the host's `/etc/resolv.conf` (3 s each, ID checked, TCP retry on truncation with its own
  waits) — turning failures into a synthesized SERVFAIL, then puts the answer in
  `Shared::dns_answers` and wakes the timer thread, which queues it on the socket (dropped if the
  socket has closed) with source `127.0.0.53:53` and wakes its waiters. Until then reads are
  `EAGAIN` or wait as for any datagram; readiness and `FIONREAD` follow the queue. The thread holds
  no kernel object, only the answer queue and the timer event.

### Low ports and traffic counters

- Mapping applies to TCP/UDP (MPTCP as TCP) non-raw sockets: in the initial namespace for guest
  ports 1..1023 (when the offset is nonzero), in new namespaces for every explicitly bound port
  (`bind_any`: reuse the namespace's host port or let the host pick; an AF_INET6 socket with
  `IPV6_V6ONLY` gets its own entry and host port). Destinations are rewritten by `map_dest` with
  `host_port`, which takes an IPv6 destination (as seen in the target namespace) to an IPv6-only
  entry first, then the dual-stack one, and an IPv4 destination only to the latter.
- `bind` candidates: if the key is live or was released within 120 s, bind that host port and
  return its error (so `TIME_WAIT` gives `EADDRINUSE`); otherwise the original port (unless
  `always_map`), the other protocol's host port for the same guest port, port + k×offset for
  k = 1..3, then 0. Only `EADDRINUSE` (or `EACCES` on the original port) moves to the next
  candidate. A new mapping that differs writes a kernel log line.
- Hiding: getsockname/getpeername, accept, recvfrom and `/proc/net` unmap host ports
  (`local_map`, `peer_map`, and the host→guest table, which lingers 10 s after release); connect
  and sendto to a local address with a mapped port use the host port.
- Listen events: every non-loopback listen (comm, guest port, host port), deduplicated per
  (port, host port), at most 32 queued, drained by `Machine::take_listens`; a listen dropped by a
  full queue is not marked reported, so its next listen is queued.
- Traffic: payload bytes and packets of initial-namespace sockets, per interface chosen from the
  socket's local address (wildcards resolved per call), packets estimated from the MSS for
  streams; `Protocols` feed `/proc/net/snmp` and `netstat` (IP/UDP/ICMP IPv4 only, TCP both,
  `CurrEstab` live from `TCP_INFO`, error and retransmission counters 0). DNS stub and AF_UNIX
  traffic are not counted; new namespaces show zeros. Counters are plain fields under the BKL.

### `SO_ORIGINAL_DST` and transparent proxying

- TCP connects whose destination nat rewrote register `(host local, host destination)` → the
  original destination per namespace; records survive the client's close (like conntrack) and go
  away when accept claims them (for the listener's namespace), after 120 s (pruned lazily), beyond
  4096 entries, or when a later non-nat connect reuses the 4-tuple. The inbound forwarder
  registers none.
- `getsockopt(SOL_IP|SOL_IPV6, SO_ORIGINAL_DST)`: non-TCP `ENOPROTOOPT`, short buffers
  `EINVAL`; the connecting side answers its guest peer when the namespace's nftables tracks
  connections; the accepted side answers the claimed address, or its own local address when
  tracking; otherwise `ENOENT`. No TPROXY: the nft `tproxy`/`TPROXY` names parse but never match.

### Raw ICMP

- A raw ICMP socket is a host ping socket with host `IP_RECVTTL`, `IP_PKTINFO` and `IP_RECVERR`
  (or `IPV6_RECVERR`), registered with its namespace's `Tap` (created on demand; the timer
  thread polls every Tap). Binding drops the port; getsockname reports the protocol number;
  `SO_TYPE` reports `SOCK_RAW`.
- Sending passes the ICMP message to the host unchanged (no IP header); the guest's echo id is
  recorded (one per socket, the last sent; every reply is rewritten to it); the host enforces
  echo-only and rewrites the id. Errors pending on the fd are
  dispatched first; ICMP-derived errors make the send fail (with one retry).
- UDP sockets that set a TTL are tapped lazily at their next I/O when a raw socket of the same
  family exists in the namespace and the guest has not enabled `IP_RECVERR` itself: host
  `IP_RECVERR` is turned on and the fd becomes an error-only source; connected UDP sockets sending
  elsewhere are reconnected on the host.
- Dispatch drains error queues and rebuilds whole ICMP error messages (quoted IP header, quoted
  UDP header or echo request with the guest id restored, checksums), applies Linux's
  `sk_err`/`IP_RECVERR` rules, then drains echo replies (guest id restored). Every live raw socket
  of the same family whose `ICMP_FILTER` passes gets a copy (queue capped at 212992 bytes of ICMP
  data, overflow dropped).
- Receiving prepends a synthesized IPv4 header (TTL from the host, protocol 1, translated
  source, `IP_PKTINFO` destination, checksum) or returns ICMPv6 as is; `IP_PKTINFO`/`IP_TTL`
  control messages only when enabled; `MSG_TRUNC`/`MSG_CTRUNC` as Linux; `MSG_ERRQUEUE` returns
  the socket's own errors. Readiness comes from the receiver queues (POLLOUT always).

### Netlink

- bind (Linux's `netlink_bind`): a whole `sockaddr_nl` of `AF_NETLINK` (`EINVAL`); groups need a
  protocol anyone may join (all but `NETLINK_NETFILTER`) or `CAP_NET_ADMIN` over the namespace
  owner, as do rtnetlink's multicast-routing groups (`EPERM`), and are cut to the protocol's
  count (36 for `NETLINK_ROUTE`, 32 for the others); a bound socket must keep its `nl_pid`
  (`EINVAL`); an unbound one takes the given id or, for 0, the caller's tgid in its pid namespace
  (ids are not checked for uniqueness); the groups replace the first 32 joined, and getsockname
  reports them. connect (`netlink_connect`): `AF_UNSPEC` does nothing, otherwise a whole
  `sockaddr_nl` (`EINVAL`), a port or group needing `CAP_NET_ADMIN` (`EPERM`); connect and send
  autobind an unbound socket to the tgid. shutdown does nothing; listen and accept `EOPNOTSUPP`.
- send handles each message synchronously and queues reply datagrams; recv pops one datagram
  (`MSG_PEEK` clones; oversized datagrams are truncated with `MSG_TRUNC` and the rest lost);
  source `{AF_NETLINK, 0, 0}`; with `NETLINK_PKTINFO` a pktinfo control message (group 0), then
  with `SO_PASSCRED` the kernel's credentials (pid 0, uid and gid 0 as the receiver sees them),
  cut as `put_cmsg` cuts them; waits honour `SO_RCVTIMEO`.
- Options: `SOL_SOCKET` reports the type, protocol, `AF_NETLINK`, `SO_PASSCRED`, the timeouts
  (`SO_SNDTIMEO` only recorded), `SO_ERROR`/`SO_ACCEPTCONN` 0, `SO_PEERCRED` pid 0 and uid/gid -1,
  and the `SockOpts` ones; other names are accepted and ignored by setsockopt and `ENOPROTOOPT` to
  getsockopt. `SOL_NETLINK` (value read only when a whole int): `NETLINK_ADD_MEMBERSHIP`/`DROP`
  (permission as for bind; the set is allocated first, then group 0 or beyond the count
  `EINVAL`), `NETLINK_LIST_MEMBERSHIPS` (32-bit words while they fit, optlen the whole set's size,
  0 before any group), and the flags `NETLINK_PKTINFO`, `BROADCAST_ERROR`, `NO_ENOBUFS`,
  `LISTEN_ALL_NSID` (`CAP_NET_BROADCAST` over the namespace owner), `CAP_ACK`, `EXT_ACK`,
  `GET_STRICT_CHK` (reading one needs room for an int, `EINVAL`); only `NETLINK_CAP_ACK` and
  `NETLINK_EXT_ACK` have effects (on device-layer change errors and nftables, not on query or
  generic-netlink errors); other names and levels `ENOPROTOOPT`. No multicast notifications are
  ever sent (mq_notify cookies are the only asynchronous messages).
- `NETLINK_ROUTE`: change requests (`NEW`/`DEL`/`SET` for links, addresses, routes) need
  `CAP_NET_ADMIN` over the namespace owner and go to `kernel/netns`'s device layer; other change
  types Linux 6.1 knows (up to `RTM_MAX`) are `EPERM` without it and `EOPNOTSUPP` with it (host
  interfaces cannot be changed; requests that change nothing succeed). Queries: link dumps
  (filtered by master and kind), single links by index or name (`EINVAL`, `ENODEV`), address
  dumps (per family, IPv4 first), route dumps (per family) and `ip route get`, rule dumps (one
  datagram), nsid; link, address and route dumps are packed into datagrams of at most 3776 bytes
  (Linux's `NLMSG_GOODSIZE` on 4 KiB pages, which bounds a dump's first datagram), so readers
  with a page-sized buffer (Go's `syscall.NetlinkRIB`, behind `net.Interfaces`) get whole
  messages; dumps are not resumable, so a datagram larger than the reader's buffer is lost;
  neighbour dumps are empty; others `EOPNOTSUPP`. In the initial namespace interfaces and
  addresses come from the host's `getifaddrs` (MTU fixed, MAC from `AF_PACKET` entries); new
  namespaces see only their own devices.
- `NETLINK_GENERIC`: `nlctrl` only, modelled on 6.8 (unknown families `ENOENT`, `GETPOLICY`
  `EOPNOTSUPP`). `NETLINK_NETFILTER` goes to nftables. `NETLINK_KOBJECT_UEVENT` never receives.

### Routing table

- Main table: the host's routes from `Machine::set_routes` (Android: the default network's
  `LinkProperties.getRoutes()`, an empty list on network loss), else the host's
  `/proc/net/route` and `/proc/net/ipv6_route`, else synthesized from interface addresses with a
  default route on the interface the host would use. Local table: local, broadcast, loopback,
  IPv6 local and multicast entries derived from addresses. Virtual-device routes are merged in;
  new namespaces have no host main table.
- `ip route get`: local-table hits answered in process; virtual routes and new namespaces choose
  the source themselves; otherwise the host chooses the source by connecting a UDP socket, and the
  interface and gateway follow; a 464xlat fallback covers hosts with no main route; `RTA_IIF`
  `EOPNOTSUPP`. Rules: the default 0/32766/32767 (plus multicast routing tables). `/proc/net/route`
  and `ipv6_route` render the tables.

## Dependencies (inbound / outbound)

### Inbound

- The syscall dispatcher; `kernel/vfs` (read/write/ioctl on sockets, `Vfs::open` writing
  `resolv.conf`); `kernel/poll`; `kernel/procfs` (`/proc/net`, sysfs counters); the machine
  (timer thread `check_taps`, `set_routes`, `take_listens`); `kernel/netns` (forward targets,
  netlink device requests); the message-queue code (mq_notify cookies on netlink sockets).

### Outbound

- `appine_hal::net` (host sockets, `dns_query`, `interfaces`, `routes`, `source_for`, epoll
  `Watch`); `kernel/netns` (`to_host`, `from_host`, `implicit_bind`, nat, filter, listener
  registration), `nft`, `netdev`, `forward`; `kernel/unix`; credentials (`ns_capable`); the
  kernel's waits (`wait_objects`, `unlocked`), wake objects; signals (`SIGPIPE`); the kernel log.

## Invariants and gotchas

- Host sockets are always nonblocking; guest blocking is emulated, and every wait must release
  the BKL.
- Addresses are rewritten at the boundary in both directions (namespaces, ports, nat, DNS stub);
  anything returning an address to the guest must go through the same unmapping.
- Netlink replies are snapshots taken at send time.
- A bind must not reach the host with `AF_UNSPEC`: Ubuntu's 6.8 kernels (colima's 6.8.0-117) oops
  in AppArmor's `aa_inet_bind_perm` on one from a confined process, and the host thread dies
  holding the BKL, hanging every guest process. bind rewrites the family to `AF_INET` first.
- The portmap key has no address, so binds of one guest port share one host port and the host
  decides their conflicts, except that IPv6-only binds in new namespaces get their own.
- The raw ICMP id rewrite assumes one echo id per raw socket (the last one sent).
- A `MSG_PEEK|MSG_WAITALL` wait changes the host socket's `SO_RCVLOWAT` while the BKL is released,
  so another thread polling the same socket meanwhile sees the raised low-water mark.

## Confirmed bugs / technical debt

- **An error after part of a `MSG_WAITALL` read is lost:** the partial count is returned, but the
  host has already taken the error (a reset, say), so the next read sees end of file where Linux
  reports the error then.
- **Netlink dumps are not resumable:** a reader whose buffer is smaller than a dump datagram (at
  most 3776 bytes, a whole rule dump in one) gets `MSG_TRUNC` and loses the rest, where Linux
  sizes later datagrams to the reader's buffer and continues the dump on the next recv.
- **Netlink memberships and ids are only recorded:** joined groups never receive anything, so
  monitors subscribe and wait forever; `NETLINK_CAP_ACK`/`EXT_ACK` are ignored for query and
  generic-netlink errors; ids are not checked for uniqueness, so a second socket of a process
  autobinds to the same tgid (Linux picks a negative number) and an explicit id another socket
  holds is not `EADDRINUSE`.
- **Option passthrough uses host semantics:** privileged options (`SO_PRIORITY` above 6,
  `IP_TRANSPARENT`) are judged by the app's host credentials (a privileged guest gets `EPERM`);
  `SO_BINDTODEVICE` names are resolved against host interfaces; `SO_ATTACH_BPF` passes a guest fd
  number to the host.
- **Raw ICMP keeps one echo id per socket:** every reply is rewritten to the last id sent, so
  programs using several ids on one socket (liboping) see them collapse; the queue cap ignores
  `SO_RCVBUF` and drops silently.
- **New-namespace loopback statistics leak the host's:** sysfs `lo/statistics/*` in a new network
  namespace reads the initial namespace's loopback counters, while `/proc/net/dev` and netlink show
  zeros there.
- **Netlink requests are not validated as Linux validates them:** `NETLINK_GET_STRICT_CHK` is
  accepted and ignored (no header checks, no header-based dump filtering), attributes of the wrong
  size are skipped instead of rejected (`ERANGE`; `EINVAL` for a short IPv6 `RTA_DST` of `ip route
  get`, while a longer one is used by its first bytes), and a single IPv6 `RTM_GETADDR` is
  `EOPNOTSUPP` without Linux's header checks (`EINVAL` for a short body); malformed or mis-typed
  requests, such as a route dump carrying an `ifinfomsg` or an `ip route get` with a short
  `RTA_FLOW`, are answered with data where Linux returns an error.
- **Single `RTM_GETNEIGH` requests always answer `ENOENT`:** the header and address are never
  checked, so a bad state or type, a missing or wrong-size `NDA_DST` (`EINVAL`), an unknown family
  (`EAFNOSUPPORT`) and an unknown interface index (`ENODEV`) are all reported as "not found", and
  `ip neigh get` with a wrong device or address family prints a misleading error.
- **Netlink destination addresses are ignored:** sendto/sendmsg on a netlink socket never looks at
  `msg_name`, so a message addressed to another port or to multicast groups is handled as a kernel
  request, without `netlink_sendmsg`'s checks (`EINVAL` for a short or non-`AF_NETLINK` address,
  `EPERM` for sending to a group or port without `CAP_NET_ADMIN` on rtnetlink).
- **A wildcard connect from a bound IPv4 socket reports the loopback:** connecting to `0.0.0.0`
  gives getpeername `127.0.0.1` even when the socket is bound to an address of its own, which
  Linux's route lookup would take instead.
- **A raw ICMP socket shut down for reading still waits in recv:** the aggregation point's receive
  path does not look at the read shutdown, so a blocking recv with nothing queued sleeps until a
  reply, a timeout or a signal, where Linux's `skb_recv_datagram` returns 0 at once (and wakes
  receivers already waiting); only internet datagram sockets on host sockets return 0.
- **Odd destinations reach the host unchecked:** sendto hands a guest address the layer does not
  understand to the host socket as is, so the host kernel's own checks answer instead of Linux
  6.1's; under AppArmor that is `EAFNOSUPPORT` for a bogus family where Linux says `EINVAL`.
- **A raw socket cannot be bound to another address:** Linux's `raw_bind` lets a raw socket be
  bound any number of times; the host ping socket that emulates a raw ICMP socket refuses a second
  bind, so binding it again succeeds only for the address it already has (`EINVAL` otherwise).

## Open questions

- Should container namespaces reach the DNS stub (for example through the gateway address), and
  should nftables see stub queries?
- Should AF_PACKET report `EPERM` (Linux without `CAP_NET_RAW`) instead of `EAFNOSUPPORT`?
- Why is `nlctrl` modelled on 6.8 while the contract is 6.1?

## To verify

- "Byte-identical to Linux" for address, route and rule messages: only generic netlink has a
  differential test; link messages omit `IFLA_AF_SPEC` and use a fixed MTU.
- Host behaviour for passed-through options (`SO_PRIORITY`, `IP_TRANSPARENT`, `SO_PEERCRED` on
  inet) and that the host ping socket rewrites the id and rejects non-echo types.
- `SO_ORIGINAL_DST` for a DNAT whose target is in the initial namespace (Linux answers the
  pre-DNAT address; here the forwarder records nothing).
- On a real phone, that other apps can reach guest services on loopback (the design expected it
  and wanted it documented for users; not yet confirmed on a device).
