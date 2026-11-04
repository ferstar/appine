# kernel/net Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/net.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 15 confirmed and 3 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: socket timeouts of at least `KTIME_MAX` mean none; IPv6-only port map entries in new namespaces)

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
  ICMP-tap state, portmap `lease`/`local_map`/`peer_map`, cached traffic route. Guest
  `O_NONBLOCK` lives on the open file.
- `Message` (one recv result), `Routed` (connect/sendto routing result: namespace map, nat,
  filter verdict, original destinations).
- `NetlinkSocket` — protocol, `nl_pid`, datagram queue, arrival counter, `rcvtimeo`, namespace,
  `ext_ack`/`cap_ack`; `Host` trait / `KernelHost` (interfaces, links, tables, routes, stats,
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
- `socketpair` is AF_UNIX only (inet and netlink `EOPNOTSUPP`, others `EAFNOSUPPORT`/
  `EPROTONOSUPPORT`).

### Binding, listening, connecting, accepting

- bind: sockaddr over 128 bytes `EINVAL`; raw sockets drop the port; the namespace translation
  runs; ports below `ip_unprivileged_port_start` need `CAP_NET_BIND_SERVICE` over the namespace
  owner (`EACCES`); then portmap (section 6) or a plain host bind; namespace registration and
  forwarding hooks follow.
- listen binds implicitly if needed, listens on the host, registers the listener in a new
  namespace and with the forwarder, and reports non-loopback listens to the app.
- connect: a UDP connect to the DNS stub only records `stub_peer`. Otherwise `route()` applies
  nat, namespace translation and the filter chains (`kernel/netns`); a TCP reject returns its
  errno, a drop connects to a blackhole listener, no listener in the target namespace is
  `ECONNREFUSED`; then implicit bind, portmap destination mapping and the host connect.
  Nonblocking returns the host's `EINPROGRESS`; blocking waits for `POLLOUT` with the BKL
  released (no `SO_SNDTIMEO` deadline, `EINTR` on a signal, never restarted; a repeated connect
  then gets the host's `EALREADY`), then reads `SO_ERROR` and connects again so the host reports
  `EISCONN`. TCP connects register `SO_ORIGINAL_DST` records before the BKL is released.
- accept4: bad flags `EINVAL`; host accept with waits bounded by the listener's `SO_RCVTIMEO`
  (`EAGAIN` on timeout); the new socket inherits protocol, local port map, namespace state,
  mark and the claimed original destination (but not the kernel-layer timeouts); the peer address
  is unmapped and translated; the fd is installed before the address is written back (a failed
  write leaves it installed), and `EMFILE` comes after the connection was already taken.

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
  `EAGAIN` waits (`SO_RCVTIMEO`, `ERESTARTSYS` on signals). `MSG_WAITALL` loops only while the
  host has data and returns a partial result when it runs dry; with `MSG_PEEK` each pass peeks the
  head of the queue again, filling the buffer with repeated copies.
- Limits: at most 1 MiB per call, 1024 iovecs (`EMSGSIZE`), 4096 bytes of send control data;
  recvmsg sums the guest's iovec lengths before clamping (unchecked); setsockopt values are cut to
  256 bytes (longer ones then fail on the host) and getsockopt asks the host for at most 256.
- Host sendmsg/recvmsg run under the BKL (nonblocking); only waits and the DNS query release it.
  There is no BKL-free fast path for sockets.

### Options and ioctls

- Emulated: `SO_RCVTIMEO`/`SO_SNDTIMEO` (the old numbers; short buffers, negative times or times
  of at least `KTIME_MAX` mean no timeout, the last read back as 0 like Linux's
  `MAX_SCHEDULE_TIMEOUT`), `SO_MARK` (needs `CAP_NET_RAW` or `CAP_NET_ADMIN` over the namespace
  owner; stored only, inherited by accepted sockets, fed to nftables, cleared when entering
  another namespace), `SO_TYPE` (`SOCK_RAW` for raw sockets), `SO_ERROR` (pending ICMP errors
  first),
  `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE` (need `CAP_NET_ADMIN`, then become the plain options),
  `SO_ATTACH_FILTER`/`SO_ATTACH_REUSEPORT_CBPF` (filter pointer rewritten, 1..4096 instructions),
  `IPV6_V6ONLY` in new namespaces, `IP_TTL`/`IP_RECVERR` on UDP (raw ICMP taps),
  `SO_ORIGINAL_DST` (section 7). Everything else (including `SO_PEERCRED`, `SO_BINDTODEVICE` —
  resolved against host interface names —, `SO_PRIORITY`, `IP_TRANSPARENT`, `TCP_*`,
  `SO_RCVTIMEO_NEW`/`SO_SNDTIMEO_NEW`, and `SO_ATTACH_BPF` with its guest fd number) goes to the
  host and is checked against the app's host credentials.
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
- The query runs synchronously in the caller's thread with the BKL released, even for nonblocking
  sockets and `MSG_DONTWAIT` and regardless of `SO_SNDTIMEO`: Android uses `android_res_nsend`
  (so Private DNS, VPNs and per-network routing apply; 30 s limit, then cancel); other hosts send
  to up to three servers from the host's `/etc/resolv.conf` (3 s each, ID checked, TCP retry on
  truncation with its own waits). Failures become a synthesized SERVFAIL. The answer is
  queued on the socket with source `127.0.0.53:53`, readiness and `FIONREAD` follow the queue,
  and the send returns the query length.

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
  (port, host port), at most 32 queued, drained by `Machine::take_listens`.
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

- bind takes `nl_pid` (0 → the caller's tgid in its pid namespace; explicit ids unchecked;
  rebinding allowed; groups ignored); unbound sockets keep id 0 (no autobind); connect and
  shutdown do nothing; listen and accept `EOPNOTSUPP`.
- send handles each message synchronously and queues reply datagrams; recv pops one datagram
  (`MSG_PEEK` clones; oversized datagrams are truncated with `MSG_TRUNC` and the rest lost);
  source `{AF_NETLINK, 0, 0}`; waits honour `SO_RCVTIMEO`. Only `SO_RCVTIMEO`, `NETLINK_CAP_ACK`
  and `NETLINK_EXT_ACK` have effects (the ack flags apply to device-layer change errors and to
  nftables, not to query or generic-netlink errors); other options are accepted, and getsockopt
  returns 0 for everything but `SO_RCVTIMEO` (`SO_TYPE`, `SO_DOMAIN`, `SO_RCVBUF` read 0). No
  multicast notifications are ever sent (mq_notify cookies are the only asynchronous messages).
- `NETLINK_ROUTE`: change requests (`NEW`/`DEL`/`SET` for links, addresses, routes) need
  `CAP_NET_ADMIN` over the namespace owner and go to `kernel/netns`'s device layer (host
  interfaces cannot be changed; requests that change nothing succeed). Queries: link dumps
  (split at 8 KiB; filtered by master and kind), single links by index or name (`EINVAL`,
  `ENODEV`), address dumps (one datagram per family), route dumps and `ip route get`, rule dumps,
  nsid; dumps are not resumable, so a datagram larger than the reader's buffer is lost; neighbour
  dumps are empty; others `EOPNOTSUPP`. In the initial namespace interfaces and addresses come
  from the host's `getifaddrs` (MTU fixed, MAC from `AF_PACKET` entries); new namespaces see only
  their own devices.
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
- The portmap key has no address or family, so IPv4/IPv6 and wildcard/specific binds of one guest
  port share one host port and the host decides conflicts.
