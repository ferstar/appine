# Host abstraction layer (hal) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: hal"`
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the design doc §3 and the round
> records in `docs/rounds/`; no `MODULE: hal` Decision blocks exist yet, so sources cite rounds
> instead of commits.

## Active

### D1: One implementation for Android and Linux hosts, passing host errnos through
- **What**: The hal has a single implementation for bionic and glibc, with `cfg` branches only where the hosts differ; functions return the host's errno, which the kernel hands to the guest unchanged; no macOS hal is written; the core crates depend on no Android component.
- **Why**: Development runs on Linux arm64 and production on Android, both Linux kernels whose errno numbering matches the guest's; keeping Android-specific code in the hal and the Kotlin shell lets 90% of the work happen in a Linux VM.
- **Tradeoffs**: Other hosts are not supported; the hal's own comment promises only that they compile, and even that has lapsed (see the map's bugs).
- **Watch out**: A host-only behaviour (Android's app seccomp, missing `/proc` files) must be handled in the hal or its caller, not assumed from the Linux development host.
- **Source**: design doc §3, §13.3; round 07 (`07-m5`)

### D2: DNS goes to the host's resolver: `android_res_nsend` on Android
- **What**: `dns_query` sends the guest's query packet through `android_res_nsend` on the default network (linked against libandroid) with a 30 s backstop, and on other hosts to the servers in the host's `/etc/resolv.conf`.
- **Why**: Through the system resolver, Android's Private DNS, VPNs and per-network routing apply to the guest (see `kernel/net` D5 for the stub that calls it).
- **Tradeoffs**: The call blocks the calling thread; callers must not hold locks.
- **Source**: round 07 (`07-m5`)

### D3: Edge-triggered readiness of a host socket comes from a per-fd host epoll
- **What**: `Edges` is a host epoll registered edge-triggered on one fd; `take()` reports whether read or write edges arrived since the last call.
- **Why**: Host socket data never passes through the kernel layer, and a host poll only shows current readiness; guest `EPOLLET` and `EPOLLEXCLUSIVE` need to know that new data arrived.
- **Tradeoffs**: One extra host fd per such open file; several arrivals between two calls fold into one edge; fds the host epoll rejects (regular files as stdio) fall back to level checks in the caller.
- **Source**: round 42 (`42-epoll-edges`)

### D4: Thread policy and statistics act on host threads, best effort
- **What**: Affinity, nice and ioprio are applied to the host thread running the guest thread, ignoring failures (the kernel applies its own floors first); context switches and CPU time come from the host thread (`getrusage(RUSAGE_THREAD)` for the calling thread, `/proc/self/task/<tid>` for others).
- **Why**: Guest threads are host threads scheduled by the host, so the host's scheduler and I/O queues should see the guest's priorities; reading the counters with one `getrusage` replaced parsing the host status file under the BKL (about ten microseconds a call).
- **Tradeoffs**: A host refusal is invisible to the guest; values from pooled host threads need a base taken when a guest thread starts.
- **Source**: rounds 48 (`48-exclusive-wakeup`), 109 (`109-spawn-mmap-bkl`), 140 (`140-ioprio-syslog-adjtimex`)

### D5: Host routes and source addresses come from the host kernel
- **What**: `routes()` parses the host's `/proc/net/route` and `/proc/net/ipv6_route` (unicast main-table routes; `None` where unreadable, as on Android); `source_for` connects a UDP socket to the destination and reads its local address.
- **Why**: `ip route` and `ip route get` should show what the host would actually use, and the host picks the source address itself.
- **Tradeoffs**: On Android, the routes come from the app side instead (`kernel/net`).
- **Source**: round 98 (`98-routes`)

### D6: `adjtimex` is a read-only host query, synthesized on Android
- **What**: `adjtimex_query` makes the real read-only call on Linux hosts and returns a synthesized unsynchronized state on Android.
- **Why**: Android's app seccomp forbids `adjtimex`, and Android sets the time with settimeofday instead of the kernel's NTP discipline, so the synthesized state matches what the Android kernel actually holds.
- **Source**: round 140 (`140-ioprio-syslog-adjtimex`)

### D7: Host file copies use `sendfile`, not `copy_file_range`
- **What**: `copy_file` seeks the output fd and calls the host's `sendfile`; the kernel uses it for `sendfile` and `copy_file_range` between regular files, falling back to chunked copies when the host refuses.
- **Why**: Before Android 14 the app seccomp does not allow `copy_file_range` (the call gets `SIGSYS`).
- **Tradeoffs**: `sendfile` writes at the output fd's host position, so the caller must hold the output file position while copying.
- **Source**: round 33 (`33-file-ops-host-cost`)
