# I/O multiplexing (kernel/poll) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/poll"`
>
> Last distilled: 2026-11-06 (HEAD = afd086a). D1–D6 were seeded by hand from the retired design doc
> §10 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: poll, select and epoll are implemented in process over one wait mechanism
- **What**: Every multiplexer scans readiness of guest files (host fds polled with zero timeout) and waits through the kernel's wait, which puts host fds and the thread's wake event in one host poll; the guest epoll is not a host epoll.
- **Why**: Guest files mix host fds and in-process objects (pipes, PTYs, eventfds, AF_UNIX); one wait covering both is simpler, and a host epoll would only cover host fds.
- **Tradeoffs**: Each wait scans all interests (no ready list) and costs a host poll per host fd; a host-epoll-backed design remains the option when fd counts grow.
- **Watch out**: Waits must release the BKL and include the thread's own event so signals interrupt them.
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`)

### D2: `EPOLLET` fires on every new arrival, as Linux's wait-queue wakeups do
- **What**: Per-direction data generations (bumped at each event) and drain counts decide when an edge-triggered interest reports again; host sockets get a private edge-triggered host epoll so each host arrival counts.
- **Why**: tokio/mio's waker never reads its eventfd and relies on each write re-reporting; the earlier "not ready → ready" approximation lost the second wakeup and `uv pip install --offline` hung forever (eventfd, pipes, AF_UNIX and TCP all behaved that way on Linux).
- **Tradeoffs**: Every readiness source must maintain generation counters and drains must be detected from syscall results.
- **Watch out**: Leave reported-and-still-ready bits out of waits, or the host poll returns at once and busy-loops.
- **Source**: round 42 (`42-epoll-edges`)

### D3: Nested epolls re-report when inner interests see new events
- **What**: An inner epoll's generation is the sum of its interests' event counts, and an outer edge-triggered interest fires again when it grows.
- **Why**: Linux's `ep_poll_callback` wakes the outer epoll on every inner wakeup, even when the interest is already on the ready list; appine reported only the first event.
- **Tradeoffs**: Generations only grow; removed interests' shares are kept in a base.
- **Watch out**: Edge-watching an inner epoll must propagate host edge tracking inward.
- **Source**: round 47 (`47-nested-epoll`)

### D4: `EPOLLEXCLUSIVE` reports and wakes only one waiter
- **What**: Each edge is claimed by one epoll, and exclusive waiters sleep on a per-file gate watched by the timer thread, which wakes only the first.
- **Why**: Several processes waiting on one listening socket were all woken (host poll for TCP, the file-class broadcast for AF_UNIX); Linux wakes only one exclusive waiter.
- **Tradeoffs**: Wakeups go through the timer thread, which then follows every kernel change while gates exist.
- **Watch out**: Gated waiters must still see deadlines and edges of every kind of file.
- **Source**: rounds 42 (`42-epoll-edges`), 48 (`48-exclusive-wakeup`)

### D5: Waiters are woken only by changes to the objects they wait on
- **What**: Readiness sources wake their own objects (and the lock channel for locks); poll/epoll waiters register exactly the objects of their files.
- **Why**: Any pipe, socket, eventfd or PTY operation woke every thread parked in poll/epoll (8 idle processes woke 520,000 times; socketpair round trips went from 30 to 56 µs), hurting Go's netpoller, libuv, asyncio and Java NIO.
- **Tradeoffs**: Every module must name precise wake objects.
- **Watch out**: A missing wake request becomes a hang, not just a slowdown.
- **Source**: rounds 49 (`49-targeted-wakeups`), 53 (`53-locks-host-sockets`), 54 (`54-mqueue-fasync`)

### D6: Signal-driven I/O is checked by the timer thread with the same counters
- **What**: `O_ASYNC` files are checked by the timer thread, which sends `SIGIO` or the `F_SETSIG` signal (with band and fd) to the owner when new events appear, using the edge-triggering counters.
- **Why**: `O_ASYNC`, `F_SETOWN*` and `F_SETSIG` used to be recorded only; the timer thread already wakes on kernel changes and polls host sockets.
- **Tradeoffs**: Delivery waits for the timer thread to run.
- **Watch out**: Re-notify when more data arrives on an already-readable file, as Linux does.
- **Source**: rounds 19 (`19-deferred`), 54 (`54-mqueue-fasync`)

### D7: I/O multiplexing is a module of its own
- **What**: kernel/poll covers `syscall/poll.rs`: the readiness model, ppoll and pselect6, epoll, eventfd2 and signal-driven I/O.
- **Why**: The retired design doc's multiplexing section described EPOLLET, nesting and exclusive wakeups but not the readiness contract, select semantics or signal-driven I/O permissions.
- **Tradeoffs**: The readiness sources stay in their own modules and are only described here as a contract.
- **Watch out**: Bugs confirmed at the split stay open in the map, among them signal-driven I/O signalling any process, `EPOLLEXCLUSIVE` timerfds that may never fire, edge-triggered host sockets stalling without host edges and epoll results lost on `EFAULT`.
- **Source**: c3df5d3

### D8: select waits only for what its sets ask and clamps nfds to Linux's table size
- **What**: nfds is clamped to an estimate of Linux's fd table size from the highest open fd (64, then 128 · 2^k), bits above it are neither read nor written back, only events in an fd's masks end the wait, and all three masks include `POLLNVAL` as native select does.
- **Why**: select ended its wait on events it then did not report, and refused nfds values Linux accepts.
- **Tradeoffs**: Linux's table never shrinks and a forked child's is sized by its parent's open fds, so a set bit on a closed fd just above the estimate is ignored where Linux returns `EBADF` (recorded in the map).
- **Watch out**: A hung-up host socket in the except set is no longer polled, so a later event on it is noticed only on another wakeup or the timeout.
- **Source**: 5c179b0

### D9: poll.rs reads from the syscalls down to the readiness model
- **What**: Shared constants stay at the top, then ppoll and pselect6, epoll, waiting (the exclusive gates in trigger order), signal-driven I/O, the readiness model and data generations, and eventfd last.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; poll.rs started with the readiness helpers and ended with the syscalls that use them.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; the readiness model the module docs describe first now comes near the end.
- **Watch out**: Code appended at the end of the file drifts from this order.
- **Source**: d795d07
