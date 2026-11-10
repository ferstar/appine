# Run and test harness (harness) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness"`
>
> Last distilled: 2026-11-10 (HEAD = 27609fe). D1–D8 were seeded by hand from the design doc §13.3
> and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite commits.

## Constraint index (compressed active decisions)

> Full text of D1, D3–D6, D8, D10–D12, D14–D17, D20, D21: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/harness.md`

- **D1** — Each `tests/diff` case must print byte-identical output (stderr merged, exit code appended) in a docker container built from the store's own files and under `appine-run`, itself in a container so both sides share the network setup; behaviour that changes with the host kernel version is not compared, and scripts normalise addresses, pids and random markers themselves (round 09)
- **D3** — `-j` lanes (4 on macOS, 1 on Linux hosts and CI) each run on a copy-on-write clone of the store, slowest cases first, and the clones must be rebuilt with `-r` after packages are added to the main store (round 178)
- **D4** — `scripts/ltp` keeps its store on the VM-local `appine-ltp` volume (`LTP_STORE` overrides it), because eight `appine-run`s committing to one store through virtiofs outwaited SQLite's busy timeout and got `EIO` (round 178)
- **D5** — `appine-run` enables the kernel's `borrow-check` by default, and difftest, LTP and the survey treat `appine: internal error` on stderr as a failed case, a `BUG` or a counted line, so measurements with the default build include the check (`--no-default-features` removes it) (round 89)
- **D6** — Integration tests skip without a rootfs tarball or `musl-gcc` unless `APPINE_REQUIRE_ROOTFS=1` / `APPINE_REQUIRE_MUSL=1`, which CI sets so that a missing one fails (rounds 03, 04)
- **D8** — `tests/crash` checks store durability by dropping writes on a dm-flakey device (or killing the process) against a native control run; it needs root, loop devices and dm-flakey in the VM, so it runs by hand, not in CI (rounds 190, 198)
- **D10** — CI's Android job installs zig 0.17.0 from ziglang.org checked against a copied sha256, runs `scripts/fetch-ghostty` and caches `target/ghostty` and `~/.cache/zig` by the script's hash (android/native D6), so a ziglang.org download failure fails the job (b38d29c)
- **D11** — Where Linux 6.1 differs from the native kernel the differential tests run on (unprivileged TIOCSTI), a `tests/guest` program runs on both tiers through `rootfs_programs` and the differential case keeps only what both kernels share (ef5f836)
- **D12** — difftest counts every requested case without an ok or FAIL line as a failed "no result", fails a group name without a script and starts lanes by the script's absolute path, so a lane printing a malformed report line fails its case (1f93e3d)
- **D14** — Cases check only what Linux guarantees: they count processes with the shell's own glob (`set -- /proc/[0-9]*`) rather than a pipeline that races, accept independent answers (DNS A and AAAA) in either order, wait up to 5 s for asynchronous effects such as a joined thread's `/proc/<tid>` going, and read a pipe written by several commands until EOF (404a804, fe4bb2b, c11c1db, 093cb96)
- **D15** — Test files keep their tests in order with a single-use helper after its test and shared helpers after all tests in run order (tests/bench puts `main` right after the types), and helpers appended above the tests drift from this order (50f9284)
- **D16** — rust-analyzer is a component in rust-toolchain.toml and scripts/dev.Dockerfile installs the same rustup components, so a component added to one list but not the other is downloaded again by every `scripts/dev` call (503022d)
- **D17** — scripts/ltp runs LTP's device tests on tmpfs over loop devices (`LTP_DEV_FS_TYPE=tmpfs`, kernel/vfs D21) and `tests/diff/loop.sh` makes its own nodes and waits for autoclear; each test fallocates a sparse 300 MB file in the store's `/tmp`, and the native side sharing the host's loop devices is flaky (recorded in `docs/milestones.md`) (a26c8b4)
- **D20** — difftest, ltp and survey choose Alpine tarballs with `sort -V` and the rootfs tests with `version_key`, as the APK build does (android/native D12), never by lexical order (b014e84)
- **D21** — `appine-run` rejects an inline value on a flag (`--pty=0`) with exit status 2, while options that take a value still accept one inline (`--tier=0`) (c11c1db)

## Active (full detail)

### D2: Test cases carry their own container and `appine-run` options
- **What**: A case line `# docker-run-options: …` is added to its native container (capabilities, AppArmor off) and `# appine-options: …` to `appine-run`.
- **Why**: Mount tests need `CAP_SYS_ADMIN` and AppArmor off on colima and GitHub's runners; the low-port case needs `--always-map-low-ports` because a root container may bind the real port; a native side making an `AF_UNSPEC` inet bind must run unconfined (as inet-bind does), since the VM's kernel oopses on one from an AppArmor-confined container.
- **Watch out**: A colima update may change the VM's kernel, and with it which cases need AppArmor off.
- **Source**: rounds 105 (`105-mount-subset`), 208 (`208-low-ports-direct`); 0b2c3c8

