# harness Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/harness.md` for the paired decision history.
>
> Verified: 2026-11-10 (full mapping: 4 research concerns; 15 claims checked by 3 independent verifiers — 12 confirmed and 3 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping; latest change: difftest's -R retry of failed cases, used by full.yml)

## Responsibilities

`harness` is how Appine is run and tested on the development host:

- `appine-run` (`hosts/cli`), the command-line front end that runs one guest program, optionally
  in an inode store, through a PTY bridge or the host's stdio, and imports stores;
- the cargo integration tests that drive it (`guest_programs`, `rootfs_programs`,
  `bench_programs`) and the guest test programs (`tests/guest`);
- the benchmark runner `appine-bench` (`tests/bench`);
- the differential tests (`scripts/difftest`, `tests/diff`), including Fuchsia Starnix's and
  gVisor's syscall tests (`scripts/build-starnix-tests`, `tests/starnix`,
  `scripts/build-gvisor-tests`, `tests/gvisor`), the LTP build and runner
  (`scripts/build-ltp`, `scripts/ltp`), the workload survey (`scripts/survey`, `tests/survey`) and
  the power-loss test (`tests/crash`);
- the development container (`scripts/dev`, `scripts/dev.Dockerfile`) and CI's three workflows
  (`.github/workflows/ci.yml`, `android.yml`, `full.yml`).

Root: `hosts/cli/`, `tests/guest/`, `tests/bench/`, `tests/diff/`, `tests/survey/`,
`tests/crash/`, `tests/starnix/`, `tests/gvisor/`, `scripts/dev`, `scripts/dev.Dockerfile`,
`scripts/difftest`, `scripts/build-ltp`, `scripts/ltp`, `scripts/build-starnix-tests`,
`scripts/build-gvisor-tests`, `scripts/survey`, `.github/workflows/`.

Outside the module: the instruction-level lockstep tests (`engine`); unit tests inside each crate;
the kernel's `run`, boot and console bridge (`kernel/process`, `kernel/tty`); the kernel's bug
reports and test hooks (`bug.rs`, `kernel/process`); Android instrumentation tests
(`android/native`); rootfs downloads (`android/setup`).

## Key types

- `RunOptions` (tier, rootfs, terminal, memory limit, low-port offset, always-map-low-ports, init,
  stop pipe) and `Terminal` (input and output fds, the fd whose size is followed, the resize pipe)
  — what `appine-run` passes to the kernel's `run`; `RunReport` (status, Tier 1 statistics),
  `RunError` (I/O, rootfs, exec, load, console, Tier 1 unavailable, internal), `ExitStatus`
  (exited, or signalled with signal, pc, fault address and core dump).
- `appine_bench::Program` (name, rounds, test rounds, workload); `PROGRAMS` (sha256, lz, sort,
  crc32, vm), the shared freestanding `CFLAGS` and `build()` (with `$CC`).
- `Interactive` (in `rootfs_programs.rs`) — a child `appine-run --pty /bin/sh -i` with a fixed
  prompt and `expect`/`send`/`exit` under 30 s deadlines, killed on drop, for PTY tests.
- difftest's per-case directives, matched on any line: `# docker-run-options:`,
  `# appine-options:` and `# difftest-files:`; outputs `target/difftest/<case>.{native,appine,
  appine.err}`, failure copies `failed/<time>-t<tier>-<case>.*`, timings
  `durations-t<tier>.txt` (`<case> <seconds>`), the run lock `.lock` (a symbolic link naming the
  run's pid), lane claims
  `.slot-<n>`, lane stores `<store>-w<n>` (marked `.cloned`); the image `appine-difftest`,
  labelled with the exported directory list.
- Suite tarballs: `target/starnix-tests/starnix-tests-<commit12>.tar` and
  `target/gvisor-tests/gvisor-tests-<commit12>-<list hash>.tar`, each with `bin/`, `BUILT.txt`,
  `SKIPPED.txt` (`<program>: <first error>`), `VERSION` and the licences.
- LTP's `results.txt` lines `<name> <PASS|CONF|FAIL|TIMEOUT|BUG|SKIP> <code> <secs>` (`- -` for
  SKIP), `logs/<name>.log` (and `.prev.log`), `durations.txt`, and the store's `.ltp` stamp;
  survey's `target/survey/<name>.{out,err}` (and `.native.*`) and report lines.

