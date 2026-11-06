# harness Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/harness.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 3 research concerns; 15 claims checked by 2 independent verifiers — 11 confirmed and 4 partial; bugs and debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: rust-analyzer in the toolchain file and the dev image; LTP's test devices are loop devices running tmpfs; difftest cases can carry files; Starnix's syscall tests and PTY scenarios, skipped only where they cannot be compared)

## Responsibilities

`harness` is how appine is run and tested on the development host:

- `appine-run` (`hosts/cli`), the command-line front end that runs one guest program, optionally
  in an inode store, through a PTY bridge or the host's stdio;
- the cargo integration tests that drive it (`guest_programs`, `rootfs_programs`,
  `bench_programs`) and the guest test programs (`tests/guest`);
- the benchmark runner `appine-bench` (`tests/bench`);
- the differential tests (`scripts/difftest`, `tests/diff`), including Fuchsia Starnix's syscall
  tests (`scripts/build-starnix-tests`, `tests/starnix`), the LTP build and runner
  (`scripts/build-ltp`, `scripts/ltp`), the workload survey (`scripts/survey`, `tests/survey`) and
  the power-loss test (`tests/crash`);
- the development container (`scripts/dev`, `scripts/dev.Dockerfile`) and CI
  (`.github/workflows/ci.yml`).

Root: `hosts/cli/`, `tests/guest/`, `tests/bench/`, `tests/diff/`, `tests/survey/`,
`tests/crash/`, `tests/starnix/`, `scripts/dev`, `scripts/dev.Dockerfile`, `scripts/difftest`,
`scripts/build-ltp`, `scripts/ltp`, `scripts/build-starnix-tests`, `scripts/survey`,
`.github/workflows/`.

Outside the module: the instruction-level lockstep tests (`engine`); unit tests inside each crate;
the kernel's `run`, boot and console bridge (`kernel/process`, `kernel/tty`); bug reporting and
test hooks (`kernel/process`, `bug.rs`); Android instrumentation tests (`android/native`); rootfs
downloads (`android/setup`). Design doc §13.3 keeps the overview of the five test layers.

## Key types

- `RunOptions` (tier, rootfs, terminal, memory limit, low-port offset, always-map-low-ports, init)
  and `Terminal` (input, output, size, resize pipe) — what `appine-run` passes to the kernel's
  `run`; `RunError`, `RunReport`, `ExitStatus`.
- `appine_bench::Program` (`PROGRAMS`, five benchmark programs with rounds and workloads), the
  shared `CFLAGS` and `build()`.
- `Interactive` (in `rootfs_programs.rs`) — a child `appine-run` with `expect`/`send`/`exit` and a
  30 s deadline, for PTY tests.
