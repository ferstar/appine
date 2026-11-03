# Processes and threads (kernel/process) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/process"`
>
> Last distilled: 2026-11-03 (HEAD = f9edf81) — seeded by hand from the retired design doc §6 and
> the round records in `docs/rounds/`; no `MODULE: kernel/process` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: Each guest thread is a host pthread, scheduled by the host
- **What**: A 1:1 thread model; finished host threads and engine CPU states are pooled for the next guest thread.
- **Why**: The host kernel already schedules threads well, and everything must run inside the app's single process; pooling removes per-thread creation cost from shell workloads.
- **Tradeoffs**: Every guest thread costs a host thread and an eventfd, so thread limits must protect the host.
- **Watch out**: A pooled host thread keeps the previous guest thread's nice, affinity and I/O priority until they are re-applied.
- **Source**: rounds 04 (`04-m3a`), 28 (`28-spawn-host-cost`)

### D2: One big kernel lock, split only where profiling shows it matters
- **What**: All kernel-layer state sits behind one mutex; guest code runs without it, and only waits and `Cx::unlocked` release it mid-syscall.
- **Why**: A single lock keeps the Linux-semantics code simple, and for most workloads syscalls are a few percent of the time.
- **Tradeoffs**: Several processes spawning at once serialise on the lock; the next step (locking address spaces per fork family) is not done.
- **Watch out**: Kernel objects use non-atomic `Rc`/`RefCell`, so anything that releases the lock must not hold them.
- **Source**: rounds 04 (`04-m3a`), 29 (`29-bkl-wait-hold`), 36 (`36-parallel-processes`), 38 (`38-split-bkl`), 109 (`109-spawn-mmap-bkl`), 189 (`189-sigmask-dbt-running`)

### D3: Waiters spin briefly before sleeping on the BKL
- **What**: Up to half the host CPUs (1 to 4) may spin for about 30 µs reading only a "held" flag; others sleep.
- **Why**: Critical sections usually last microseconds while sleeping and waking costs tens; allowing up to 4 spinners instead of 1 was 7–14% faster on 8 cores.
- **Tradeoffs**: Spinning burns CPU on contended phones.
- **Watch out**: Spinners must not touch the mutex until the flag reads free.
- **Source**: round 188 (`188-bkl-spinners`)

### D4: All waits are one host `poll()` on a per-thread event
- **What**: A guest thread never blocks in a host syscall; it polls its own eventfd plus the host fds it waits on.
- **Why**: Guest signals, futexes and I/O readiness share one wake path, with no host signals to interrupt blocking calls and no fight with ART/bionic over signal numbers.
- **Tradeoffs**: Every blocking operation needs a poll-compatible shape.
- **Watch out**: Events are level-triggered; waiters must re-check their condition in a loop.
- **Source**: design review 2026-10-24 (§6), round 04 (`04-m3a`)

### D5: Wake-ups are targeted and delivered after the BKL is released
- **What**: Wakers request channels or objects; at release only matching waiters are signalled, after unlocking.
- **Why**: Waking every waiter on every change, and waking them while the lock was still held (they woke, found the lock taken, and slept again), cost a large share of context switches.
- **Tradeoffs**: Every file kind must register all objects that affect its readiness; a few paths still wake everyone.
- **Watch out**: A missing object registration shows up as a hang, not a crash.
- **Source**: rounds 16 (`16-futex-targeted-wake`), 28 (`28-spawn-host-cost`), 49 (`49-targeted-wakeups`)