## Public entry points

- `appine-run [options] [--] <program> [args…]`: `--tier 0|1` (default 0), `--rootfs <dir>`,
  `--env N=V` (repeatable), `--pty`/`--no-pty`, `--memory-limit <n[KMG]|none>`,
  `--low-port-offset <n>`, `--always-map-low-ports`, `--init`, `--stats`, `-h`/`--help`,
  `--version`; `appine-run --rootfs <dir> --import <tar.gz>` makes a store.
- `cargo test --workspace` (the three integration test files build only on aarch64 Linux).
- `appine-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]`.
- `scripts/dev [command…]`; `scripts/difftest [-t tier] [-r] [-s store] [-j n] [-R] [case…]`;
  `scripts/build-ltp`; `scripts/ltp [-a] [-j n] [-t tier] [regex]`; `scripts/build-starnix-tests`;
  `scripts/build-gvisor-tests [--list | --all]`;
  `scripts/survey [-t tier] [-s store] [-n image] [names…]`;
  `tests/crash/crash.sh <appine|native> <power|kill> [runs] [bin]` (as root in the VM).
- CI: quick checks on every push and pull request (`ci.yml`), the APK when its inputs change or on
  demand (`android.yml`), the slow suites nightly, on pushes to master and on demand (`full.yml`,
  `gh workflow run full.yml --ref <branch>`).

## Data flow / lifecycle

### `appine-run`

- Options stop at the first argument that is not UTF-8, does not start with `-`, or is `--`; an
  option with a value takes it as the next argument or after `=`, and a flag given a value
  (`--pty=0`) is a usage error. argv is the program and its arguments as given; the environment is
  only the `--env` values. Usage errors (unknown options, bad values, no program) exit 2.
- `--import` (only with `--rootfs`) imports and syncs the store, prints its counts and exits (1 on
  failure, "the inode store already exists" for an existing one); anything after it is ignored.
- Without `--rootfs` the program is a host file and the guest has no filesystem (dynamically linked
  programs fail to load); with `--rootfs` the path is resolved in the store.
- A PTY is used by default when fds 0 and 1 are both terminals (then the host terminal goes raw,
  continuing with a message if it cannot, and `SIGWINCH` resizes the guest console); `--pty`
  forces one (24×80 unless both are terminals), `--no-pty` gives the guest the host's stdio.
  Through the PTY, guest stdout and stderr both reach fd 1.
- The kernel's `run` starts a machine, opens the console, boots by inittab with `--init`, spawns
  the program and bridges the terminal; afterwards it shuts down by inittab (`--init`; a reboot
  request is only a shutdown) or kills everything, then joins the bridge.
- Exit status (after the host terminal is restored): the guest's code; 128 + signal (with a line
  naming the signal, and the pc and address when known); 127 when the program is not found; 1 when
  the store cannot be opened; 126 for every other error (cannot load or execute, the console, no
  Tier 1 on this host, internal errors). `--stats` prints Tier 1's statistics.
- `SIGINT`, `SIGTERM` and `SIGHUP` to `appine-run` (`timeout`, ^C with `--no-pty`, a closed
  terminal) are caught once a run starts (`appine_hal::stop_notifier`) and handed to the kernel's
  `run` (`RunOptions::stop`): the first is sent on to the program, one at least a second later
  ends every guest process (duplicates sent to the whole process group by `timeout` or a terminal
  arrive together); the run then ends as usual, so the inittab shutdown, the final store sync and
  the host terminal's restore still happen.
