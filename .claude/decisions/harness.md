# Run and test harness (harness) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness"`
> Last distilled: 2026-11-03 (HEAD = 12c4afa) — seeded by hand from the design doc §13.3 and the
> round records in `docs/rounds/`; no `MODULE: harness` Decision blocks exist yet, so sources cite
> rounds instead of commits.

## Active

### D1: Differential tests compare appine with a real Linux kernel running the same rootfs
- **What**: Each `tests/diff` script runs in a docker container built from the store's own files and under `appine-run`, and the two outputs (stderr merged, exit code appended) must match byte for byte; `appine-run` itself runs in a container so both sides see the same kind of network.
- **Why**: The real kernel is the reference; exporting the image from the test store keeps the userland identical, so any difference is the kernel layer's; route, interface and address cases are only comparable when both sides share the network setup.
- **Tradeoffs**: Behaviour that changes with the host kernel version is not compared; a failure identical on both sides passes; scripts must normalise addresses, pids and random markers themselves.
- **Source**: round 09 (`09-m7`); design doc §13.3

### D2: Test cases carry their own container and `appine-run` options
- **What**: A case line `# docker-run-options: …` is added to its native container (capabilities, AppArmor off) and `# appine-options: …` to `appine-run`.
- **Why**: Mount tests need `CAP_SYS_ADMIN` and AppArmor off on colima and GitHub's runners; the low-port case needs `--always-map-low-ports` because a root container may bind the real port.
- **Source**: rounds 105 (`105-mount-subset`), 208 (`208-low-ports-direct`)

### D3: Parallel lanes get their own store clones, slowest cases first
- **What**: `-j` lanes (4 by default on macOS, 1 on Linux hosts and CI) each use a copy-on-write clone of the store (APFS `clonefile`, else reflink) and claim a lane with `mkdir`; cases run in descending order of their last recorded time.
- **Why**: Lanes sharing one store overwrote each other's files in the guest `/tmp`; store objects are rewritten in place, so hard links cannot share them; with lanes, the longest case decides the total time. On 8 CPUs, `-j 8` was slower than `-j 4`.
- **Tradeoffs**: Clones must be rebuilt (`-r`) after packages are added to the main store; Linux hosts and CI keep one lane.
- **Source**: round 178 (`178-test-speed`)

### D4: The LTP store lives on a VM-local volume
- **What**: Inside `scripts/dev`, `scripts/ltp` keeps its store on the `appine-ltp` docker volume (`/ltp/store`); `LTP_STORE` can point elsewhere.
- **Why**: Through virtiofs every file operation reaches the Mac, and eight `appine-run`s committing to the same store waited past SQLite's 10 s busy timeout, giving `EIO`; on a local volume a full run takes about a minute.
- **Tradeoffs**: Several `appine-run`s sharing one store happens only in testing, never on Android.
- **Source**: round 178 (`178-test-speed`)

### D5: Release test builds check borrows, and the scripts grep for kernel-layer reports
- **What**: `appine-run` enables the kernel's `borrow-check` feature by default; difftest fails a case, LTP records `BUG` and the survey counts lines when `appine-run`'s stderr has `appine: internal error`.
- **Why**: The borrow check across BKL releases used to exist only in debug builds, while difftest, LTP and the survey use release builds; a release build only kills the offending process, so a test does not necessarily fail (the victim may be a helper in the background).
- **Tradeoffs**: Measurements with the default `appine-run` include the counting; `--no-default-features` removes it.
- **Source**: round 89 (`89-release-borrow-check`)

### D6: CI requires the rootfs and musl instead of skipping
- **What**: Integration tests skip with a message when no rootfs tarball or `musl-gcc` is found, unless `APPINE_REQUIRE_ROOTFS=1` / `APPINE_REQUIRE_MUSL=1`, which CI sets after fetching the rootfs and installing musl-tools.
- **Why**: In CI a missing rootfs or musl must fail the tests rather than skip them.
- **Source**: rounds 03 (`03-m1b`), 04 (`04-m3a`)

### D7: The workload survey is read by a human, not compared
- **What**: `scripts/survey` runs services, tools, suites and AI CLIs inside a store with `APPINE_LOG=1` and reports exit code, time, internal errors and unimplemented syscalls per workload; `-n` runs the same scripts natively for comparison when something looks wrong.
- **Why**: The differential tests cover only a few runtimes (C, coreutils, Go, Java, Node.js, Python, Lua/SQLite, gdb); most services and tools common on a phone terminal had never run in appine, and the survey finds the gaps they actually hit.
- **Source**: round 62 (`62-workload-survey`)

### D8: Store durability is tested by simulating power loss with dm-flakey
- **What**: `tests/crash` runs a write workload on a dm-flakey device, drops writes at a random moment (or kills the process), and checks that every file is one complete version and every fsynced file is whole, with a native run as control.
- **Why**: Killing the app leaves the host page cache intact and never loses data; only a real power loss exposes ordering problems, such as renamed files turning into zero bytes.
- **Tradeoffs**: Needs root, loop devices and dm-flakey in the VM, so it runs by hand, not in CI.
- **Source**: rounds 190 (`190-settings-default-alpine`), 198 (`198-power-loss`)
