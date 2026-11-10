# Processes and threads (kernel/process) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/process"`
>
> Last distilled: 2026-11-10 (HEAD = 300d384). D1–D13 were seeded by hand from the retired design
> doc §6 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D1–D5, D7–D10, D12, D15–D17, D20–D22, D24–D26: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/process.md`

> Full text of D27–D32: `git show 260512d530d136e70f10564e248f7db876f07062:.claude/decisions/kernel/process.md`

- **D1** — Each guest thread is a host pthread scheduled by the host and pooled with its engine CPU state, so every guest thread costs a host thread and an eventfd, and a pooled host thread keeps the previous guest thread's nice, affinity and I/O priority until they are re-applied (rounds 04, 28)
- **D2** — All kernel-layer state sits behind one big kernel lock that guest code runs without and only waits and `Cx::unlocked` release mid-syscall, so kernel objects use non-atomic `Rc`/`RefCell` and nothing may hold them across a release (rounds 04, 29, 36, 38, 109, 189)
- **D3** — Up to half the host CPUs (1 to 4) spin about 30 µs on the BKL before sleeping, reading only its "held" flag and never touching the mutex until the flag reads free (round 188)
- **D4** — Every guest wait is one host `poll()` on the thread's own eventfd plus the host fds it waits on, never a blocking host syscall, and since events are level-triggered waiters must re-check their condition in a loop (design review §6, round 04)
- **D5** — Wakers request channels or objects and only matching waiters are signalled, after the BKL is released, so every file kind must register each object that affects its readiness or its waiters hang (rounds 16, 28, 49)
- **D7** — execve resolves and checks under the BKL but maps segments and builds the stack unlocked, touching only the new address space and self-locked structures (file maps) and re-checking after re-locking whether it was killed (round 28)
- **D8** — `threads-max` (by memory, at most half the host fd limit) binds root too and `RLIMIT_NPROC` defaults to half of it, so a fork bomb cannot exhaust the app's threads and fds, while host-launched processes bypass both by design (round 20)
- **D9** — PID 1 is a built-in init with no guest program that adopts orphans, runs a subset of `/etc/inittab` (no `respawn` or `askfirst`) at boot and shutdown on its own host thread so it never blocks the tabs, and takes root's SIGTERM/SIGUSR1/SIGUSR2 as reboot/halt/poweroff (design review §6, rounds 186, 187)
- **D10** — nice, affinity and I/O priority are per thread and applied to host threads, with raised priority stopping at host nice -4, real-time I/O becoming best-effort and real-time policies only recorded, and they must be re-applied whenever a pooled host thread starts a guest thread (rounds 19, 140)
- **D12** — Kernel pids and credential IDs stay global and pid and user namespaces translate them at every boundary (syscall arguments and results, procfs, siginfo, socket credentials), where a missed translation leaks a global ID into a namespace (rounds 116–123)
- **D15** — Every reader of guest times (`read_timespec`, the fast path, `timer_settime`, `read_itimerval`) goes through `timespec()` in `syscall/mod.rs`, which validates and saturates at KTIME_MAX, and a new reader that bypasses it brings back the `Instant + Duration` overflow panic (96cd8bd, 63b9e59)
- **D16** — A namespace's pid 1 becomes the new parent only while the namespace is not dead, so a dying namespace's orphans fall to the built-in init, which reaps them at once, though a subreaper inside the namespace still takes them first (65c44f6)
- **D17** — execve copies the fd table only above two references (another process sharing it through `CLONE_FILES`), as `unshare_files` does, relying on exiting threads dropping their references under the BKL before `kill_other_threads` returns (68cafce)
- **D20** — A thread leaving a live process hands every shared pending signal to `signal::retarget` once it has left its group, as `exit_signals` does (kernel/signal D16) (22e2b41)
- **D21** — `ThreadGroup::leader_exit` keeps an exited leader's name and CPU time from its exit until reaping, cleared when another thread's execve takes over the tgid, so procfs can show it as a zombie (kernel/procfs D17) (79a60cf)
- **D22** — `RLIMIT_MSGQUEUE` defaults to (819200, 819200) as on Linux, though mq_open does not enforce it (97b1d62)
- **D24** — With `CLONE_PARENT` the child takes the creating thread group's exit signal, while ptrace still picks the fork or clone event from the requested signal, as on Linux (14dede6)
- **D25** — A thread-targeted POSIX timer signal is sent only while its task still exists in the timer's process and is otherwise dropped, though a thread of the same process reusing that number still receives it (d98f60b)
- **D26** — Marking a robust lock owner-died translates its address with copy-on-write broken (or the page faulted in) before the compare-and-swap loop, since a plain translate does not break copy-on-write (kernel/mm's rule for writers through translate) (e4d3386)
- **D27** — `set_policy` follows `__sched_setscheduler`'s checks (reset-on-fork only with a kept policy, clamps against the current ones, `KEEP_PARAMS` validating only, nice only for `SCHED_OTHER` and `SCHED_BATCH`, `EPERM` beyond `RLIMIT_NICE` or for clearing reset-on-fork without `CAP_SYS_NICE`) and `KCMP_IO` compares I/O priority contexts, though the policy is still applied before the nice check (70c1bc6, dcacfb6)
- **D28** — `Kernel::ucounts` charges a new namespace to (namespace, euid) and each (parent, owner) pair up to the initial namespace, and clone, unshare and open_tree return `ENOSPC` at a `max_*_namespaces` limit before creating anything, so a new namespace kind must be checked and counted where it is created, and dead namespaces leave the counts only at the next check (6d05aa9)
- **D29** — When the first stage of `task::exit` panics, a second `bug::catch` still completes `vfork_done` and calls `remove_user` before the kill, while the rest of the stage is skipped and a salvage over borrowed state is only reported (6d1d9c0)
- **D30** — wait applies one eligibility rule (`__WCLONE` and `__WALL` by exit signal, zombies only for waits taking exits), #! nests five deep, tids wrap to 300 below `PID_MAX`, `appine-run` forwards its first stop signal and kills everything on one at least a second later, and the periodic checks stop through `Shared::stop_checks` with SeqCst on both sides (5d23297)
- **D31** — `loader.rs` reads a host file's `st_dev` and `st_ino` without a cast now that only Linux and Android hosts are built, so a 32-bit or other-libc target would only fail to compile (7c5e636)
- **D32** — Private memory and `FUTEX_PRIVATE_FLAG` operations key futexes by (mm, address) and shared memory by host memory or file, and `uid_map`/`gid_map` writes are checked against their opener as well as the writer, so the no-BKL path and `Mm::futex_key` must agree on which addresses are private and every writable shared mapping must call `note_shared_maps` (f6a82cc)

## Active (full detail)

### D6: A few hot syscalls run without the BKL
- **What**: stat, clock reads, futex wait/wake, nanosleep and sched_yield (D49) have a lock-free fast path that falls back to the BKL path when it cannot finish.
- **Why**: Parallel `lstat` (git's default) and runtime thread pools were serialised by the BKL, making `git status` slower than single-threaded, and spinning threads' sched_yield took the BKL only to return 0.
- **Tradeoffs**: Per-thread caches must be invalidated by a global epoch whenever root, cwd, credentials, mounts or namespaces change.
- **Watch out**: The fast path must stay "in guest" while touching guest memory so shootdowns wait for it, and a call that stops counting as running guest code (sched_yield) must not touch guest memory afterwards.
- **Source**: rounds 25 (`25-stat-without-bkl`), 38 (`38-split-bkl`); 78b3284

### D11: A dedicated timer thread delivers timers and runs periodic checks
- **What**: One host thread in `Machine` sleeps until the earliest deadline and handles interval and POSIX timers, CPU limits, the memory limit, load average, DNS stub answers, eventfd-signalling AIO polls (waiting on their host fds and timerfd deadlines, and off after a kernel-layer panic; kernel/vfs D42), the next expiry of timerfds behind `EPOLLEXCLUSIVE` gates (kernel/poll D4) and other periodic work.
- **Why**: Timer expiry, limits and AIO polls that signal an eventfd (which must complete while no thread is in `io_getevents`) cannot rely on guest threads, and the memory limit can only be checked periodically because the host kernel handles guest page faults directly; when every thread waits, memory cannot grow, so the periodic checks stop.
- **Tradeoffs**: Its work runs under the BKL and competes with guest syscalls; a finite `RLIMIT_CPU` keeps a 10 ms tick; while such AIO polls are pending it listens on every wake channel, so every kernel change wakes it.
- **Watch out**: Anything that changes a deadline or hands it work (the DNS threads, new AIO polls, exclusive gates) must signal its event, and programs keeping many eventfd-signalling polls pending load it.
- **Source**: rounds 19 (`19-deferred`), 20 (`20-robustness`); fe4bb2b, fffe1c1, f5c09f7

### D13: Time namespaces follow Linux 6.1 plus the 6.2 relaxations
- **What**: Monotonic and boottime offsets apply to clock reads and absolute waits; threads and vfork children enter at execve; entering one with offsets (at fork, execve or setns) puts svc stubs over the host vDSO's clock entries so clock reads reach the kernel layer, which adds the offsets (D40).
- **Why**: dockerd adds a time namespace when `/proc/<pid>/ns/time` exists and runc then requires `timens_offsets`; since all clocks (including the vDSO stubs) then passed through the kernel layer, offsets were cheap, and it was the last namespace type missing.
- **Tradeoffs**: Diverges slightly from the 6.1 contract where 6.2 relaxed it, and processes in a namespace with offsets read clocks through svc stubs, slower than the host's vDSO.
- **Watch out**: The fast path caches the offsets and must be invalidated on namespace entry, and a new way to enter a time namespace must patch the vDSO too.
- **Source**: round 162 (`162-time-namespaces`); 16364c0

### D14: Processes, threads, scheduling, timers and namespaces are one kernel/process module
- **What**: kernel/process covers tasks, execve and the ELF loader, syscall dispatch, the BKL with its watchdog and fast path, kernel bugs that end one process and their kill reports, waits and futexes, timers, PID 1 and namespaces, leaving mount trees to kernel/mount, network namespaces to kernel/netns and credentials to kernel/cred.
- **Why**: The retired design doc's §6 was the only current-state description of processes, threads and scheduling and had drifted from the source (wake channels, fast-path coverage, timer-thread duties, boot plan, pid-namespace orphans).
- **Tradeoffs**: The map is large because the module spans many files, and rounds keep § references to the deleted section.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them a non-atomic `unshare`, PI futexes without priority inheritance or requeueing, a host spawn that fails to start leaking its process, doomed images started at pc 0 by `Machine::spawn`, a stale fast-path deadline ending a later sleep early and CPU-clock POSIX timers outliving their target.
- **Source**: dd6c288, d2b1dc3 (bug list since updated by 1459292, 4e64e74, ce8a9ce and d9e1f0c, and shortened by D29, D30, D32, D43 and D44)

### D18: Process files read in the order their code runs
- **What**: kernel.rs runs global state, the BKL and the syscall context before waiting and waking; syscall/proc.rs follows the module docs from clone and execve through exit and reaping to pidfds, unshare and sessions; `impl Machine` follows a machine's life; syscall/mod.rs puts dispatch first and the shared argument helpers last; the other files move single helpers after their callers, bug.rs and debug.rs included (the registry now puts debug.rs in kernel/signal).
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; kernel.rs opened with the wake-request helpers and syscall/proc.rs with exit.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `store_changed` sits at the end of machine.rs, away from `RUNNING`, which describes it.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 0758e3c; d2b1dc3 (registry)

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

### D33: `FUTEX_WAKE_OP` and Linux's futex argument rules; `PRIO_USER` and shm removal follow namespaces and exits
- **What**: `FUTEX_WAKE_OP` runs its atomic op on a writable word (`futex::writable_word`, shared with robust lists, D26) with the EQ to GE comparisons, wake counts of 0 or less wake one, `FUTEX_CLOCK_REALTIME` is accepted only with absolute waits and unknown command bits are `ENOSYS` after the timeout is read (both left by the fast path to the BKL path); the fork count behind `/proc/stat`'s `processes` counts every task; `PRIO_USER` translates through the caller's user namespace, matches each thread by its own real uid (D48) and skips invisible processes; `reap_shm` runs after a thread leaves the task table and after execve (kernel/ipc D7).
- **Why**: WAKE_OP and several futex argument rules were missing, `/proc/stat`'s process count skipped threads, `PRIO_USER` used kernel uids, and segments marked `SHM_DEST` outlived their last attachment at exit and execve.
- **Tradeoffs**: The fast path leaves these commands and argument checks to the slower BKL path.
- **Watch out**: `reap_shm` runs at every thread exit, cheap only while no segment is marked for removal.
- **Source**: 5d7b936, 1be1142

### D34: setpriority and sched_setaffinity honour `CAP_SYS_NICE` over the target's user namespace
- **What**: setpriority and sched_setaffinity accept the same owner, compared with the target thread's own credentials (D48), or `CAP_SYS_NICE` over the target's user namespace, as Linux's `set_one_prio` and `sched_setaffinity` do, while sched_setscheduler and sched_setattr keep requiring the same owner or `CAP_SYS_NICE` in the initial namespace and raising priority still goes through `can_nice`.
- **Why**: Rootless runtimes act as root of their user namespace, and crun's sched_setaffinity on the container process failed with `EPERM`, so `podman run --userns keep-id` failed.
- **Tradeoffs**: Two owner checks coexist in `syscall/sched.rs` and must stay matched to the right syscalls.
- **Watch out**: Holders of the capability over an ancestor namespace are accepted through `ns_capable`, so a mistake in namespace ancestry would widen who can renice others.
- **Source**: 2aca260, 1be1142

### D35: The app starts its kernels with exception-trace on
- **What**: `MachineOptions::exception_trace` makes `start_kernel` record `debug.exception-trace` as 1 before PID 1 exists; the app sets it, and `appine-run` passes false, keeping arm64 Linux's default of 0 (kernel/signal D23).
- **Why**: The app wants crash reports from its kernels' first process in the kernel log, while Linux and the development host start with them off.
- **Tradeoffs**: Every `MachineOptions` literal names one more field, and guests in the app see a value other than Linux's default.
- **Watch out**: A guest tool expecting Linux's default sees 1 in the app, and a new host must choose the value explicitly.
- **Source**: 7b7f73b

### D36: A task marks itself as a vfork child for the OOM killer
- **What**: `Task::vfork` is set at creation from `vfork_done` and cleared wherever `vfork_done` is taken (`release_mm` and D29's exit salvage), so the OOM killer on the timer thread can skip vfork children (kernel/mm D32); syscall 448 dispatches to `mem::process_mrelease`, and `FlockHolder` records the taker's tgid.
- **Why**: Only the thread-private `TaskState` knew whether a task was still a vfork child, which the timer thread cannot borrow, and process_mrelease and flock holders needed a dispatch entry and a pid.
- **Tradeoffs**: The mark duplicates what `vfork_done` says.
- **Watch out**: A new path completing `vfork_done` must clear the mark too.
- **Source**: 39ea702

### D37: prctl and futex count the options and commands Appine lacks
- **What**: prctl's guarded options given refused arguments return `EINVAL` from an arm of their own, so the prctl and futex default arms count only what is missing, in the stub table `Kernel` holds (kernel/signal D24).
- **Why**: Missing prctl options and futex commands (`PR_SET_PTRACER`, `PR_SET_VMA`, the PI futexes before D44) could not be told from refused arguments.
- **Tradeoffs**: prctl options newer than 6.1 or defined nowhere are counted too.
- **Watch out**: A new guarded prctl arm must join the refused-arguments arm, or its bad arguments are counted as a missing option.
- **Source**: 99b4216

### D38: A watchdog thread reports long BKL holds without waiting for the BKL
- **What**: Every BKL acquisition records its holder (count, host tid, guest tid, tgid, comm, `Doing` and pc) in the cache-line `Hold`; a `bkl-watchdog` thread samples it every fifth of the threshold (10 s by default, `APPINE_BKL_WATCHDOG` in seconds, 0 off), reports a hold unchanged that long on stderr and in `Shared::stall` (which `Machine::stall` reads without the BKL), and at the hold's end spawns a `bkl-watchdog-log` thread that writes the KERN_ERR record under the BKL; nothing is killed.
- **Why**: A hold that never ended (a loop in kernel code, a host call blocking with the lock held) froze everything without saying where, and the first version's `ended` took the BKL itself to log, blocking the only watcher behind the next holder.
- **Tradeoffs**: Each acquisition pays a few relaxed stores and each entry a 40-byte thread-local write, durations are right only to one look (2 s by default), a thread releasing and retaking the lock in a loop is not caught, and the kernel-log record may arrive well after the stall or be lost if its thread cannot start.
- **Watch out**: Anything added to `Hold` must stay on its cache line as a plain store, a logger waiting for a BKL that is never released stays blocked and may outlive its `Machine`, and a hold that never ends still blocks `Machine`'s drop.
- **Source**: 64a6107, d20fd3c (the blocking `ended` recorded in d9e1f0c)

### D39: Unsharing a store object waits per inode and forgets the executable cache entry
- **What**: `Kernel::unsharing` (a set of inodes) holds files whose shared object is being copied with the BKL released, waiters sleep on `wake::LOCK`, and `ExeCache::forget` drops an inode whose data moved to another object (kernel/store D17).
- **Why**: Unsharing copies with the BKL released, and a file's data can now move to another object while the executable cache checked only the old object's version.
- **Tradeoffs**: Waiters also wake for unrelated lock-channel wakes and look again.
- **Watch out**: None known.
- **Source**: df6ee70

### D40: Processes in a time namespace with offsets get the vDSO's svc stubs
- **What**: When the clock offsets are not zero, clone (a child entering the namespace for children), execve (entering the namespace for children) and setns call `Mm::vdso_syscalls` to put the svc stubs back, never undoing it; the loader maps `[vvar]` below the vDSO and sets `vdso_svc` for the built image, and the fast path records (vdso, vdso_svc), passes x0 to `clock_stub_at` and reports the host timezone for gettimeofday (kernel/signal D4, D13).
- **Why**: Guests now run the host's own vDSO, whose code would return the host's clocks inside a time namespace with offsets, and the fast path must know which svcs come from stubs.
- **Tradeoffs**: An address space shared with a process that went back to the boot namespace keeps the stubs.
- **Watch out**: A new way to enter a time namespace must patch too.
- **Source**: 16364c0

### D41: execve refuses a script open for writing and reports a shared fs context
- **What**: execve checks the script itself for writers (`ETXTBSY`) right after recognising it, before looking at any interpreter, as well as the final program, and beside the tracer check tells the credential transition whether another process shares the caller's fs context, so a set-id or file-capability program gains nothing then (kernel/cred D4).
- **Why**: Linux's `do_open_execat` denies write access to every file execve opens, the script included, and `LSM_UNSAFE_SHARE` withholds privileges while the fs context is shared, but Appine checked only the final program and raised privileges regardless.
- **Tradeoffs**: The ELF interpreter and nested scripts are still not checked for writers, where Linux checks each (recorded in the map).
- **Watch out**: Programs that rewrite a script they are about to run while holding it open for writing now get `ETXTBSY`, as on Linux.
- **Source**: c3e2f57, 6d9a9e8

### D42: The dispatcher reruns a syscall to answer FUSE queries and read mapped pages
- **What**: `dispatch()` gives each syscall a fresh FUSE token and on `FUSE_RETRY` reads the page an access recorded (`EFAULT` if unreadable, `ERESTARTSYS` if a signal took the read back) and serves the walk queries before running the handler again, at most 16384 times (then `EIO`) and once as ptrace and seccomp see it; read and write calls have their buffers read in first, the FUSE node sweep runs after every syscall, a guest fault on an unread page reads it and re-executes (else SIGBUS), the run loop flushes the FUSE files of an fd table its last thread leaves, and execve reads a FUSE program through a retry and runs it nosuid (kernel/fuse D1 and D2).
- **Why**: Walks and memory accessors cannot wait for a daemon under the BKL, so the dispatcher and the run loop, where the BKL can be released and no syscall of the thread holds a FUSE node, answer instead.
- **Tradeoffs**: Handlers run several times per syscall, each syscall pays one more atomic load and each read or write an mm borrow, a huge read's buffer is read in whole, and FUSE programs never use set-id bits or file capabilities.
- **Watch out**: A syscall with a side effect before its last walk repeats it on a retry, and handlers that map memory errors to `EFAULT` themselves lose the retry.
- **Source**: c008749, 3957a03

### D43: The ELF loader follows Linux on unusual files, killing late failures as doomed images
- **What**: `Elf::parse` checks the header, program-header count and the first `PT_INTERP` (cut at its first NUL) as 6.1 does and takes `AT_PHDR` from the `PT_LOAD` holding `e_phoff`; `prepare` checks the interpreter's headers (`ELIBBAD`) and an empty path (`EACCES`); misaligned file contents, filesz > memsz, ranges beyond `TASK_SIZE`, a doomed interpreter and any mapping failure make an `Image::doomed` that execve kills with SIGSEGV after its point of no return; segments map in order over each other with brk-style anonymous tails, ET_EXEC interpreters load at their own addresses with `AT_BASE` 0, and the loader records the code and data bounds `/proc/<pid>/stat` shows.
- **Why**: Executables without `PT_LOAD`, with program headers outside every `PT_LOAD`, with a NUL inside `PT_INTERP` or with `PT_LOAD`s past EOF or above 2^48 failed execve, misaligned segments were copied, a wrong-architecture interpreter was `ENOEXEC`, and most of gVisor's exec_binary_test could not run.
- **Tradeoffs**: Segments without file contents at unaligned addresses map as 6.7's `elf_load` does rather than by the 6.1 contract, a PIE whose segments start above `ET_DYN_BASE` loads at its own addresses, and a doomed image maps nothing, so its core file would differ from Linux's partial one.
- **Watch out**: Segments sharing a page now give it the later segment's permissions, as on Linux, instead of the union, and `Machine::spawn` still starts a doomed image at pc 0 (recorded in the map).
- **Source**: ef68770, 6d9a9e8 (code and data bounds)

### D44: PI futexes keep their waiters in the futex queue and the word as the owner record
- **What**: `Futexes` entries carry an optional `PiWaiter`; `LOCK_PI`, `LOCK_PI2` and `TRYLOCK_PI` follow `futex_lock_pi_atomic` and `attach_to_pi_owner` (`EDEADLK` for one's own lock, `ESRCH` for a missing owner, `EAGAIN` for a trylock, else queue), waiting on the BKL path with `ERESTARTNOINTR` and 6.1's clocks; `UNLOCK_PI` hands the word to the first PI waiter or clears it, plain wakes and requeues skip PI waiters, and `futex::pi_owners_exited` gives each lock whose owner has left the task table to its first waiter with `FUTEX_OWNER_DIED`.
- **Why**: PI futex commands were `ENOSYS`, so `PTHREAD_PRIO_INHERIT` mutexes failed to initialize (musl probes with `FUTEX_LOCK_PI`, glibc with `FUTEX_UNLOCK_PI`) and gVisor's PI tests were skipped.
- **Tradeoffs**: No priority is inherited and waiters are not ordered by priority, `WAIT_REQUEUE_PI` and `CMP_REQUEUE_PI` stay `ENOSYS`, PI locks are not handed over at execve, and each thread exit reads every queued PI lock's word while PI waiters exist.
- **Watch out**: Tids are written as each waiter's own pid namespace sees them, which differs from Linux only when a lock's users are in different pid namespaces.
- **Source**: edd7f95

### D45: BKL syscalls are numbered and stamped on the task for the overlay sweep
- **What**: The global `SYSCALLS` count numbers each BKL-path syscall, and `InSyscall` records (number, syscall) in `Task::syscall` from after the entry stop and seccomp until the end-of-syscall sweeps, cleared on every way out of dispatch before the exit stop and signal delivery, so the overlay sweep can keep nodes removed after another thread's syscall began (kernel/vfs D49).
- **Why**: A syscall that began before a name was removed may hold the removed overlay node across a BKL release, and sweeping only while no other thread is in a syscall would be blocked for good by long waits such as an init's wait4.
- **Tradeoffs**: One atomic increment, an `Rc` clone and two `Cell` stores per BKL syscall.
- **Watch out**: A stamp left set while a thread stops or runs guest code holds back sweeps, so anything waiting inside dispatch after the sweeps must come after the guard is dropped.
- **Source**: 0d82807

### D46: An overlay program's exe keeps its overlay node
- **What**: `ExeFile::ovl` holds (mount, node, `Hold`), set by execve from the walk, node or open file it ran through, or from the removed overlay program of the process whose exe was run, and `open_exe_file` runs a removed one from the store inode behind its node with its overlay mount's noexec and nosuid (kernel/procfs D36).
- **Why**: A program found through an overlay had no node in its `ExeFile` (an overlay file's store inode must not become a store node), so nothing could tell whether its name was removed or lead back to it.
- **Tradeoffs**: One more global-mutex hold per process running an overlay program, and execve of one reads the overlay table once more.
- **Watch out**: Once the overlay instance is gone `real_of` returns None, so running through exe fails with `ENOENT`.
- **Source**: 9590431

### D47: Watched cgroup2 files are compared where cgroups change
- **What**: `cgroup::notify_changes` runs after each BKL syscall's sweeps, at the end of `task::exit`, before a thread waits in the freezer and in the timer thread after the OOM check, returning at once while nothing observes a cgroup2 file (kernel/procfs D38).
- **Why**: cgroup2 changes happen in syscalls, thread exits, freezer stops and the OOM killer, and the comparison that wakes pollers and inotify watches must run after them.
- **Tradeoffs**: One map emptiness check per BKL syscall.
- **Watch out**: None known.
- **Source**: affb1e2

### D48: Each thread carries its own credentials through clone, execve and the scheduling calls
- **What**: `Task::new` starts from the group's credentials and clone then sets the creating thread's; execve compares against the caller's old credentials and sets both copies; `PRIO_USER` and `IOPRIO_WHO_USER` match each thread by its own real uid, setpriority, ioprio and sched checks use the target thread's, and `RLIMIT_NPROC` counts tasks by their own real uid (kernel/cred D1).
- **Why**: Credentials became per thread, and thread creation, execve and the scheduling calls assumed one credential per process.
- **Tradeoffs**: `finish_group`'s parent-death signals still name the process's (main thread's) real uid as sender rather than the last exiting thread's.
- **Watch out**: Code that writes `Task.cred` or `ThreadGroup.cred` directly leaves the main thread's two copies apart.
- **Source**: 1be1142

### D49: sched_yield runs without the BKL and yields the host thread
- **What**: The fast path (D6) handles sched_yield by clearing `in_guest` and calling `std::thread::yield_now`, and when the fast path is off (tracing, a seccomp filter that does not always allow it) the BKL path yields inside `Cx::unlocked`.
- **Why**: sched_yield took the BKL just to return 0, so spinning threads (parking_lot and crossbeam backoff, Go's osyield, HotSpot's GC termination) slowed every other thread's syscalls up to 8.6 times and kept their CPU, and eight threads passing a turn ran 230 times slower than natively.
- **Tradeoffs**: A call costs 296 ns instead of 173 (a host syscall), a thread yielding on the BKL path reads as sleeping in /proc, and real workloads gain nothing measurable.
- **Watch out**: A fast-path call that stops counting as running guest code must not touch guest memory afterwards, and the host scheduler (CFS or Android's EAS) decides how long a yielder waits.
- **Source**: 78b3284

### D50: A parent waiting for its vfork child counts as frozen
- **What**: The vfork wait marks the parent frozen before waiting and afterwards (or after an error) clears the mark unless its thread group is freezing, leaving the signal path's freezer stop to clear it after thaw, as Linux 6.1's `wait_for_vfork_done` does.
- **Why**: Linux enters and leaves the frozen state around the vfork wait, and clearing the mark early would let `cgroup.events` flicker to unfrozen before the parent stops for a freeze still in progress.
- **Tradeoffs**: A parent woken while freezing stays counted while it runs on to its freezer stop, including through a ptrace `VFORK_DONE` stop.
- **Watch out**: A path back to guest code that skips the freezer check would leave the mark set.
- **Source**: b593172

### D51: execve clears `READ_IMPLIES_EXEC`, as arm64 does for every 64-bit program
- **What**: execve's point of no return clears `READ_IMPLIES_EXEC` and then, when privileged, the `PER_CLEAR_ON_SETID` flags, and brk reads the flag before borrowing the address space (kernel/mm D43).
- **Why**: personality(2)'s `READ_IMPLIES_EXEC` now takes effect, and natively a plain execve also cleared it, since arm64's `SET_PERSONALITY` drops it for every 64-bit program, which the first version kept.
- **Tradeoffs**: `setarch -X program` has no effect on the program, as on arm64 Linux.
- **Watch out**: None known.
- **Source**: 5760479

## Superseded

- ~~Deadlines stay plain `Instant`s (D15 in part, 96cd8bd)~~ → replaced by **D19** in 63b9e59 (2026-11-07)
- ~~PI futexes stay `ENOSYS` (D33's tradeoff, 5d7b936)~~ → replaced by **D44** in edd7f95 (2026-11-09)
- ~~bug.rs, debug.rs and syscall/mod.rs are filed in kernel/process though the registry does not list them (D18 in part, 0758e3c)~~ → replaced by **D14** in d2b1dc3 (2026-11-10)
