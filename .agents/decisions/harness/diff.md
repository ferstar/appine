# Differential and upstream suites (harness/diff) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: harness/diff"`
>
> Last distilled: 2026-11-26 (HEAD = e8ac8ebf)
>
> Decisions on harness/diff made before it was split from harness keep their `harness` tags and live in `.agents/decisions/harness.md`: D1–D3, D5, D9, D11–D14, D17–D20, D23, D27, D29, D31, D33, D34 and D36–D41.

## Active

### D1: Differential suites own runners, cases and recorded deviations

- **What**: harness/diff owns differential and upstream runners, case inputs and recorded test deviations, while implementation policy remains in the kernel maps.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: Cgroup watch tests avoid registering an event-file reader

- **What**: Test inherited freeze with separate file and directory watches, then retain a directory watch across pids enable and re-enable and require IN_MODIFY after each denied fork.
- **Why**: Opening an event file for reading could mask a broken watch lifetime or missing controller registration.
- **Tradeoffs**: The directory cases use O_PATH to pin the child dentry for comparison with native Linux.
- **Watch out**: A never-looked-up dentry can behave differently on Linux 6.8, and a notification delayed beyond two seconds fails the pids case.
- **Source**: 0f5c5132, b65799c8

### D3: The gtest runner dumps `gtest.out` diagnostics on unexpected `exit 0` or `exit 1`

- **What**: `tests/diff/gtest/run.sh` prints `out: file=<bytes>B cap=<len>B` and the first 30 lines of `gtest.out` (mapping NUL bytes to `?`) when a test produces `exit 0` or `exit 1` without a gtest result line.
- **Why**: Discarding `gtest.out` hid whether intermittent `exit 0`/`exit 1` CI failures came from an empty or truncated file versus command-substitution read loss.
- **Tradeoffs**: Assumes legitimate crashes exit via signal codes rather than `0` or `1`, and truncates long outputs to 30 lines.
- **Watch out**: If a future gtest legitimately exits `0` or `1` without a standard gtest status line on both sides, differing diagnostic output could cause a mismatch.
- **Source**: 45495baf

### D4: Difftest store creation retries initial `apk add` up to three times

- **What**: `scripts/difftest` wraps its initial `apk add -q` in a 3-attempt loop with a 5-second pause between retries before failing.
- **Why**: A single transient DNS failure against `dl-cdn.alpinelinux.org` aborted an entire `Full tests` shard before any test ran.
- **Tradeoffs**: Adds up to 10 seconds before failing when offline while keeping the existing partial-store check on final failure.
- **Watch out**: Mirror outages longer than the retry window still fail store creation and leave a partial store that must be removed on local reruns.
- **Source**: 166dd6ee

### D5: Legacy Starnix and gVisor skip filters narrow only after verifying individual gtest outcomes

- **What**: Starnix and gVisor case scripts and `gvisor.filters` restore verified passing tests after native Linux and both Appine tiers agree, enforcing per-group pass counts, listing remaining multicast exclusions individually, running `uio_test` in `starnix-mm`, placing memfd, link-count and `O_TMPFILE` checks on tmpfs, routing bind-mount ID, trusted xattr and cross-namespace abstract-socket checks through the sys-admin group, and retaining the hosted musl auxv exclusion with Appine's own stack-versus-proc check.
- **Why**: Stale or overly broad skip filters masked regressions in working syscall behavior, while some restored checks require tmpfs or elevated capabilities to run on native Linux.
- **Tradeoffs**: Named exclusions add filter entries, newly added multicast tests enter by default, and some programs now run across multiple groups with distinct filters and expected pass counts.
- **Watch out**: Group-level output matching can hide identical failures on both sides, hosted CI uses a different native kernel, and three existing Starnix overlay execution cases still fail on both sides because the tarball lacks their helper binaries.
- **Source**: b5673c4c, fc3d5c51, f6a8240a, aeaffd4b, 74584914, 68b70897, 93a9b204, 37051c60, 865d24c1, ced93039, 231480cb, e8ac8ebf

### D6: Legacy gVisor coverage adds `gvisor-mm` and `gvisor-io` after native and Appine trials