- The raw ICMP id rewrite assumes one echo id per raw socket (the last one sent).

## Confirmed bugs / technical debt

- **DNS queries block nonblocking sockets:** a send to the stub resolves synchronously in the
  sending thread even with `O_NONBLOCK`/`MSG_DONTWAIT` and ignores `SO_SNDTIMEO` (up to 30 s on
  Android). Event-loop resolvers freeze their loop for the whole lookup (c-ares and with it Node's
  `dns.resolve*`); their own timeouts and parallel queries stop working.
- **`MSG_PEEK|MSG_WAITALL` returns duplicated data** on stream sockets: each pass peeks the queue
  head again into the next part of the buffer, so 10 queued bytes and a 100-byte read return the
  same 10 bytes ten times. Without `MSG_PEEK`, `MSG_WAITALL` returns short reads instead of
  waiting, and requests above 1 MiB come back short.
- **Blocking connect ignores `SO_SNDTIMEO` and is never restarted:** it waits without a deadline,
  returns `EINTR` even under `SA_RESTART`, and a retried connect then fails with `EALREADY` where
  Linux keeps waiting (and times out with `EINPROGRESS`).
- **accept4 leaks fds and drops connections:** the fd is installed before the peer address is
  written, so a bad address pointer returns an error with the fd left open; `EMFILE` is reported
  after the connection was taken from the queue and closed. Accepted inet sockets also lose the
  listener's `SO_RCVTIMEO`/`SO_SNDTIMEO`.
