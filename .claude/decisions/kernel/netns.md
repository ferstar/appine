# Network namespaces (kernel/netns) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/netns"`
> Last distilled: 2026-11-03 (HEAD = 5d21cc4) — seeded by hand from the retired design doc §10 and
> the round records in `docs/rounds/`; no `MODULE: kernel/netns` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: Network namespaces translate addresses instead of running a network stack
- **What**: A new namespace is a block of host loopback addresses (127.a.b.0/24); its sockets stay host sockets and addresses are rewritten at the syscall boundary in both directions.
- **Why**: bubblewrap's `--unshare-net` (Codex's sandbox) needs `CLONE_NEWNET` with a working lo; the app has no usable host network namespaces, and a separate TCP/IP stack was not worth building.
- **Tradeoffs**: Isolation only holds inside the guest: host loopback sockets are reachable from the initial namespace and from other apps; IPv6 has only loopback and host-routed paths.
- **Watch out**: Every address returned to the guest must be translated for the viewing socket's namespace.
- **Source**: round 122 (`122-net-namespaces`)

### D2: Virtual devices are kept as Linux state, without packets
- **What**: bridge, veth and dummy devices, their addresses, routes and sysctls follow Linux 6.1 per namespace and are visible through netlink, ioctl, `/proc` and `/sys`; no packet ever passes through them.
- **Why**: dockerd and netavark failed at the first netlink request (`Failed to create bridge docker0`), so containers could not start; matching Linux's state lets `ip addr`/`ip route` in containers look native.
- **Tradeoffs**: Device statistics are zero; `forwarding` and similar settings are recorded only.
- **Watch out**: Host interfaces in the initial namespace must stay unchangeable (requests that change nothing still succeed).
- **Source**: rounds 169 (`169-container-network-survey`), 170 (`170-netdev`)

### D3: Virtual IPv4 addresses map to host loopback addresses at the socket layer
- **What**: Connections and datagrams to addresses on virtual devices go through the host's loopback, to the owning namespace's 127.c.d.1 (or 127.255.x.y for the initial namespace); traffic routed through a gateway to unowned addresses goes to the host network.
- **Why**: After devices existed, containers still could not reach each other or be reached from outside ("Network unreachable"), docker's `-p` containers failed and aardvark-dns could not bind the gateway; this reuses the namespace loopback mapping.
- **Tradeoffs**: IPv6 virtual addresses are not mapped.
- **Watch out**: The owner of an address is found from routes over virtual devices; directly connected misses are `EHOSTUNREACH`.
- **Source**: round 171 (`171-netmap`)

### D4: nftables is recorded like Linux and evaluated at connect and sendto
- **What**: The ruleset arrives over `NETLINK_NETFILTER` in Linux's format (iptables-nft works); nat chains run at connect/sendto and DNAT rewrites destinations in the socket layer.
- **Why**: docker's embedded DNS (127.0.0.11) relies on iptables nat rules, dockerd with `"iptables": true` and netavark's nftables driver failed without `NETLINK_NETFILTER`; with no packets, connect/sendto are the only points where rules can act.
- **Tradeoffs**: No conntrack, no POSTROUTING effects, no per-segment filtering; counters are zero.
- **Watch out**: Unknown expressions must make rules not match rather than fail evaluation.
- **Source**: round 172 (`172-nftables`)

### D5: Filter chains are evaluated along the emulated path
- **What**: OUTPUT of the sender, PREROUTING/FORWARD/INPUT of the namespaces a connection passes through are evaluated; rejects return errors immediately and TCP drops time out like Linux.
- **Why**: Without filtering, docker network isolation, `--internal` networks, `DOCKER-USER` rules and podman's isolate had no effect.
- **Tradeoffs**: IPv4 only; UDP drops outside the sender are silent successes.
- **Watch out**: The path steps must match how the address mapping routes the connection.
- **Source**: round 175 (`175-filter`)

### D6: Namespace ports are registered in the namespace, not left to the host
- **What**: Explicit binds get separate host ports; listeners and bound UDP sockets are registered per namespace; connections without a matching listener are refused.
- **Why**: A port bound on the outer wildcard made the same port `EADDRINUSE` inside containers (`-p 8080:8080` style publishes failed); connections to unused ports were captured by outer wildcard listeners, and services bound only to a container's 127.0.0.1 were reachable from other containers.
- **Tradeoffs**: 127.0.0.1 and the virtual address share one host address, so the same port on both conflicts.
- **Watch out**: Unregister listeners when sockets close.
- **Source**: round 174 (`174-namespace-ports`)

### D7: A host thread forwards outside connections to published ports
- **What**: Initial-namespace sockets matched by PREROUTING DNAT are served by a forwarder thread that accepts and connects to the target; IPv4 TCP clients from outside keep their real address through a 127.254.x.y binding.
- **Why**: Connections from outside appine (the phone's browser, other devices) reached podman's port-holding socket, which never accepts; Linux delivers them through DNAT, and containers' access logs, rate limits and allow lists need the real client address.
- **Tradeoffs**: The decision comes from a synthetic probe; loopback, IPv6 and UDP clients see the gateway.
- **Watch out**: The thread must never take the BKL.
- **Source**: rounds 173 (`173-port-forwarding`), 176 (`176-real-client-address`)

### D8: Containers going out pass the outer namespace's PREROUTING
- **What**: Connections from a namespace to the host network run the initial namespace's PREROUTING nat, and unbound UDP sockets keep working when they first talk inside the namespace and then outside.
- **Why**: Transparent proxies for containers are written as outer `REDIRECT` rules on traffic entering from the bridge (`iifname "podman0"`); UDP sockets that first sent to the gateway then got `EINVAL` sending outside, unlike native.
- **Tradeoffs**: PREROUTING nat runs only once and only for IPv4.
- **Watch out**: Wide UDP sockets must keep both paths usable.
- **Source**: rounds 182 (`182-outer-prerouting`), 183 (`183-udp-wide-first`)