### D6: A few hot syscalls run without the BKL
- **What**: stat, clock reads, futex wait/wake and nanosleep have a lock-free fast path that falls back to the BKL path when it cannot finish.
- **Why**: Parallel `lstat` (git's default) and runtime thread pools were serialised by the BKL, making `git status` slower than single-threaded.
- **Tradeoffs**: Per-thread caches must be invalidated by a global epoch whenever root, cwd, credentials, mounts or namespaces change.
- **Watch out**: The fast path must stay "in guest" while touching guest memory so shootdowns wait for it.
- **Source**: rounds 25 (`25-stat-without-bkl`), 38 (`38-split-bkl`)

### D7: execve builds the new image without the BKL
- **What**: Path resolution and checks run under the BKL; mapping segments and building the stack run unlocked on the new address space.
- **Why**: execve was the longest BKL holder in pipeline workloads.
- **Tradeoffs**: The thread must re-check after re-locking whether it was killed meanwhile.
- **Watch out**: Unlocked code may only touch the new address space and structures with their own locks (file maps).
- **Source**: round 28 (`28-spawn-host-cost`)

### D8: Thread limits apply to root and protect the host
- **What**: `threads-max` (by memory, at most half the host fd limit) binds root too, and `RLIMIT_NPROC` defaults to half of it.
- **Why**: Each guest thread is a host thread with an eventfd, so a fork bomb would otherwise exhaust the app's threads and fds and lose every session.
- **Tradeoffs**: Unusually thread-heavy workloads may hit the limit earlier than on Linux.
- **Watch out**: Host-launched processes bypass the limits by design.
- **Source**: round 20 (`20-robustness`)

### D9: PID 1 is a built-in virtual init that can run inittab
- **What**: Init is a thread group with no guest program; it adopts orphans, and on request boots and shuts down from a subset of `/etc/inittab`, with root's SIGTERM/SIGUSR1/SIGUSR2 as reboot/halt/poweroff requests.
- **Why**: Login shells need a parent the guest cannot kill, and OpenRC needs someone to run it at boot and shutdown without a terminal.
- **Tradeoffs**: No `respawn` or `askfirst`; boot output goes to a log file.
- **Watch out**: Boot runs on its own host thread and must not block the terminal tabs.
- **Source**: design review 2026-10-24 (§6), rounds 186 (`186-inittab`), 187 (`187-app-init`)

### D10: Scheduling parameters go to host threads, with a floor
- **What**: nice, affinity and I/O priority are per thread and applied to host threads; raising priority stops at host nice -4 and real-time I/O becomes best-effort.
- **Why**: Guest priorities should matter on the host, but a guest busy loop must never slow Android's display threads.
- **Tradeoffs**: Real-time scheduling policies are only recorded.
- **Watch out**: Re-apply parameters whenever a pooled host thread starts a guest thread.
- **Source**: rounds 19 (`19-deferred`), 140 (`140-ioprio-syslog-adjtimex`)

### D11: A dedicated timer thread delivers timers and runs periodic checks
- **What**: One host thread in `Machine` sleeps until the earliest deadline and handles interval and POSIX timers, CPU limits, the memory limit, load average and other periodic work.
- **Why**: Timer expiry and limits cannot rely on guest threads, and the memory limit can only be checked periodically because the host kernel handles guest page faults directly; when every thread waits, memory cannot grow, so the periodic checks stop.
- **Tradeoffs**: Its work runs under the BKL and competes with guest syscalls; a finite `RLIMIT_CPU` keeps a 10 ms tick.
- **Watch out**: Anything that changes a deadline must signal its event.
- **Source**: rounds 19 (`19-deferred`), 20 (`20-robustness`)

### D12: Namespaces keep global IDs inside the kernel and translate at the edges
- **What**: Kernel pids and credential IDs are global; pid and user namespaces translate syscall arguments, results, procfs and signal info.
- **Why**: bubblewrap (the sandbox of Codex and Claude Code), podman and docker need user, mount and pid namespaces, and translating at the boundary avoids threading namespaces through every internal table.
- **Tradeoffs**: Every boundary (syscalls, procfs, siginfo, socket credentials) must remember to translate.
- **Watch out**: A missed translation leaks a global ID into a namespace.
- **Source**: rounds 116 (`116-store-binds`) to 123 (`123-bwrap`)

### D13: Time namespaces follow Linux 6.1 plus the 6.2 relaxations
- **What**: Monotonic and boottime offsets apply to clock reads and absolute waits; threads and vfork children enter at execve.
- **Why**: dockerd adds a time namespace when `/proc/<pid>/ns/time` exists and runc then requires `timens_offsets`; since all clocks (including the vDSO stubs) pass through the kernel layer, offsets were cheap, and it was the last namespace type missing.
- **Tradeoffs**: Diverges slightly from the 6.1 contract where 6.2 relaxed it.
- **Watch out**: The fast path caches the offsets and must be invalidated on namespace entry.
- **Source**: round 162 (`162-time-namespaces`)
