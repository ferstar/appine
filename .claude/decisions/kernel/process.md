# Processes and threads (kernel/process) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/process"`
>
> Last distilled: 2026-11-08 (HEAD = fffe1c1). D1–D13 were seeded by hand from the retired design
> doc §6 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D1–D10, D12, D13, D15–D18, D20–D22, D24–D26: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/process.md`

- **D1** — Each guest thread is a host pthread scheduled by the host and pooled with its engine CPU state, so every guest thread costs a host thread and an eventfd, and a pooled host thread keeps the previous guest thread's nice, affinity and I/O priority until they are re-applied (rounds 04, 28)
- **D2** — All kernel-layer state sits behind one big kernel lock that guest code runs without and only waits and `Cx::unlocked` release mid-syscall, so kernel objects use non-atomic `Rc`/`RefCell` and nothing may hold them across a release (rounds 04, 29, 36, 38, 109, 189)
- **D3** — Up to half the host CPUs (1 to 4) spin about 30 µs on the BKL before sleeping, reading only its "held" flag and never touching the mutex until the flag reads free (round 188)
- **D4** — Every guest wait is one host `poll()` on the thread's own eventfd plus the host fds it waits on, never a blocking host syscall, and since events are level-triggered waiters must re-check their condition in a loop (design review §6, round 04)
- **D5** — Wakers request channels or objects and only matching waiters are signalled, after the BKL is released, so every file kind must register each object that affects its readiness or its waiters hang (rounds 16, 28, 49)
- **D6** — stat, clock reads, futex wait/wake and nanosleep have a lock-free fast path that falls back to the BKL path, whose per-thread caches a global epoch invalidates whenever root, cwd, credentials, mounts or namespaces change, and which stays "in guest" while touching guest memory so shootdowns wait for it (rounds 25, 38)
- **D7** — execve resolves and checks under the BKL but maps segments and builds the stack unlocked, touching only the new address space and self-locked structures (file maps) and re-checking after re-locking whether it was killed (round 28)
- **D8** — `threads-max` (by memory, at most half the host fd limit) binds root too and `RLIMIT_NPROC` defaults to half of it, so a fork bomb cannot exhaust the app's threads and fds, while host-launched processes bypass both by design (round 20)
- **D9** — PID 1 is a built-in init with no guest program that adopts orphans, runs a subset of `/etc/inittab` (no `respawn` or `askfirst`) at boot and shutdown on its own host thread so it never blocks the tabs, and takes root's SIGTERM/SIGUSR1/SIGUSR2 as reboot/halt/poweroff (design review §6, rounds 186, 187)
- **D10** — nice, affinity and I/O priority are per thread and applied to host threads, with raised priority stopping at host nice -4, real-time I/O becoming best-effort and real-time policies only recorded, and they must be re-applied whenever a pooled host thread starts a guest thread (rounds 19, 140)
- **D12** — Kernel pids and credential IDs stay global and pid and user namespaces translate them at every boundary (syscall arguments and results, procfs, siginfo, socket credentials), where a missed translation leaks a global ID into a namespace (rounds 116–123)
- **D13** — Time namespaces follow Linux 6.1 plus the 6.2 relaxations, applying monotonic and boottime offsets to clock reads and absolute waits with threads and vfork children entering at execve, and the fast path's cached offsets must be invalidated on namespace entry (round 162)
- **D15** — Every reader of guest times (`read_timespec`, the fast path, `timer_settime`, `read_itimerval`) goes through `timespec()` in `syscall/mod.rs`, which validates and saturates at KTIME_MAX, and a new reader that bypasses it brings back the `Instant + Duration` overflow panic (96cd8bd, 63b9e59)
- **D16** — A namespace's pid 1 becomes the new parent only while the namespace is not dead, so a dying namespace's orphans fall to the built-in init, which reaps them at once, though a subreaper inside the namespace still takes them first (65c44f6)
- **D17** — execve copies the fd table only above two references (another process sharing it through `CLONE_FILES`), as `unshare_files` does, relying on exiting threads dropping their references under the BKL before `kill_other_threads` returns (68cafce)
- **D18** — Process files read in the order their code runs (kernel.rs from global state to waking, syscall/proc.rs from clone to sessions, `impl Machine` by a machine's life, syscall/mod.rs dispatch first and helpers last), which code appended at a file's end breaks, and bug.rs, debug.rs and syscall/mod.rs are filed here though the registry does not list them (0758e3c)
- **D20** — A thread leaving a live process hands every shared pending signal to `signal::retarget` once it has left its group, as `exit_signals` does (kernel/signal D16) (22e2b41)
- **D21** — `ThreadGroup::leader_exit` keeps an exited leader's name and CPU time from its exit until reaping, cleared when another thread's execve takes over the tgid, so procfs can show it as a zombie (kernel/procfs D17) (79a60cf)
- **D22** — `RLIMIT_MSGQUEUE` defaults to (819200, 819200) as on Linux, though mq_open does not enforce it (97b1d62)
- **D24** — With `CLONE_PARENT` the child takes the creating thread group's exit signal, while ptrace still picks the fork or clone event from the requested signal, as on Linux (14dede6)
- **D25** — A thread-targeted POSIX timer signal is sent only while its task still exists in the timer's process and is otherwise dropped, though a thread of the same process reusing that number still receives it (d98f60b)
- **D26** — Marking a robust lock owner-died translates its address with copy-on-write broken (or the page faulted in) before the compare-and-swap loop, since a plain translate does not break copy-on-write (kernel/mm's rule for writers through translate) (e4d3386)

## Active (full detail)

### D11: A dedicated timer thread delivers timers and runs periodic checks
- **What**: One host thread in `Machine` sleeps until the earliest deadline and handles interval and POSIX timers, CPU limits, the memory limit, load average, DNS stub answers, eventfd-signalling AIO polls (waiting on their host fds and timerfd deadlines, and off after a kernel-layer panic; kernel/vfs D42) and other periodic work.
- **Why**: Timer expiry, limits and AIO polls that signal an eventfd (which must complete while no thread is in `io_getevents`) cannot rely on guest threads, and the memory limit can only be checked periodically because the host kernel handles guest page faults directly; when every thread waits, memory cannot grow, so the periodic checks stop.
- **Tradeoffs**: Its work runs under the BKL and competes with guest syscalls; a finite `RLIMIT_CPU` keeps a 10 ms tick; while such AIO polls are pending it listens on every wake channel, so every kernel change wakes it.
- **Watch out**: Anything that changes a deadline or hands it work (the DNS threads, new AIO polls) must signal its event, and programs keeping many eventfd-signalling polls pending load it.
- **Source**: rounds 19 (`19-deferred`), 20 (`20-robustness`); fe4bb2b, fffe1c1

### D14: Processes, threads, scheduling, timers and namespaces are one kernel/process module
- **What**: kernel/process covers tasks, execve and the ELF loader, the BKL and fast path, waits and futexes, timers, PID 1 and namespaces, leaving mount trees to kernel/vfs, network namespaces to kernel/netns and credentials to kernel/cred.
- **Why**: The retired design doc's §6 was the only current-state description of processes, threads and scheduling and had drifted from the source (wake channels, fast-path coverage, timer-thread duties, boot plan, pid-namespace orphans).
- **Tradeoffs**: The map is large because the module spans many files, and rounds keep § references to the deleted section.
- **Watch out**: Bugs confirmed at the split or found by Starnix's and gVisor's tests stay open in the map, among them a non-atomic `unshare`, missing PI futexes and the ELF loader's handling of unusual program headers (a `PT_INTERP` with a NUL, `PT_LOAD`s past EOF or above 2^48, `ELIBBAD`).
- **Source**: dd6c288 (bug list since updated by 1459292, 4e64e74 and ce8a9ce, and shortened by D29, D30 and D32)

### D19: Wall-clock deadlines stay on their own clocks
- **What**: `itimer::Expiry` keeps a deadline as a monotonic instant or as a time on its host clock, mapping clocks as Linux's `__hrtimer_init` does (absolute realtime and TAI on the realtime clock, boottime on the boottime clock, relative realtime on monotonic); waits re-derive their instant on every pass, and the timer thread watches the host for clock jumps (hal D11) and wakes every task when one happens, for timerfds (kernel/vfs D23) and message queues (kernel/ipc D3) too.
- **Why**: Absolute realtime and boottime deadlines were turned into monotonic instants when set, so they fired late after the device slept or the wall clock was stepped.
- **Tradeoffs**: A clock jump wakes every thread once, and alarm clocks still cannot wake a sleeping device.
- **Watch out**: An instant cached across a wait comes back late, and without the host watch (non-Linux hosts, a seccomp policy refusing timerfds) jumps go unnoticed until something else wakes the waiter.
- **Source**: 63b9e59

### D23: sched_setattr accepts the keep and clamp flags and short structures
- **What**: The keep flags are resolved before validation in `sys_sched_setattr`'s order, utilization clamps are recorded on the task and inherited like Linux's `uclamp_req`, and a structure shorter than the current version is read into a zeroed full-size buffer.
- **Why**: Linux 6.1 accepts these flags (with uclamp built in, as on Android) and refusing them broke tools that only meant to set a hint; the first fix then read past the 48-byte first version and killed the caller.
- **Tradeoffs**: The clamps change nothing on the host, and clamp flags with the short structure are still `EINVAL`.
- **Watch out**: Other syscalls taking versioned structures may read past a short copy the same way (LTP's clone3, openat2 and mount_setattr tests would show it).
- **Source**: bc44b65, e43c40c

### D27: sched_setattr follows `__sched_setscheduler`'s checks
- **What**: `set_policy`, shared with sched_setscheduler, follows Linux's `__sched_setscheduler`: the reset-on-fork bit inside the policy is refused unless the policy is kept, clamps are checked against the current ones, `KEEP_PARAMS` validates without applying, nice is set only for `SCHED_OTHER` and `SCHED_BATCH`, and a nice beyond `RLIMIT_NICE` (also when leaving `SCHED_IDLE`) or clearing reset-on-fork without `CAP_SYS_NICE` is `EPERM`; `KCMP_IO` compares the tasks' I/O priority contexts.
- **Why**: Four of Starnix's sched_setattr tests still failed after D23, on the reset bit, clamps against current values, `KEEP_PARAMS` and `EACCES` beyond `RLIMIT_NICE`, and `KCMP_IO` always compared equal.
- **Tradeoffs**: The policy is still applied before the nice check, so a refused nice can leave the policy changed.
- **Watch out**: Unprivileged programs that cleared reset-on-fork or left `SCHED_IDLE` now get `EPERM`, as on Linux.
- **Source**: 70c1bc6, dcacfb6

### D28: Namespace creation is counted per user up the user-namespace chain
- **What**: `Kernel::ucounts` charges a namespace created by euid u in user namespace n to (n, u) and then to each (parent, owner) pair up to the initial namespace, and clone, unshare and open_tree's anonymous mount namespace return `ENOSPC` after the level or `CAP_SYS_ADMIN` check, before creating anything, when a level is at its `max_*_namespaces` limit (half of threads-max in the initial namespace, `INT_MAX` elsewhere), which the sysctls read and write per user namespace (kernel/procfs D22).
- **Why**: The `max_*_namespaces` limits were only recorded, so bubblewrap's `--disable-userns` (a limit of 1 inside the sandbox) and `max_user_namespaces=0` had no effect.
- **Tradeoffs**: Dead namespaces leave the counts only at the next check, not when released as on Linux, and the initial default follows the current threads-max where Linux fixes it at boot.
- **Watch out**: A stray strong reference keeps a namespace counted, and a new namespace kind must be checked and counted where it is created.
- **Source**: 6d05aa9

### D29: A panic in the first stage of thread exit still releases vfork and the address space
- **What**: When the first stage of `task::exit` fails its `bug::catch`, a second `bug::catch` completes `vfork_done` (signalling the parent) and calls `remove_user`, reporting its own failure, before the existing kill.
- **Why**: The first stage (statistics, RSS, `release_mm`, ptrace and seccomp) is one `bug::catch`, so a panic before `release_mm` finished left a vfork parent blocked and a stale `Kicker` in the mm.
- **Tradeoffs**: The rest of the stage (robust list, `clear_child_tid` wake, ptrace and seccomp cleanup) is still skipped after a panic, though the process is killed anyway.
- **Watch out**: If the panic left the task state or the mm borrowed, the salvage fails too and is only reported.
- **Source**: 6d1d9c0

### D30: wait, #! nesting and tid allocation follow Linux, and the CLI can stop the guest
- **What**: `find_event` applies one eligibility rule (`Wanted::clones` filters non-traced children by exit signal as `eligible_child` does, and zombies count only for waits taking exits); #! nests five deep; `alloc_tid` wraps to 300 below `PID_MAX` and `set_last_pid` clamps; `Machine::signal` and `RunOptions::stop` let `appine-run` forward its first stop signal to the program and kill everything on one at least a second later (hal D14); the periodic checks stop through `Shared::stop_checks`, which sets the idle flag and re-checks the sleeping flags with SeqCst on both sides (kernel/mm D29).
- **Why**: wait ignored `__WCLONE`, `__WALL` and exit signals, waitid for stops only blocked on zombies, #! allowed one level fewer than Linux, the tid allocator could hand out `PID_MAX` and wrapped to 2, `appine-run` had no way to ask the guest to stop, and the periodic checks could stop while a thread was running.
- **Tradeoffs**: A guest that ignores the forwarded signal needs a second one at least a second later, duplicates within that second are dropped, and every sleep and wake does SeqCst stores on its flag.
- **Watch out**: Code that reaped any child without `__WALL` now gets `ECHILD`, as on Linux.
- **Source**: 5d23297

### D31: The loader reads a host file's identity fields directly
- **What**: `loader.rs` checks a host file's version through `st_dev` and `st_ino` without a cast, as kernel/mm does for file mappings (kernel/mm D30), now that only Linux and Android hosts are built (hal D1).
- **Why**: The cast existed only because `st_dev` is `i32` on macOS.
- **Tradeoffs**: A 32-bit target or another libc would need the cast back.
- **Watch out**: Such a target would only fail to compile.
- **Source**: 7c5e636

### D32: Private futexes are keyed by address space, and id maps are checked against their opener
- **What**: Private memory and every `FUTEX_PRIVATE_FLAG` operation key futexes by (mm, address) as `get_futex_key` does, shared anonymous memory by host memory and shared file mappings by file, with kernel wakes (robust lists, `clear_child_tid`) as shared operations; `uid_map`/`gid_map` writes check the opener's namespace, `CAP_SYS_ADMIN` and `CAP_SETFCAP` for the parent's uid 0 (kernel/cred D10), and need `CAP_SETUID`/`CAP_SETGID` in both opener and writer for more than their own id, with `setgroups` checked at open and `timens_offsets` against the opener.
- **Why**: Private futexes were keyed by host memory, so waiters on a page replaced by `MAP_FIXED` were never woken, and id-map writes trusted the writer, a confused deputy through a setuid program.
- **Tradeoffs**: Private and shared operations on one private address no longer meet, as on Linux, so a program mixing them stays waiting here too.
- **Watch out**: The no-BKL path and `Mm::futex_key` must agree on which addresses are private, and anything creating a writable shared mapping must call `note_shared_maps` (kernel/mm D31).
- **Source**: f6a82cc

### D33: `FUTEX_WAKE_OP` and Linux's futex argument rules; `PRIO_USER` and shm removal follow namespaces and exits
- **What**: `FUTEX_WAKE_OP` runs its atomic op on a writable word (`futex::writable_word`, shared with robust lists, D26) with the EQ to GE comparisons, wake counts of 0 or less wake one, `FUTEX_CLOCK_REALTIME` is accepted only with absolute waits and unknown command bits are `ENOSYS` after the timeout is read (both left by the fast path to the BKL path); the fork count behind `/proc/stat`'s `processes` counts every task; `PRIO_USER` translates through the caller's user namespace and skips invisible processes; `reap_shm` runs after a thread leaves the task table and after execve (kernel/ipc D7).
- **Why**: WAKE_OP and several futex argument rules were missing, `/proc/stat`'s process count skipped threads, `PRIO_USER` used kernel uids, and segments marked `SHM_DEST` outlived their last attachment at exit and execve.
- **Tradeoffs**: PI futexes stay `ENOSYS` (recorded in the map).
- **Watch out**: `reap_shm` runs at every thread exit, cheap only while no segment is marked for removal.
- **Source**: 5d7b936

### D34: setpriority and sched_setaffinity honour `CAP_SYS_NICE` over the target's user namespace
- **What**: setpriority and sched_setaffinity accept the same owner or `CAP_SYS_NICE` over the target's user namespace, as Linux's `set_one_prio` and `sched_setaffinity` do, while sched_setscheduler and sched_setattr keep requiring the same owner or `CAP_SYS_NICE` in the initial namespace and raising priority still goes through `can_nice`.
- **Why**: Rootless runtimes act as root of their user namespace, and crun's sched_setaffinity on the container process failed with `EPERM`, so `podman run --userns keep-id` failed.
- **Tradeoffs**: Two owner checks coexist in `syscall/sched.rs` and must stay matched to the right syscalls.
- **Watch out**: Holders of the capability over an ancestor namespace are accepted through `ns_capable`, so a mistake in namespace ancestry would widen who can renice others.
- **Source**: 2aca260

## Superseded

- ~~Deadlines stay plain `Instant`s (D15 in part, 96cd8bd)~~ → replaced by **D19** in 63b9e59 (2026-11-07)
