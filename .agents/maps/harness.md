# harness Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/harness.md` for the paired decision history, including pre-split decisions for harness/cli and harness/diff.
>
> Verified: 2026-11-10 (full mapping: 4 research concerns; 15 claims checked by 3 independent verifiers — 12 confirmed and 3 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-22 (targeted verification only since the full mapping; latest change: add strace, gdb and stat-delta deep probes for stuck R-state threads in full.yml's sy-sample sampler)

## Responsibilities

`harness` owns development-container and CI orchestration, benchmarks, LTP, workload surveys
and power-loss testing. Root: `tests/bench/`, `tests/survey/`, `tests/crash/`, `scripts/dev`,
`scripts/dev.Dockerfile`, `scripts/build-ltp`, `scripts/ltp`, `scripts/survey` and `.github/workflows/`.

The command-line front end, cargo integration tests and guest programs are `harness/cli`;
differential cases and Starnix/gVisor builders are `harness/diff`. Instruction lockstep tests
remain `engine`, crate unit tests their respective modules, Android instrumentation `android/tests`,
Android builds `android/build`, rootfs fetching `android/setup`, kernel run/console handling
`kernel/boot`/`kernel/tty`, and kernel bug hooks `kernel/process`.

## Key types

- `appine_bench::Program` (name, rounds, test rounds, workload); `PROGRAMS` (sha256, lz, sort,
  crc32, vm), the shared freestanding `CFLAGS` and `build()` (with `$CC`).
- LTP's `results.txt` lines `<name> <PASS|CONF|FAIL|TIMEOUT|BUG|SKIP> <code> <secs>` (`- -` for
  SKIP), `logs/<name>.log` (and `.prev.log`), `durations.txt`, and the store's `.ltp` stamp;
  survey's `target/survey/<name>.{out,err}` (and `.native.*`) and report lines.

## Public entry points

- `appine-bench [--runs N] [--quick] [--skip-tier0] [--only NAME] [--stats]`.
- `scripts/dev [command…]`; `scripts/build-ltp`; `scripts/ltp [-a] [-j n] [-t tier] [regex]`;
  `scripts/survey [-t tier] [-s store] [-n image] [names…]`;
  `tests/crash/crash.sh <appine|native> <power|kill> [runs] [bin]` (as root in the VM).
- CI: quick checks on every push and pull request (`ci.yml`), the APK when its inputs change or on
  demand (`android.yml`), the slow suites nightly, on pushes to master and on demand (`full.yml`,
  `gh workflow run full.yml --ref <branch>`).

## Data flow / lifecycle

### Benchmarks

- `appine-bench` builds the benchmark programs, runs each natively and under Tier 1 (minimum of N
  runs, 5 by default) and under Tier 0 (once, unless skipped), fails if any output differs, and
  prints times, ratios and geometric means; `--quick` uses a twentieth of the rounds; it uses the
  `appine-run` next to its own executable. `bench_programs` checks the outputs with small round
  counts inside `cargo test`.

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

- `tests/survey/gpu/` contains the native Vulkan Android instrumentation probe and an ARM64
  distribution fixture builder. The compute sample checks each shader result and reports stage timings.
  The DRM lifetime sample exercises shared mapping access after GEM close, fd close and fork.

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
- `ci.yml`, on every push and pull request that changes more than `docs/`, `.agents/`, Markdown
  or `android/`: Linux arm64 (`ubuntu-24.04-arm`) fmt, clippy (`-D warnings`), fetch the rootfs
  and `cargo test --workspace` with the require switches.
- `android.yml`, on pushes and pull requests that change `core/`, `hosts/android/`, `android/`,
  the Cargo files or the rootfs, Ghostty and virglrenderer fetch scripts (or on demand), on x86_64 (`ubuntu-24.04`,
  for aapt2): NDK 29.0.14206865, SDK CMake 3.22.1, Java 21, cargo-ndk (unpinned), the rootfs, Zig 0.17.0 (from
  ziglang.org, checked against a pinned sha256), `scripts/fetch-ghostty` (with
  `target/ghostty` and Zig's global cache cached by the script's hash) and `scripts/fetch-virglrenderer`
  (with `target/virglrenderer` cached by the script's hash), then cargo-ndk clippy at
  API level 30, `assembleDebug lintDebug`, the APK as an artifact. Both quick workflows cancel a
  run in progress when the same branch is pushed again.
- `full.yml`, nightly at 18:00 UTC on develop when its tip was committed in the last 25 hours, on
  pushes to master and on demand, splits its work after the `changed` gate into three parallel
  paths on `ubuntu-24.04-arm`:
  - `build`: restores/builds/saves `scripts/build-starnix-tests` (keyed on the script and
    `tests/starnix/compat`) and `scripts/build-gvisor-tests` (keyed on the script, `tests/gvisor`
    and its program list) in separate restore/save steps so misses persist even if a later step
    fails, exports both cache keys, builds release `appine-run`, and uploads it as the
    `appine-run` artifact (`retention-days: 7`, `overwrite: true`).
  - `checks`: runs in parallel with `build` and `difftest` — the network test, a 200,000-iteration
    lockstep run, and the engine/lockstep tests built with clang.
  - `difftest` (`needs: [changed, build]`, `fail-fast: false`): five independent single-lane
    shards (`gvisor-process`, `gvisor`, `starnix`, `core-1`, `core-2`) that partition
    `tests/diff/*.sh` deterministically without installing Rust. Each shard downloads and
    `chmod +x`s `appine-run`, installs `strace` and `gdb`, restores `target/rootfs` and (if
    needed) its suite cache with the builder script as fallback, opens `ping_group_range`, runs
    `scripts/difftest -R` on its slice with 10 s `vmstat` and 30 s sampling (`target/sy-sample.log`,
    which also runs up to two `stat`-delta, `strace` and `gdb` deep probes when the same
    `(pid, tid, comm)` stays `R` across consecutive samples), and uploads
    `difftest-<shard>-attempt-<run_attempt>` (`*.appine.err`, `failed/`, `vmstat.log`,
    `sy-sample.log`) on `always()`.
  No workflow sets a timeout, and `full.yml` has no concurrency group.
- Not in CI: LTP, the survey, instrumentation tests (no KVM), Tier 0 difftest, parallel lanes,
  the performance probe (`appine-bench`).

## Dependencies (inbound / outbound)

### Inbound

- Developers and scripts; CI.

### Outbound

- `harness/cli` supplies `appine-run` and cargo integration tests; `harness/diff` supplies
  differential cases and upstream-suite builders. CI builds the CLI before invoking difftest.
- `appine-bench` also serves CLI integration tests as a dev-dependency.
- Docker/colima, musl-tools, `android/setup`'s rootfs artifacts and package/LTP downloads.
- GitHub Actions/runners and the SDK manager/ziglang.org for `android/build`.

## Invariants and gotchas

- The bug detector everywhere is a line starting with `appine: internal error` on `appine-run`'s
  stderr; release builds kill only the guest process, so the run continues and only the grep
  notices.
- Survey and LTP stores persist across runs. `harness/diff` records its own persistent-store
  and clone behavior; a survey store is not re-provisioned merely because its package list changes.
- A partial LTP run overwrites `results.txt` with that subset; durations are merged.
- The rustup components in `scripts/dev.Dockerfile` must match those in `rust-toolchain.toml`:
  rustup installs a listed component the image lacks on every `cargo` call, and the container is
  removed after each run, so it downloads it again every time. CI installs all listed components
  through `rustup toolchain install`.
- A test that holds the BKL in one long syscall, or spins in one, cannot be stopped by the case's
  guest `timeout`; only killing `appine-run` ends it.
- GitHub runs scheduled workflows only from the default branch (master): the nightly `full.yml`
  uses master's copy of the workflow while checking out develop.
- `full.yml`'s `build` job must build release `appine-run` and pass it to the `difftest` shards
  via artifact (`chmod +x` after download, since `actions/download-artifact` strips execute bits):
  no other job builds it (`checks` builds debug tests and `appine-lockstep`), and difftest never
  builds one.
- CI's clang step relies on the runner image's clang; CI installs only llvm and musl-tools.
- `Swatinem/rust-cache@v2` caches all of `target/` and cleans non-Cargo files before saving, which can leave empty subdirectories behind; fetched asset trees under `target/` need their own `actions/cache` step after `rust-cache` and sentinel-file checks rather than bare directory-existence checks.

## Confirmed bugs / technical debt

- On Pixel 11 Pro, Alpine llama.cpp b9564 selects incompatible matrix-multiply and Flash Attention tiles for the PowerVR GPU's 128-lane subgroups. Full offload with `-fa off -ub 16` passed both surveyed Qwen2.5 prompts. The native driver also signals zero-batch submission fences early. The [device survey](../../docs/mobile-gpu.md#powervr-correctness) records the independent controls.
- On Galaxy S23 firmware `S9110ZCS8FZJ1`, llama.cpp b9564 reproduces the Adreno 740 shader link failure during warm-up in both Alpine and native Android. Native runs identify `quantize_q8_1_x4` for both surveyed Qwen2.5 models. The guest logs do not name the shader. The [device survey](../../docs/mobile-gpu.md#llamacpp-on-alpine-324) records the CPU controls and alternative Vulkan settings.
- On Galaxy S25 Ultra firmware `S9380ZCS8BZA1`, Alpine llama.cpp b9564 completes full Vulkan offload on Adreno 830 but produces incorrect text for both surveyed Qwen2.5 prompts. The cause remains open. The [device survey](../../docs/mobile-gpu.md#llamacpp-on-alpine-324) records the CPU controls and alternative Vulkan settings.

## Open questions

- Should partial LTP runs merge into `results.txt` as the durations do?

## To verify

- Whether IPv6 is available inside `scripts/dev` containers (if not, the two always-map low-port
  cases skip with a message captured by the test harness); how much Docker's default seccomp in
  `scripts/dev` differs from the unconfined Linux difftest path.
