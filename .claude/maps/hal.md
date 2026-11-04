# hal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/hal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 3 research concerns; 7 claims checked by 1 independent verifier — 6 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-05 (targeted verification: `fill_random` uses the host's getrandom on Linux and Android; removed the `EMFILE` bug)

## Responsibilities

`hal` (crate `appine-hal`, depending only on `libc`) is the host abstraction layer under the kernel
layer. It wraps the host syscalls the kernel needs, with one implementation for Android (bionic)
and Linux (glibc), returning raw host errnos:

- memory: anonymous and reserved mappings, file mappings, discard, msync, residency queries;
- files and storage: `*at` calls on the object directory, positional and plain I/O, `sendfile`
  copies, the fsync family, `flock`, `ioctl`, `statvfs`, fd limits, randomness;
- host statistics: process memory usage, host meminfo, CPU set and sysfs, per-thread CPU time and
  context switches;
- waiting: `poll`/`ppoll`, `Event` (eventfd), `Edges` (edge-triggered host epoll on one fd), `Watch`
  (level-triggered host epoll over many fds);
- threads and time: tid, thread names, CPU clocks, affinity, nice, ioprio, clocks, a read-only
  `adjtimex`;
- networking: nonblocking host sockets, interfaces, routes, source selection, DNS resolution;
- terminal helpers for the CLI: raw mode and a `SIGWINCH` pipe.

Root: `core/hal/src/lib.rs`, `core/hal/src/net.rs`.

Outside the module: every policy built on these primitives (`kernel/*`); the Android-side network
callbacks and JNI (`android`); the CLI front end (`hosts/cli`).

## Key types

- `FileMapping` — `ptr` at the requested offset; `base`/`len` the host-page-aligned range actually
  mapped (what `unmap` takes).
- `MemoryUsage` (RssAnon — VmRSS before Linux 4.5 —, RssShmem, VmSwap), `MemInfo` (a subset of
  host `/proc/meminfo`), `FsStats` (statvfs; block size `f_frsize` or `f_bsize`).
- `Event` — wakes one thread from another: an eventfd (`EFD_CLOEXEC|EFD_NONBLOCK`), or a pipe on
  non-Linux hosts; `signal`, `clear`, `is_signaled`, `fd`.
- `Edges` — a host epoll with `EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET` on one fd; `take()` returns
  read/write edge booleans (several arrivals between calls fold into one); `ENOSYS` on non-Linux.
- `Watch` — a level-triggered host epoll over many fds (raw ICMP taps), with an errors-only mode.
- `RawMode` — `cfmakeraw` guard restoring the terminal on drop.
- `Received` (recvmsg result: length, raw address ≤ 128 bytes, control bytes, flags), `Interface`
  (name, index, flags, MTU, MAC, addresses with prefix lengths), `Route` (family, destination,
  prefix, gateway, interface, metric).

## Public entry points

- Memory: `map_anonymous(len, reserve)`, `map_inaccessible`, `unmap`, `discard`, `map_file`,
  `map_file_read_only`, `msync`, `populate`, `page_size`, `resident_pages`, `page_residency`,
  `touched_pages`, `cachestat`.
- Statistics: `memory_usage`, `memory_info`, `ram_and_swap`, `host_meminfo`, `statvfs`,
  `fd_limit`, `raise_fd_limit`, `host_cpus`, `cpu_sysfs`, `thread_cpu_clock`,
  `thread_system_ns(_self)`, `thread_ctxt_switches(_self)`.
- Files: `open_dir`, `open_at`, `stat_at`, `fstat`, `chmod_at`, `unlink_at`, `set_times_at`,
  `set_times_fd`, `read`, `write`, `pread`, `pwrite`, `lseek`, `copy_file`, `fsync`,
  `sync_file_range`, `syncfs`, `flock`, `ioctl`, `dup`, `fill_random`, `drain`, `isatty`.
- Waiting and threads: `poll`, `Event`, `Edges`, `Watch`, `gettid`, `set_thread_name`,
  `set_thread_affinity`, `set_thread_nice`, `set_thread_ioprio`, `thread_ioprio`, `clock_gettime`,
  `clock_getres`, `adjtimex_query`, `RawMode`, `sigwinch_notifier`.
- Network (`net`): `socket`, `close`, `bind`, `connect`, `listen`, `accept`, `sockname`,
  `setsockopt`, `getsockopt`, `shutdown`, `sendmsg`, `recvmsg`, `interfaces`, `routes`,
  `tx_queue_len`, `source_for`, `sockaddr`, `dns_query`.

