# Signals, ptrace and ABI (kernel/signal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/signal"`
>
> Last distilled: 2026-11-06 (HEAD = afd086a). D1–D11 were seeded by hand from the retired design
> doc §7 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Signal frames copy the Linux arm64 layout byte for byte
- **What**: siginfo, ucontext, sigcontext and the `__reserved` records (fpsimd always, esr for faults) are laid out exactly as arm64 Linux writes them.
- **Why**: Unwinders, debuggers and runtimes (Go's asynchronous preemption, V8's wasm traps) read and modify the frame directly.
- **Tradeoffs**: No SVE/SME records, so the corresponding extensions can never be advertised without extending the frame.
- **Watch out**: `AT_MINSIGSTKSZ` (4720) and the record sizes are pinned by a test.
- **Source**: rounds 03 (`03-m1b`), 04 (`04-m3a`)

### D2: Handlers return through a vDSO trampoline with fixed bytes
- **What**: Without `SA_RESTORER`, x30 points at the vDSO's `__kernel_rt_sigreturn`, which is exactly `mov x8, #139; svc #0`.
- **Why**: Go on arm64 does not set `SA_RESTORER` and returns through the vDSO, and libgcc's unwinder recognises signal frames by matching these bytes.
- **Tradeoffs**: Every process needs a vDSO mapping.
- **Watch out**: Changing the trampoline bytes breaks unwinding through signal handlers.
- **Source**: round 10 (`10-m8`); risk A1

### D3: Asynchronous signals interrupt guest code with a kick, never with host signals
- **What**: Sending sets the engine's pending flag; translated code checks it at block entries and the interpreter at instruction boundaries, then exits to the kernel, which delivers.
- **Why**: Host signals would conflict with ART/bionic's signal handling, and stopping only at defined points keeps guest state precise.
- **Tradeoffs**: A delivery waits until the thread reaches a check (at most one block).
- **Watch out**: Every loop in translated code must pass through a kick check.
- **Source**: design review 2026-10-24 (§7.3, §7.4), round 04 (`04-m3a`)

### D4: vDSO time functions are `svc` stubs served without the BKL
- **What**: `clock_gettime`, `gettimeofday` and `clock_getres` in the vDSO enter the kernel and take the no-BKL fast path; there is no vvar page.
- **Why**: Clocks must go through the kernel layer (time namespaces, the thread CPU clock), and the fast path makes the entry cheap.
- **Tradeoffs**: Each call costs a kernel entry, unlike Linux's userspace vDSO (a vvar page reading `CNTVCT_EL0` remains a possible future step).
- **Watch out**: The stubs must not stop under ptrace or seccomp, as on Linux where they never enter the kernel.
- **Source**: design review 2026-10-24 (§7.2), rounds 25 (`25-stat-without-bkl`), 38 (`38-split-bkl`)

### D5: ptrace follows Linux 6.1, with tracers as processes
- **What**: strace, gdb, delve, lldb and valgrind work through a Linux-compatible ptrace in which any thread of the tracer process may issue requests; there are no Yama restrictions.
- **Why**: ptrace used to return EPERM, so neither strace nor gdb worked; developers need both on the device.
- **Tradeoffs**: Tracees stay attached until the whole tracer process exits.
- **Watch out**: Register changes must go through the saved copy taken at each stop.
- **Source**: rounds 40 (`40-ptrace-strace`), 41 (`41-ptrace-gdb`), 45 (`45-ptrace-procfs`)

### D6: Hardware breakpoints and watchpoints are emulated, watchpoints without leaving Tier 1
- **What**: 6 breakpoint and 4 watchpoint slots; breakpoints force the interpreter, watchpoints keep translated code and only route accesses to watched pages through the interpreter.
- **Why**: Without slots gdb fell back to single-stepping software watchpoints, and running a watched thread entirely in the interpreter was an order of magnitude slower.
- **Tradeoffs**: Breakpointed threads still run in the interpreter.
- **Watch out**: Watched pages must never enter the TLB.
- **Source**: rounds 44 (`44-hw-watchpoints`), 50 (`50-watchpoints-tier1`)

### D7: Core dumps follow Linux 6.1's `do_coredump`
- **What**: Default core-generating signals write an arm64 ELF core per `core_pattern`, `core_uses_pid` and `coredump_filter`; pipe patterns are not supported.
- **Why**: Programs that crashed could not be analysed afterwards, and `WCOREDUMP` was always 0 (several LTP tests failed).
- **Tradeoffs**: No pipe form (no user-mode helper), no `suid_dumpable`.
- **Watch out**: The dumping thread waits for all other threads to exit first.
- **Source**: round 43 (`43-coredump`)