### D7: The workload survey is read by a human, not compared
- **What**: `scripts/survey` runs services, tools, suites and AI CLIs inside a store with `APPINE_LOG=1` and reports exit code, time, internal errors, unimplemented syscalls (its ENOSYS column) and the other unimplemented paths the kernel counts (an UNIMPLEMENTED column, kernel/signal D24) per workload; `-n` runs the same scripts natively for comparison when something looks wrong.
- **Why**: The differential tests cover only a few runtimes (C, coreutils, Go, Java, Node.js, Python, Lua/SQLite, gdb); most services and tools common on a phone terminal had never run in Appine, and the survey finds the gaps they actually hit.
- **Tradeoffs**: The survey shows which paths were hit but not how often.
- **Watch out**: The split depends on the format of the kernel's `appine: unimplemented` lines.
- **Source**: round 62 (`62-workload-survey`); 99b4216

### D9: The harness module is appine-run plus the layers that drive it
- **What**: harness holds `hosts/cli`, `tests/{guest,bench,diff,survey,crash,starnix,gvisor}`, the dev container, the difftest, LTP, Starnix- and gVisor-build and survey scripts and CI; lockstep stays with engine, `scripts/fetch-rootfs` with android/setup and `scripts/release-sources` with android/native, and design §13.3 remains the overview.
- **Why**: hosts/cli was the last code with no module, its value lies in the test layers that drive it, and those layers' gotchas lived only in personal notes; `harness` names the drivers rather than every test.
- **Tradeoffs**: "All tests" span two maps (lockstep is in engine), and the map repeats part of §13.3's overview.
- **Watch out**: Recorded weaknesses stay open in the map: a forced PTY on a terminal with piped stdout is half set up, and three waiters can still pass difftest's lock together (D29).
- **Source**: 16ac7a2 (list since updated by 1f93e3d, 1459292 and ce8a9ce; bug list by 5d23297, 7c5e636, dcacfb6, 6d9a9e8, f5c09f7, d9e1f0c, 300d384 and ca8c583)

