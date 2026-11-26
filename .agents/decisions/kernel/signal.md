# Signals, ptrace and ABI (kernel/signal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/signal"`
>
> Last distilled: 2026-11-26 (HEAD = e8ac8ebf). D1–D11 were seeded by hand from the retired design
> doc §7 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D2, D3, D5–D11, D13–D15, D17, D19–D22, D24–D26: `git show 1ea0c1ef9d686d7126bec2029fc928d4f6fdfc6b:.agents/decisions/kernel/signal.md`

- **D2** — Without `SA_RESTORER`, signal handlers return through the vDSO's `__kernel_rt_sigreturn` trampoline with fixed bytes `mov x8, #139; svc #0`, which must never change or libgcc unwinding breaks (round 10; risk A1)
- **D3** — Asynchronous signals interrupt guest code via the engine's pending kick flag rather than host signals, so every translated loop must pass through a kick check (design review 2026-10-24, round 04)
- **D5** — ptrace follows Linux 6.1 with tracers as processes (any tracer thread may issue requests, no Yama restrictions), and register edits must go through the saved copy taken at each stop (rounds 40, 41, 45)
- **D6** — Hardware breakpoints (6 slots) force the interpreter while watchpoints (4 slots) stay in Tier 1 and route only watched pages to the interpreter, so watched pages must never enter the TLB (rounds 44, 50)
- **D7** — Core dumps follow Linux 6.1's `do_coredump` (`core_pattern`, `core_uses_pid`, `coredump_filter`, no pipe patterns or `suid_dumpable`), waiting for all other threads to exit and faulting in ELF magic and `PT_LOAD` pages first (round 43, 865d24c1)
- **D8** — The kernel layer maintains its own 128 KB / 4096-record printk ring buffer read via `/dev/kmsg`, `syslog(2)` and `/proc/kmsg` with Linux's `dmesg_restrict`/`CAP_SYSLOG` rules, fed by guest writes and kernel-layer events including D23 and kernel/process D38 (rounds 140, 143; 7b7f73b, 64a6107)
- **D9** — `uname` reports fixed `6.1.0-appine` as the ABI contract, decoupled from the host kernel, to be upgraded (likely to 6.12) when major software needs newer features, exceptions near ten, or major distros leave 6.1 (design review 2026-10-24, round 10; contract review 2026-11-02)
- **D10** — Unimplemented syscalls return `ENOSYS` (never crashing) and count as stubs (D24), with `io_uring` staying `ENOSYS` and the missing-syscall list kept documented (rounds 19, 139; 99b4216, 39ea702)
- **D11** — Setting host time or modifying `adjtimex` returns `EPERM`, while read-only `adjtimex` queries report host NTP state (synthesized as unsynchronized on Android where app seccomp forbids `adjtimex`) (round 140)
- **D13** — `pidfd_send_signal` through a `/proc/<tid>` directory fd resolves `group_of` and signals its thread group, matching Linux up to 6.8 (173da76)
- **D14** — Signal files read in the order their code runs (`signal.rs`, `ptrace.rs`, `klog.rs`, `syscall/misc.rs` with `Default` impls before inherent impls and helpers after callers), so new code goes where it runs rather than at file ends (bf240bc)
- **D15** — `kill` accepts a thread's tid to signal its thread group when no group has that id (keeping leader-exited groups reachable), matching `kill_pid_info` (f85963d)
- **D17** — Ptrace detach/attach and tracer exit preserve `syscallno` and recorded hardware debug slots (and pending changes) across trace-state reset, so a dead tracer's watchpoints still trap into `SIGTRAP` and any new kept fields must be carried explicitly (b877707, 6d9a9e8)
- **D19** — `rt_sigqueueinfo` reaches zombie thread groups (all existing groups except pid 1), and `%d` in `core_pattern` prints the dumpable value directly (5d23297)
- **D20** — `Trace::cred` stores the tracer's credentials at attach/seize/auto-attach or the caller's at `PTRACE_TRACEME`, and `tracer_capable` checks `CAP_SYS_PTRACE` over the tracee's credential user namespace (used for non-dumpable `PEEK*`/`POKE*` and execve privilege suppression per kernel/cred D4) (f6a82cc, 1be1142)
- **D21** — `rt_sigaction` masks unknown flags with `UAPI_SA_FLAGS`, and `InfoFields::User` retains `si_errno`, translated `si_pid`/`si_uid`, and bytes 24–48 of user-supplied siginfo for signalfd and delivery (dcacfb6)
- **D22** — signalfds open `O_RDWR` (write still `EINVAL`), poll as `POLLIN` alone (kernel/poll D15), and install via the caller's bounded fd installer (e6bfdf8, 576501d3)
- **D24** — `stub.rs` counts up to 128 unimplemented paths per kernel (reporting the top ten in `KillReason::Bug` and logging first hits under `APPINE_LOG`), so default arms must filter ordinary Linux refusals to avoid noise (99b4216, c008749)
- **D25** — `Pending::add` retains the first send's siginfo for an already-pending standard signal (as Linux's `legacy_queue` drops later sends), so paths that update siginfo in place (such as POSIX timer overruns) must use `infos_mut` (f5c09f7)
- **D26** — Over per-thread credentials (kernel/cred D1), `may_kill` checks the target thread's credentials for thread-directed signals and the main thread's for process signals, ptrace reports carry the tracee's or replacing tracer's real uid, and core file `%u`/`%g` use the dumping thread's (1be1142)