- `borrow-check` is a default feature, so every build, the release binaries used by difftest,
  LTP and the survey included, checks that no borrow is held across a BKL release.
- Environment knobs read by the kernel and engine: `APPINE_LOG` and `APPINE_STRACE` (on when set,
  not empty and not `0`; tracing also routes fast syscalls through the BKL), `APPINE_PERF_MAP`
  (any value; Tier 1 writes `/tmp/perf-<pid>.map`), `APPINE_ON_BUG` (`kill`/`abort`; by default release
  builds kill only the guest process), `APPINE_TEST_HOOKS` (exactly `1`, only in debug or
  `test-hooks` builds), `APPINE_DBT_REGS` (`go` or `default`), `APPINE_BKL_WATCHDOG` (the
  watchdog's threshold in whole seconds, 10 by default, 0 off; the tests set 1 with a test hook
  that holds the BKL).

### Integration tests and benchmarks

- Guest programs (`tests/guest`): assembly built with `$CC` or the host `cc` (`-nostdlib -static
  -no-pie`), C with `musl-gcc -static -O2 -Wall -Wextra -Werror -pthread`, both natively on the
  aarch64 host; most run as host paths, a few are copied into the store.
- `rootfs_programs` imports the newest Alpine tarball by version under `target/rootfs/` (or
  `APPINE_ROOTFS_TARBALL`) into one shared store per test process, which tests running at the same
  time share; groups cover busybox and files, core dumps and the kernel log, shell semantics and
  job control, procfs/sysfs/net, the PTY (interactive shell, ^C/^Z, vi, end of input without a
  store), limits, OOM and cgroups, the Tier 1 code cache, the `Machine` API (low ports, several
  consoles, inittab with `appine-run --init`, the Files app's inotify and writes), importing into
  an existing store, user namespaces, answers of Linux 6.1 that kernels after 6.8 changed
  (`tiocsti.c`, and `linux61.c`: timerfd and signalfd reads, epoll_wait's error order, FUSE clones,
  loop block sizes), and one network test (`apk add`, `git clone`, Python imports
  at Tier 1 in a store of its own, gated by `APPINE_NETWORK_TESTS=1`, run only by `full.yml`).
  Most cases run under both tiers.
- Without a store or `musl-gcc` the tests skip with a message (each rootfs test that skips says
  so), unless `APPINE_REQUIRE_ROOTFS=1` / `APPINE_REQUIRE_MUSL=1` (set in CI); bug-injection and
  BKL-watchdog tests run only in debug builds and return silently otherwise. No cargo test sends
  `appine-run` a stop signal. The rootfs tests, difftest, LTP and the survey choose the newest
  Alpine tarball by version (`version_key`, `sort -V`).
- `appine-bench` builds the benchmark programs, runs each natively and under Tier 1 (minimum of N
  runs, 5 by default) and under Tier 0 (once, unless skipped), fails if any output differs, and
  prints times, ratios and geometric means; `--quick` uses a twentieth of the rounds; it uses the
  `appine-run` next to its own executable. `bench_programs` checks the outputs with small round
  counts inside `cargo test`.

### Differential tests

- One run at a time: a run creates `target/difftest/.lock`, a symbolic link naming its pid made in
  one step, before touching the store; it waits while the run it names is alive (`ps -p`, which
  also finds another user's process), takes over a lock whose run is gone (or an earlier version's
  lock directory, with or without its pid file) by moving it aside first, so that of two waiters
  finding the same dead run the one that moved away the other's new lock puts it back, and
  removes it on exit; its lanes run under it.
- On aarch64 Linux `appine-run` runs inside a container (`APPINE_IMAGE`, default
  `debian:trixie-slim`, the caller's uid, seccomp unconfined, the repository at its own path) for
  the store import, the tool check, the export and the cases, while the store's package and suite
  installs run it on the host; elsewhere
  everything goes through `scripts/dev`. The binary is a prebuilt one, which difftest never
  builds: on aarch64 Linux `APPINE_RUN`, else `target/linux-arm64/release/appine-run`, falling
  back to `target/release/appine-run` when the chosen one is not executable; elsewhere always
  `target/linux-arm64/release/appine-run`. Only `APPINE_DBT_REGS` is passed through.
- A store (`target/difftest/store`, inside the work tree, since both containers mount only the
  repository) is imported once and gets 22 packages (fuse3, fuse-overlayfs, sshfs, rclone and
  openssh-sftp-server among them, for the `fuse` and `fuse-tools` cases, whose native side needs
  `--device /dev/fuse`); a tool check on every top-level run (`command -v` of each of 17 commands,
  and util-linux's `nsenter` and `dmesg`) aborts if one is missing.
- Each suite tarball (`starnix`, `gvisor`; the last one in `ls` order) is installed into the store
  at `/usr/lib/<suite>-tests` when its `VERSION` differs from the store's.
- The store's top-level directories `bin etc home lib media mnt opt run sbin srv tmp usr var`
  (those that exist, `/tmp` and `/var/tmp` empty; docker provides `/dev`, `/proc` and `/sys`, and
  each case makes `/root`) are exported from inside Appine and imported as the docker image
  `appine-difftest`, labelled with that list, replacing the previous image: on the first run, with
  `-r`, after a package or suite install, when the image is missing, when its label changed or
  when its suite versions differ from the store's (which also remakes the lane clones).
- Each case script runs with stderr merged into stdout on both sides, after `/root` (`$HOME`) and
  `/tmp` are removed and made again empty (modes 700 and 1777), so caches and files earlier runs
  left in the store do not make the sides differ: natively with `docker run` (seccomp unconfined,
  `PATH`, `HOME=/root`) plus the case's `# docker-run-options:` (62 of the 93 cases, 40 of them
  `--privileged`), and as `/bin/sh -c` under `appine-run` plus its `# appine-options:` (two
  cases); an `exit N` line is appended to each output. A case passes when the two outputs are
  byte-identical and `appine-run`'s own stderr has no `appine: internal error` line; scripts
  normalise varying output themselves. A script of 128 KB or more (prefix and packed files
  included) fails, which is why `compat` is split into `compat` to `compat9`.
- `# difftest-files: <dir>` ships `tests/diff/<dir>` with the script: a ustar archive, gzipped and
  base64-encoded, unpacked by the guest's busybox into `/tmp/difftest-files/<dir>` before the case
  runs on either side. `line-discipline` uses it for Starnix's PTY scenarios and trace generator
  (`tests/diff/line-discipline`, BSD-licensed, copied unchanged with its source commit in a
  README) and runs all 83 of them, each under a 300 s timeout; the eleven suite cases use it for
  the shared gtest runner.
- Lanes (`-j`, default 4 on macOS, 1 elsewhere): each lane claims a slot and uses that slot's store
  clone (APFS clone, or a copy with `--reflink=auto`; remade only when unmarked or on a rebuild),
  cases go slowest first by recorded time, and each lane prints its report in one write; failures
  are copied to `failed/`. Only parallel runs record durations, and only for cases that passed.
- A case name without `tests/diff/<name>.sh` is a failure, and so is every requested case that no
  lane reported (a lane that died); lanes rerun the script by its absolute path, so a run started
  from another directory works.
- With `-R` every failed case runs once more after all the others, alone and on the main store: one
  that passes then is reported `flaky` (counted in the summary, the run's exit status 0 if nothing
  else failed), one that fails again prints both reports; the first failure's outputs stay in
  `failed/`.
- A case running a prebuilt suite (`run.sh <suite> "$SKIP" <program>…`) is reported `SKIP` when
  that suite's tarball has not been built, and fails when the tarball lacks a program it names
  (unless `SKIPPED.txt` lists it as not building); the summary counts the skipped cases, which do
  not fail the run.

### Starnix's and gVisor's syscall tests

- `build-starnix-tests` fetches a pinned Fuchsia commit (a sparse checkout of
  `src/starnix/tests/syscalls/cpp` and the libraries it uses: fit, stdcompat, fbl's headers, files,
  fxl strings, utf-utils) and a pinned googletest (sha256), and in `alpine:3.24` builds every
  `*_test.cc` with clang, statically against musl, stripped, into its tarball; failures go to
  `SKIPPED.txt` with their first error. The sources are used unchanged; `tests/starnix/compat`
  supplies glibc's `TEMP_FAILURE_RETRY` (forced include), a `<linux/prctl.h>` that defers to
  musl's `<sys/prctl.h>`, and Android's copy of `<linux/bpf.h>`.
- `build-gvisor-tests` fetches a pinned gVisor commit (a sparse checkout of `test/syscalls/linux`
  and `test/util`) and pinned abseil, googletest and google benchmark releases (sha256), and in
  `alpine:3.24` (`tests/gvisor/build.sh`, which reuses dependencies built earlier in the same
  container) compiles the test library and, without Bazel, each program from the sources and local
  libraries `tests/gvisor/parse-build.awk` reads from the BUILD file (arm64 branches of
  `select_arch` only; test-case libraries linked as objects so their tests register; a source that
  does not compile shows only when a program needing it fails to link). `tests/gvisor/compat`
  supplies glibc names musl lacks (forced include), headers that defer kernel headers to musl's
  where both define the same structures, and a `RunfilePath` that finds helper programs in
  `/usr/lib/gvisor-tests/bin`. Only the programs named in the `gvisor-*` cases (the list `--list`
  prints and whose hash names the tarball) and every helper (targets not ending in `_test`) are
  built, older tarballs removed; `--all` builds every program that builds for arm64 into
  `target/gvisor-tests/all/` for a full run by hand (too large for the difftest image on the
  development VM).
- Each builder does nothing while its tarball's name exists, so a local change to `tests/starnix`
  or `tests/gvisor` takes effect only after the tarball is deleted (CI keys its caches on them).
- Seven Starnix cases cover 65 of its built programs by subsystem (`starnix-mm`, `-process`,
  `-signal`, `-fs`, `-proc`, `-io`, and `-overlay`, which runs overlayfs_test and fs_test's
  `OverlayFs` tests with `TEST_TMPDIR` on a tmpfs, since Linux refuses docker's overlay root as an
  upper layer), and four gVisor cases 43 of its programs (`gvisor-process`, `-fs`, `-net`,
  `-unix`). Each calls the shipped `tests/diff/gtest/run.sh` once (difftest reads the suite from
  that one call) with the suite, its skip filter and its program list, and optionally `ONLY`, a
  gtest filter of the only tests to run; the runner lists each program's tests (minus disabled
  ones) and runs each in its own process under `timeout -s KILL 60`, stdin from `/dev/null` and
  its output in a file (a crash or hang, a test ignoring `SIGTERM`, a child keeping the output open
  or a test reading stdin does not take the rest with it), printing `<test> ok|failed|skipped|exit
  <code>`, with core files off, the shell's crash reports filtered out and `/tmp` set back to 1777
  at the end.
- Skips are a negative gtest filter in each case, grouped by reason: a deviation recorded in a
  kernel map (named with its module), a feature decided against, or a test that cannot be compared
  (it fails on the native side because of docker, the VM or a newer kernel, answers differently
  only on CI's runner, depends on timing, or hangs on both sides), or a gVisor test that sleeps
  10 s or more by design (left out for CI time). The runner's header lists the programs in no case and why: those that would reboot,
  suspend or set the clock of the VM, features Appine does not have (bpf maps and other program
  types, perf, userfaultfd, tun), devices neither side has, and two with nothing to compare.

### LTP

- `build-ltp` builds a pinned LTP release in `alpine:3.24` with gcc: its libraries (a failure
  there stops the build), then each `testcases/kernel/syscalls` directory with `make -k` (a
  directory that fails is reported and installed as far as it built), into
  `target/ltp/ltp-<ver>.tar`.
- `ltp` (GNU tools, so inside `scripts/dev` on macOS) builds `appine-run` into
  `target/linux-arm64` first (unless `APPINE_RUN` names one), and uses a store on the `appine-ltp`
  volume when `/ltp` exists (inside `scripts/dev`), else `target/ltp/store` (`LTP_STORE`
  overrides), made again with LTP unpacked whenever the stamp of the LTP tarball and rootfs it
  records (names, sizes, times) changes (not when `appine-run` changes); a kernel config listing
  Appine's features (loop devices included) is rewritten on every run and passed as
  `KCONFIG_PATH`.
- Tests that need a device get one of Appine's loop devices over a file in their `/tmp` directory,
  and `LTP_DEV_FS_TYPE=tmpfs` makes LTP skip mkfs and mount a tmpfs there instead (Appine has no
  disk filesystems), as Starnix runs LTP; tests that need a particular disk filesystem report
  TCONF, and some (remounting the device to find files again, mounting a read-only device, mount
  errors of a block device) fail on Linux's tmpfs too.
- Tests from `runtest/syscalls` matching the regex (minus `sigtimedwait01`, `sigwaitinfo01` unless
  `-a`) run `-j` at a time (8 by default; tier 1 by default), slowest first, all on the one store,
  each in a fresh `appine-run` under `timeout 900` with `LTP_TIMEOUT_MUL=5`, `LTP_RUNTIME_MUL=0.1`;
  exit 0 PASS, 32 CONF, 124 TIMEOUT, else FAIL, and any `appine: internal error` in the log makes
  it BUG.

### Survey and crash

- `survey` keeps a store with 36 packages (network needed to build it; one whose creation did not
  finish, marked by `<store>.partial`, is made again; a store is never re-provisioned when the
  package list changes) and uses a prebuilt `appine-run` (`APPINE_RUN`, else the
  `target/linux-arm64` or `target/release` one), going through `scripts/dev` off aarch64 Linux.
  Each workload script runs under `APPINE_LOG=1` and a timeout (`SURVEY_TIMEOUT`, 1500 s), and
  one line reports exit code, time, the count of internal errors, the unimplemented syscalls seen
  (`ENOSYS:`) and the other unimplemented paths (`UNIMPLEMENTED:`: prctl, fcntl, ioctl, madvise and
  futex values, missing names under `/proc`, `/sys` and `/dev`, devices without a driver), read
  from the kernel's first-hit lines (`appine: unimplemented …`, at most 128 distinct paths a run).
  `-n image` runs only the native side, in docker, reporting exit code and time. With no names only
  the top-level scripts run, the container-engine ones included, though those need a store of
  their own (`-s target/survey/docker` or `podman`); `glibc/`, `suites/` and `ai/` (a mock model
  API) run by name. Nothing is judged automatically.
- `tests/crash` (root in the VM, dm-flakey, loop devices, the `appine-dev` image, Alpine 3.24.3
  hard-coded) runs a write workload in `appine-run`, drops writes at a random moment (power loss)
  or kills it, remounts, and checks that each file is one complete version and every fsynced file
  is whole; `native` mode is the control. `fsbench.c` is a standalone benchmark.

### Development container and CI

- `scripts/dev` rebuilds the image `appine-dev` (quietly, from `scripts/`) on every call and runs
  a command in it (a shell without one): `rust:1.90.0-slim-trixie` plus rustfmt, clippy,
  rust-analyzer, llvm, clang and musl-tools, as root, with the repository at `/work`,
  `CARGO_TARGET_DIR=/work/target/linux-arm64`, volumes for the cargo registry and `/ltp`,
  `--ulimit nice=40:40`, the Docker daemon's proxy settings, `-it` only when stdin and stdout are
  terminals, docker's default seccomp, and only `APPINE_DBT_REGS` forwarded (when set).
- `ci.yml`, on every push and pull request that changes more than `docs/`, `.claude/`, Markdown
  or `android/`: Linux arm64 (`ubuntu-24.04-arm`) fmt, clippy (`-D warnings`), fetch the rootfs
  and `cargo test --workspace` with the require switches.
- `android.yml`, on pushes and pull requests that change `core/`, `hosts/android/`, `android/`,
  the Cargo files or the rootfs and Ghostty scripts (or on demand), on x86_64 (`ubuntu-24.04`,
  for aapt2): NDK 29.0.14206865, Java 21, cargo-ndk (unpinned), the rootfs, Zig 0.17.0 (from
  ziglang.org, checked against a pinned sha256) and `scripts/fetch-ghostty` (with
  `target/ghostty` and Zig's global cache cached by the script's hash), then cargo-ndk clippy at
  API level 30, `assembleDebug lintDebug`, the APK as an artifact. Both quick workflows cancel a
  run in progress when the same branch is pushed again.
- `full.yml`, nightly at 18:00 UTC on develop when its tip was committed in the last 25 hours, on
  pushes to master and on demand: the network test, a lockstep run of 200,000 iterations,
  `scripts/build-starnix-tests` (its tarball cached by the hash of the script and
  `tests/starnix/compat`) and `scripts/build-gvisor-tests` (cached by the hash of the script,
  `tests/gvisor` and its program list), both caches restored and saved in steps of their own so
  that a build is saved even when a later step fails, a release build of `appine-run`,
  `scripts/difftest -R` at Tier 1 with one lane (`ping_group_range` opened, `vmstat` sampling the
  runner every 10 s, `target/difftest` deleted before and after, so every run imports the store and
  installs its packages again; every case's `.appine.err`, the failed cases' outputs and the
  samples uploaded as the `difftest` artifact whatever the result), and the engine tests built
  with clang. No workflow sets a timeout, and `full.yml` has no concurrency group.
- Not in CI: LTP, the survey, instrumentation tests (no KVM), Tier 0 difftest, parallel lanes,
  the performance probe (`appine-bench`).

## Dependencies (inbound / outbound)

### Inbound

- Developers and scripts; CI.

### Outbound

- `appine_kernel` (`run`, `import_rootfs`, `sync_store`, `Machine` in some tests), `appine_hal`
  (`RawMode`, `isatty`, `sigwinch_notifier`, `stop_notifier`); `appine-bench` as a dev-dependency;
  docker and colima; `scripts/fetch-rootfs`; musl-tools; the network for package installs, suite
  sources (fuchsia.googlesource.com, GitHub) and the LTP download; GitHub Actions and their
  runners, ziglang.org, the Android SDK manager.

## Invariants and gotchas

- The bug detector everywhere is a line starting with `appine: internal error` on `appine-run`'s
  stderr; release builds kill only the guest process, so the run continues and only the grep
  notices.
- Difftest compares the two sides only with each other: a failure identical on both sides passes.
- CI's native side is not the development VM's: a 7.x kernel (overlayfs defaulting to
  `redirect_dir=on`, Linux 5d39580f68e6's `CAP_NET_ADMIN` check that `appine-run`'s host sockets
  then fail), a CPU with SVE and AArch32 at EL0, and runner settings (`rmem_default` and `rmem_max`
  of 1 MiB, `fs.suid_dumpable` 2, docker's stack limit). Cases print only what both kernels give
  (relations instead of such values, explicit mount options), the suite cases skip the rest, and
  6.1's answers that newer kernels changed are checked by `tests/guest` programs instead.
- Store state persists across runs (the survey store, LTP's store, difftest's store and each
  lane's clone, in whatever a case leaves outside `/root` and `/tmp`), while the native side
  starts fresh; `-r` re-exports the image and remakes the clones but installs no packages.
- A partial LTP run overwrites `results.txt` with that subset; durations are merged.
- The native side of a case using loop devices (`loop`) shares the host's devices: it takes
  whatever loop-control hands out, makes its own node for it (a new device has no node in the
  container's `/dev`), prints no device numbers, and waits for an autoclear device to be unbound
  (kernels after 6.1 unbind after the last close, not at once). Disk read-only state survives
  unbinding on Linux, so the case leaves its device writable, with autoclear, as it closes it.
- A case that counts processes must not count its own pipeline: `ls /proc | grep -c` sees `grep`
  only when it started before `ls` read `/proc`, which differs run to run on either side; a shell
  glob (`set -- /proc/[0-9]*; echo $#`) starts no process.
- The rustup components in `scripts/dev.Dockerfile` must match those in `rust-toolchain.toml`:
  rustup installs a listed component the image lacks on every `cargo` call, and the container is
  removed after each run, so it downloads it again every time. CI installs all listed components
  through `rustup toolchain install`.
- Cases with `--privileged` act on the development VM itself: a test that sets the clock succeeds
  natively and moves the VM's clock until its time sync corrects it (Starnix's
  `timerfd_mutable_test` and `time_test` call settimeofday), so such tests must stay out of cases.
- The development VM's Ubuntu 6.8 kernel oopses in AppArmor (`aa_inet_bind_perm`) on an inet bind
  with `AF_UNSPEC` from a confined container. Appine no longer passes such a bind to the host (the
  kernel/net map), but a case whose native side makes one must run unconfined (`--privileged`, or
  `--security-opt apparmor=unconfined` as `inet-bind` does).
- A test that holds the BKL in one long syscall, or spins in one, cannot be stopped by the case's
  guest `timeout`; only killing `appine-run` ends it.
- GitHub runs scheduled workflows only from the default branch (master): the nightly `full.yml`
  uses master's copy of the workflow while checking out develop.
- `full.yml` must build release `appine-run` itself before the differential tests: no other step
  there makes it (cargo test builds debug, lockstep builds only `appine-lockstep`), and difftest
  never builds one.
- busybox's `timeout` sends `SIGTERM` by default: a test that ignores it, or a child that keeps the
  output pipe open, makes `$(timeout …)` wait forever on either side. `timeout -s KILL` with the
  output written to a file avoids both. Output that goes through `grep` inside the guest is
  block-buffered, so a running case's `.appine` file shows no progress.
- CI's clang step relies on the runner image's clang; CI installs only llvm and musl-tools.

## Confirmed bugs / technical debt

- **A forced PTY on a terminal with piped stdout is half set up.** Raw mode needs both fds to be
  terminals, so the host keeps ISIG (^C reaches the program as `SIGINT` through `appine-run`, ^Z
  stops `appine-run` itself), line-buffers input and echoes it while the guest echoes into the
  pipe; the size stays 24×80.
- **Three waiters can still run together:** when one moves aside a lock another waiter has just
  made and a third makes its own before it is put back, the moved one's run goes on without it.

## Open questions

- Should difftest pass `APPINE_ON_BUG` and `APPINE_LOG` through, as it does `APPINE_DBT_REGS`?
- Should partial LTP runs merge into `results.txt` as the durations do?
- Should `--import` install the stop handlers (SIGINT now ends it by default), and is a forced PTY
  with only one side a terminal meant to be supported?

## To verify

- Whether IPv6 is available inside `scripts/dev` containers (if not, the two always-map low-port
  cases skip with a message captured by the test harness); how much Docker's default seccomp in
  `scripts/dev` differs from the unconfined Linux difftest path.
- The suite sizes seen in built tarballs, not in source: 81 Starnix programs build at 86962cb and
  12 do not; `--all` builds 264 gVisor programs (256 tests).
