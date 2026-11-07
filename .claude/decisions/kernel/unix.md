# AF_UNIX sockets (kernel/unix) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/unix"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930). D1–D3 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: AF_UNIX is emulated entirely in process
- **What**: Stream, datagram and seqpacket sockets, `socketpair`, listen/accept and message queues are kernel-layer objects; no host socket is involved.
- **Why**: `SCM_RIGHTS` must pass guest open files and `SO_PEERCRED` must name guest processes; host sockets would carry host fds and host pids.
- **Tradeoffs**: Every AF_UNIX behaviour (merging, boundaries, readiness, credentials) has to be reimplemented after Linux.
- **Watch out**: Stream reads and poll events follow Linux's `unix_stream_read_generic` and `unix_poll` rules.
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`)

### D2: Path names are socket nodes in the store; abstract names live in the kernel
- **What**: Binding a path creates an `S_IFSOCK` node in the store (`EADDRINUSE` if it exists) and records its inode; abstract names and autobind names are only in the kernel's name table and disappear with the socket.
- **Why**: Pathname sockets must be visible and permission-checked like files (logging through `/dev/log` was among M4's tests); abstract names need no file.
- **Tradeoffs**: Path names are global across network namespaces (only abstract names are scoped per namespace).
- **Watch out**: Look path sockets up by inode, not by path string.
- **Source**: round 06 (`06-m4`)

### D3: Cycles through in-flight messages are collected like Linux's `unix_gc`
- **What**: When a unix socket is closed (close, close_range, dup2 over it, execve) or a process exits, sockets referenced only by in-flight messages and unreachable from outside have their receive queues emptied.
- **Why**: Sockets passed through themselves formed `Rc` cycles (open file → socket → queue → message → open file) that were never freed.
- **Tradeoffs**: The collector is conservative (any temporary extra reference keeps a socket) and runs synchronously on unix socket closes; Linux 6.8 runs it asynchronously, so it cannot be compared natively and is covered by unit tests.
- **Watch out**: Only unix-socket files can be candidates.
- **Source**: round 19 (`19-deferred`)

### D4: AF_UNIX is a small module of its own
- **What**: kernel/unix covers `unix.rs` and `syscall/unix.rs`, separate from kernel/net.
- **Why**: The retired design doc's §10 gave a single line to a full in-process socket family with its own garbage collector.
- **Tradeoffs**: Its map was verified more lightly (two research concerns, one verifier) than the large modules'.
- **Watch out**: Bugs confirmed at the split stay open in the map, among them no `ECONNRESET`, datagram sends raising `SIGPIPE`, stream reads split and merged at the wrong places and wrong `SO_PEERCRED` values without a peer.
- **Source**: c3df5d3 (bug list since updated by 2b72eeb)

### D5: AF_UNIX parses socket timeouts with kernel/net's parser
- **What**: Unix sockets use net's `timeval()` for `SO_RCVTIMEO` and `SO_SNDTIMEO` instead of their own copy.
- **Why**: The copy had the same overflow on huge timeouts as the inet parser (kernel/net D10).
- **Tradeoffs**: The two socket families now share one parser.
- **Watch out**: None known.
- **Source**: 96cd8bd

### D6: AF_UNIX files read in a connection's life order
- **What**: unix.rs runs sockets, names, channels, /proc/net/unix and garbage collection, with methods in the order a connection lives and accessors last; syscall/unix.rs runs creation, names and connections, sending and receiving, options and shutdown, addresses and waiting; the `POLL*` constants stay at the top.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; the AF_UNIX files put most helpers above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 2847887

### D7: Zero-length reads and shutdown of unconnected sockets answer as Linux does
- **What**: A stream read of nothing with data queued returns 0 and passes the head message's files (moved, or cloned with `MSG_PEEK`) and credentials while leaving its data, and shutdown sets its flags on any socket.
- **Why**: Programs probing with zero-length reads or shutting a socket down before connecting failed or hung.
- **Tradeoffs**: Shutdown flags on a listening socket change nothing observable.
- **Watch out**: None known.
- **Source**: 2b72eeb