- File formats: difftest's per-case directives `# docker-run-options:`, `# appine-options:` and
  `# difftest-files:`;
  outputs `target/difftest/<case>.{native,appine,appine.err}`, failure copies under `failed/`,
  timings `durations-t<tier>.txt`; LTP's `results.txt` lines `<name> <PASS|CONF|FAIL|TIMEOUT|BUG|SKIP>
  <code> <secs>`, `logs/<name>.log` (and `.prev.log`), `durations.txt`; survey's
  `target/survey/<name>.{out,err}` (and `.native.*`).

## Public entry points

- `appine-run [options] [--] <program> [args…]`: `--tier 0|1` (default 0), `--rootfs <dir>`,
  `--import <tar.gz>` (with `--rootfs`), `--env N=V` (repeatable), `--pty`/`--no-pty`,
  `--memory-limit <size|none>`, `--low-port-offset <n>`, `--always-map-low-ports`, `--init`,
  `--stats`, `--help`, `--version`.
- `cargo test --workspace` (the three integration test files build only on aarch64 Linux).
- `appine-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]`.
- `scripts/dev <command>`; `scripts/difftest [-t tier] [-r] [-s store] [-j n] [case…]`;
  `scripts/build-ltp`; `scripts/ltp [-a] [-j n] [-t tier] [regex]`; `scripts/build-starnix-tests`;
  `scripts/survey [-t tier] [-s store] [-n image] [names…]`;
  `tests/crash/crash.sh <appine|native> <power|kill> [runs] [bin]` (as root in the VM).
- CI on every push and pull request.

## Data flow / lifecycle

### `appine-run`

- Options stop at the first non-option, `--` or a non-UTF-8 argument. argv is the program and its
  arguments as given; the environment is only the `--env` values.
- Without `--rootfs` the program is a host file and the guest has no filesystem (dynamically linked
  programs fail to load); with `--rootfs` the path is resolved in the store.
- A PTY is used by default when fds 0 and 1 are both terminals (then the host terminal goes raw and
  `SIGWINCH` resizes the guest console); `--pty` forces one (24×80 without a terminal), `--no-pty`
  gives the guest the host's stdio. Through the PTY, guest stdout and stderr both reach fd 1.
- The kernel's `run` opens the console, boots by inittab with `--init`, spawns the program and
  bridges the terminal; afterwards it shuts down by inittab (`--init`) or kills everything.
- Exit status: the guest's code; 128 + signal (with a line naming the signal, and the pc and address
  when known); 127 when the program is not found; 1 when the store cannot be opened or `--import`
  fails; 2 for usage errors; 126 for other errors (cannot load, no Tier 1 on this host, internal
  errors).
- No host signal handlers besides `SIGWINCH`: `SIGTERM` (from `timeout`) or ^C with `--no-pty` kill
  the host process outright.
- Release builds enable the `borrow-check` feature by default, so the release binaries used by
  difftest, LTP and the survey check that no borrow is held across a BKL release.
- Environment knobs read by the kernel and engine: `APPINE_LOG`, `APPINE_STRACE` (also routes fast
  syscalls through the BKL), `APPINE_PERF_MAP`, `APPINE_ON_BUG` (`kill`/`abort`; release builds
  kill only the guest process by default), `APPINE_TEST_HOOKS` (only in debug or `test-hooks`
  builds), `APPINE_DBT_REGS`.

### Integration tests and benchmarks

- Guest programs (`tests/guest`): assembly built with the host `cc` (`-nostdlib -static`), C with
  `musl-gcc -static -O2 -Werror -pthread`, both natively on the aarch64 host; most run as host
  paths, a few are copied into the store.
- `rootfs_programs` imports the lexically newest Alpine tarball under `target/rootfs/` (or
  `APPINE_ROOTFS_TARBALL`) into one shared store per test process; groups cover busybox and files,
  core dumps and the kernel log, shell semantics and job control, procfs/sysfs/net, the PTY
  (interactive shell, ^C/^Z, vi), limits, OOM and cgroups, the Tier 1 code cache, the `Machine` API
  (low ports, several consoles, inittab, Files app inotify), and one network-gated `apk add` + `git
  clone`. Most cases run under both tiers.
- Without a store or `musl-gcc` the tests skip with a message, unless `APPINE_REQUIRE_ROOTFS=1` /
  `APPINE_REQUIRE_MUSL=1` (set in CI); bug-injection tests run only in debug builds.
- `appine-bench` builds the benchmark programs, runs each natively, under Tier 0 (once) and Tier 1
  (minimum of N runs), fails if any output differs, and prints times, ratios and a geometric mean;
  it uses the `appine-run` next to its own executable.

### Differential tests

- On aarch64 Linux `appine-run` runs inside a container (`APPINE_IMAGE`, default
  `debian:trixie-slim`, the caller's uid, seccomp unconfined); elsewhere through `scripts/dev`.
- A store (`target/difftest/store`) is imported once and gets about 18 packages; a tool check aborts
  if any is missing; its `bin etc lib sbin usr tmp` are exported from inside appine and imported as
  the docker image `appine-difftest` (first run or `-r`).
- Each case script runs with stderr merged into stdout on both sides: natively with
  `docker run` plus the case's docker options, and as `/bin/sh -c` under `appine-run` plus the
  case's appine options; an `exit N` line is appended to each output. A case passes when the two
  outputs are byte-identical and `appine-run`'s own stderr has no `appine: internal error` line;
  scripts normalise varying output themselves. A script over 128 KB is rejected.
- `# difftest-files: <dir>` ships `tests/diff/<dir>` with the script: a ustar archive, gzipped and
  base64-encoded, unpacked by the guest's busybox into `/tmp/difftest-files/<dir>` before the case
  runs on either side; it counts towards the 128 KB. `line-discipline` uses it for Starnix's PTY
  scenarios and trace generator (`tests/diff/line-discipline`, BSD-licensed, copied unchanged with
  its source commit in a README) and runs all 83 of them.