## Active (full detail)

### D1: Signal frames copy the Linux arm64 layout byte for byte
- **What**: siginfo, ucontext, sigcontext and the `__reserved` records (fpsimd always, esr for faults) are laid out exactly as arm64 Linux writes them. Restore FPCR through the engine write mask and clear exclusive reservations at frame setup, sigreturn, group or ptrace stop and freezer transitions.
- **Why**: Unwinders, debuggers and runtimes (Go's asynchronous preemption, V8's wasm traps) read and modify the frame directly.
- **Tradeoffs**: No SVE/SME records, so the corresponding extensions can never be advertised without extending the frame.
- **Watch out**: `AT_MINSIGSTKSZ` (4720) and the record sizes are pinned by a test.
- **Source**: rounds 03 (`03-m1b`), 04 (`04-m3a`), 9163f493, 9951951a, d1fc9715

### D4: vDSO clock reads run the host's own vDSO code, with `svc` stubs as the fallback
- **What**: The guest's vDSO is the host's `[vdso]` (hal D18) with every symbol but the clock functions and the sigreturn trampoline made undefined and its Linux note set to 6.1.0, used once the interpreter has run its clock functions against host readings; otherwise, and in time namespaces with offsets, the built image's `svc` stubs take the no-BKL fast path.
- **Why**: Every vDSO clock read entered the kernel layer, about 70 ns on top of the host's 19 ns, and Go, the JVM and node read clocks constantly.
- **Tradeoffs**: Guests see the host kernel's vDSO code, build id and vvar size, a host vDSO the engine cannot run falls back to the stubs silently, gettimeofday reports the host's timezone (the device's on Android, not 0), and time namespaces with offsets keep the stub speed.
- **Watch out**: Time-namespace detection reads the clock mode as the second word of the host's vvar data (Linux 5.6 on), an instruction a future host vDSO needs and the engine lacks shows only as the fallback, and the stubs must still not stop under ptrace or seccomp.
- **Source**: 16364c0 (replacing the stubs of design review 2026-10-24 §7.2 and rounds 25, 38)

### D12: Signal delivery and the Linux ABI remain separate from tracing and vDSO construction
- **What**: kernel/signal owns signals, frames, restart, faults, core dumps, kernel logging and Linux ABI policy; tracing implementation belongs to kernel/ptrace and vDSO image construction to kernel/vdso, with integration call sites retaining their existing owners.
- **Why**: Separating independently maintained subsystems makes the oversized parent map practical to read and maintain.
- **Tradeoffs**: Cross-boundary work follows child maps, while historical decisions keep their original parent tags and stable IDs.
- **Watch out**: Bugs confirmed at the split or found by Starnix's and gVisor's tests stay open in the map, among them an early `CLD_STOPPED`, late single-step reports, unforced ptrace reads, no `PTRACE_EVENT_EXIT` for threads ended by another thread, a repeated stop report after detaching at an attach-time group stop, `SIGILL` frames carrying an `esr_context` with EC 0 where Linux 6.1 emits none, and synthesized abort ESRs that differ from Linux's; these pre-split qualifications remain discoverable through the child maps and are not resolved by the ownership split.
- **Source**: 9e5be2b, d2b1dc3 (bug list since updated by 4e64e74, ce8a9ce, 5d7b936 and 7b7f73b, and shortened by D17–D21 and D25); 8a17e4a9; d053be24; 60790c14; 39e55b4b

### D16: Process signals are retargeted to a thread that will take them
- **What**: `retarget` wakes, per signal, the first thread that does not block it; delivery calls it for shared pending signals the thread blocks, thread exit for all of them (kernel/process D20), and group sends skip threads in a ptrace stop. Skip ptrace-stopped recipients when retargeting and notify the parent of a group stop only after all participating threads stop or exit.
- **Why**: Linux retargets shared pending signals when a thread blocks them or exits and never picks a stopped or traced thread, while Appine left them with the chosen thread, so Ctrl-C or SIGTERM sometimes did nothing.
- **Tradeoffs**: While such a signal stays pending, every delivery on the blocking thread kicks the other thread again, and `CLD_STOPPED` still comes early.
- **Watch out**: A thread that blocks a signal another thread is slow to take wakes that thread repeatedly until it does.
- **Source**: 22e2b41, 849bf98a, 13dd5ae4, 3a90a0ef

### D18: A signal frame that cannot be used forces SIGSEGV through normal delivery
- **What**: When `setup_frame` fails, delivery puts back a swapped sigsuspend/ppoll mask, leaves the mask alone and forces `SIGSEGV` with `SI_KERNEL` (`force_fatal` when the signal was `SIGSEGV`) before continuing the delivery loop, with restart decided once by the first signal taken; rt_sigreturn with a bad frame forces `SIGSEGV` at SP (`SEGV_ACCERR` when `Mm::mapped_above`, else `SEGV_MAPERR`; kernel/mm D28) and returns before the syscall-exit stop and delivery. Commit a new alternate signal stack only after copying the old stack description to the caller.
- **Why**: Frame-setup failures and invalid sigreturn frames called group exit directly, skipping the handlers, core dumps and ptrace stops that Linux's forced `SIGSEGV` goes through.
- **Tradeoffs**: The frame is validated before anything changes, so after a bad sigreturn the registers are the caller's, where Linux may have restored some before failing.
- **Watch out**: A program looping on signals over a broken stack now runs one `SIGSEGV` handler per failure instead of dying at the first.
- **Source**: 41bd6ec, 849bf98a

### D23: Unhandled exceptions are reported in the kernel log as arm64's `arm64_show_signal` does
- **What**: While `debug.exception-trace` is non-zero (0 by default as on arm64, kernel/procfs D16, and 1 from boot in the app, kernel/process D35), `show_unhandled` logs each synchronous exception that ends an untraced process whose handler is default or ignored (page faults, `SIGBUS` past EOF, alignment, undefined instructions, `brk`, hardware breakpoints and bad rt_sigreturn frames) with Linux 6.1's names, the faulting mapping and the registers, at most 10 per 5 s kernel-wide. An undefined instruction clears the saved ESR so its SIGILL frame omits an unrelated syndrome. Build arm64 abort ESR values from current page-table validity and the level of the missing translation.
- **Why**: Guest programs killed by a fault left nothing in the kernel log beyond the shell's "Segmentation fault", and the Starnix survey listed arm64's per-fault report as a small gain.
- **Tradeoffs**: A report takes about 16 of the log's 4096 records, synthesized ESRs name other levels and fault kinds than native, there is no Hardware name line, a bad sigreturn frame shows the caller's registers (D18), and seccomp's `SIGSYS`, unwritable frames and single-step traps go unreported, as on Linux.
- **Watch out**: A program crashing in a loop keeps adding 10 reports per 5 s, pushing older records out of the log that bug reports carry, and a fault path added later without a `show_unhandled` call goes unreported.
- **Source**: 7b7f73b, 13dd5ae4, 2971a1a9

### D27: Delivery after a syscall forgets it, as Linux's `forget_syscall`
- **What**: `deliver`, after the restart rewind of a syscall's return, calls `KernelEntry::forget_syscall` (kernel/process D55) with the possibly rewound pc, so a thread stopped by `SIGSTOP` in a syscall shows -1 with its sp and the restart pc in `/proc/<pid>/syscall` (kernel/procfs D40). Clear abandoned restart-block state when interruption becomes EINTR.
- **Why**: Linux's do_signal forgets the syscall before taking a signal, while Appine showed the syscall the thread had been in.
- **Tradeoffs**: Every BKL-path syscall pays the record's stores once more, and the difference shows only while the thread waits inside `deliver` (stops, and ptrace stops after this point).
- **Watch out**: A ptrace stop placed later in `deliver` shows -1 where Linux might still show the syscall.
- **Source**: f1212d8, 849bf98a

### D28: Freezer entry and group kills mark cgroup observations

- **What**: Use the same freezer-entry transition as vfork and mark the group when setting killed flags.
- **Why**: Entering a freezer stop or killing tasks can complete a subtree freeze.
- **Tradeoffs**: Group exit can mark observations whose contents stay unchanged.
- **Watch out**: Stop, thaw and kill paths must agree on which tasks contribute to freeze completion.
- **Source**: 0f5c5132

### D29: Signal queue limits and copy retries preserve siginfo semantics

- **What**: Retain the selected signal across copyout retries and distinguish timer, ordinary kernel/user and other real-time sources when the pending limit is full.
- **Why**: A failed queued delivery must not manufacture a pending record with incorrect metadata.
- **Tradeoffs**: Some source classes retain only a pending bit and others fail.
- **Watch out**: Timer delivery retains its siginfo and callers that can fall back need a failure result.
- **Source**: f985c1f8, 6fcaff37

### D30: `Pending` unifies standard and real-time siginfos in one insertion-ordered `VecDeque`

- **What**: `Pending::queue: VecDeque<(Signal, SigInfo)>` stores both standard and real-time queued siginfos in exact insertion order alongside an `rt_queued: usize` counter for `RLIMIT_SIGPENDING`/`SigQ` accounting and `Pending::peek(&self, index)` for `PTRACE_PEEKSIGINFO`.
- **Why**: The earlier split `[SigInfo; 31]` array plus real-time-only `VecDeque` could not preserve Linux's mixed standard-and-RT insertion order (`pending->list`) for `PTRACE_PEEKSIGINFO`.
- **Tradeoffs**: Dequeuing a standard signal scans `queue` linearly instead of O(1) array indexing, while shrinking inline `Pending` size on every `Task` and `ThreadGroup` from ~4.5 KiB to a few words.
- **Watch out**: Every path mutating `Pending::queue` (`add`, `take`, `discard`, or future helpers) must keep `rt_queued` synchronized for real-time entries.
- **Source**: 5872f977

## Superseded

- ~~vDSO time functions are `svc` stubs served without the BKL, with no vvar page (D4, rounds 25, 38)~~ → replaced by **D4** in 16364c0 (2026-11-08)
- ~~D12 combined ownership of the newly extracted child modules~~ → replaced by **D12** in 60790c14 (2026-11-15; scope only, inherited implementation decisions remain here)