## Data flow / lifecycle

### Errors and hosts

- Every function returns the raw host errno (`last_errno` falls back to `EIO`); the kernel wraps it
  and passes it to the guest unchanged, which is only correct on Linux and Android hosts, whose
  numbering matches the guest's.
- Android and Linux share one implementation; per-host divergences are `cfg` branches: DNS
  (`android_res_nsend` vs. the host's resolv.conf servers), `adjtimex` (synthesized on Android,
  where the call is forbidden), and non-Linux fallbacks (pipe `Event`, millisecond `poll`,
  `ENOSYS` `Edges`/`Watch`, reduced memory statistics). Linux-only functions (`copy_file`, the
  fsync family, `msync`) are `cfg`-gated in the hal but called ungated by the kernel.

### Memory

- `map_anonymous` maps RW private anonymous memory (guest permissions live in the soft-MMU), adding
  `MAP_NORESERVE` for reservations; `map_inaccessible` maps `PROT_NONE` with `MAP_NORESERVE` (used
  only by the kernel's bug-simulation test hook).
- `map_file` maps shared or private file ranges (private mappings are always RW copy-on-write;
  only shared read-only ones are read-only), rounding the offset down to the host page (16K on some
  devices) and the length up; `map_file_read_only` serves shared read-only mappings.
- `discard` is `MADV_DONTNEED`; residency comes from `mincore` (`resident_pages`, `page_residency`)
  or `/proc/self/pagemap` (`touched_pages`, present or swapped bits — zram pages count as touched),
  all in 4K units (pages within one 16K host page share a result, so `resident_pages` may count 4K
  pages just outside the range); `cachestat` maps the file in chunks to count cached pages.
- `memory_usage` reads `/proc/self/status`; `host_meminfo` copies host `/proc/meminfo` (falling
  back to `sysinfo`); long-lived fds for `/proc/self/status` and pagemap are kept in `OnceLock`s.

### Files and storage

- The store opens its object directory with `open_dir` (`O_PATH`) and addresses objects with
  `open_at` (always `O_CLOEXEC`, follows symlinks, applies the umask), `stat_at`, `chmod_at`,
  `set_times_*`, `unlink_at`; `flock` and a shared mapping of the `lock` file coordinate store users.
- `read`/`write`/`pread`/`pwrite`/`copy_file`/`flock` retry `EINTR`; others do not; partial
  transfers are returned as is. `copy_file` seeks the output fd and calls `sendfile` (callers fall
  back to chunked copies on `EINVAL`/`ENOSYS`/`EOPNOTSUPP`/`EXDEV`).
- Durability: `fsync(data_only)`, `sync_file_range` (raw syscall, arm64 argument order), `syncfs`,
  `msync` — Linux/Android only.
- `raise_fd_limit` raises the soft `RLIMIT_NOFILE` at machine start (after the store is opened; the
  result is ignored and `fd_limit` re-reads the limit later); `fill_random` fills the buffer with
  the host's `getrandom` (retrying `EINTR` and short results), so it needs no fd; other hosts
  open and read `/dev/urandom` on every call.

### Waiting, threads, CPUs, time

- `poll` uses `ppoll` with nanosecond timeouts (seconds clamped to `i32::MAX`; `poll` rounded up to
  milliseconds elsewhere), allocates per call and returns `EINTR` to the caller, which treats it as
  a wakeup. The kernel's waits drop the BKL around one `poll` of host fds plus the thread's `Event`.
- `Event::signal` writes 1 (ignoring overflow), `clear` reads until empty.
- `host_cpus` snapshots the process's affinity once (first N CPUs, N = `available_parallelism`
  capped at 64; fallback `0..n`), taken at kernel creation; guest CPU i is `host_cpus()[i]`.
  `cpu_sysfs` reads host CPU sysfs files (unreadable → `None`).
- Per-thread: `gettid`, `set_thread_name` (15 bytes), `thread_cpu_clock` (`pthread_getcpuclockid`),
  `getrusage(RUSAGE_THREAD)` for the calling thread, `/proc/self/task/<tid>/{stat,status}` for
  others; `set_thread_affinity`/`set_thread_nice`/`set_thread_ioprio` act on host tids and ignore
  failures (the kernel applies its own policy first: nice floor −4, RT ioprio downgraded).
