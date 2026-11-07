# Fuchsia Starnix as a reference

Starnix is Fuchsia's Linux syscall layer, written in Rust. It was surveyed twice for code, tests
and ideas Appine could use: on 2026-11-06 at Fuchsia 86962cb and on 2026-11-07 at 06ce521f.
Almost none of its code can be moved over, because it is built on Zircon, FIDL and `starnix_core`,
and its execution model (Zircon restricted mode) has nothing in common with a soft MMU. What pays
is its test suites and its reading of Linux semantics. The deviations its tests found are recorded
in the module maps, not here.

## Code

- **Usable on its own:**
  - `lib/line_discipline` depends only on `starnix_uapi`; its algorithms and its 83 recorded PTY
    scenarios were used.
  - The arm64 assembly of `lib/usercopy` is a hermetic copy, but its fault handling would have to
    become the pc-range fixup of the engine's host fault guard.
  - The runtime lock-order checks of `starnix_sync`.
  - `lib/ebpf`, which has no use yet.
- **Not usable:**
  - `starnix_rcu` needs Fuchsia's `fuchsia_rcu`. Appine's no-BKL path resolver already validates
    its reads with change counters.
  - The vendored `rust_netlink` crates would add dependencies. Appine's netlink encoders are small
    and checked byte for byte by the differential tests.
  - The FUSE, overlayfs, userfaultfd and perf modules are written against its `FsNode`/`FileOps`
    and Zircon.
- **Locking:** Starnix's per-object locks are no argument for replacing the BKL (kernel/process D2).
  It has some of the same signal bugs as Appine.

## Tests

- **LTP:** the device tests run on tmpfs over loop devices, with `LTP_DEV` plus
  `LTP_DEV_FS_TYPE=tmpfs`, as Starnix runs them. LTP passes went from 1014 to 1081.
- **PTY scenarios:** the 83 recorded scenarios are the `line-discipline` differential case. All of
  them pass since the line discipline follows n_tty.
- **`tests/syscalls/cpp`:** `scripts/build-starnix-tests` builds 81 of its 93 gtest programs
  statically against musl. Only `mmap_test` and the two waitpid programs have cases
  (`starnix-mmap`, `starnix-wait`).
- **Full run of the other programs (2026-11-07, at 8dc166b):**
  - About 70 more programs ran one test per process, as root, on native Linux 6.8 and on Appine.
    Of 2484 tests, 1186 passed on both sides, 532 passed only natively (203 of them in
    `bpf_test`) and 138 were skipped only on Appine, mostly for missing sysctls.
  - The failures came down to about 45 unrecorded causes, now in the maps of kernel/process, mm,
    signal, net, tty, cred, poll, procfs and vfs, plus causes the maps had already recorded
    (propagation, PI futexes, absent proc files, the vvar page, the decided `ENOSYS` syscalls).
  - The biggest single causes were a thread-group leader that exits before its threads vanishing
    from `/proc/<pid>/task` (35 tests) and missing or misjudged sysctls (about 170). Both are
    fixed, with seven smaller causes (see "Taken so far"); the run has not been repeated since.
  - Excluded:
    - behaviour newer than 6.1 (the io_uring sysctls of 6.6, a setresuid test that needs 6.3);
    - docker's environment (the `RLIMIT_NOFILE` hard limit, `/proc/2`, input devices);
    - eight cgroup tests that failed only because an earlier hung test left its directory behind;
    - a pagemap test that crashed only after its frame-number checks had already failed.
- **Not yet used:**
  - **gVisor's syscall tests** (Apache-2.0): 298 sources, 258 programs, 4298 `TEST`s before
    parameterisation. They need googletest, abseil and google benchmark, not Bazel. A syntax-only
    pass under musl with a few compatibility headers suggests about 250 programs would build;
    linking was not tried. They cover pty, mount, netlink route, ptrace, signals, exec and sockets,
    which are Appine's weaker areas.
  - **kselftest**: mostly GPL, so download it at build time. Of 456 sources in the relevant
    directories, 311 pass a raw syntax check under musl. Of those directories, clone3,
    pid_namespace, ptrace and timens all pass; futex, pidfd, proc and timers almost all do.

## Taken so far

| Commit | What |
|---|---|
| a26c8b4 | Loop devices; LTP's device tests on tmpfs |
| 24ecd25 | Starnix's PTY scenarios as a differential case |
| b295644 | readv/writev as one operation on pipes, terminals and sockets |
| 5dcf7db | mremap with an old length of 0 duplicates shared mappings |
| 63b9e59 | Wall-clock deadlines stay on their clocks across clock jumps and sleep |
| 1459292 | `scripts/build-starnix-tests`, the `starnix-mmap` and `starnix-wait` cases |
| e03c0cc | Linux's VMA boundaries, `PROT_GROWSDOWN` and stack growth (the gaps `mmap_test` showed) |
| 8dc166b | The line discipline follows n_tty |
| 4e64e74 | The full run's deviations recorded in nine maps |
| 09188ec | Record-lock ranges at the edges of `off_t`, among other arguments at the edges of their types |
| 42d2ffe | The common sysctls; root can write the read-only ones |
| 79a60cf | An exited main thread stays in `task/` as a zombie until the process is reaped |
| 60921b2 | `/proc/vmstat` |
| 97b1d62 | `RLIMIT_MSGQUEUE` starts at 819200 bytes |
| bc44b65, e43c40c | `sched_setattr`'s keep and clamp flags |
| cbf7267 | membarrier registrations survive fork |
| 6591eb6 | Thread names shown as their bytes |
| 566da95 | `RssAnon`, `RssFile` and `RssShmem` in `status` |