- Lanes (`-j`, default 4 on macOS, 1 elsewhere): each lane uses its own store clone (APFS clone or
  reflink copy), cases go slowest first by recorded time, and each lane prints its report in one
  write; failures are copied to `failed/`.
- A case name without `tests/diff/<name>.sh` is a failure, and so is every requested case that no
  lane reported (a lane that died); lanes rerun the script by its absolute path, so a run started
  from another directory works.

### Starnix's syscall tests

- `build-starnix-tests` fetches a pinned Fuchsia commit (a sparse checkout of
  `src/starnix/tests/syscalls/cpp` and the small libraries it uses: fit, stdcompat, fbl's
  `unique_fd`, files, fxl strings, utf-utils) and a pinned googletest (sha256), and in `alpine:3.24`
  builds every `*_test.cc` with clang, statically against musl, stripped, into
  `target/starnix-tests/starnix-tests-<commit>.tar` (`bin/`, `BUILT.txt`, `SKIPPED.txt` with each
  failure's first error, `VERSION`, both licences). The sources are used unchanged;
  `tests/starnix/compat` supplies glibc's `TEMP_FAILURE_RETRY` (forced include), a `<linux/prctl.h>`
  that defers to musl's `<sys/prctl.h>`, and Android's copy of `<linux/bpf.h>`. At 86962cb, 81
  tests build and 12 do not (glibc-only interfaces, Fuchsia-only devices).
- difftest installs the newest tarball into its store at `/usr/lib/starnix-tests` whenever the
  tarball's `VERSION` differs from the installed one, and then exports the image again.
- The `starnix-*` cases list a binary's tests and run each in its own process under
  `timeout 60` (a crash or hang does not take the rest with it), printing `<test> ok|failed|
  skipped|exit <code>`, with core files off and the shell's crash reports filtered out. Tests that
  cannot be compared are skipped through a negative gtest filter, with the reason beside them: in
  `starnix-mmap` two that fail on the native kernel itself (VMA merging of file mappings),
  in `starnix-wait` the `__WCLONE`/`__WALL` bug of the kernel/process map. Without the tarball the
  cases print that it is missing and pass.

### LTP

- `build-ltp` builds a pinned LTP release in `alpine:3.24`, directory by directory, skipping what
  does not build with musl, into `target/ltp/ltp-<ver>.tar`.
- `ltp` uses a store on the `appine-ltp` volume when `/ltp` exists (inside `scripts/dev`), else
  `target/ltp/store` (`LTP_STORE` overrides), built once with LTP unpacked; a kernel config listing
  appine's features (loop devices included) is written and passed as `KCONFIG_PATH`.
- Tests that need a device get one of appine's loop devices over a file in their `/tmp` directory,
  and `LTP_DEV_FS_TYPE=tmpfs` makes LTP skip mkfs and mount a tmpfs there instead (appine has no
  disk filesystems), as Starnix runs LTP; tests that need a particular disk filesystem report
  TCONF, and some (remounting the device to find files again, mounting a read-only device, mount
  errors of a block device) fail on Linux's tmpfs too.
- Tests from `runtest/syscalls` matching the regex (minus `sigtimedwait01`, `sigwaitinfo01` unless
  `-a`) run 8 at a time, slowest first, each in a fresh `appine-run` under `timeout 900` with
  `LTP_TIMEOUT_MUL=5`, `LTP_RUNTIME_MUL=0.1`; exit 0 PASS, 32 CONF, 124 TIMEOUT, else FAIL, and any
  `appine: internal error` in the log makes it BUG.

### Survey and crash

- `survey` keeps a store with about 40 packages (network needed to build it); each workload script
  runs under `APPINE_LOG=1` and a timeout, and one line reports exit code, time, the count of
  internal errors and the unimplemented syscalls seen; `-n image` runs the same scripts in docker
  for comparison; with no names only the top-level scripts run (`glibc/`, `suites/`, `ai/` by name;
  `ai/` uses a mock model API). Nothing is judged automatically.
- `tests/crash` (root in the VM, dm-flakey, loop devices, docker images) runs a write workload in
  `appine-run`, drops writes at a random moment (power loss) or kills it, remounts, and checks that
  each file is one complete version and every fsynced file is whole; `native` mode is the control.

### Development container and CI

- `scripts/dev` runs a command in `rust:1.90.0-slim-trixie` plus rustfmt, clippy, rust-analyzer,
  llvm, clang and musl-tools, as root, with the repository at `/work`,
  `CARGO_TARGET_DIR=/work/target/linux-arm64`, volumes for the cargo registry and `/ltp`, `nice` up
  to 40, the Docker daemon's proxy, `-it` only when attached to a terminal, and only
  `APPINE_DBT_REGS` forwarded.
- CI, Linux arm64 (`ubuntu-24.04-arm`): fmt, clippy (`-D warnings`), fetch the rootfs, `cargo test
  --workspace` with the require switches, the network test, a long lockstep run, release build and
  `appine-bench --runs 3 --skip-tier0`, `scripts/difftest` at Tier 1 with one lane, and the
  engine tests built with clang. CI, Android (x86_64): the NDK, the rootfs, Zig 0.17.0 (from
  ziglang.org, checked against a pinned sha256) and `scripts/fetch-ghostty` (with `target/ghostty`
  and Zig's global cache cached by the script's hash), then cargo-ndk clippy, `assembleDebug
  lintDebug`, the APK as an artifact. Not in CI: LTP, the survey, instrumentation tests (no KVM),
  Tier 0 difftest, parallel lanes.

## Dependencies (inbound / outbound)

### Inbound

- Developers and scripts; CI; `appine-bench` (sibling binary); round records that quote results.

### Outbound

- `appine_kernel` (`run`, `import_rootfs`, `sync_store`, `Machine` in some tests), `appine_hal`
  (`RawMode`, `sigwinch_notifier`, `isatty`); docker and colima; `scripts/fetch-rootfs`; musl-tools;
  the network for package installs and the LTP download.

## Invariants and gotchas

- The bug detector everywhere is a line starting with `appine: internal error` on `appine-run`'s
  stderr; release builds kill only the guest process, so the run continues and only the grep
  notices.
- Difftest compares the two sides only with each other: a failure identical on both sides passes.
- Store state persists across runs (difftest's `/tmp` and `/root`, the survey store, LTP's store),
  while the native side starts fresh.
- Durations are recorded only by parallel difftest runs (`-j 1`, as in CI, never updates them);
  a partial LTP run writes a `results.txt` holding only that subset (durations are merged).
- The native side of a case using loop devices (`loop`) shares the host's devices: it takes
  whatever loop-control hands out, makes its own node for it (a new device has no node in the
  container's `/dev`), prints no device numbers, and waits for an autoclear device to be unbound
  (kernels after 6.1 unbind after the last close, not at once). Disk read-only state survives
  unbinding on Linux, so the case leaves its device bound writable last.
- A case that counts processes must not count its own pipeline: `ls /proc | grep -c` sees `grep`
  only when it started before `ls` read `/proc`, which differs run to run on either side (the
  `bwrap` case failed this way in CI); a shell glob (`set -- /proc/[0-9]*; echo $#`) starts no
  process.
- The rustup components in `scripts/dev.Dockerfile` must match those in `rust-toolchain.toml`:
  rustup installs a listed component the image lacks on every `cargo` call, and the container is
  removed after each run, so it downloads it again every time. CI installs all listed components
  through `rustup toolchain install`.

## Confirmed bugs / technical debt

- **Concurrent difftest runs collide.** Per-case outputs and the report files are not
  tier-qualified, each run wipes the other's lane claims, `-r` deletes clones another run is using,
  and there is no lock.
- **The two difftest sides start from different files.** The native image lacks `/root`, `/var`,
  `/opt`, `/home` and `/run`, while the appine store keeps `/tmp` and `/root` between runs, so
  caches such as `/root/.cache` (Go, uv) exist only on the appine side; cases clean `/tmp` by hand
  and nothing handles `$HOME` (one epoll case assumes an empty uv cache).
- **`scripts/ltp` is stale and fragile outside its container.** Its store is never rebuilt when the
  LTP tarball or rootfs changes, an existing binary is never rebuilt, and outside `scripts/dev` its
  own `cargo build` writes `target/release` while it runs `target/linux-arm64`, with no fallback.
- **Tests can pass vacuously.** The low-ports test returns from the whole test (skipping all four
  cases) when its first bind fails; without a rootfs only the first test prints the skip and the rest
  pass silently; the tarball (and the survey's) is chosen by the lexically greatest name, so 3.3
  would beat 3.24.
- **`appine-run` drops inline flag values** (`--pty=0` turns the PTY on, `--stats=no` turns stats
  on).
- **Killing `appine-run` skips the guest's shutdown.** Without signal handlers, `timeout` or ^C
  (with `--no-pty`) skip the inittab shutdown and the final store sync (losing the 5 ms deferred
  commit window) and, in PTY mode, leave the host terminal raw.
- **A forced PTY on a terminal with piped stdout is half set up.** Raw mode needs both fds to be
  terminals, so the host keeps ISIG (^C/^Z signal `appine-run` itself), line-buffers input and
  echoes it while the guest echoes into the pipe; the size stays 24×80.
- **The host window size uses Linux's `TIOCGWINSZ`** (0x5413), so on macOS the console stays 24×80
  and resizes are ignored.
- **The survey keeps a half-built store.** Only the directory's existence is checked, so a failed
  `apk add` leaves a store the next run uses as is.
- LTP's BUG check greps a log that also holds the guest's output (theoretical misclassification).
- **The `starnix-*` cases pass vacuously without the built tests:** CI does not build them
  (`build-starnix-tests` needs the network and about four minutes), so there they only print that
  the tests are missing.
- **compat2's "after the thread exits" check races:** it tests whether `/proc/<tid>` of a
  non-leader thread still exists right after Python's `join()`, which returns before the kernel has necessarily
  reaped the thread; native usually prints True, and appine printed False once in a full parallel
  run (it passes on its own).

## Open questions

- Should difftest pass `APPINE_ON_BUG` and `APPINE_LOG` through, as it does `APPINE_DBT_REGS`?
- Is native macOS a supported host for `appine-run` itself?
- Should partial LTP runs merge into `results.txt` as the durations do?
- Should CI build Starnix's tests (with a cache keyed by the pinned commit) so the `starnix-*`
  cases run there, and which of the other 79 built test programs should get cases next?

## To verify

- Whether IPv6 is available inside `scripts/dev` containers (if not, the low-ports test is skipped
  there silently); how much Docker's default seccomp in `scripts/dev` differs from the unconfined
  Linux difftest path.
