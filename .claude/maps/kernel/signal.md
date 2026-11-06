# kernel/signal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/signal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 4 research concerns; 16 claims checked by 2 independent verifiers — 13 confirmed and 3 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: stale source comments fixed; bugs from Starnix's syscall tests)

## Responsibilities

`kernel/signal` covers how guest code is interrupted, notified, debugged and dumped, and the
user-visible ABI contract:

- signal state, generation and delivery (handlers, masks, queues, job control);
- the arm64 Linux signal frame, `rt_sigreturn`, syscall restart, and mapping synchronous engine
  exits to signals;
- the vDSO image;
- ptrace (tracer and tracee sides) and `process_vm_readv/writev`;
- core dumps;
- the kernel log (`/dev/kmsg`, `syslog(2)`, `/proc/kmsg`);
- the kernel-version contract, the `ENOSYS` policy for unimplemented syscalls, and refusing guest
  clock changes.

Root: `core/kernel/src/signal.rs`, `syscall/sig.rs`, `vdso.rs`, `ptrace.rs`, `syscall/ptrace.rs`,
`coredump.rs`, `klog.rs`, signalfd (in `fd.rs`/`syscall/sig.rs`), and in `syscall/misc.rs`:
`uname`, `syslog`, `adjtimex`, clock setting, `restart_syscall`; the dispatcher's restart and
unimplemented-syscall handling in `syscall/mod.rs`; the fault-to-signal branch of the run loop in
`task.rs`.

Outside the module: waking and namespace translation (`kernel/process`), CPU state, kicks and the
host fault guard (`engine`), page-fault classification (`kernel/mm`), seccomp (with credentials),
procfs.

## Key types

- `SigAction` — handler, flags, restorer, mask, and `immutable` (`SA_IMMUTABLE`, used by seccomp
  kills); `SigHandlers` — the 64-entry table, shared under `CLONE_SIGHAND`.
- `SigInfo` — code, typed fields (kill, child, queue, timer, fault, poll, sys, raw), the synthesized
  ESR for faults, sender and user-namespace data for translation; encodes the 128-byte siginfo.
- `Pending` — a set plus one siginfo per standard signal (coalesced) and a FIFO queue for real-time
  signals; per thread and per thread group.
- `AltStack` — sp, size, flags; thread-private.
- Thread state: `pending`, `sigmask`, `saved_sigmask`, `killed`; group state: `pending`,
  `sighand`, `stopped`, `stop_signal`, stop/continue reports, `exiting`.
- `SignalFd` — mask and the creating group.
- `vdso::Image` — the vDSO bytes and the sigreturn offset, built once.
- `ptrace::Trace` — per-thread tracing state: tracer tgid, seized, options, run mode
  (`Cont`/`Syscall`/`Step`), current `Stop` (`Signal`, `Group`, `Syscall(entry)`, `Event`), the saved
  `Regs` copy, siginfo, event message, syscall number, `HwDebug` slots.
- `TraceExit`, `leader_tracer`, `exit_tracer` — tracee exits held back for the tracer.
- `coredump::ThreadCore`, `CoreWriter` — per-thread state collected for a core file, and its writer.
- `klog::Klog`, `Record`, `KmsgFile` — the ring buffer, its records, one `/dev/kmsg` open.

## Public entry points

- **Signal syscalls**: `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`,
  `pidfd_send_signal`, `rt_sigaction`, `rt_sigprocmask`, `rt_sigpending`, `rt_sigsuspend`,
  `rt_sigtimedwait`, `sigaltstack`, `signalfd4` (and reads), `rt_sigreturn`, `restart_syscall`.
- **Kernel-internal senders**: `send_to_task`, `send_to_group`, `send_to_pgrp`, `send_cpu_timer`,
  `force` (synchronous faults), `force_fatal` (seccomp), `group_exit`; `wake`, `interrupted`, `take`.
- **Delivery**: `signal::deliver(cx, cpu, syscall)` after each syscall, kick, fault handling,
  sigreturn and exec.
- **ptrace**: `ptrace(2)` requests, `process_vm_readv`/`writev`; tracee hooks called from the
  dispatcher, delivery, clone/exec/exit, seccomp and wait.
- **Core dumps**: `coredump::dump` from the default-action path.
- **Kernel log**: `klog::printk`/`emit` (used by OOM, bug kills, cgroups, networking), `/dev/kmsg`,
  `/proc/kmsg`, `syslog(2)`.
