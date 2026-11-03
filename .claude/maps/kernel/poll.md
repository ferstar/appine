# kernel/poll Map
> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/poll.md` for the paired decision history.
> Verified: 2026-11-03 (full mapping: 3 research concerns; 12 claims checked by 2 independent verifiers — 11 confirmed and 1 partial, corrected below; bugs and debts recorded)

## Responsibilities

`kernel/poll` implements I/O multiplexing inside the kernel layer:

- the readiness model shared by every multiplexer (`readiness`, `arrivals`, `wake_objects`);
- ppoll and pselect6;
- epoll, including Linux's `EPOLLET` timing, nested epolls, `EPOLLONESHOT` and
  `EPOLLEXCLUSIVE`;
- eventfd2;
- signal-driven I/O (`O_ASYNC`, `F_SETOWN*`, `F_SETSIG`) checked by the timer thread.

Root: `core/kernel/src/syscall/poll.rs`.

Outside the module: the wait mechanism, wake objects, the BKL and the timer thread (`kernel/process`);
per-kind readiness of sockets (`kernel/net`, `kernel/unix`), terminals (`kernel/tty`), pipes,
inotify, timerfd and eventfd objects (`kernel/vfs`), signalfd (`kernel/signal`), message queues
and seccomp notify (not yet mapped); fcntl and ioctl entry points (`kernel/vfs`); signal delivery
(`kernel/signal`).

## Key types

- `Waits` — one blocking wait: host fds with events, in-process wake objects, pidfd/signalfd
  flags (CHILD/SIGNAL channels), the earliest timerfd deadline, edge state, gated files.
- `Interest` — one watched file of an epoll: weak open file, events, data, `armed` (ONESHOT),
  `seen`/`drained`/`arrivals` snapshots for edge triggering, nested-generation share.
- `Epoll` — interests (no ready list), round-robin `cursor`, `base` generation, sticky
  `edge_watched`, waiters.
- `HostEdges` (on `OpenFile`) — a private edge-triggered host epoll on one host fd, accumulating
  read/write arrivals.
- `OpenFile` fields used here: `drained` (per direction), `reads`, `exclusive_claim`, `owner`,
  `sig`, `host_edges`.
- `ExGate` — per-file queue of sleeping exclusive waiters plus the last generation seen
  (`Kernel::ex_gates`).
- `Fasync` — one `O_ASYNC` file: weak open file, fd, last events, counter snapshots
  (`Kernel::fasync`).
- `EventFd` (in `fd.rs`) — counter, semaphore flag, `writes`/`reads` generations.

## Public entry points

- Syscalls: `ppoll` (73), `pselect6` (72), `epoll_create1` (20), `epoll_ctl` (21), `epoll_pwait`
  (22), `epoll_pwait2` (441), `eventfd2` (19); eventfd read/write through `IoPath::EventFd`.
- Shared helpers: `readiness`, `poll_once`, `poll_now`, `arrivals`, `wake_objects`, `host_fd`,
  `set_temp_mask`/`restore_mask`, `deadline_ms`/`deadline_ts` (also used by AIO).
- Hooks: `drained` (from the dispatcher after read-like and write-like syscalls), `arrived` (pipe
  writes to a non-empty pipe), `register_fasync` (fcntl/ioctl), `check_exclusive` and
  `check_fasync` (timer thread), `send_to_owner`, `Epoll::fdinfo`, `Epoll::target` (kcmp).

## Data flow / lifecycle

**1. The readiness contract**

- `readiness(file)` is one `match` over `FileKind` returning known events and, for host-backed
  files (host stdio, inet sockets other than ICMP), the host fd: pipes and eventfd inline; timerfd
  (expirations), inotify, signalfd, pidfd (exited), epoll (nested, up to 4 levels), PTYs
  (`tty::readiness`), sockets (`net::readiness` → unix, netlink, inet, raw ICMP), message queues,
  seccomp notify, kmsg; `O_PATH` files `POLLNVAL`; everything else (regular files, directories,
  other devices, synthetic files, bpf) is always readable and writable.
- `poll_now` adds a zero-timeout host poll for the host fd (and its `HostEdges`), keeping
  `POLLERR`/`POLLHUP`/`POLLNVAL` regardless of the requested mask.
- `arrivals(file)` gives monotonic `[in, out]` generation counters (plus host edges);
  `wake_objects(file)` the objects every state change wakes.

**2. Waiting**

- `Waits` collects host fds and wake objects (readiness is computed again per file) and calls the
  kernel's wait: the BKL is dropped around one host poll of all host fds plus the thread's event
  fd; the timeout is the earliest of the deadline and any timerfd deadline; pidfd and signalfd
  waits also listen on the CHILD and SIGNAL channels. Spurious wakeups just rescan. The BKL is held
  for every scan.

**3. ppoll and pselect6**

- ppoll: `nfds` above the soft `RLIMIT_NOFILE` `EINVAL`; timespec validated (`EINVAL`); each fd
  looked up once (closed → `POLLNVAL`, negative → ignored); revents written back and counted.
- pselect6: `nfds` negative or above the soft limit `EINVAL`; sets read in 64-bit words; read →
  `IN|RDNORM|HUP|ERR`, write → `OUT|WRNORM|ERR`, except → `PRI`; a set bit on a closed fd `EBADF`;
  results rebuilt per set (one count per bit).
- Both: temporary signal mask (size 8, SIGKILL/SIGSTOP removed; kept on `ERESTARTNOHAND`/`EINTR`
  so the handler runs with it); loop scan → return if ready or past the deadline → interrupted →
  `ERESTARTNOHAND` → wait; remaining time written back whenever a nonzero timeout was given (a
  failing write turns the restart into `EINTR`); restarts re-read the remaining time.

**4. epoll**

- `epoll_create1`: only `O_CLOEXEC` (else `EINVAL`).
- `epoll_ctl` checks: epfd (`EBADF`, `EINVAL` if not an epoll); target (`EBADF`, `O_PATH`
  `EBADF`); self `EINVAL`; regular files, directories, synthetic files and
  `/dev/null|zero|full` `EPERM`; on ADD loops or depth over 4 `ELOOP`; `EPOLLEXCLUSIVE` only on ADD,
  not on epolls, and not with ONESHOT/RDHUP/PRI (`EINVAL`). Interests are keyed by (fd, open file);
  ADD `EEXIST`, MOD replaces (re-arms; MOD of an exclusive interest `EINVAL`), DEL, missing
  `ENOENT`. Every call prunes dead interests and wakes the epoll's waiters.
- Interests hold weak references: they live while any fd or fork copy holds the open file and are
  skipped once it is gone (removed at the next `epoll_ctl`). Fork shares the epoll.
- `epoll_pwait`/`epoll_pwait2`: `maxevents` ≤ 0 or too large `EINVAL`; scan from the cursor (O(n),
  no ready list); return if anything is ready or past the deadline; interrupted → `EINTR` (never
  restarted; the temporary mask is kept); otherwise wait. Reports are marked (ONESHOT disarmed,
  exclusive claim taken) before the events are copied out.
- Edge triggering: each interest compares the file's per-direction drain counts and arrival
  generations with its snapshots; a direction that was drained or received new data clears its
  reported bits; an interest fires only for bits not yet reported. Host fds with edge-triggered
  or exclusive interests (or in an edge-watched epoll) get a private edge-triggered host epoll
  (`HostEdges`) so each host arrival counts. While waiting, reported-and-still-ready bits are left
  out (avoiding busy loops); drains and pipe arrivals wake edge waiters. Drains are counted for
  read, readv, accept, recv* and write, writev, send*, sendfile, splice, tee when they return
  `EAGAIN`, and for short read, write, recvfrom and sendto (pread/pwrite and their vector forms
  never count; tee's write-side entry reads the length argument instead of `fd_out`). On non-Linux
  hosts `HostEdges` cannot be created, so only drains re-arm host-fd edges there.
- Nested epolls: an inner epoll's generation is its `base` plus the shares of its armed
  interests, read side only, growing monotonically; edge-watching an inner epoll propagates host
  edge watching inward. An epoll fd reports `POLLIN` only.
- `EPOLLEXCLUSIVE`: an edge is the file's drain and arrival counts; each edge is claimed by one
  epoll (level-triggered exclusive interests keep reporting to the claiming epoll). Exclusive
  waiters do not wait on the file: they queue on its `ExGate`, and the timer thread (subscribed to
  every kernel change while gates exist, and to the host edges) wakes only the first waiter when a
  new edge appears; woken waiters requeue at the back.

**5. eventfd**

- `eventfd2` flags `EFD_SEMAPHORE|O_CLOEXEC|O_NONBLOCK` (else `EINVAL`). Read: buffer under 8
  `EINVAL`; takes the count (or 1 in semaphore mode) or blocks/`EAGAIN`/`ERESTARTSYS`. Write: under
  8 bytes or `u64::MAX` `EINVAL`; blocks or `EAGAIN` until the sum fits below `u64::MAX`; writing 0
  still counts as an arrival. Readiness: `POLLIN|RDNORM` when nonzero, `POLLOUT|WRNORM` below
  `u64::MAX - 1`; never `POLLERR`. Generations `[writes, reads]`.

**6. Signal-driven I/O**

- Turning `O_ASYNC` on (`F_SETFL`, `FIOASYNC`) registers a `Fasync` entry for any file kind,
  snapshotting the current events (already-ready events are not signalled) and waking the timer
  thread.
- The timer thread's `check_fasync` drops entries whose file is gone or no longer `O_ASYNC`,
  clears reported read/write bits when the counters moved (`drained`, `reads` — bumped by read-file
  syscalls only — and arrivals), computes new events, adds host fds' not-yet-ready bits to its
  host poll, and sends: `SIGIO` with kernel siginfo, or the `F_SETSIG` signal with `POLL_*` code,
  band (Linux's band table) and fd. Owners are a thread, process or process group
  (`send_to_owner`), stored as global ids.

## Dependencies (inbound / outbound)

**Inbound**

- The syscall dispatcher (also the drain hook after I/O syscalls); AIO (`poll_once`, masks, resfd
  eventfds); the timer thread (`check_exclusive`, `check_fasync`); fcntl/ioctl (`register_fasync`);
  pipe writes (`arrived`); fdinfo and kcmp.

**Outbound**

- The kernel's waits (`wait_objects`, `block_for`, `interrupted`), wake objects and channels,
  `edge_waiters`; `appine_hal::poll` and `appine_hal::Edges` (host epoll; unavailable on non-Linux
  hosts); per-kind readiness functions in `net`, `unix`, `tty`, inotify, signalfd, message queues,
  seccomp; signals (`send_to_task`, `send_to_group`, `send_to_pgrp`).

## Invariants and gotchas

- Readiness must be cheap and side-effect free except for edge snapshots kept in `Cell`s.
- Generation counters only grow; anything that changes readiness must bump a counter and wake its
  object, or edge-triggered waiters miss events.
- Without `HostEdges` (host regular files, non-Linux hosts) only drains re-arm host-fd edges.

## Confirmed bugs / technical debt

- **Signal-driven I/O can signal any process:** no credentials are recorded at `F_SETOWN*` and
  nothing like Linux's `sigio_perm` runs at delivery, so an unprivileged user can set a pipe's
  owner to a root process, `F_SETSIG(SIGKILL)` (or leave the default `SIGIO`, which terminates),
  turn on `O_ASYNC` and kill it with one write. Unknown owners are accepted without `ESRCH`;
  `F_SETOWN` ignores pid namespaces and overflows on `INT_MIN`.
- **Huge timeouts panic:** a valid timespec with a very large `tv_sec` overflows
  `Instant::now() + timeout` in ppoll, pselect6 and epoll_pwait2 (and in the same pattern in other
  modules' waits — see the `kernel/process` item), killing the calling process with a kernel-bug
  report; Linux saturates.
- **pselect6 returns 0 early:** an fd in a set whose mask does not include its pending
  `POLLHUP`/`POLLERR`/`POLLNVAL` (a hung-up pipe only in the except or write set, an `O_PATH`
  fd) ends the scan with nothing to report, so select returns 0 immediately even with no timeout
  and looping callers spin. pselect6 also fails with `EINVAL` when `nfds` exceeds the soft fd limit
  (Linux clamps), breaking `select(FD_SETSIZE, …)` after `ulimit -n 256`.
- **`EPOLLEXCLUSIVE` timerfds may never fire:** gated interests skip collecting timerfd deadlines
  and the timer thread does not watch them, so expiry is noticed only when something unrelated
  wakes the timer thread or the epoll times out. Gate wakeups also ignore the interest's
  direction (spurious wakeups, and with mixed masks a matching waiter can miss an edge consumed by
  another) and rotate waiters instead of keeping Linux's order.
- **Edge-triggered host sockets can stall:** without `HostEdges` (always on non-Linux hosts) a
  program that drains with `recvmsg` until a short read can miss data that arrives before the next
  scan, sleeping until an unrelated event; the pipe write end has no write-side counter, so
  `writev`-until-short drains can miss `POLLOUT`.
- **epoll results lost on `EFAULT`:** events are marked reported (ONESHOT disarmed, exclusive
  claims taken) before the copy-out, so a bad buffer loses them; Linux requeues them and returns a
  partial count.
- **Readiness details differ:** a pipe write end without readers reports `POLLERR` but not
  `POLLOUT`; ppoll leaves revents unwritten when interrupted; eventfd `readv` checks each iovec
  separately (small iovecs `EINVAL`; `[8, 8]` blocks or consumes twice); an epoll fd reports only
  `POLLIN`.
- **`O_ASYNC` on every file kind:** regular files, eventfds, timerfds and message queues accept
  it, so a regular file with `O_ASYNC` raises `SIGIO` after every read.
- **Per-scan cost:** every scan holds the BKL and makes one zero-timeout host poll per host fd (and
  computes readiness twice per file); epoll has no ready list.

## Open questions

- Is the cost of one host poll per host fd per scan, under the BKL, accepted, or should scans batch
  host fds?
- Should exclusive waiters keep Linux's stable queue order instead of rotating?

## To verify

- Linux 6.1 comparisons above come from knowledge of `fs/select.c`, `fs/eventpoll.c`,
  `fs/eventfd.c`, `fs/fcntl.c` and `fs/pipe.c`, not a source checkout.
