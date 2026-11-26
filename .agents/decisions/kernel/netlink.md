# Netlink sockets (kernel/netlink) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/netlink"`
>
> Last distilled: 2026-11-26 (HEAD = e8ac8ebf)
>
> Decisions on netlink sockets made before kernel/netlink was split from kernel/net keep their `kernel/net` tags and live in `.agents/decisions/kernel/net.md`: D3, D24, D26 and the netlink halves of D19 and D23.

## Active

### D1: kernel/netlink is split out of kernel/net
- **What**: kernel/netlink owns `net.rs`'s sections from netlink through `NETLINK_SOCK_DIAG` and, in `syscall/net.rs`, the `netlink_*` functions, the netlink section (`netlink_send`, `rtnl_watched`, `CHANGING_IOCTLS`, `rtnl_notify`, `KernelHost`) and the `Socket::Netlink` arms: binding, groups and options, requests and reply queues, rtnetlink queries, changes and notifications, `nlctrl`, `NETLINK_SOCK_DIAG`, the `NETLINK_NETFILTER` entry point and `NETLINK_KOBJECT_UEVENT`.
- **Why**: Netlink is a subsystem of its own in Linux (af_netlink, rtnetlink, genetlink, sock_diag) with its own protocols, notifications and the largest group of kernel/net's bugs, and the fully refreshed kernel/net map neared one Read.
- **Tradeoffs**: Route and rule messages and `ip route get` come from kernel/net's `route.rs`, the boundary inside `net.rs` and `syscall/net.rs` is recorded only in the registry, and the map carries the facts verified in kernel/net's full mapping of 2026-11-11 rather than a mapping of its own; nftables control-plane handling belongs to kernel/nft while netlink transport remains here.
- **Watch out**: Code added to the netlink sections or arms may be described in the wrong map, the netlink unit tests stay in `net.rs`'s test module, and bugs open in the map include non-resumable dumps, requests not validated as Linux validates them, notifications only for rtnetlink link, address and route changes, and ignored destination addresses.
- **Source**: 0c1b416; 60790c14; 39e55b4b

### D2: Netlink port identity and unicast delivery are namespace-aware

- **What**: Weakly register sockets, reject duplicate explicit port IDs, allocate negative fallback IDs and deliver unicast to the resolved peer.
- **Why**: Several netlink protocols need consistent port ownership.
- **Tradeoffs**: Registration must not extend socket lifetime.
- **Watch out**: Validate destination group shifts and neighbor queries.
- **Source**: a1fd0beb, 3a90a0ef

### D3: Netlink dumps resume across reads and report validation failures in-band

- **What**: Queue resumable dump state alongside atomic datagrams, pack complete messages and end strict failures with the errno and extended acknowledgement.
- **Why**: Large dumps and filtered errors cannot fit the single-datagram model.
- **Tradeoffs**: The receive queue retains a cursor between reads.
- **Watch out**: Always make progress and preserve the terminal NLMSG_DONE.
- **Source**: 9fc6d332, d11379f7

### D4: Rtnetlink echoes distinguish direct changes from link notifications

- **What**: Keep request sequence and sender metadata for direct address and route changes, with link notifications generated as indirect snapshot changes.
- **Why**: Linux does not echo link changes as direct request replies.
- **Tradeoffs**: Echo handling depends on request type.
- **Watch out**: Never attach the request port or sequence to an indirect link notification.
- **Source**: 76da8eff, 06fc2277

### D5: Rtnetlink and generic netlink preserve strict and extended acknowledgements

- **What**: Validate each request's header and attributes and return the error code, message or offset appropriate to strict dumps and CAP_ACK or EXT_ACK.
- **Why**: Callers need the location and reason for a rejected request.
- **Tradeoffs**: Single GETNEIGH retains a documented short-message difference.
- **Watch out**: GETPOLICY resolves a supplied family ID before its family name.
- **Source**: d11379f7

### D6: `disable_ipv6` sysctl toggles emit single-namespace rtnetlink snapshot diffs

- **What**: `rtnl_snapshot_ns` and `rtnl_notify_ns` snapshot the pinned namespace only when a subscribed `NETLINK_ROUTE` listener exists and broadcast resulting address and route diffs with `seq = 0` and `port = 0` after the write.
- **Why**: Toggling `/proc/sys/net/ipv6/conf/*/disable_ipv6` previously tore down or restored IPv6 addresses and routes without notifying `ip monitor` or network managers.
- **Tradeoffs**: Unlistened namespaces pay no snapshot cost and reuse `Snapshot::changes`, while `RTNLGRP_*_NETCONF` and `IFLA_AF_SPEC` link notifications remain unmodeled.
- **Watch out**: Future sysctls that add or remove snapshot-visible links, addresses or routes must be wired into the same `write_proc` hook.
- **Source**: 8bd3fa4c

### D7: Protocol-scoped netlink multicast broadcasts committed `NFNLGRP_NFTABLES` notifications

- **What**: `NetlinkSocket::notify_proto` generalizes multicast delivery across netlink protocols, while `nft_notify` and `rtnl_notify` share `notify_netlink` over `Kernel::rtnl_sockets` to broadcast stashed `NftNote`s in the same namespace after releasing the sender borrow and skipping echoed senders.
- **Why**: `NetlinkSocket::notify` hardcoded `NETLINK_ROUTE` with no broadcast path for committed `NETLINK_NETFILTER` events, and both broadcasters duplicated socket pruning, filtering and wakeups.
- **Tradeoffs**: Reusing `Kernel::rtnl_sockets` avoids an extra registry and keeps `SO_RCVBUF`/`ENOBUFS` and echo-suppression semantics consistent while adding a small closure-taking `notify_netlink` helper.
- **Watch out**: Broadcasts walk all live netlink sockets in `Kernel::rtnl_sockets`, so socket borrows must always drop before calling `request_wake_object`.
- **Source**: e399082a, b15971ac

## Superseded

- ~~Echoing link create/delete with the request sequence and port~~ → replaced by **D4** in 06fc2277 (2026-11-25)