- **ABI**: `uname`, `/proc/version`, `/proc/sys/kernel/{osrelease,version,ostype}`, the
  dispatcher's default arm, `settimeofday`, `clock_settime`, `adjtimex`, `clock_adjtime`.

## Data flow / lifecycle

### Sending a signal

- The siginfo is prepared for the target (`si_uid` translated, `si_pid` hidden if the target's pid
  namespace cannot see the sender); default-handled signals aimed at a pid-namespace init are
  dropped.
- `SIGKILL` is never queued: it goes straight to group exit (every thread marked killed and woken).
- Otherwise: a stop signal discards pending `SIGCONT`; `SIGCONT` discards pending stop signals,
  ends a group stop and reports `CLD_CONTINUED`. An ignored signal that is not blocked and not
  traced is discarded at send time.
- Standard signals coalesce (one pending instance; a later send replaces the stored siginfo);
  real-time signals queue in order, limited by `RLIMIT_SIGPENDING` (default 16384), counting only
  queued real-time entries per thread group. When the queue is full, `kill` and kernel-generated
  signals still set the pending bit but lose the siginfo; `tkill`/`tgkill` of a real-time signal,
  and the queue syscalls with a non-`SI_USER` siginfo, return `EAGAIN`.
- Waking: a thread-directed signal wakes its thread if unblocked; a process-directed one wakes the
  first thread (leader first) that has it unblocked (CPU-timer signals rotate among running
  threads); if every thread blocks it, only a `SIGNAL` wake is requested. Waking kicks the CPU and
  signals the thread's event (`kernel/process`).
- Permissions: `may_signal` (same user or `CAP_KILL`); `kill` handles `-1` (all except init and
  self), `0` and negative process groups; pid 1 is handled by `kernel/process` (power requests).

### Delivery

- `deliver` runs before every return to guest code. For an interrupted syscall it first rewinds
  (`x0 = orig_x0`, PC back to the `svc`).
- Loop: wait while the group is stopped (and while frozen by a cgroup), handle ptrace group-stop
  and trap checks, then dequeue — the thread's own pending before the group's, synchronous signals
  (SEGV, BUS, ILL, TRAP, FPE, SYS) first, then the lowest number — and offer a ptrace
  signal-delivery stop (not for immutable actions).
- Disposition: ignored (or ignored by default, or an unkillable init) → skip; default stop → group
  stop (dropped for an orphaned process group except `SIGSTOP`); other defaults → group exit, with
  a core dump for core-generating signals; a handler → set up a frame.
- For the first handler frame, an interrupted syscall returns `EINTR` instead of restarting when
  the code is `ERESTARTNOHAND`, `ERESTART_RESTARTBLOCK`, or `ERESTARTSYS` without `SA_RESTART`.
- After a frame: the mask becomes mask | action mask | the signal (unless `SA_NODEFER`), never
  including `SIGKILL`/`SIGSTOP`; `SA_RESETHAND` resets the handler to default. More signals may
  stack further frames.
- If no handler ran: `saved_sigmask` (from sigsuspend or ppoll) is restored, and an
  `ERESTART_RESTARTBLOCK` syscall re-executes as `restart_syscall`, which resumes the saved sleep or
  futex deadline.
- Group stop: marks the group stopped, notifies the parent with `CLD_STOPPED` (`SIGCHLD` unless the
  parent set `SA_NOCLDSTOP`) and requests a `CHILD` wake.

### Signal frames

- The frame copies Linux arm64 byte for byte: `siginfo` (always written), `ucontext` (flags and
  link 0, `uc_stack` describing the altstack before auto-disarm, the mask to restore),
  `sigcontext` (fault address = `si_addr` or 0, x0–x30, sp, pc, pstate with NZCV and DIT), and
  `__reserved` records: `fpsimd_context` (FPSR, FPCR, V0–V31) always; `esr_context` only when the
  siginfo carries a synthesized ESR; zero-filled to a null terminator. No SVE, ZA or extra records.
- Below it a `frame_record {fp, lr}`; everything 16-byte aligned. `AT_MINSIGSTKSZ` reports 4720; the
  `sigaltstack` minimum is 5120.