- **What**: Legacy gVisor coverage adds `gvisor-mm` (seven memory and cross-process transfer programs with the verified SharedAnon case restored) and `gvisor-io` (three file I/O programs without exclusions) after native and Appine trials.
- **Why**: These previously omitted gVisor programs exercise partial transfers, memory mappings, and file I/O behaviors beyond the existing differential suites.
- **Tradeoffs**: Regular comparisons build ten additional gVisor programs and add roughly 1.5 s locally for the initial `gvisor-mm` pair.
- **Watch out**: The CI runner executes against a different native Linux kernel than the local trial environment.
- **Source**: 54ce6b50, 48f1c17e, 74584914

### D7: Selected gVisor filter groups enforce expected pass counts and refresh stale lane clones

- **What**: `scripts/build-gvisor-tests` builds from both legacy scripts and `tests/diff/gtest/gvisor.filters`, `tests/diff/gtest/selected.sh` and `run.sh` require each selected row to reach its expected `exit 0` pass count before comparing outputs, and `scripts/difftest` refreshes lane clones when their image ID changes.
- **Why**: Plain output comparison alone could treat identical failures, skips, teardown errors, or empty selections on native Linux and Appine as passing runs.
- **Tradeoffs**: Updating upstream gVisor requires re-checking both the filter rows and their expected pass counts.
- **Watch out**: Native kernel-version differences and socket timing can shift native test outcomes across environments.
- **Source**: 231480cb

### D8: The differential output lock survives parent exit

- **What**: Acquire an output-directory lock in host Perl before the legacy PID lock and inherit its descriptor in the lanes.
- **Why**: Several waiters could race while taking over a stale PID link.
- **Tradeoffs**: A surviving lane can keep the output busy.
- **Watch out**: The lock must precede legacy PID cleanup.
- **Source**: cd5ea85d

### D9: Differential cases compare externally visible syscall contracts

- **What**: Extend existing groups with alias and namespace identity, shared mappings, blocked sends, fd reservations, credentials, pipe slots, wait events, copy-up, TSYNC and sysctl writes.
- **Why**: Return values alone miss ordering and lifetime errors.
- **Tradeoffs**: Some cases require synchronization or tightly scoped output normalization.
- **Watch out**: Capture errors before later calls and restrict normalization to the documented legacy difference.
- **Source**: 45035024, 515ecd7f, b421eac0, 576501d3, 25b0f2a0, 71790fde, bb31dbda

### D10: Native-version differences use portable assertions and dedicated guest checks

- **What**: Retain host-independent TCP filter and namespace checks, move down-iif coverage to the guest suite, compare fixed mremap growth over an unlocked target, and skip `ZombieProcTest.Mem` and `ZombieProcTest.PageMap` in `starnix-process` where native 6.8+/7.x `mm_access` diverges from Linux 6.1 EOF behavior.
- **Why**: Host kernel versions disagree with Linux 6.1 in these edge cases, causing differential comparisons to fail even when Appine matches Linux 6.1.
- **Tradeoffs**: The differential suite omits the host-dependent variants and relies on `tests/guest/mm_mount_pseudo.c` and kernel unit tests for zombie `mem`/`pagemap` regression coverage.
- **Watch out**: Guest coverage must preserve the intended Linux 6.1 behavior, and conservative skips stay excluded until revisited if native kernels or upstream tests change.
- **Source**: f7ddba97, 06fc2277, 298da10e, 14caa5c4

### D11: Differential test harnesses deduplicate repeated check loops with local helpers

- **What**: `peeksiginfo.c` embedded in `tests/diff/ptrace.sh` extracts a `print_peek` helper for its private, offset and shared queue checks without changing any output bytes.
- **Why**: Repeating the same peek-and-print loop three times added boilerplate to the differential test case.
- **Tradeoffs**: Individual queue checks read through a small local helper instead of inline loops.
- **Watch out**: Helper extractions in differential cases must preserve exact output bytes across native Linux and Appine.
- **Source**: 8a839900

## Superseded

- ~~Letting waiters take over the output directory using only a PID link (harness D29)~~ → replaced by **D8** in cd5ea85d (2026-11-24)
- ~~Excluding OverlayLower from the ACL ctime checks~~ → replaced by **D5** in aeaffd4b (2026-11-24)
- ~~Excluding SharedAnon from the gVisor mremap group~~ → replaced by **D6** in 74584914 (2026-11-24)