### D13: Kernel fixes and features get a differential case that reproduces their calls
- **What**: Each fix adds a case that reproduces the calls in Python, C or busybox to a differential group (an existing one such as edge-cases, namespaces, pty, fork, nsports, compat2 or mount, since the third map-bug triage one privileged group per batch, compat5 to compat9, and for a new feature a case of its own such as vdso or overlay-inotify) instead of a kernel unit test or the real program (nginx, tmux), checks that it fails with the previous binary (moving a check whose trigger a later fix removes to one that still fails without the earlier fix, as `stale.c` in edge-cases moved to a seccomp filter installed with TSYNC), has C cases define the kernel constants and structures they need, since the image has no Linux headers, runs anything that could hang under `timeout`, and removes the Starnix and gVisor skips the fix makes pass.
- **Why**: Native Linux is the reference for each fix's semantics, the programs that hit the bugs are not in the difftest store, gtests skipped for a fixed bug guard nothing until their skips are removed, and a C case that fails to compile on both sides prints the same and passes.
- **Tradeoffs**: The programs themselves are not exercised, each interrupted wait costs 100 ms, timing cases such as `stale.c` take seconds on each side, `O_SYNC` durability is not checked, compat5 to compat9 need `--privileged` on the native side, values that differ between the sides are compared by range or relation (oom_score) or filtered to the case's own (`/proc/locks`), and checks whose answer depends on the host (block counts on the macOS development host's APFS, tmpfs `O_DIRECT` where 6.8 differs from the 6.1 contract, writing the VM's global fs.mount-max) are left out.
- **Watch out**: CI's newer native kernel may differ (netlink group counts, block accounting, the fs.mount-max default, the order of overlay events, sealed system mappings for vdso's mprotect checks), unprivileged user namespaces depend on the native host's settings (Ubuntu's `apparmor_restrict_unprivileged_userns`), the nftables rate case needs ten sends to finish well within half a second, and `stale.c` loses its reproductions without failing when the waiting thread takes more than 0.3 s to start its wait.
- **Source**: 96cd8bd, 65c44f6, d8136b6, e020977, ea08993, 68cafce, 0a4712e, f6a82cc, b43ff37, dcacfb6, 5d7b936, 39ea702, 99b4216, 16364c0, d7076fc, edd7f95, 970cf6a, f5c09f7, 9593bae, 27609fe

### D18: Starnix's PTY scenarios run from files packed into the difftest script
- **What**: A `# difftest-files:` line makes scripts/difftest pack that `tests/diff/<dir>` as a gzipped, base64 ustar archive prepended to the script, which both sides unpack with busybox into `/tmp/difftest-files/<dir>`, and the line-discipline case runs all 83 of Starnix's recorded scenarios (copied unchanged with Fuchsia's licence) one by one against the native kernel.
- **Why**: The scenarios and generator are about 410 KB, a difftest script travels as one `sh -c` argument (128 KB limit), neither side has the work tree, and the store should not change for test data.
- **Tradeoffs**: Packed files count towards the 128 KB, behaviour that changed after 6.1 is compared with the native kernel, and a future failing scenario needs the removed skip mechanism back.
- **Watch out**: CI's native kernel may trace some scenario differently, the generator's 2 s deadlines leave little margin at Tier 0 (where the case takes about three minutes), and updating the copied files must keep the README's commit current.
- **Source**: 24ecd25, 8dc166b