- `clock_gettime`/`clock_getres` pass clock ids through; `adjtimex_query` makes the real read-only
  call on Linux and synthesizes an unsynchronized state elsewhere (Android).
- CLI: `RawMode::enter` on stdin; `sigwinch_notifier` installs a handler writing one byte to a
  nonblocking pipe (documented as callable once per process; the CLI calls it once).

### Networking

- Sockets are created `SOCK_NONBLOCK|SOCK_CLOEXEC` (`accept4` likewise; non-Linux hosts set the
  flags afterwards); addresses, options and control bytes pass through raw (shared arm64 Linux
  ABI; `sockaddr` writes the Linux layout); `sendmsg` always adds `MSG_NOSIGNAL`; `recvmsg` clamps
  the address to 128 bytes and returns the full length with `MSG_TRUNC`; `accept` and `sockname` do
  not clamp, which is safe because only `AF_INET`/`AF_INET6` host sockets exist (AF_UNIX and
  netlink never reach the hal) and Linux caps the reported length at 128.
- `interfaces()` walks `getifaddrs` (one entry per name, index from `if_nametoindex`, flags from
  the first entry, MTU fixed at 65536 for loopback and 1500 otherwise, MAC from `AF_PACKET` entries,
  IPv4/IPv6 addresses with prefix lengths; no broadcast or scope), sorted by index.
- `routes()` parses host `/proc/net/route` and `/proc/net/ipv6_route` (`None` when unreadable, as on
  Android), dropping reject/anycast/cache/local and multicast routes; `source_for(dst)` connects a
  UDP socket and reads its local address; `tx_queue_len` reads sysfs.
- `dns_query`: on Android `android_res_nsend` on the default network (linked against libandroid,
  API 29), up to 30 s, then cancel and `ETIMEDOUT`; the answer is returned whatever its rcode. On
  other hosts up to three servers from `/etc/resolv.conf`, UDP with a 3 s wait and ID check, TCP
  retry on truncation (length-prefixed), `EHOSTUNREACH` without servers; a failed server (including
  an interrupted wait) moves on to the next.

## Dependencies (inbound / outbound)

### Inbound

- `core/kernel` throughout (mm, store, VFS, syscalls, timer thread, forwarder, raw ICMP, terminal,
  procfs, OOM); `hosts/cli` (`RawMode`, `sigwinch_notifier`, `isatty`, tests).

### Outbound

- `libc` (syscalls, epoll, eventfd, getifaddrs, termios, sigaction), `libandroid` on Android (DNS),
  host files (`/proc/self/status`, `/proc/self/pagemap`, `/proc/meminfo`, `/proc/net/route*`,
  `/proc/self/task/*`, `/sys/devices/system/cpu`, `/sys/class/net`, `/dev/urandom` on non-Linux
  hosts, `/etc/resolv.conf`), Rust std (`available_parallelism`).

## Invariants and gotchas

- The hal holds no locks and knows nothing of the BKL; every function may run on any thread
  (calling-thread functions — `gettid`, `set_thread_name`, `thread_cpu_clock`, `_self` statistics —
  must run on the guest's own host thread).
- Guest pages are 4K while hosts may use 16K pages: file mappings and residency results are
  translated accordingly.
- `ioctl` is safe to call but trusts the caller's buffer size.

## Confirmed bugs / technical debt

- **macOS no longer compiles.** `open_dir` uses `O_PATH` (not defined for Apple targets) without a
  `cfg` gate, and the kernel calls the Linux-only `copy_file`, fsync family and `msync` ungated, and
  CI builds only Linux and Android.
- **A signal makes a DNS lookup fail.** Non-Android `dns_query` treats an `EINTR` from `poll` (the
  CLI's `SIGWINCH` handler can cause one) as that server failing, possibly turning into SERVFAIL;
  Android reports an interrupted wait as `ETIMEDOUT` after cancelling. Mismatched-ID datagrams also
  restart the 3 s wait, so there is no overall deadline.
- **The `SIGWINCH` handler does not preserve `errno`** (clobbered only when its write fails, for
  example `EAGAIN` with a full pipe); a failing `sigaction` leaks both pipe fds.

## Open questions

- Should macOS still compile (and if so, with `ENOSYS` stubs or `cfg` gates in the kernel)?
- Should `host_cpus` prefer big cores when the CPU quota is smaller than the affinity set?

## To verify

- Android permissions assumed by comments: `ioprio_*` allowed, `adjtimex` forbidden,
  `/proc/self/task/*` and pagemap readable, cpufreq sysfs blocked.