- **recvmsg iovec lengths are summed unchecked:** two large `iov_len` values overflow the sum (a
  panic in debug builds, a wrong short read in release); Linux rejects negative lengths with
  `EINVAL`.
- **Netlink dumps are not resumable:** address, route and rule dumps are one datagram per family
  and link dumps use 8 KiB chunks; a reader with a smaller buffer gets `MSG_TRUNC` and loses the
  rest (Linux continues the dump on the next recv).
- **Netlink socket options misreport:** getsockopt returns 0 for `SO_TYPE`, `SO_DOMAIN`,
  `SO_RCVBUF` and the ack flags; unknown options and `NETLINK_ADD_MEMBERSHIP` succeed silently, so
  monitors subscribe and never hear anything; `NETLINK_CAP_ACK`/`EXT_ACK` are ignored for query
  and generic-netlink errors; unbound sockets keep `nl_pid` 0 and explicit ids are not checked for
  uniqueness.
- **Option passthrough uses host semantics:** privileged options (`SO_PRIORITY` above 6,
  `IP_TRANSPARENT`) are judged by the app's host credentials (a privileged guest gets `EPERM`);
  `SO_BINDTODEVICE` names are resolved against host interfaces; `SO_ATTACH_BPF` passes a guest fd
  number to the host; `SO_RCVTIMEO_NEW`/`SO_SNDTIMEO_NEW` have no effect.
- **Raw ICMP keeps one echo id per socket:** every reply is rewritten to the last id sent, so
  programs using several ids on one socket (liboping) see them collapse; the queue cap ignores
  `SO_RCVBUF` and drops silently.
- **New-namespace loopback statistics leak the host's:** sysfs `lo/statistics/*` in a new network
  namespace reads the initial namespace's loopback counters, while `/proc/net/dev` and netlink show
  zeros there.
- **Listen events can be lost:** the "already reported" set is marked before the 32-event queue
  check and never cleared, so a listen dropped by a full queue is never reported.

## Open questions

- Should container namespaces reach the DNS stub (for example through the gateway address), and
  should nftables see stub queries?
- Should AF_PACKET report `EPERM` (Linux without `CAP_NET_RAW`) instead of `EAFNOSUPPORT`?
- Why is `nlctrl` modelled on 6.8 while the contract is 6.1?

## To verify

- Whether glibc's `getifaddrs` (page-sized receive buffer, fails on `MSG_TRUNC`) breaks on phones
  with enough interfaces for one 8 KiB link-dump chunk to exceed 4 KiB.
- "Byte-identical to Linux" for address, route and rule messages: only generic netlink has a
  differential test; link messages omit `IFLA_AF_SPEC` and use a fixed MTU.
- Host behaviour for passed-through options (`SO_PRIORITY`, `IP_TRANSPARENT`, `SO_PEERCRED` on
  inet) and that the host ping socket rewrites the id and rejects non-echo types.
- `SO_ORIGINAL_DST` for a DNAT whose target is in the initial namespace (Linux answers the
  pre-DNAT address; here the forwarder records nothing).
- On a real phone, that other apps can reach guest services on loopback (the design expected it
  and wanted it documented for users; not yet confirmed on a device).