### D19: Fuchsia Starnix's and gVisor's syscall tests run as differential cases, built in CI
- **What**: scripts/build-starnix-tests (a pinned Fuchsia commit and googletest built with clang statically against musl) and scripts/build-gvisor-tests (gVisor's BUILD files parsed with awk, building only the programs the cases name plus helpers) make tarballs that difftest installs into its store and image when their VERSION changes and CI caches by the build inputs' hash, and six `starnix-*` cases by subsystem (plus starnix-overlay, D31) and four `gvisor-*` cases ship the shared `tests/diff/gtest/run.sh` through difftest-files, which runs each test in its own process under `timeout -s KILL 60` with stdin from `/dev/null`, skipping divergences by name under a comment naming the map item, the decision or the native-side cause.
- **Why**: Both suites are Linux-validated gtest corpora (gVisor's covers AF_UNIX, netlink, ptrace, mounts, PTYs and signals in depth, Appine's weaker areas), and while only mmap_test and the waitpid programs had cases and CI never built the tests, the cases passed vacuously there and the other built programs guarded nothing.
- **Tradeoffs**: Skip lists must be edited when a recorded deviation is fixed or the fix goes untested; programs that reboot, suspend, set the VM's clock, test features Appine decided against or need devices are left out, tests that hang on both sides or sleep 10 s or more by design are skipped, and tests the native side is denied under this VM's AppArmor (a file-capability test in an unprivileged user namespace) are not compared; the awk build breaks if gVisor changes its BUILD format; building needs the network, and the suites add about 15 minutes to a one-lane run.
- **Watch out**: CI's newer native kernel may differ on tests that pass under 6.8, googlesource must keep serving fetches by hash on cache misses, privileged cases act on the development VM, busybox's `timeout` sends only SIGTERM unless told otherwise, Pty.SendSignals hangs only under difftest for a reason not found, and a Fuchsia or gVisor upgrade can change tests and need new skips.
- **Source**: 1459292, e03c0cc, 612d238, c11c1db, 70c1bc6, ce8a9ce, 84ff4a7

### D22: CI's Android clippy checks the app's API level
- **What**: The Android job runs `cargo ndk --platform 30 clippy`, the minSdk the app is built for (android/native D3).
- **Why**: CI's Android clippy should check the API level the app is built for.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 0709982

### D23: Script comments say Appine; labels, names and test data stay lowercase
- **What**: Comments in `scripts/`, `tests/diff`, `tests/guest`, `tests/survey`, `tests/crash` and the CLI tests name the system Appine (knowledge-loop D6), while difftest's `appine()` function, the `.appine.err` suffix and the `Ns appine)` label its timing parser reads back, crash.sh's `appine|native` mode, the `appine: internal error` lines the scripts grep for and test data stay lowercase.
- **Why**: Scripts and cases use the same word both as the system's name in comments and as a label, a function name or data.
- **Tradeoffs**: A comment can say "the Appine side" next to an output line that labels it "appine".
- **Watch out**: Renaming the label later would also need difftest's timing parser and the saved reports changed together.
- **Source**: 248d79f

### D24: CI is split into quick checks, the Android build and nightly full tests
- **What**: ci.yml runs fmt, clippy and cargo test on every push and pull request that touches more than docs, `.claude`, Markdown or `android/`; android.yml runs when `core/`, `hosts/android/`, `android/`, the Cargo files or the rootfs and Ghostty scripts change, or by hand; full.yml runs the network test, extended lockstep, the Starnix and gVisor builds, the differential tests (after building the release `appine-run` in a step of its own) and the clang engine tests nightly at 02:00 China time when develop has a commit from the last 25 hours, on pushes to master and by hand; the quick workflows cancel a run a newer push supersedes, and the performance probe is dropped.
- **Why**: Each of the frequent pushes waited 16 minutes (30 or more with the Starnix and gVisor cases) for suites that rarely change their verdict between two pushes.
- **Tradeoffs**: Regressions only the differential tests catch show up the next morning, so risky changes need a local difftest or a manual full run; the nightly run uses master's copy of full.yml; a rust-cache miss adds the release build to full.yml; Tier 1's overhead on Neoverse N2 is no longer recorded in CI logs.
- **Watch out**: GitHub runs scheduled and manual workflows only from the default branch, so a change to full.yml takes effect only once master has it (without 48dce39's build step the differential tests never ran from 0202f1e on), the differential tests' first runs on CI's newer native kernel may show differences colima's 6.8 did not, and a new Android build input outside the path filters goes unbuilt until a run that matches.
- **Source**: 0202f1e, 48dce39

### D25: Signals to appine-run go through the guest, so shutdown still runs
- **What**: `appine-run` catches SIGINT, SIGTERM and SIGHUP with `appine_hal::stop_notifier` (hal D14) and hands them to the kernel's run through `RunOptions::stop` (kernel/process D30): the first is sent on to the program, one at least a second later ends every guest process, and the run then ends as usual.
- **Why**: Killing `appine-run` (`timeout`, ^C with `--no-pty`, a closed terminal) skipped the inittab shutdown, the final store sync and the host terminal's restore.
- **Tradeoffs**: A guest that ignores the forwarded signal needs a second one at least a second later, and duplicates within that second (`timeout` and terminals signal the whole process group) are dropped.
- **Watch out**: None known.
- **Source**: 5d23297

### D26: Test scripts remake stores and binaries they cannot trust
- **What**: scripts/ltp builds `appine-run` into `target/linux-arm64` unless `APPINE_RUN` names one and remakes its store whenever the recorded stamp of the LTP tarball and rootfs (names, sizes, times) changes, and scripts/survey writes `<store>.partial` before creating a store, removes it after `apk add`, and remakes a store it finds marked.
- **Why**: scripts/ltp reused a stale store and binary, and a failed survey store creation left a store the next run used as is.
- **Tradeoffs**: The LTP store was remade once after the change, and a survey store created before the marker existed is trusted as complete.
- **Watch out**: None known.
- **Source**: 5d23297, dcacfb6

### D27: Both sides start every case from the same files
- **What**: difftest exports the store's bin, etc, home, lib, media, mnt, opt, run, sbin, srv, tmp, usr and var (with `/tmp` and `/var/tmp` empty) into the native image, labelled `appine.export` with that list and exported again when it changes, and both sides remove `/root` and `/tmp` before each case and make them again (modes 700 and 1777); the shared gtest runner still deletes gVisor's `/tmp/exec-only-*` leftovers and restores `/tmp` to 1777 within its case.
- **Why**: The native image held only six top-level directories while the reused Appine lane store kept `/root` (Go's and uv's caches) and `/tmp` between groups and runs, so cases made `/var/run` and cleaned up by hand, and leftovers (gVisor's fs tests leaving `/tmp` 0777, pid-named files from a failed ExecTest.ExecuteOnlySpecialFiles) made later cases fail like regressions.
- **Tradeoffs**: Go and uv rebuild their caches every run, the first run after a change to the list exports the image again, and the intermittent ExecuteOnlySpecialFiles failure is only recorded as a known flaky test.
- **Watch out**: A case that relied on files an earlier case or run left in `/root` or `/tmp` now finds them gone.
- **Source**: 5d23297, f6a82cc, 84ff4a7, 6d9a9e8, f5c09f7

### D28: appine-run runs only on Linux hosts
- **What**: Native macOS is not a host for `appine-run` (hal D1), so the map's open question and its macOS console-size bug (`TIOCGWINSZ`) are gone; a Mac remains a development machine through `scripts/dev` and difftest's APFS lanes.
- **Why**: The harness map's open question whether native macOS is a supported host for `appine-run` had its answer: it is not.
- **Tradeoffs**: `appine-run` cannot be tried on a Mac outside the container.
- **Watch out**: None for CI, which never built macOS.
- **Source**: 7c5e636

### D29: One difftest runs at a time, and a case whose suite is not built reports SKIP
- **What**: The top-level run makes `target/difftest/.lock` a symbolic link naming its pid before it touches the store and a second run waits, checking the holder with `ps -p` and taking a dead lock (or an earlier version's lock directory) over by moving it aside and putting back a newer lock it moved by mistake (lanes run under their parent's lock); a case calling `run.sh <suite> "$SKIP" <programs>` is reported SKIP, counted but not failed, when `target/<suite>-tests` has no tarball, and fails when the tarball lacks a named program that SKIPPED.txt does not list as not building.
- **Why**: Runs share `target/difftest` (per-case outputs, lane claims, lane clones that `-r` deletes), so a second run started during one made both report nonsense, and a case without its suite printed the same "not installed" line on both sides and passed, so verifying a fix by removing its skips proved nothing locally; the first lock, a directory made before its pid file, could not tell a killed run from one starting, let two waiters take a dead lock together and took another user's live run (`kill -0` failing with `EPERM`) for dead.
- **Tradeoffs**: Runs are serialized, so another tier or an agent waits instead of sharing the VM; three waiters can still run together when one moves aside a lock another has just made and a third makes its own before it is put back; SKIP keeps exit status 0, so a missing suite is visible only in the output.
- **Watch out**: A pid reused by an unrelated process after a run was killed keeps the lock held until that process ends (`rm target/difftest/.lock` frees it), the host needs `ps -p`, and a case written in another shape than `run.sh <suite> "$SKIP"` escapes the absent-program check.
- **Source**: 3c6bf5e, 300d384

### D30: Container engines are surveyed on their native drivers with storage of their own
- **What**: Surveys of their own (podman-rootless, podman-overlay, docker-overlay, podman-rootless-overlay, docker-rootless-overlay) give each engine its own storage root, run root, data root and socket, the overlay ones on a tmpfs natively because docker's overlay root cannot be an upper layer, and check probes, changes over image layers, diff, commit, build, volumes (`:O` included), whiteouts, `user.overlay.opaque` and start time; rootless docker runs dockerd inside RootlessKit's host network with containers on `--network none`.
- **Why**: Whether the engines really use Appine's overlay, including rootless podman's own `userxattr` mounts and rootless containerd's layer extraction, needs a run of each with its native driver compared with native Linux, without disturbing the vfs surveys' images.
- **Tradeoffs**: The surveys need the network for image pulls and are compared by reading their output, and neither rootless docker's networking nor its usual start through dockerd-rootless (which needs a TUN device Appine lacks) is tested.
- **Watch out**: `docker build` fails on both sides under rootless docker ("driver not connecting"), so builds there stay unchecked, and engine, RootlessKit or dockerd-rootless updates can change probe output or break the manual start.
- **Source**: 5d7b936, 450fb00, 869de68, 8089088

### D31: Overlay gtests run on a tmpfs TEST_TMPDIR, picked by a positive filter
- **What**: The starnix-overlay case mounts a tmpfs as `TEST_TMPDIR` and runs Starnix's overlayfs_test and fs_test's overlay tests in one `run.sh` call limited by the runner's `ONLY` gtest filter, starnix-fs skips fs_test's OverlayFs tests, gvisor-fs keeps only the sysfs overlay test skipped, and an overlay-upper case covers the upper layer.
- **Why**: These tests could not run natively because docker's root is an overlay, which Linux refuses as an upper layer, and gVisor's overlay mount tests were skipped while Appine had no overlayfs.
- **Tradeoffs**: A case can still call `run.sh` only once, since difftest reads the suite name from that call.
- **Watch out**: Three overlayfs_test tests fail on both sides in docker and so compare equal without checking anything.
- **Source**: 4b4af75

### D32: What native Linux cannot show is checked by guest programs in cargo tests
- **What**: Appine's own reports and internals are checked by `tests/guest` programs that `rootfs_programs` and `guest_programs` run rather than by differential cases: exception reports against the lines Linux writes with Appine's synthesized ESRs (`crashlog.c`, kernel/signal D23), unimplemented paths logged once (`bugs.c` and a test reading `appine-run`'s stderr, kernel/signal D24), a 2.5 s BKL hold through test hook 12 (`stall.c`, kernel/process D38), a host spawn whose thread start fails through test hook 13 followed by a spawn on the same console (`bugs.c`, kernel/process D53), readv on `appine-run`'s host stdio answered as input arrives, within 20 s per answer (`stdinreadv.c`, kernel/vfs D22), clones sharing data until written (`reflink.c`, kernel/store D17) and a shootdown race forced every round (`cowspin.c` under a 60 s limit, kernel/mm D39), the last four at both tiers.
- **Why**: The native side cannot set a global sysctl from a container, differs in ESRs, CPU numbers and pids, answers `FICLONE` with `EOPNOTSUPP` on ext4 and has no BKL watchdog or host stdio, a hang seen only under full-LTP load needs a reproduction that fails every time, and thread creation cannot otherwise be made to fail, nor spawn be called on a console outside a Machine test.
- **Tradeoffs**: These programs encode Appine's own formats and are not compared with native, the watchdog and spawn-failure tests run only in debug builds (the watchdog ones adding about 3 s each), `stdinreadv.c` checks only the read side (a writev's atomicity against other writers is not observable deterministically), `reflink.c` checks contents and errors rather than the sharing itself, and python.sh now prints the same line for a correct clone and for `EOPNOTSUPP`.
- **Watch out**: A slow runner skips crashlog's ratelimit item with only an `ok ratelimited (skipped: too slow)` line and could approach cowspin's 60 s limit or fail `stdinreadv.c` by taking 20 s for one answer, crashlog's items depend on engine alignment checks that could change, and a thread start between arming hook 13 and the host's spawn would take the failure instead.
- **Source**: 7b7f73b, 99b4216, 64a6107, df6ee70, f51f688, a36860b, 143bb86

### D33: Skipped suite tests are re-run on both sides and unskipped when they match
- **What**: Every skipped Starnix and gVisor test except the slow, hanging and incomparable ones was run on both sides, those matching natively were unskipped (narrowing wildcards where only some instances still differ), and a test that passes alone but fails within its whole case stays skipped.
- **Why**: Skips added with the suite cases (70c1bc6, ce8a9ce) outlived the fixes for their causes, so about 140 passing tests guarded nothing and several skip comments named causes that no longer existed.
- **Tradeoffs**: The suite cases run about 140 more tests, and tests that skip themselves on both sides now run only to report that.
- **Watch out**: The unskipped tests were compared only against colima's Linux 6.8, so CI's newer native kernel may answer some of them differently.
- **Source**: f713148

### D34: FUSE is compared with native through real daemons and one written in the case
- **What**: difftest installs fuse3, fuse-overlayfs, sshfs, openssh-sftp-server and rclone into its store; `fuse` drives fuse-overlayfs (I/O, namespace and attribute changes, xattrs, programs run from the mount, mappings read on demand, errors and unmount) and a Python daemon of its own that counts lookups not forgotten after `drop_caches`, and `fuse-tools` mounts sshfs against a local sftp-server as root and through fusermount3, and rclone (kernel/fuse D1).
- **Why**: FUSE behaviour has to match a real kernel with the daemons people use, and forgets need counts no packaged daemon reports.
- **Tradeoffs**: The store gains five packages, the native side needs `--device /dev/fuse`, the case writes `drop_caches` on the whole docker VM, and interrupts, oversized requests, notifications and a refused INIT are covered only by unit tests or not at all.
- **Watch out**: Alpine's sshfs and rclone versions change what the cases see, and the native side's forgets must arrive within the 1 s the case sleeps after `drop_caches`.
- **Source**: c008749, 3957a03

## Superseded

- ~~Line-discipline scenarios skipped by group until fixed (24ecd25)~~ → replaced by **D18** in 8dc166b (2026-11-07)
- ~~Each Starnix test captured with `$(timeout 60 …)` (1459292, kept in 612d238)~~ → replaced by **D19** in c11c1db (2026-11-07)
- ~~Starnix cases with their own runner loops, only mmap_test and the waitpid programs, and no CI build (1459292)~~ → replaced by **D19** in 70c1bc6 (2026-11-07)
- ~~A Starnix-only runner at `tests/diff/starnix/run.sh` (70c1bc6)~~ → replaced by **D19** in ce8a9ce (2026-11-07)
- ~~Cases clean up what they leave in the reused lane store's `/tmp` themselves (5d23297, f6a82cc, 84ff4a7)~~ → replaced by **D27** in f5c09f7 (2026-11-09)
- ~~difftest's lock as a directory holding a pid file, checked with `kill -0` (3c6bf5e)~~ → replaced by **D29** in 300d384 (2026-11-10)
- ~~`stale.c` reproduces the stale deadline through an unmapped timeout and prints only the later 3 s wait (9593bae)~~ → replaced by **D13** in 27609fe (2026-11-10, no SUPERSEDES declared)
