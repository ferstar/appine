# Socket layer (kernel/net) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/net"`
>
> Last distilled: 2026-11-08 (HEAD = fffe1c1). D1–D8 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: TCP and UDP pass through to host sockets
- **What**: Each guest internet socket owns a nonblocking host socket; addresses, most options and control messages are passed byte for byte, and blocking is emulated in the kernel layer's waits (interruptible by signals, `SO_RCVTIMEO`/`SO_SNDTIMEO` timed by the kernel layer).
- **Why**: Guest and host share the arm64 Linux ABI, so the host's TCP/IP stack can serve the guest directly without an own network stack or privileges; emulated blocking keeps guest threads interruptible and the BKL free while they wait.
- **Tradeoffs**: Host restrictions apply to the guest (no raw sockets, no marks, privileged options checked against the app); no tun device, since that would need a user-space TCP/IP stack.
- **Watch out**: Every path that returns addresses to the guest must undo the boundary rewrites (namespaces, ports, nat, DNS stub).
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`); tun rejected in round 180 (`180-original-dst`)

### D2: Raw ICMP is emulated on unprivileged ping sockets
- **What**: A guest raw ICMP/ICMPv6 socket is a host ping socket; a per-namespace aggregation point collects echo replies and `IP_RECVERR` errors (including those of TTL-limited UDP probes) and rebuilds what raw sockets see — IPv4 headers, restored echo ids, full ICMP error messages — for every raw socket of the family.
- **Why**: The app has no `CAP_NET_RAW`; busybox `ping` as root, `ping6`, traceroute and mtr use raw sockets and received nothing from bare ping sockets (no IP header, rewritten ids, no ICMP errors).
- **Tradeoffs**: Only echo requests can be sent; other raw protocols stay `EPERM` as on Android.
- **Watch out**: The timer thread must keep polling the aggregation points so raw sockets become readable.
- **Source**: rounds 125 (`125-raw-icmp`), 128 (`128-traceroute`)

### D3: Netlink is answered in process; unimplemented protocols cannot be created
- **What**: `NETLINK_ROUTE` replies are built from the host's `getifaddrs`, the synthesized routing table and the virtual devices, matching Linux's message layout; `NETLINK_GENERIC` has `nlctrl`; `NETLINK_KOBJECT_UEVENT` sockets never receive; any other protocol is `EPROTONOSUPPORT`.
- **Why**: Apps cannot use the host's netlink fully on Android. Creating sockets for unimplemented protocols made PAM's libaudit wait 40 s for an ACK and fail every Debian/Ubuntu login; `EPROTONOSUPPORT` is what Linux returns without the subsystem. Generic netlink always exists on Linux, and programs (iw, ethtool, wg, iotop) expect the socket to open and then look up their family.
- **Tradeoffs**: No multicast notifications; host interfaces and routes cannot be changed.
- **Watch out**: Single (non-dump) `RTM_GETLINK`/`RTM_GETROUTE` requests are used by iproute2 and scripts and must be answered.
- **Source**: rounds 06 (`06-m4`), 83 (`83-netlink-protocols`), 91 (`91-generic-netlink`), 101 (`101-link-addr`)

### D4: The routing table is synthesized like Linux's, from routes the app passes in
- **What**: Main and local tables are built as Linux builds them without custom rules; on Android the main table comes from the default network's `LinkProperties.getRoutes()` through `Machine::set_routes`, elsewhere from the host's `/proc/net/route`; `ip route get` lets the host choose the source address.
- **Why**: `ip route`, `route -n`, `netstat -r` and the common `ip route get 1.1.1.1` idiom for finding the primary address returned nothing or `EOPNOTSUPP`; Android apps cannot read the host's routing table.
- **Tradeoffs**: Routes are only as fresh as the app's network callbacks.
- **Watch out**: An empty route list empties the main table instead of falling back to synthesis.
- **Source**: round 98 (`98-routes`)

### D5: DNS goes through a stub at 127.0.0.53 answered by the host resolver
- **What**: `resolv.conf` points at `127.0.0.53`; UDP queries to it are intercepted in the socket layer and resolved by `android_res_nsend` (other hosts forward to the host's servers) on a `dns-stub` host thread per query, whose answer or SERVFAIL the timer thread queues on the socket.
- **Why**: Going through the system resolver makes Android's Private DNS, VPNs and per-network routing apply to the guest; failing fast with SERVFAIL stops resolvers from waiting for their timeouts.
- **Tradeoffs**: Each query starts a host thread and answers arrive in resolution order, not send order; TCP DNS and container namespaces are not intercepted.
- **Watch out**: Keep `resolv.conf` written only when missing so user edits survive; the DNS threads hold only the answer queue and the timer event, never the kernel, which they could outlive.
- **Source**: rounds 06 (`06-m4`), 07 (`07-m5`); fe4bb2b

### D6: Low ports first try the real port, then fixed offsets, hidden from the guest
- **What**: Binding a port below 1024 tries the port itself on the host, then +10000, +20000, +30000, then a host-chosen port; the guest always sees its own port, and the app is told about the first non-loopback listen.
- **Why**: The app lacks `CAP_NET_BIND_SERVICE`, so sshd, nginx, caddy and dnsmasq failed on their default ports. Fixed offsets (not +1) keep neighbouring ports free and make mappings predictable; the user agreed this design, including the notification. Google Play system updates (May 2026) opened 22, 80, 443 and a few others, so the real port is tried first.
- **Tradeoffs**: Other devices must use the mapped port, which the user only learns from the notification or the kernel log.
- **Watch out**: A port released within 120 s keeps its host port, so `TIME_WAIT` shows as `EADDRINUSE` rather than a silent remap.
- **Source**: rounds 156 (`156-low-ports`), 208 (`208-low-ports-direct`)

### D7: The kernel layer counts its own traffic
- **What**: Payload bytes and packets of guest sockets are counted per interface and per protocol and reported by `/proc/net/dev`, sysfs `statistics/`, netlink stats and `/proc/net/snmp`.
- **Why**: `/proc/net/dev` was all zeros (ifconfig, htop, nload, btop); apps cannot read the host's counters on Android, and those would include other apps' traffic anyway.
- **Tradeoffs**: Errors, retransmissions and header bytes are not visible; new namespaces show zeros.
- **Watch out**: Count on both send and receive paths, skipping `MSG_PEEK` and the error queue.
- **Source**: round 90 (`90-interface-counters`)

### D8: `SO_ORIGINAL_DST` is answered from records the socket layer keeps
- **What**: Connects rewritten by nat record the original destination keyed by the host 4-tuple; accept claims it; `SO_MARK` is stored on the socket and only used by nftables.
- **Why**: Transparent proxies (`sslocal --protocol redir`, redsocks, clash) ask `SO_ORIGINAL_DST` after accepting; nat happens in Appine's socket layer, so the host has no conntrack entry and answered `ENOENT`. The app cannot set host marks.
- **Tradeoffs**: TCP only; no TPROXY.
- **Watch out**: Register the record before releasing the BKL so the peer cannot accept first.
- **Source**: round 180 (`180-original-dst`)

### D9: Internet and netlink sockets are one kernel/net module
- **What**: kernel/net covers `net.rs`, `syscall/net.rs`, raw ICMP, the routing table, the port map, traffic counters and `SO_ORIGINAL_DST`, with host socket wrappers in hal and the namespace hooks described from kernel/netns's side.
- **Why**: The retired design doc's §10 rows for sockets, ICMP, netlink, routes, DNS, low ports and counters had drifted and omitted their limits.
- **Tradeoffs**: The map's Linux comparisons come from knowledge of the kernel sources.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them an error after part of a `MSG_WAITALL` read being lost, non-resumable netlink dumps, unvalidated rtnetlink requests, netlink memberships that never receive and destination addresses that are ignored, one echo id per raw ICMP socket, raw ICMP sockets that cannot be bound to another address or woken by a read shutdown, odd sendto destinations reaching the host unchecked and the host's counters in a new namespace's loopback statistics.
- **Source**: c3df5d3 (bug list since updated by 58ce3df, 4e64e74, ce8a9ce, 5d23297 and 5d7b936)

### D10: Socket timeouts of KTIME_MAX or more mean none
- **What**: An `SO_RCVTIMEO` or `SO_SNDTIMEO` of at least KTIME_MAX means no timeout and reads back as 0, like Linux's `MAX_SCHEDULE_TIMEOUT`.
- **Why**: `LONG_MAX` seconds, a common "never" value, overflowed the receive and send deadlines.
- **Tradeoffs**: Values between KTIME_MAX (about 9.2e9 s) and Linux's `CONFIG_HZ`-dependent cut-off read back as 0 where Linux returns them.
- **Watch out**: None known; kernel/unix uses the same parser.
- **Source**: 96cd8bd

### D11: Namespace port-map entries tell IPv6-only binds apart
- **What**: The port map's key gains an IPv6-only flag (AF_INET6 with `IPV6_V6ONLY` and a non-mapped address), so such binds get their own host port, and `host_port` prefers that entry for a real IPv6 destination and never for IPv4.
- **Why**: Keyed by namespace, protocol and guest port only, an IPv6-only and an IPv4 socket on one port (nginx's usual pair) got the same host port and their dual-stack host sockets conflicted.
- **Tradeoffs**: An IPv6-only `[::]:P` and a dual-stack `[::]:P` now both bind where Linux refuses the second, and binds on different guest addresses that share a host address are still judged by host address.
- **Watch out**: A new caller of `host_port` must pass the destination's family, or an IPv6 client could reach the IPv4 socket's host port.
- **Source**: ea08993

### D12: Socket files read in a socket's life order
- **What**: syscall/net.rs runs creation, binding to accepting, names, options, sending and receiving (shutdown last), netlink, ioctls, readiness, routing, forwarding, low ports and shared helpers, and net.rs, rawicmp.rs, route.rs, traffic.rs and portmap.rs put callers before helpers, with net.rs's `IPPROTO_*` constants at the top and /proc/net last.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; syscall/net.rs kept its socket calls in no particular order and most of net.rs's helpers sat above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `ensure_bound`, `register_udp` and `count_connect` stay with binding although sending and options use them too.
- **Watch out**: Code appended at the end of a file drifts from this order, and a constant paragraph shared by several sections must stay above its first user (`IPPROTO_*` was once carried to the end and moved back by hand).
- **Source**: 412ffba

### D13: Socket read, write, readv and writev are one recv or send
- **What**: `net::rw` does one recv of the iovecs' total (capped at `MSG_MAX`) scattered in order, and gathers writes into one send for message sockets or 1 MiB sends for streams until done or short (kernel/vfs D22).
- **Why**: Linux's `sock_read_iter`/`sock_write_iter` make each call one recvmsg or sendmsg, while Appine wrote 64 KiB chunks per iovec and capped reads at 64 KiB.
- **Tradeoffs**: A message over 1 MiB is cut to `MSG_MAX`, as sendmsg already did.
- **Watch out**: A blocking stream write of many MiB now goes in 1 MiB sends, which shows only in partial progress under signals.
- **Source**: b295644

### D14: MSG_WAITALL waits for the whole length on TCP
- **What**: recv keeps reading and waiting between pieces until it has the length or meets nonblocking, a timeout, a signal, end of file or a host error, a peek re-reads from the head with the host's `SO_RCVLOWAT` raised for the wait, and recvfrom and recvmsg continue past 1 MiB piece by piece.
- **Why**: Fixed-size readers using MSG_WAITALL got short reads, and `MSG_PEEK|MSG_WAITALL` returned corrupted data, once data arrived in more than one piece.
- **Tradeoffs**: An error after a partial read is lost (the host already took it), a peek against a buffer that can never hold the length returns the part where Linux blocks, and AF_UNIX is not covered.
- **Watch out**: Another thread polling the socket sees the raised `SO_RCVLOWAT` while a peek waits, and a fault on a later piece loses that piece's data.
- **Source**: 58ce3df

### D15: recvmsg and sendmsg check iovecs in `import_iovec`'s order
- **What**: A negative length is `EINVAL`, an iovec reaching past 2^48 (arm64's `TASK_SIZE_MAX`) is `EFAULT`, and the total is cut to `MAX_RW_COUNT` by shortening the iovec that reaches it.
- **Why**: recvmsg summed the guest's iovec lengths unchecked and accepted negative lengths.
- **Tradeoffs**: `TASK_SIZE_MAX` is fixed at 48-bit addresses, where a 52-bit Linux accepts up to 2^52.
- **Watch out**: sendmsg shares `read_msghdr` and returns the same new errors.
- **Source**: 09188ec

### D16: Blocking connect honours `SO_SNDTIMEO` and restarts after signals
- **What**: The host's `EALREADY` on a blocking socket joins the wait, a timeout returns the host's own answer (`EINPROGRESS` or `EALREADY`), and a signal is `ERESTARTSYS` without a timeout and `EINTR` with one.
- **Why**: Programs bound connects with `SO_SNDTIMEO` or rely on `SA_RESTART` handlers (SIGWINCH on resize, SIGCHLD) not to break a blocking connect, and Appine ignored the first and broke the second.
- **Tradeoffs**: A restarted connect redoes routing and nat before the host says `EALREADY`, though nothing is sent twice.
- **Watch out**: None known.
- **Source**: 5296a0a

### D17: A listen counts as reported only once it is queued
- **What**: The port map checks the 32-event queue's room before marking a listen reported, so a listen dropped while the queue was full is reported when the program listens again.
- **Why**: Marking it first lost the listen for good, so the app was never told about its mapped port.
- **Tradeoffs**: A dropped listen is reported only if the program listens on it again.
- **Watch out**: None known.
- **Source**: 6adbd6c

### D18: Inet binds check their family before the host sees it
- **What**: `inet_bind_addr` checks every inet bind first: a sockaddr shorter than the family's is `EINVAL`, IPv4 takes `AF_INET`, or `AF_UNSPEC` with `INADDR_ANY` rewritten to `AF_INET`, and otherwise `EAFNOSUPPORT`, raw IPv4 ignores the family, and IPv6 takes only `AF_INET6` (`EINVAL` for raw, `EAFNOSUPPORT` otherwise), so the host only ever sees `AF_INET` or `AF_INET6`.
- **Why**: An `AF_UNSPEC` bind passed to the host oopsed the development VM's kernel and hung Appine, and other bogus families got the host's (AppArmor's) error instead of Linux 6.1's.
- **Tradeoffs**: Rebinding a raw socket to another address, which `raw_bind` allows, is still refused by the host ping socket underneath (D22), and sendto's destination is still not checked by the layer.
- **Watch out**: Programs relying on the host accepting an odd bind family now get Linux's error, and differential cases that bind with `AF_UNSPEC` need an unconfined native side, since a confined one can still hit the AppArmor oops.
- **Source**: 0b2c3c8

### D19: Netlink dumps fit in a page, and UDP receives end after a read shutdown
- **What**: One `pack` helper fills link, address and route dump datagrams up to 3776 bytes (Linux's first dump datagram on 4 KiB pages), keeping one run per family and `NLMSG_DONE` alone; `SHUT_RD` or `SHUT_RDWR` sets `InetSocket::rd_shut`, also when the host answers `ENOTCONN` as Linux's `inet_shutdown` does, and a blocking recv on a datagram socket with nothing queued then returns 0 before timeouts and signals.
- **Why**: Link dumps came in 8 KiB datagrams that page-sized readers (Go's `net.Interfaces`) cannot read once there are about nine interfaces, and a blocking UDP recv after a read shutdown looped between the host's `EAGAIN` and a readable poll at full CPU, hanging programs that wake receiver threads with shutdown.
- **Tradeoffs**: Readers with large buffers get more datagrams than Linux sends them after the first, dumps are still not resumable for buffers under 3776 bytes, and raw ICMP sockets still wait after a read shutdown (recorded in the map).
- **Watch out**: The read shutdown is a guest-side flag, so a host socket shut down some other way is not seen; a reader that assumes one datagram per family would break (none known).
- **Source**: b3e134a

### D20: accept4 installs only a complete socket, and socket timeouts are parsed strictly
- **What**: accept4 finds a free fd slot before taking a connection and writes the address before installing the fd (a fault drops the socket), accepted sockets copy the listener's `SO_RCVTIMEO` and `SO_SNDTIMEO`, and the timeout parser returns `EINVAL` for short values, `EDOM` for out-of-range microseconds and a zero timeout for negative seconds.
- **Why**: accept4 installed the fd before writing the address and took a connection before finding no free fd, accepted sockets lost the listener's timeouts, and the lenient parser let a negative timeout block forever.
- **Tradeoffs**: None.
- **Watch out**: Programs passing bad timeouts now see `EDOM` or `EINVAL`.
- **Source**: 5d23297

### D21: A refused blocking connect leaves the host socket ready for another connect
- **What**: When a blocking connect fails, the failure branch connects the host socket once more and ignores the answer (`ECONNABORTED`, nothing sent).
- **Why**: After a refused blocking connect the host socket stayed connecting, so the guest's next connect on it got `ECONNABORTED` where Linux sends a new SYN.
- **Tradeoffs**: One extra host syscall on a failed blocking connect.
- **Watch out**: None known; the extra connect goes through `__inet_stream_connect`'s `sock_error` path and sends nothing.
- **Source**: f6a82cc

### D22: Binding a raw ICMP socket again to its own address succeeds
- **What**: A raw socket already bound on the host to the same address returns success without binding again.
- **Why**: A second bind of a raw ICMP socket failed with `EINVAL` because its host ping socket refuses a second bind.
- **Tradeoffs**: Binding to another address is still `EINVAL` (recorded in the map), since recreating the socket would lose its options and its aggregation-point registration.
- **Watch out**: None known.
- **Source**: dcacfb6

### D23: Netlink options, getpeername and socketpair follow Linux's rules
- **What**: `SockOpts` records generic options and reads them back by Linux's rules; netlink sockets gain a type, groups, flags, `SO_PASSCRED` and a send timeout, with bind, connect, autobind, `SOL_NETLINK` options, pktinfo and kernel credentials on receive; `SO_*TIMEO_NEW` map to the old options; getpeername reports the connected peer as Linux does; socketpair returns Linux's `EINVAL`, `ESOCKTNOSUPPORT` or `EPERM`; and rtnetlink change types up to `RTM_MAX` are `EPERM` without `CAP_NET_ADMIN`.
- **Why**: Netlink options were misreported (`SO_TYPE` 0, no memberships, no autobind, `SO_PASSCRED` ignored), `SO_*TIMEO_NEW` had no effect, getpeername echoed the connect address, socketpair's errnos differed, and unprivileged rtnetlink changes answered `EOPNOTSUPP`.
- **Tradeoffs**: Memberships and buffer sizes are only recorded, and autobind does not look for other sockets holding the tgid.
- **Watch out**: Programs that relied on unknown netlink levels being accepted now get `ENOPROTOOPT`, and a wildcard connect from a socket bound to its own address reports the loopback.
- **Source**: 5d7b936

## Superseded

- ~~DNS stub queries are resolved synchronously in the sending thread (rounds 06, 07)~~ → replaced by **D5** in fe4bb2b (2026-11-05)
