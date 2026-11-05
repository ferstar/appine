# Run and test harness (harness) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness"`
>
> Last distilled: 2026-11-06 (HEAD = afd086a). D1–D8 were seeded by hand from the design doc §13.3
> and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite commits.

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

### D9: The harness module is appine-run plus the layers that drive it
- **What**: harness holds `hosts/cli`, `tests/{guest,bench,diff,survey,crash}`, the dev container, the difftest, LTP and survey scripts and CI; lockstep stays with engine, `scripts/fetch-rootfs` with android/setup and `scripts/release-sources` with android/native, and design §13.3 remains the overview.
- **Why**: hosts/cli was the last code with no module, its value lies in the test layers that drive it, and those layers' gotchas lived only in personal notes; `harness` names the drivers rather than every test.
- **Tradeoffs**: "All tests" span two maps (lockstep is in engine), and the map repeats part of §13.3's overview.
- **Watch out**: Recorded weaknesses let tests pass without running (the low-ports early return, silent skips without a rootfs), and the two difftest sides start from different files, so results can be flaky.
- **Source**: 16ac7a2 (list since updated by 1f93e3d)

### D10: CI installs a pinned Zig and fetches the Ghostty source for the Android job
- **What**: The Android job downloads zig-x86_64-linux-0.17.0 from ziglang.org, checks it against the sha256 copied from ziglang.org/download/index.json, puts it on `PATH`, runs `scripts/fetch-ghostty`, and caches `target/ghostty` and `~/.cache/zig` keyed by the script's hash.
- **Why**: The Android native library now always builds libghostty-vt, which needs Zig and the Ghostty source (android/native D6).
- **Tradeoffs**: No third-party setup action, but ziglang.org asks CI users to prefer mirrors; the cache key ignores Zig's own build cache, so later runs reuse only the first run's artifacts.
- **Watch out**: A download failure from ziglang.org fails the job.
- **Source**: b38d29c

### D11: Semantics the native kernel cannot show are tested by guest programs on both tiers
- **What**: Where Linux 6.1's behaviour differs from the native kernel the differential tests run on (unprivileged TIOCSTI), a program in `tests/guest` runs in the rootfs store on both tiers through `rootfs_programs`, and the differential case keeps only what both kernels share (here with `--cap-add SYS_ADMIN`, treating `EPERM` and `EIO` alike).
- **Why**: Unprivileged 6.1 semantics cannot be compared with a newer native kernel, and normalising the errors in the differential case alone would hide the own-terminal case.
- **Tradeoffs**: The guest program runs only where musl-gcc and the rootfs tarball exist (CI and the dev container).
- **Watch out**: The rest of the pty group now runs natively with `CAP_SYS_ADMIN`; it passes unchanged.
- **Source**: ef5f836

### D12: difftest fails every requested case that reports no result
- **What**: After the lanes finish, each case in `.groups` without an ok or FAIL line is reported as "no result" and counted as failed, `run_group` fails a name without a script, and lanes are started through the script's absolute path computed before the cd to the repository root.
- **Why**: The differential tests are the main regression check, and a run that silently skipped cases reported success.
- **Tradeoffs**: None; a case reported twice still counts once as seen.
- **Watch out**: A lane that prints a malformed report line makes its case count as missing, which errs on the side of failing.
- **Source**: 1f93e3d

### D13: Kernel fixes get a differential case that reproduces the triggering calls
- **What**: Each fix adds a case to an existing differential group that reproduces the calls in Python, C or busybox (huge timeouts in edge-cases, a dying pid namespace in namespaces, master-side ioctls in pty, fork advice in fork, IPv4 and IPv6-only ports in nsports, locks across exec in compat2) instead of a kernel unit test or the real program (nginx, tmux), and runs anything that could hang under `timeout`.
- **Why**: Native Linux is the reference for each fix's semantics, and the programs that hit the bugs are not in the difftest store.
- **Tradeoffs**: The programs themselves are not exercised, each interrupted wait costs 100 ms, and `O_SYNC` durability is not checked in the suite.
- **Watch out**: None recorded beyond each case's scope in its source commit.
- **Source**: 96cd8bd, 65c44f6, d8136b6, e020977, ea08993, 68cafce

### D14: Cases check only what Linux guarantees, not timing or order
- **What**: Cases count processes with the shell's own glob (`set -- /proc/[0-9]*`) instead of a pipeline that races `ls`, and guest tests accept independent answers (DNS A and AAAA) in either order.
- **Why**: The bwrap case's count flipped between 3 and 4 depending on whether grep had started, failing CI on unrelated pushes, and the asynchronous DNS stub answers in resolution order.
- **Tradeoffs**: The bwrap case no longer runs ls or grep inside the sandbox and expects "2 processes".
- **Watch out**: Other cases may still count processes or read `/proc` while their own pipelines run; the map's gotchas say how to count.
- **Source**: 404a804, fe4bb2b

### D15: Test helpers follow the tests that use them
- **What**: Test files keep their tests in order, a helper with one user follows it and shared helpers follow all tests in the order they run; tests/bench puts `main` right after the types, then its helpers in call order.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; the integration tests opened with their helpers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; a reader meets the helpers' names before their definitions.
- **Watch out**: Helpers appended above the tests drift from this order.
- **Source**: 50f9284

### D16: rust-analyzer is part of the pinned toolchain
- **What**: rust-toolchain.toml lists rust-analyzer and scripts/dev.Dockerfile installs the same rustup components, with a comment that the two lists must match.
- **Why**: The VS Code extension's bundled rust-analyzer does not work with the pinned Rust 1.90.0, and a component missing from the dev image is downloaded again in every `--rm` container.
- **Tradeoffs**: The toolchain's rust-analyzer is older than the extension's own; each CI job and the dev image download one more component; the lists are kept in two places by hand.
- **Watch out**: A component added to rust-toolchain.toml but not to the Dockerfile makes every `scripts/dev` call download it again (in the map's gotchas).
- **Source**: 503022d
