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
  statically against musl, and CI builds them too. 64 programs run in seven differential cases by
  subsystem (`starnix-mm`, `-process`, `-signal`, `-fs`, `-proc`, `-io`, `-overlay`) through a
  shared runner, each skipping by reason the tests that differ for a deviation recorded in a map
  or cannot be compared; `starnix-overlay` puts its layers on a tmpfs, since Linux refuses
  docker's overlay root as an upper layer. The other 17 are listed with their reasons in
  `tests/diff/gtest/run.sh`: they would reboot, suspend or set the clock of the VM, test what
  Appine does not implement (bpf maps, perf, process_mrelease, userfaultfd, tun), need devices, or
  have nothing to compare.
- **Full run of the other programs (2026-11-07, at 8dc166b):**
  - About 70 more programs ran one test per process, as root, on native Linux 6.8 and on Appine.
    Of 2484 tests, 1186 passed on both sides, 532 passed only natively (203 of them in
    `bpf_test`) and 138 were skipped only on Appine, mostly for missing sysctls.
  - The failures came down to about 45 unrecorded causes, now in the maps of kernel/process, mm,
    signal, net, tty, cred, poll, procfs and vfs, plus causes the maps had already recorded
    (propagation, PI futexes, absent proc files, the vvar page, the decided `ENOSYS` syscalls).
  - The biggest single causes were a thread-group leader that exits before its threads vanishing
    from `/proc/<pid>/task` (35 tests) and missing or misjudged sysctls (about 170). Both are
    fixed, with seven smaller causes (see "Taken so far").
  - Excluded:
    - behaviour newer than 6.1 (the io_uring sysctls of 6.6, a setresuid test that needs 6.3);
    - docker's environment (the `RLIMIT_NOFILE` hard limit, `/proc/2`, input devices);
    - eight cgroup tests that failed only because an earlier hung test left its directory behind;
    - a pagemap test that crashed only after its frame-number checks had already failed.
- **Second full run (2026-11-07, at fcf4c93), which the cases were cut from:**
  - All 77 programs that do not act on the VM, at Tier 1. Leaving out `mmap_test` and the waitpid
    programs, as the first run did: of 2500 tests, 1294 passed on both sides, 560 only natively
    and 31 were skipped only on Appine. 267 of the native-only passes are in programs now left out
    of the cases (203 in `bpf_test`); the rest are skipped in the cases by reason.
  - The sysctl fixes turned most skips into runs: `/proc/sys` permission tests that used to skip
    now fail for the recorded DAC deviation.
  - New causes, recorded in the maps: four `sched_setattr` details left by bc44b65 (kernel/process),
    a zombie's `mounts` and an exited main thread's `pagemap` taken from someone else (procfs; the
    zombie test read that pagemap until the OOM killer ended it), no superblock flags and
    propagation changes refused on detached mounts (vfs).
  - Native-side failures that are docker's or the VM's, not Linux's: the kernel log wrapping under
    docker's messages (`/dev/kmsg` reads from the start), docker's pid namespace (a dead lock
    owner's pid is 0), its device cgroup (mknod), its network namespace (no `net.core.rmem_max`),
    and a test that leaves lo's address changed for the next one.
- **gVisor's syscall tests** (Apache-2.0), taken after this survey recommended them:
  - `scripts/build-gvisor-tests` builds them without Bazel against the abseil, googletest and
    google benchmark versions gVisor pins; 264 of the 266 programs build for arm64 (the other two
    have x86 sources only). Four `gvisor-*` cases run a first set of the programs, chosen from
    Appine's weaker areas (processes, signals and ptrace; files, terminals and mounts; inet
    sockets and netlink; AF_UNIX); the harness map has the details.
  - A full run at gVisor 9ee2906 (2026-11-07): of about 14,500 tests, about 11,200 passed on both
    sides and about 2,500 only natively, many of them one cause repeated over parameters (the
    AF_UNIX socket-option and control-message tests run once per socket kind).
  - About 40 unrecorded causes went into the maps of kernel/process, signal, vfs, net, netns, unix
    and poll. The most serious: a recursive-bind loop with no `fs.mount-max` holds the BKL for
    ever longer binds and freezes every guest process; mounts inherited by a less privileged
    namespace are not locked; a UDP socket shut down for reading spins in recv; and a bind with
    `AF_UNSPEC` passed to the host oopses the development VM's Ubuntu kernel in AppArmor, leaving
    Appine hung. All four are fixed (see "Taken so far").