- The stack is the altstack top if `SA_ONSTACK` is set, the altstack is enabled and SP is not
  already on it; `SS_AUTODISARM` resets the altstack after use.
- Handler entry: x0 = signal number; x1 = siginfo and x2 = ucontext with `SA_SIGINFO`; sp = frame;
  x29 = the frame record; x30 = `sa_restorer` with `SA_RESTORER`, else the vDSO's
  `__kernel_rt_sigreturn`; pc = handler.

### `rt_sigreturn`

- Validates before changing anything: SP 16-byte aligned, frame readable, pstate mode bits zero,
  records with valid sizes, exactly one `fpsimd_context` of 528 bytes; ESR records ignored; any
  other magic invalid.
- Restores x0–x30, SP, PC, NZCV (and DIT when advertised), FPSR, FPCR (unmasked), V0–V31, the mask
  (minus `SIGKILL`/`SIGSTOP`) and the altstack. The engine looks up translations by the new PC, so a
  handler may change it (Go preemption, V8 wasm traps).
- A frame that cannot be written, or a bad frame at sigreturn, kills the group with `SIGSEGV`.

### Synchronous faults to signals (run loop, after `kernel/mm` decides page faults)

- `brk` → `SIGTRAP`/`TRAP_BRKPT`, `si_addr` = PC, no ESR record.
- Hardware breakpoint/watchpoint → `SIGTRAP`/`TRAP_HWBKPT`, address = PC or access start, no ESR.
- Alignment → `SIGBUS`/`BUS_ADRALN` with ESR EC 0x24 (data, FSC 0x21, WnR on writes), 0x22 (PC,
  no FSC) or 0x26 (SP, `si_addr` = SP).
