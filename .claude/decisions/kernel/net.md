# Socket layer (kernel/net) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/net"`
> Last distilled: 2026-11-03 (HEAD = 5d21cc4) — seeded by hand from the retired design doc §10 and
> the round records in `docs/rounds/`; no `MODULE: kernel/net` Decision blocks exist yet, so
> sources cite rounds instead of commits.

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
- **What**: `resolv.conf` points at `127.0.0.53`; UDP queries to it are intercepted in the socket layer and answered by `android_res_nsend` (other hosts forward to the host's servers), with SERVFAIL on failure.
- **Why**: Going through the system resolver makes Android's Private DNS, VPNs and per-network routing apply to the guest; failing fast with SERVFAIL stops resolvers from waiting for their timeouts.
- **Tradeoffs**: The query runs synchronously in the sending thread; TCP DNS and container namespaces are not intercepted.
- **Watch out**: Keep `resolv.conf` written only when missing so user edits survive.
- **Source**: rounds 06 (`06-m4`), 07 (`07-m5`)

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
- **Why**: Transparent proxies (`sslocal --protocol redir`, redsocks, clash) ask `SO_ORIGINAL_DST` after accepting; nat happens in appine's socket layer, so the host has no conntrack entry and answered `ENOENT`. The app cannot set host marks.
- **Tradeoffs**: TCP only; no TPROXY.
- **Watch out**: Register the record before releasing the BKL so the peer cannot accept first.
- **Source**: round 180 (`180-original-dst`)