- **Not yet used:**
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
| 0b2c3c8 | inet binds check the address family before the host sees it (the AF_UNSPEC oops) |
| 0a4712e | `fs.mount-max`; recursive binds, unmounting and mount lists linear in the table size |
| b3e134a | A UDP socket shut down for reading returns 0 from a blocking recv instead of spinning (with netlink dumps that fit a page) |
| 5530624 | splice, tee and vmsplice wait as Linux 6.1 does (seven skipped Starnix pipe and vmsplice tests now pass) |
| 6d05aa9 | Mounts inherited by a less privileged namespace are locked; proc and sysfs mounts check owner and visibility; `max_*_namespaces` enforced |
| 2a244d2, 4b4af75 | overlayfs with several lower layers and an upper one, after Linux 6.1 rather than Starnix's single-lower module; its `overlayfs_test`, fs_test's overlay tests and gVisor's overlay mount tests run |

b295644, 5dcf7db and 63b9e59 fix bugs found while comparing Starnix's code with Appine's: readv
and writev split datagrams per iovec, mremap with an old length of 0 was always `EINVAL`, and
absolute timers were converted to the host's monotonic clock when set. The commits after 4e64e74
fix deviations the full runs found (0b2c3c8, 0a4712e, b3e134a and 6d05aa9 those of gVisor's;
b3e134a's netlink dumps and 6d05aa9's namespace limits came from the maps instead).

## Not yet taken

Ordered by value for effort.

| Idea | What Starnix does | For Appine | Effort |
|---|---|---|---|
| Count unimplemented paths | `track_stub!` counts each call site and flag value; `not_found` counts `ENOENT` under `/proc`, `/sys`, `/dev` | Count the default arms of prctl, ioctl, fcntl, madvise and futex, and put the top entries in the bug report | Small |
| BKL watchdog | A lockup detector that names threads stuck in one syscall | A thread that takes no BKL, records holder, syscall and pc after 10 s, and reports through the kernel log and the bug report | Small to medium |
| Stay awake while busy | Wake locks, `/sys/power/wake_lock`, alarms that wake the device | The keep-awake switch holds the Android wake lock for as long as the kernel runs. Hold it only while the guest has used CPU recently and drop it after N idle seconds; map `/sys/power/wake_lock`; offer `/sys/class/power_supply` from `ACTION_BATTERY_CHANGED` | Medium |
| Store reflinks | — | `FICLONE` and whole-file `copy_file_range` share a store object until written: coreutils' `cp` and Python's `Path.copy` try `FICLONE` first, and an overlay copy-up of a large file would no longer copy its data | 600–1000 lines |
| Small procfs and netlink files | Has `vmstat`, `swaps`, `locks`, `io`, `oom_score` (some as stubs) | `oom_score` from the OOM killer's score, `swaps` header, `locks` from the lock table; `NETLINK_SOCK_DIAG` that answers `ENOENT` so `ss` falls back to `/proc` quietly | Small each |
| vDSO clock reads | Reads the counter plus a vvar page under a seqlock, falling back to the syscall when the seqlock is busy | The step kernel/signal D4 leaves open; the engine already handles `CNTVCT_EL0` | Medium |
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
- **tun:** Starnix's `modules/tun` (looked at on 2026-11-08, at 7e70df49) only handles
  `TUNSETIFF`, which asks Fuchsia's netstack3 over FIDL for an interface; `read` and `write` return
  `EOPNOTSUPP`, and its one test only checks that the interface name appears. The packets are
  netstack3's work, in a separate component. Appine has no network stack to attach a device to
  (kernel/netns map), so TUN would need one in the kernel layer (smoltcp is the candidate) plus a
  second socket backend chosen by route, and policy routing for full-tunnel VPNs. Its main user,
  rootless podman through pasta, can run as root instead, where bridge networks work.
- **io_uring:** already rejected (docs/design/15-rejected.md).