- Undefined or non-whitelisted encodings → `SIGILL`/`ILL_ILLOPC`.
- `force` unblocks or resets a blocked or ignored synchronous signal so it is delivered.
- The faulting instruction has no visible effect (no register or base writeback), and the PC in the
  frame points at it (precision comes from the engine's checks).

### Signal syscalls

- `rt_sigaction`: installing an action for `SIGKILL`/`SIGSTOP` is `EINVAL` (querying them works);
  flags are stored and reported as given; any change that makes a signal ignored (`SIG_IGN`, or
  `SIG_DFL` for a default-ignored signal) discards its pending instances.
- `rt_sigsuspend` uses `saved_sigmask` and returns `ERESTARTNOHAND`; `rt_sigtimedwait` returns only
  signals from its set (`EINTR` for others); `rt_sigpending` reports pending ∩ blocked.
- `sigaltstack`: `SS_DISABLE`, `SS_ONSTACK` reporting, `SS_AUTODISARM`.
- `tkill`/`tgkill` use `SI_TKILL`. `kill(pid > 0)` matches thread-group ids only, while
  `rt_sigqueueinfo` accepts any thread's tid for its process.
- signalfd: reads dequeue matching signals of the calling thread and its group, in the Linux
  `signalfd_siginfo` layout; readiness also follows the calling thread.

### exec and clone

- exec: shared handlers are unshared, handlers reset to default (ignored ones stay ignored), the
  altstack and `saved_sigmask` cleared; pending signals and the mask survive; the thread re-kicks
  itself if something is deliverable.
- clone: the child inherits the mask with nothing pending; `CLONE_SIGHAND` shares handlers,
  `CLONE_CLEAR_SIGHAND` resets them; a `CLONE_VM` child without vfork gets no altstack.

### vDSO

- One page, built at runtime as a minimal real ELF shared object (`ET_DYN`, a loadable segment and a
  dynamic segment, `DT_HASH`, symbol versions with `LINUX_2.6.39`, soname `linux-vdso.so.1`) and
  passed through `AT_SYSINFO_EHDR`. Each process maps a private anonymous copy (debuggers can set
  breakpoints in it).
- `__kernel_rt_sigreturn` is exactly `mov x8, #139; svc #0` (libgcc's unwinder matches the bytes),
  preceded by a `nop`.
- `__kernel_gettimeofday`, `clock_gettime` and `clock_getres` are `mov x8, #nr; svc #0; ret` stubs;
  an `svc` at those addresses takes the no-BKL fast path and skips ptrace and seccomp stops.

### ptrace

- Tracees are threads; the tracer is a process (any of its threads may issue requests; tracees are
  released only when the whole tracer process exits). No Yama restrictions. Tracing pid 1 is
  `ESRCH`; `PTRACE_TRACEME` under init is `EPERM`. Access needs the same user (and dumpable, with a
  capability subset) or `CAP_SYS_PTRACE`.
- Attaching: `ATTACH` sends `SIGSTOP`; `SEIZE` takes options and never stops. Requests other than
  `KILL` and `INTERRUPT` need the tracee in a ptrace stop (else `ESRCH`); unknown requests are `EIO`.
- Every ptrace stop saves a copy of the registers; the tracer reads and writes the copy and it is
  written back on resume. Stopping notifies the tracer (`CLD_STOPPED` or `CLD_TRAPPED`, `SIGCHLD`
  unless suppressed) and waits until resumed, detached or killed.
- Signal-delivery stops: resuming with 0 suppresses the signal, another number replaces it (as
  `SI_USER` unless `SETSIGINFO` was used); blocked replacements are re-queued.
- Group stops: non-seized tracees report `Group` (no siginfo); seized ones report `EVENT_STOP`;
  `LISTEN` and `INTERRUPT` need `SEIZE`.
- Syscall stops (`PTRACE_SYSCALL`): x7 is 0 at entry and 1 at exit (restored afterwards); the
  tracer may change the number through `NT_ARM_SYSTEM_CALL` (-1 skips the call); seccomp runs after
  the entry stop; the no-BKL fast path is off while tracing syscalls or stepping.
- Event stops: fork, vfork, clone, exec (message: former tid), exit (message: wait status),
  vfork-done, seccomp. `CLONE_PTRACE` forces and `CLONE_UNTRACED` prevents tracing of children.
  A non-seized tracee exec without `TRACEEXEC` gets `SIGTRAP`.
- Single-step: the interpreter executes one instruction (`Cpu::step`) and the tracee reports
  `SIGTRAP`/`TRAP_TRACE`; a stepped `svc` reports after the syscall.
- Register sets: `NT_PRSTATUS`, `NT_PRFPREG`, `NT_ARM_TLS`, `NT_ARM_SYSTEM_CALL`,
  `NT_ARM_HW_BREAK` (6 slots), `NT_ARM_HW_WATCH` (4 slots); nothing for SVE, PAC, MTE, SME. Debug
  slots are handed to the engine (`set_hw_debug`): breakpoints force Tier 0, watchpoints keep
  Tier 1 with watched pages out of the TLB.
- Memory: `POKETEXT`/`POKEDATA` and `/proc/<pid>/mem` writes use forced writes (private copies of
  non-writable mappings, translation invalidation; written mappings stop being trusted translation
  sources). `process_vm_writev` uses plain writes.
- wait: the tracer finds its tracees without `WUNTRACED`; a stop reports `code << 8 | 0x7f`. A
  traced leader's zombie is hidden from its parent until the tracer reaps it. When the tracer
  process exits, tracees are killed with `EXITKILL`, otherwise detached; detaching a tracee in a
  stopped group returns it to the group stop.

### Core dumps

- Written when a core-generating signal (`SIGQUIT`, `SIGILL`, `SIGTRAP`, `SIGABRT`, `SIGBUS`,
  `SIGFPE`, `SIGSEGV`, `SIGXCPU`, `SIGXFSZ`, `SIGSYS`) takes its default action as the group's first
  exit, `RLIMIT_CORE` is at least a page and the process is dumpable. A pipe `core_pattern` is not
  supported (skipped with a debug log).
- The dumping thread waits until the other threads have exited, each saving its registers.
- The name expands `core_pattern` (`%%`, `%p`, `%P`, `%i`, `%I`, `%u`, `%g`, `%s`, `%t`, `%h`, `%e`,
  `%E`, `%c`, `%d`); `core_uses_pid` appends `.pid` when the pattern has no pid. The file is
  created relative to the cwd (any old file unlinked first) with `O_CREAT|O_EXCL|O_NOFOLLOW`, mode
  0600. `core_pattern` itself (at most 127 bytes) is a root-owned 0644 file guarded only by the
  normal open-time permission check. `dumpable` is a flag (`PR_SET_DUMPABLE` 0 or 1; no
  `suid_dumpable`).
- Format: arm64 Linux ELF core — `PT_NOTE` with per-thread `NT_PRSTATUS`, `NT_PRFPREG`,
  `NT_ARM_TLS`, `NT_ARM_HW_BREAK`, `NT_ARM_HW_WATCH`, `NT_ARM_SYSTEM_CALL`, plus `NT_PRPSINFO`,
  `NT_SIGINFO`, `NT_AUXV`, `NT_FILE`; `PT_LOAD` per `coredump_filter` (`kernel/mm`) with untouched
  pages left as holes; writing stops at `RLIMIT_CORE`.
- A successful dump sets the wait status's core flag and `CLD_DUMPED`.

### Kernel log

- A printk-style ring buffer: 128 KB of text, at most 4096 records, oldest dropped first; records
  carry sequence, monotonic timestamp, facility, level and text (truncated to 991 bytes).
- Writers: guest `/dev/kmsg` writes (prefix parsing, ratelimit 10 per 5 s by default), the OOM
  killer, bug kills, cgroup fork rejections, low-port remapping, permission warnings. The host
  kernel's log is not included.
- Readers: `/dev/kmsg` (a sequence per open, one record per read, `EPIPE` after records were
  dropped, seeks to start, data or end), `syslog(2)` and `/proc/kmsg` (a shared read position;
  READ_ALL/READ_CLEAR return the newest records since the clear point).
- Permissions follow `dmesg_restrict` and `CAP_SYSLOG` (`CAP_SYS_ADMIN` works with a warning);
  `printk_devkmsg=off` makes opening `/dev/kmsg` `EPERM`; console log levels are only recorded.

### ABI contract

- `uname -r` reports `6.1.0-appine` (version `#1 SMP PREEMPT appine`, machine `aarch64`, nodename
  from the UTS namespace); `/proc/version` and `/proc/sys/kernel/{osrelease,version,ostype}`
  match. Unspecified behaviour follows Linux 6.1. Deliberate exceptions taken from newer kernels:
  `cachestat` (6.5), `/proc/<pid>/fd` entries not usable as mount points (6.12), the time-namespace
  relaxations (6.2); code comments also follow newer kernels for `RWF_NOAPPEND` (6.9) and the
  generic-netlink `nlctrl` replies (6.8).
- Unimplemented syscalls (including io_uring) return `ENOSYS` from the dispatcher's default arm and
  never crash; with `APPINE_LOG=1` a local debug line is written to the host's stderr. The list of
  `ENOSYS` numbers is kept in round 139 (`docs/rounds/139-unimplemented-syscalls.md`).
- The guest cannot change host time: `settimeofday` is `EPERM` without reading its arguments;
  `clock_settime` validates the timespec, then is `EPERM` for `CLOCK_REALTIME` and `EINVAL` for every
  other clock; any modifying `adjtimex`/`clock_adjtime` is `EPERM` (as without `CAP_SYS_TIME`);
  queries read the host's NTP state on Linux hosts and report unsynchronized (`STA_UNSYNC`,
  `TIME_ERROR`) on Android and other hosts.

## Dependencies (inbound / outbound)

### Inbound

- The run loop and dispatcher (`kernel/process`): delivery after every syscall, kick and fault;
  syscall-entry/exit ptrace stops; `orig_x0`; restart handling.
- Timers, the TTY layer (job-control signals to process groups), process exit (child status),
  seccomp (`force_fatal`, `SECCOMP` event stops), procfs (signal state, `/proc/<pid>/mem`), the OOM
  killer and bug handling (kernel log).

### Outbound

- `kernel/process`: `request_wake*`, `signal_later`, group exit, namespace translation, wait.
- `kernel/mm`: `read`/`write`/`write_force`, `grow_stack`, `dump_regions`/`dump_page`/`residency`.
- `engine`: CPU state (registers, FP, DIT), `set_hw_debug`, `Cpu::step`, the `Kicker`.
- vfs (core files, `/dev/kmsg`), credentials (`may_signal`, `may_ptrace`, `CAP_SYSLOG`),
  `appine_hal` (clocks, `adjtimex` query).

## Invariants and gotchas

- Signals are delivered only at the kernel boundary; nothing interrupts guest code except a kick
  that makes the engine exit.
- `SIGKILL` and `SIGSTOP` can never be blocked, caught or ignored; `SA_IMMUTABLE` actions skip
  ptrace signal-delivery stops.
- The sigreturn trampoline's bytes are part of the ABI and must not change.
- The tracer works on the saved register copy; changes made directly to the CPU state during a
  stop would be overwritten on resume.
- Only threads calling `exit`/`exit_group` or taking the fatal signal report
  `PTRACE_EVENT_EXIT`.

- `ERESTARTNOINTR` is recognised by delivery but nothing produces it.
- `LOG_CONT` is never set (no `KERN_CONT` merging); `/dev/kmsg` writes always carry a facility, so
  this is not visible to users.

## Confirmed bugs / technical debt

- **Unwritable frames and bad sigreturn frames kill the group silently:** both call group exit with
  `SIGSEGV` directly, so no `SIGSEGV` handler (on an altstack) gets a chance, no core file is
  written and tracers see no stop or exit event (Linux forces a deliverable `SIGSEGV`). A stack
  overflow inside a handler is the common way to hit this.
- **Process-directed signals can stall:** if the thread chosen to take a process-directed signal
  blocks it (or exits) before dequeuing it, no other eligible thread is woken (Linux retargets
  shared pending signals). `CLD_STOPPED` is also reported as soon as the first thread starts a group
  stop, while siblings may still be running.
- **`kill(tid)` is `ESRCH`:** `kill` matches only thread-group ids, so `kill` and `kill -0` on a
  non-leader thread's id fail where Linux signals its process.
- **Coalesced standard signals report the last sender:** a second send overwrites the pending
  siginfo (Linux keeps the first), e.g. `SIGCHLD` from several children reports the last; a unit test
  pins the current behaviour.
- **Detach leaves hardware debug slots armed but invisible:** detaching resets the recorded
  breakpoints and watchpoints while the CPU keeps them until the next exec, so a later tracer reads
  empty regsets while hits still raise `SIGTRAP`.
- **Single-step over some syscalls reports one instruction late:** after `rt_sigreturn`, execve or a
  skipped syscall (number -1), the step is not reported at syscall return.
- **ptrace reads are not forced:** `PTRACE_PEEK*` and `/proc/<pid>/mem` reads fail on `PROT_NONE`
  pages Linux can read; `/proc/<pid>/mem` opens are checked with real rather than filesystem
  credentials; `PTRACE_PEEKSIGINFO` is unsupported (`EIO`).
- **`SIGILL` frames carry an `esr_context` with EC 0:** Linux 6.1 emits none for undefined
  instructions.
- **`rt_sigaction` echoes unknown flag bits:** Linux 5.11+ clears them, so `SA_UNSUPPORTED` probing
  reads as an older kernel (harmless while tag-bit exposure is unimplemented).
- **Core name `%d` prints 0:** it expands the inverse of the dumpable flag; Linux prints the dumpable
  value (1).
- **`MADV_DONTNEED` wipes the vDSO:** the vDSO is a private anonymous copy, so discarding it (alone
  or inside a larger range) zeroes it and the next vDSO clock call or signal return through
  `__kernel_rt_sigreturn` raises `SIGILL`, where Linux refills the vDSO from the kernel image.
- **Minor divergences:** `core_pattern` access follows file DAC with `CAP_DAC_OVERRIDE` instead of
  Linux's euid-only sysctl rule; `settimeofday` skips argument validation and `clock_settime`
  returns `EINVAL` for CPU clocks where Linux returns `EPERM`; sigreturn silently drops DAIF bits
  Linux would reject; kernel-generated real-time signals lose their siginfo when the queue is full;
  `process_vm_readv`/`writev` with a non-empty local vector but no remote bytes look up the pid and
  check access (`ESRCH`, `EPERM`) where Linux 6.1 returns 0 first.

## Open questions

- Is the per-group (instead of per-user) `RLIMIT_SIGPENDING` accounting an intended
  simplification?
- Should the other threads of an `exit_group` also stop at `PTRACE_EVENT_EXIT`?

## To verify

- The Linux 6.1 behaviours used for comparison above (`legacy_queue`, `retarget_shared_pending`,
  `force_sigsegv`, `do_undefinstr`, `UAPI_SA_FLAGS`, FOLL_FORCE reads, `PTRACE_MODE_FSCREDS`,
  `format_corename`, sysctl `test_perm`, the order of checks in time-setting syscalls) were checked
  from knowledge of the kernel, not against its source.
