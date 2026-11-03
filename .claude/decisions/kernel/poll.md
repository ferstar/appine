# I/O multiplexing (kernel/poll) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/poll"`
> Last distilled: 2026-11-03 (HEAD = 5d21cc4) — seeded by hand from the retired design doc §10 and
> the round records in `docs/rounds/`; no `MODULE: kernel/poll` Decision blocks exist yet, so
> sources cite rounds instead of commits.

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