### D8: The kernel layer keeps its own printk-style log
- **What**: A 128 KB / 4096-record ring buffer, read through `/dev/kmsg`, `syslog(2)` and `/proc/kmsg`, fed by guest writes and kernel-layer events (OOM kills, bug kills).
- **Why**: Processes killed by the OOM killer or internal errors showed only "Killed" in the terminal, with the reason in host logcat; `dmesg` and programs writing `/dev/kmsg` had nothing to use.
- **Tradeoffs**: The host kernel's log is not available (Android's app seccomp forbids `syslog`).
- **Watch out**: Permission rules (`dmesg_restrict`, `CAP_SYSLOG`) must match Linux so unprivileged reads behave the same.
- **Source**: rounds 140 (`140-ioprio-syslog-adjtimex`), 143 (`143-kmsg`)

### D9: `uname` reports a fixed Linux 6.1 as the ABI contract
- **What**: The guest always sees `6.1.0-appine`, and unspecified behaviour follows Linux 6.1; a few newer features are deliberate exceptions.
- **Why**: A fixed LTS version decouples guest behaviour from the phone's kernel; 6.1 is new enough for guest software and used by Debian 12.
- **Tradeoffs**: Native comparison hosts run newer kernels, so tests compare only behaviour common to versions.
- **Watch out**: Upgrade the contract (likely to 6.12) when important software needs newer features, when exceptions approach about ten, or when major distributions leave 6.1.
- **Source**: design review 2026-10-24 (§7.5), round 10 (`10-m8`); contract review 2026-11-02

### D10: Unimplemented syscalls return ENOSYS and never crash
- **What**: The dispatcher's default arm returns `ENOSYS` and optionally logs locally (`APPINE_LOG=1`); io_uring stays `ENOSYS`.
- **Why**: Guest programs probe syscalls and fall back on `ENOSYS`; a crash would take down the session. io_uring was tried (about 3800 lines for the interface and 49 opcodes) and abandoned.
- **Tradeoffs**: Programs that require a missing syscall fail with an error instead of working.
- **Watch out**: Keep the list of `ENOSYS` numbers documented (round 139).
- **Source**: rounds 19 (`19-deferred`), 139 (`139-unimplemented-syscalls`)

### D11: The guest cannot change host time
- **What**: Setting the clock or modifying `adjtimex` is `EPERM`, as if `CAP_SYS_TIME` were missing; `adjtimex` queries report the host's NTP state.
- **Why**: Guest time is the host's, and tools (busybox `adjtimex`, `ntptime`, node_exporter) only need read-only queries; Android's app seccomp forbids calling `adjtimex`/`clock_adjtime` (the whole app would get `SIGSYS`), so the state there must be synthesized.
- **Tradeoffs**: NTP daemons in the guest only observe.
- **Watch out**: On Android the synthesized state is unsynchronized, which is the real state there (Android sets time with `settimeofday` and runs no kernel NTP adjustment).
- **Source**: round 140 (`140-ioprio-syslog-adjtimex`)

### D12: Signals, ptrace, core dumps, the kernel log and the ABI contract are one kernel/signal module
- **What**: kernel/signal covers signal generation and delivery, frames and sigreturn, the vDSO, ptrace, core dumps, the kernel log and the Linux 6.1 ABI contract (uname, the ENOSYS policy, clock setting), leaving host-side signal handling to engine and auxv to kernel/process.
- **Why**: The retired design doc's §7 was the only current-state description of signals, ptrace, core dumps and the ABI contract, and it said nothing about generation and delivery semantics.
- **Tradeoffs**: The project-wide ABI contract lives in a module map rather than a design section, and rounds keep § references to the deleted section.
- **Watch out**: Bugs confirmed at the split stay open in the map, among them silent group kills on unwritable or bad frames, stalled process-directed signals, `kill(tid)` returning `ESRCH`, coalesced siginfo, hardware slots left armed after detach, late single-step reports and unforced ptrace reads.
- **Source**: 9e5be2b

### D13: pidfd_send_signal through a `/proc/<tid>` directory signals the thread group
- **What**: A `/proc/<tid>` directory fd finds its process with `group_of` and the signal goes to its tgid, as `kill_pid_info` does for such an fd up to Linux 6.8.
- **Why**: pidfd_send_signal through a non-leader thread's `/proc` directory found no process.
- **Tradeoffs**: The differential test cannot cover it, since kernels from 6.9 infer the scope from the fd and treat this case differently.
- **Watch out**: None known.
- **Source**: 173da76

### D14: Signal files read in the order their code runs
- **What**: signal.rs puts senders and `deliver` before their helpers (the queue limits under generation, the alternate stack under frames); ptrace.rs follows stops from signal delivery to traced exits; klog.rs runs storing, readers, formatting, lookups and positions; the other files, syscall/misc.rs included, put helpers after their callers.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; signal.rs and klog.rs put most helpers above their callers.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; Default impls stay before the inherent impls, as constructors.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: bf240bc