b295644, 5dcf7db and 63b9e59 fix bugs found while comparing Starnix's code with Appine's: readv
and writev split datagrams per iovec, mremap with an old length of 0 was always `EINVAL`, and
absolute timers were converted to the host's monotonic clock when set. The commits after 4e64e74
fix deviations the full run found.

## Not yet taken

Ordered by value for effort.

| Idea | What Starnix does | For Appine | Effort |
|---|---|---|---|
| Wire the other Starnix programs | Runs them in CI with expectation files | Run them all again to see what the fixes since the full run changed, then one case per group of programs, skipping by reason as `starnix-mmap` does; CI does not build the tests yet (an open question in the harness map) | Small |
| gVisor's tests | Runs prebuilt glibc binaries with a long expectation file | A pinned build script like `build-starnix-tests`, starting with about 25 programs | Medium |
| Count unimplemented paths | `track_stub!` counts each call site and flag value; `not_found` counts `ENOENT` under `/proc`, `/sys`, `/dev` | Count the default arms of prctl, ioctl, fcntl, madvise and futex, and put the top entries in the bug report | Small |
| BKL watchdog | A lockup detector that names threads stuck in one syscall | A thread that takes no BKL, records holder, syscall and pc after 10 s, and reports through the kernel log and the bug report | Small to medium |
| Stay awake while busy | Wake locks, `/sys/power/wake_lock`, alarms that wake the device | The keep-awake switch holds the Android wake lock for as long as the kernel runs. Hold it only while the guest has used CPU recently and drop it after N idle seconds; map `/sys/power/wake_lock`; offer `/sys/class/power_supply` from `ACTION_BATTERY_CHANGED` | Medium |
| Store reflinks | — | `FICLONE` and whole-file `copy_file_range` share a store object until written; podman's and docker's vfs storage driver try `FICLONE` first | 600–1000 lines |
| Small procfs and netlink files | Has `vmstat`, `swaps`, `locks`, `io`, `oom_score` (some as stubs) | `oom_score` from the OOM killer's score, `swaps` header, `locks` from the lock table; `NETLINK_SOCK_DIAG` that answers `ENOENT` so `ss` falls back to `/proc` quietly | Small each |
| vDSO clock reads | Reads the counter plus a vvar page under a seqlock, falling back to the syscall when the seqlock is busy | The step kernel/signal D4 leaves open; the engine already handles `CNTVCT_EL0` | Medium |
| overlayfs | One lower layer only | Docker and podman need several; directories merged, other files given as the store nodes of either layer | 2–3k lines |
| FUSE | INIT negotiation, about 25 opcodes, interrupts, no page cache | A new node kind, waiting on the daemon with the BKL released, private mappings of FUSE files; sshfs and rclone mount are the users | 3–5k lines |
| `process_mrelease`, `process_madvise` | `process_mrelease` implemented for lmkd | Revisits the decision to wait for a user (round 139) | About 100 lines |
| Guest crash lines | Keeps the last 64 fatal signals with throttling per program | `debug.exception-trace` is only recorded (42d2ffe). While it is non-zero, one rate-limited kernel log line per unhandled fault signal, as arm64's `arm64_show_signal` does | Small |

## Not worth taking

- **userfaultfd:** Starnix supports only the `SIGBUS` mode for Android's runtime. On Linux its users
  are CRIU and VM migration, and docker's default seccomp profile blocks it.
- **perf_event_open:** Starnix builds it on a Zircon job sampler and stubs the counters; perf and
  async-profiler fall back to itimers without it.
- **PSI beyond empty files:** Starnix has real data only for memory. A zero-filled
  `/proc/pressure` would at most quiet the tools that refuse to start without it.
- **Alarm clocks through AlarmManager, and a `CLOCK_MONOTONIC` that counts suspend:** the first
  needs exact-alarm permission and is useless without the wake-lock change. The second changes
  the 6.1 contract.
- **Netlink stubs that only answer `NLMSG_DONE`:** Appine stopped doing this for unsupported
  families in round 83.
- **Guest-initiated suspend, container freezing, `wakeup_count`:** Android decides when the phone
  sleeps.
- **tun and io_uring:** already rejected (docs/design/15-rejected.md).
