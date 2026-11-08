# kernel/process Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/process.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 23 claims checked by 3 independent verifiers — 17 confirmed, 5 partial and 1 refuted, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping: timeouts; dying pid namespaces; exited main thread; rlimits; exit signals; test-suite findings; namespace limits; exit panics; waits, pids; sched_setattr, kcmp; futex commands, shm reaping last)

## Responsibilities

`kernel/process` runs guest threads and processes inside the app's single host process:

- guest tasks (threads) and thread groups (processes): creation (`clone`/`clone3`), the run loop,
  exit, reaping and `wait`;
- `execve`: target resolution, credential effects, and the ELF loader that builds the new image;
- the big kernel lock (BKL) that serialises the kernel layer, its borrow checks, and the no-BKL
  fast path;
- the unified wait and wake-up mechanism, and futexes;
- the timer thread (interval and POSIX timers, CPU-time limits, periodic checks);
- the built-in virtual init (PID 1) and inittab boot/shutdown;
- scheduling parameters applied to host threads;
- the namespace mechanism (clone/unshare/setns, user and pid ID translation, time offsets, nsfs).

Root: `core/kernel/src/task.rs`, `process.rs`, `kernel.rs` (`Kernel`, `Shared`, `Bkl`, `Cx`, the
`wake` module), `cell.rs`, `syscall/proc.rs`, `syscall/fast.rs`, `futex.rs`, `loader.rs`,
`machine.rs` (timer thread, boot/shutdown, host-facing calls), `itimer.rs`,
`syscall/posix_timer.rs`, `loadavg.rs`, `inittab.rs`, `syscall/sched.rs`, `pidns.rs`, `nsproxy.rs`,
`syscall/ns.rs`, and `UserNs` in `cred.rs`.

Outside the module: address spaces and fork copy-on-write (`kernel/mm`), CPU state and translation
(`engine`), signals and ptrace, credentials beyond user namespaces, mount trees (`kernel/vfs`),
network namespaces (`kernel/netns`), cgroups, the OOM policy.

## Key types

### Tasks and processes

- `Task` — one guest thread: tid, `killed`, `pdeathsig`, exit status, CPU-clock bookkeeping, nice,
  affinity, I/O priority, its wake `event`, `sleeping`, `pid_ns_children`, `NsProxy`; thread-private
  `TaskState` (address space, fd table and `FsContext`, each shared or not per clone flags; thread
  group; alternate stack; `clear_child_tid`; vfork completion).
- `TaskSync` — lock-free `in_guest` flag and the engine `Kicker` (used by `kernel/mm` shootdowns).
- `ThreadGroup` — one process: tgid, ppid, `parent_thread` (the creating thread, Linux
  `real_parent`), pgid, sid, live `threads` (leader first while alive), `children` (zombies
  included), `exiting` / `leader_status` / `zombie`, `exit_signal`, own and children's CPU time and
  max RSS, `child_subreaper`, rlimits (Linux's defaults; `RLIMIT_MSGQUEUE` starts at 819200 but is
  not enforced), credentials, pid namespace, timers.
- `ExitStatus` — exited with a code or killed by a signal; encoded for wait.
- `CpuTime` — total and system nanoseconds from the host thread CPU clock.
- `task::pool` — pooled host threads (at most 8 idle, kept 5 s).
- `Kernel` — the global state behind the BKL: tasks, groups, waiters, pid allocator, namespaces,
  init state, power requests.
- `Shared` — the BKL mutex, its `held` hint and spinner counter, the fast-path epoch, the futex
  queue, the timer thread's event.
- `Bkl` — the held-lock guard; its `Drop` performs release-time work.
- `Cx` — the context of one syscall or signal delivery; holds the `Bkl` (absent while released).

### Waiting and futexes

- `wake` channels: `FILE`, `OTHER`, `CHILD`, `WATCHED`, `EXITED`, `LOCK`, `SIGNAL` (plus the
  internal `OBJECTS`); `wake::Object` is the address of a value inside a `RefCell`.
- Waiter — (event, interest channels, optional sorted object list), stored in `Kernel::waiters`.
- `futex::Key` — `Private{mm, addr, shared}`, `Host(address)` or `File{dev, ino, offset}`;
  `Futexes` — one queue of
  (id, key, bitset, event) with its own mutex; `Enqueued` — removes its entry on drop.

### Exec, timers, init, namespaces

- `loader::Executable`, `ExeCache` (32 opened executables), `Args`, `Prepared` (the hand-off
  between the locked and unlocked exec phases), `Image`; `process::ExeFile` (the process's
  `exe_file`).
- `itimer::ITimer`, `CpuTimer`, `PosixTimer`; `itimer::Expiry` — a wall-clock deadline on Linux's
  hrtimer base: `Mono(Instant)` (monotonic, and relative realtime) or `On(host clock, time)`
  (absolute realtime and TAI, boottime), compared with that clock whenever looked at; the global
  wall-clock jump count (`clock_epoch`); `loadavg::LoadAvg`; `inittab::Entry`, `Power`.
- `NsProxy` (per task: UTS, IPC, net, cgroup, time, time-for-children), `UtsNs`, `IpcNs`,
  `CgroupNs`, `TimeNs` (offsets, frozen), `UserNs` (parent, level, owner, uid/gid maps, setgroups,
  whether the creator had `CAP_SETFCAP`; held in credentials), `PidNs` (parent, level, owner, global↔local tables, reaper, dead; held per
  thread group), `MntNs` (held in `FsContext`), `NsRef` (a strong reference to any namespace),
  `NsSet`; `Ucounts` (`Kernel::ucounts`: the live namespaces by kind with the (user namespace,
  user) pairs each counts for, the counts, and each user namespace's `max_*_namespaces`).

## Public entry points

- **Syscalls**: `clone` (220), `clone3`, `exit`, `exit_group`, `set_tid_address`, `wait4`,
  `waitid`, `execve`, `execveat`, `futex`, `futex_waitv`, `set_robust_list`, `setitimer`/
  `getitimer`, `timer_*`, `getpriority`/`setpriority`, `ioprio_*`, `sched_*`, `unshare`, `setns`,
  namespace ioctls (`NS_GET_*`), `prctl` (`PR_SET/GET_PDEATHSIG`, `PR_SET/GET_CHILD_SUBREAPER`),
  `kill` to pid 1 (power requests).
- **Kernel-internal**: `task::start`, `thread_main` → `run` → `exit`, `process::finish_group`,
  `Shared::lock`, `block`/`block_for`, `Cx::wait*`, `Cx::unlocked`, `request_wake*`,
  `signal_later`/`flush_signals`, `Shared::fast_changed`, `loader::prepare`/`load_prepared`.
- **Host-facing** (`Machine`): `start_kernel` (creates PID 1 and the timer thread), `spawn`, `boot`,
  `shutdown`, `take_power`, `wait_*`, `signal` (a host signal to a guest process, `SI_KERNEL`);
  `run` (`appine-run`), which with `RunOptions::stop` passes the first stop signal the host got on
  to the program and ends every process at one arriving at least a second later, then finishes as
  usual.
- **procfs**: `threads-max`, `/proc/<pid>/task/<tid>/children`, `uid_map`/`gid_map`/`setgroups`,
  `timens_offsets`, `/proc/<pid>/ns/*`.

## Data flow / lifecycle

### Tasks and host threads

- Each guest thread runs on its own host pthread, scheduled by the host kernel. A finished thread's
  host thread is pooled (at most 8 idle, 5 s) and its engine CPU state goes to the engine's spare
  list (up to 8), so creating threads rarely creates host threads.
- A thread starting on a host thread records the host CPU-clock base and host tid and re-applies its
  nice, I/O priority and affinity (pooled threads are reused).
- CPU time is the host thread's CPU clock minus the base; user and system time are each kept
  monotonic like Linux `cputime_adjust`.

### clone

- Global ids come from `alloc_tid`: the numbers after the last one, up to `PID_MAX - 1`, then from
  300 again (Linux's `RESERVED_PIDS`); `PID_MAX` itself is never handed out, and `ns_last_pid`
  writes move the start.
- `clone` takes the low 32 bits of the flags (the pidfd shares the parent-tid slot); `clone3` checks
  the structure size (non-zero tail is `E2BIG`). There are no separate fork or vfork syscalls on
  aarch64.
- Invalid combinations are `EINVAL` (thread without shared signal handlers, shared handlers
  without shared memory, PIDFD with THREAD or DETACHED, namespace conflicts — section 14).
- Limits: `RLIMIT_NPROC` counts live threads per real uid (not for real uid 0 or with
  CAP_SYS_RESOURCE/CAP_SYS_ADMIN); `threads-max` applies to root too (`EAGAIN`) and counts live
  tasks only (default: physical memory in MB, at most half the host fd limit, at least 20, 4096 if
  memory is unknown; writable in 20..=0x3fffffff); the cgroup `pids.max` is checked; then a pid is
  chosen. Host-launched processes (`Machine::spawn`) are a trusted path and skip these limits.
- The child's address space is shared (CLONE_VM) or forked (`kernel/mm`); fds and fs are shared or
  copied. A new process gets `ppid` and `parent_thread` from the caller (or the caller's parent with
  CLONE_PARENT, which also gives it the caller's process's exit signal, as Linux's
  `copy_process` does). A fresh CPU state copies the parent's registers with x0 = 0, the new stack and TLS.
- Registration: the pid is registered (in every pid-namespace level), the parent/child tid and pidfd
  are written, then `task::start` queues the host thread and inserts the task into the group, the
  address space's users and the kernel's task table. A failure releases the pid and the pidfd.
- vfork: the parent waits (interest none) until the child execs or exits, or it is killed.

### Run loop

- Tier: the kernel's tier is the interpreter or the translator; if attaching a translator fails or
  hardware breakpoints are set the thread runs `Cpu::run`; single-stepping uses `Cpu::step`.
- An `Svc` exit first tries the fast path (section 8) without the BKL; everything else takes the
  BKL and is handled: a killed thread exits; syscalls are dispatched; kicks deliver signals;
  undefined instructions raise `SIGILL`, breakpoints and watchpoints `SIGTRAP`, alignment faults
  `SIGBUS`, host faults kill the process; page faults go to `kernel/mm`.
- The result (`Continue`, `Exec`, `Exit`) decides the next step; `Exec` re-attaches the translator
  for the new address space. After each BKL-path handling the fast-path cache is refreshed.

### Exit and reaping

- `exit` sets the thread's status (the leader's also records `leader_status`); `exit_group` and
  fatal signals set the group's `exiting` status once, mark every thread killed and wake them.
  Before any `PTRACE_EVENT_EXIT`, `begin_exit` makes the last thread to start exiting fill in the
  group status (Linux 6.1 `synchronize_group_exit`).
- `task::exit` runs in stages, each contained by `bug::catch`: collect statistics and CPU time,
  sample RSS if last, release the address space (robust list, `clear_child_tid` wake when the
  address space is shared, vfork completion, leave its users), ptrace and seccomp cleanup; when
  that stage panics, vfork completion and leaving the users run again on their own (both are
  idempotent) before the process is killed, so no stale user is kicked and no vfork parent waits;
  remove
  the task and its pid, and reap SysV shm segments marked for removal whose last attachment that
  was (`kernel/ipc`); hand this thread's children to another live thread (non-last) or finish
  the group (last); drop references under the BKL; request an `EXITED` wake. The engine CPU state is
  freed after the BKL is released.
- `finish_group`: release AIO; reparent children to the nearest `PR_SET_CHILD_SUBREAPER` ancestor
  in the same pid-namespace level that still has live threads (one in the middle of exiting can be
  chosen, as on Linux), else the pid-namespace init while it has not exited, else the virtual init
  (children get `exit_signal = SIGCHLD`; zombies handed to the virtual init are freed at once);
  send `PR_SET_PDEATHSIG` signals (keyed to the creating thread); release SysV IPC and the
  controlling terminal.
- When a pid-namespace init exits, the namespace is marked dead and every other member gets
  `SIGKILL`; the init's own children, and the orphans other members leave while they die, go to the
  virtual init (which frees them at once, as Linux's exiting init reaps them), and its exit is
  reported only once all other members are reaped (it stays registered meanwhile).
- Reporting: the status is `exiting` or the leader's; it is reported to a tracer or the parent. The
  group is reaped immediately when the parent is the virtual init, gone, ignores `SIGCHLD` or has
  `SA_NOCLDWAIT`; otherwise it becomes a zombie and the parent gets `exit_signal`. Orphaned process
  groups get SIGHUP/SIGCONT. A `CHILD | WATCHED` wake follows.
- `wait4`/`waitid` scan `children` (filtered by the target and `__WNOTHREAD`, and by exit signal as
  Linux's `eligible_child`: without `__WALL`, only children reporting with `SIGCHLD`, or with
  `__WCLONE` only the others; a child the caller traces always counts): a zombie is reaped (its CPU
  time and max RSS added to the waiter's children totals), then stop and continue reports, then
  tracees; nothing matching is `ECHILD`, and a zombie counts only for a wait that takes exits (so
  waiting for stops and continues of children that have all exited is `ECHILD`); otherwise the
  waiter blocks on `CHILD` and restarts if interrupted.
- An exited leader keeps its tgid registered until the group is reaped. Its task leaves the task
  table like any thread, but the group records its name and own CPU time (`leader_exit`, cleared
  when a non-main thread's execve takes the tgid), and procfs shows it as a zombie thread until
  the group is reaped, as Linux keeps the leader as a zombie task.

### execve

- Arguments: the limit comes from `RLIMIT_STACK` (`bprm_stack_limits`: a quarter of the limit, at
  most 6 MB, at least 128 KB); a string of 128 KB or more or a larger total is `E2BIG`; an empty
  argv becomes `[""]`.
- Target: by path, by fd (`AT_EMPTY_PATH`), through `/proc/<pid>/fd/<n>` (the open file) or
  `/proc/<pid>/exe`. It must be a regular executable file (`EACCES`), not on a noexec mount;
  nosuid is noted.
- `#!` scripts: the interpreter line is parsed (up to 256 bytes) and the interpreter resolved
  relative to the cwd; five nested scripts run, a sixth is `ELOOP` (Linux 6.1's `exec_binprm`).
- `ETXTBSY` if a writable open file refers to the final executable.
- Credentials: setuid/setgid apply unless `no_new_privs`, nosuid, or the owner/group are unmapped;
  file capabilities come from the xattr. A secure exec clamps the stack limit to 8 MB (written back
  to `RLIMIT_STACK`).
- Under the BKL the main ELF is parsed and the interpreter file opened and read; then the BKL is
  released while the interpreter's headers are parsed and a new address space (sharing the kernel's
  read-only file mappings, the vDSO image, host random bytes) and initial stack are built. Back
  under the BKL, a killed thread gives up with `EINTR`.
- Point of no return: the other threads are killed and waited for (the BKL is released while
  waiting; only the caller being killed can still fail it, with `EINTR`); a non-leader takes over
  the tgid. Then: enter the time namespace for children, unshare and close-on-exec the fd table,
  release locks, sample RSS, record the new `exe_file`, apply dumpable/pdeathsig rules and
  credentials, clear POSIX timers and AIO, unshare and reset signal handlers, release the old
  address space (reaping shm segments marked for removal it held last), reset the CPU state for
  the new page table (re-kicking if interrupted), and set PC and SP. The run loop then re-attaches the translator.

### ELF loader

- Accepts 64-bit little-endian aarch64 `ET_EXEC` and `ET_DYN` (else `ENOEXEC`); a `PT_INTERP`
  (ld-musl, or glibc's ld.so) is loaded the same way and must itself be `ET_DYN` without an
  interpreter.
- A position-independent main program loads at `0x5555_5555_0000`; the interpreter and then the
  vDSO are placed top-down below the mmap area (so the vDSO is below the interpreter). There is no
  ASLR.
- Segments are private file mappings of the host object when every segment's offset and address
  agree modulo the page size and no two share a page; otherwise every page is anonymous memory with
  the union of the covering segments' permissions, filled by copying. The `.bss` tail of the last
  file page is zeroed.
- The initial stack holds argv, envp, the execfn, the platform string and 16 random bytes, and the
  auxv in this order: `AT_SYSINFO_EHDR`, `AT_MINSIGSTKSZ`, `AT_HWCAP`, `AT_PAGESZ` (4096),
  `AT_CLKTCK` (100), `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_BASE`, `AT_FLAGS`, `AT_ENTRY`, `AT_UID`,
  `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_SECURE`, `AT_RANDOM`, `AT_HWCAP2`, `AT_EXECFN`, `AT_PLATFORM`
  (`aarch64`), `AT_NULL`. IDs are mapped into the process's user namespace.
- A Go build-ID note in the main program selects the translator's Go register set (`engine`).
- Opened executables are cached (32 entries, checked against the host file's identity).

### The BKL

- One mutex over `Kernel`. Kernel objects are shared with `Rc` and the kernel's own `RefCell`; their
  counts and borrow flags are not atomic, so even cloning or dropping them needs the BKL.
- Held: for every guest exit except fast-path syscalls (syscalls, faults, kicks, signal
  delivery), by the timer thread while it works, and by host-facing `Machine` calls. Not held while
  running guest code.
- Released inside a syscall only by the wait primitives and `Cx::unlocked`, whose closure must be
  `Send` and so cannot touch kernel objects. Uses of `unlocked`: host fd I/O, regular-file
  `pread`/`pwrite` (one positional operation per open file at a time, like `f_pos_lock`), fsync,
  `sync_file_range`, msync, copy_file_range/sendfile copies, store metadata syncs, DNS stub queries,
  and the execve image build. A guard re-takes the BKL on return and on unwinding.
- Acquisition: try once; otherwise up to half the host CPUs (1 to 4) spin for at most about 30 µs,
  reading only the `held` flag; the rest sleep on the mutex.
- Release (`Drop for Bkl`), in order: hang up closed terminals, match pending wake requests against
  waiters, clear `held`, unlock, clear the thread's held marker, notify pooled host threads, signal
  the matched events, release deferred host memory and refill fork spares (`kernel/mm`).
- Waits outside a syscall context (the timer thread, `Machine::wait_*`, the terminal bridge) call
  `block`/`block_for` directly; they hold no kernel objects while the BKL is released.
- Borrow checks: the kernel's `RefCell` counts borrows per thread; releasing the BKL in the middle of
  a syscall (`block_for`, `unlocked`) asserts the count is zero. Always on in debug builds; in
  release builds with the `borrow-check` feature (enabled for `appine-run` and the debug app). The
  inode store also checks that its owning kernel's BKL is held.

### Fast path (`syscall::fast`)

- Without the BKL, a thread can run `futex` (WAIT, WAKE and the BITSET forms, `FUTEX_CLOCK_REALTIME`
  only with WAIT_BITSET, on a 4-byte-aligned address that translates writable and is not in a
  writable shared file mapping), `nanosleep`, `clock_nanosleep` (REALTIME, MONOTONIC, BOOTTIME),
  `clock_gettime` (host clocks or its own CPU clock), `clock_getres`, `gettimeofday`, and
  `newfstatat`/`statx` (when the root and cwd are in the store's metadata cache, no synthetic
  filesystem or other mount is crossed, and the credentials can skip search-permission checks).
- It is off while strace-tracing, under `PTRACE_SYSCALL`, single-stepping, or with a pending
  `ivau_page`; with seccomp only for numbers the filter always allows (vDSO clock stubs always).
- Per-thread state (root, cwd, credentials, time offsets, the vDSO address) is cached under the BKL
  and invalidated by a global epoch bumped by `fast_changed` (chdir, chroot, credential changes,
  mounts, setns, time-namespace entry). A call that cannot complete falls back to the BKL path.
- Fast futex: under the futex lock the value is compared and the waiter enqueued; it sleeps on its
  event; dequeued means woken, still queued after waking hands over to the BKL path (a relative
  timeout's end time is preserved).

### Waiting and waking

- A guest thread never blocks in a host syscall: every wait is one host `poll()` on the thread's own
  event (an eventfd) plus the host fds it waits on, with the deadline as
  timeout. Wall-clock deadlines (sleeps, futex and mqueue timeouts, timers) are `Expiry` values
  turned into an instant at each wait and checked with their own clock, so a wait woken after the
  realtime or boottime clock jumped (the host's clock set, or the device resuming) waits again
  for the right time; relative `CLOCK_BOOTTIME` sleeps count the time the device slept.
- `block_for`: register the waiter (unless its interest is empty), release the BKL, poll, re-take
  the BKL, clear the event, deregister. Host `EINTR` counts as a wake. Callers re-check their
  condition, deadline and interruption.
- Timeouts and timer values read from the guest (every `timespec` reader, `itimerval`, socket
  timeouts) saturate at `KTIME_MAX` (about 292 years) as Linux's `timespec64_to_ktime` does, so
  GNU `sleep infinity` waits until interrupted and timers report the saturated time left.
- Wake requests made under the BKL are recorded per thread (several kernel instances may share the
  host process) as channels and objects; requests made without the BKL go to a global fallback.
  At BKL release, a waiter fires if its interest shares a channel with the request, or if objects
  changed and it is `FILE`-interested with no object list or a matching object. Matched events are
  signalled after unlocking, so woken threads do not immediately sleep on the BKL.
- Objects: waiters register what they wait on (pipes, PTYs, eventfd, timerfd, inotify, signalfd,
  epoll, socket queues, message queues, seccomp notifications, kernel log); poll/select/epoll
  register each watched file's objects. SysV IPC, AIO, SIGCONT of a stopped group, a host terminal
  closing and machine teardown still wake everyone.
- Signals: waking a task kicks its CPU (so running guest code exits) and signals its event (so a
  sleeper wakes); a signal blocked everywhere only requests `SIGNAL`.
- `/proc/<pid>/stat` shows S while a thread waits and R once its event is signalled, before it
  re-takes the BKL (like `try_to_wake_up`). A thread woken by host-fd readiness or a timeout reads S
  until it re-takes the BKL; fast-path sleeps read R as soon as they return; a thread in
  `Cx::unlocked` reads S only while its event is unsignalled. The process line uses the main
  thread, Z once it has exited (even while other threads run, as on Linux); group zombie (Z), the
  thread's ptrace stop (t) and group stop (T) take precedence.
- Pipes wake like Linux: a write wakes readers only if the pipe was empty; a read wakes writers only
  when enough space for the writer's request appears or `PIPE_BUF` is crossed.

### Futexes

- Keys follow Linux's `get_futex_key`: private memory, and any memory for an operation with
  `FUTEX_PRIVATE_FLAG`, is keyed by the address space's id and the address (with whether the
  operation was shared, so private and shared operations never meet), whatever page is there now;
  without the flag, shared anonymous memory (shared mappings, SysV shm) uses its host address and
  a shared file mapping (even a read-only one) (device, inode, offset), so processes sharing them
  meet. Kernel wakes (robust list, `clear_child_tid`) are shared operations. The no-BKL path
  handles only writable private pages outside the address space's writable shared mappings. The
  single waiter list is scanned linearly.
- Supported: WAIT, WAKE, REQUEUE, CMP_REQUEUE, WAKE_OP, WAIT_BITSET, WAKE_BITSET, `futex_waitv`;
  the PI operations return `ENOSYS`. As in 6.1's `sys_futex`/`do_futex`: the command is everything
  but `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME` (unknown bits `ENOSYS`); the timeout of a
  command taking one is read and checked first (`EFAULT`, `EINVAL`), then `FUTEX_CLOCK_REALTIME`
  with anything but the absolute waits is `ENOSYS`. Wake counts are ints that stop after the
  first waiter, so 0 and negative counts wake one.
- WAKE_OP (`futex_wake_op`): the word at the second address must be writable (`EFAULT`; a page
  not written since fork or without an entry is made writable first) and is changed atomically
  (set, add, or, and-not, xor; `FUTEX_OP_OPARG_SHIFT` makes the argument a shift count; another
  operation `ENOSYS` before the change); up to the first count of waiters on the first address
  are woken and, when the old value compares as asked with the 12-bit argument (`EQ`, `NE`, `LT`,
  `LE`, `GT`, `GE`; another comparison `ENOSYS` after the change), up to the fourth argument's
  count on the second.
- Wait: compare and enqueue under the futex lock, then wait alone (no channels); dequeued means
  woken; timeouts are `ETIMEDOUT`; interruptions restart (with a restart block when timed).
- Wake removes up to n matching waiters in FIFO order and signals their events directly. The robust
  list and `clear_child_tid` at thread exit also wake waiters; a robust lock word on a
  copy-on-write page (not written since fork) or a page without an entry is made writable first
  (`cow_break`, `fault_in`), as Linux faults it in.
- Lock order is BKL then futex lock; the fast path takes only the futex lock, so BKL and fast paths
  share one queue without lost wakeups.

### Timer thread

- One host thread ("timers") loops: take the BKL; if its host `ClockWatch` reports a wall-clock
  jump, run `clock_jumped` (bump the jump count, signal every task's event: BKL waiters, futex
  waiters and no-BKL sleepers alike look at their deadlines again); expire interval timers, POSIX
  timers and CPU-time limits; run the OOM check, the translator code-view release, load-average sampling, deferred
  store commits, `O_ASYNC` and `EPOLLEXCLUSIVE` checks, DNS stub answers (`kernel/net`) and raw
  ICMP taps; then wait (BKL released) until the earliest deadline or its event. It listens on
  `OTHER` (or all channels while `O_ASYNC` files or exclusive gates exist).
- Setting a timer writes the deadline into the thread group and signals the timer thread's event.
  POSIX timers on wall clocks keep an `Expiry`, so absolute realtime and boottime timers fire by
  their own clock; the watch's fd is in the timer thread's poll.
- `ITIMER_REAL` uses wall time; `ITIMER_VIRTUAL`/`PROF` and process CPU-time timers use host thread
  CPU clocks. Armed CPU timers or a finite `RLIMIT_CPU` make it tick every 10 ms. Over the soft
  `RLIMIT_CPU` sends `SIGXCPU` and raises the soft limit by 1 s; over the hard limit `SIGKILL`.
- A POSIX timer whose previous signal is still pending counts an overrun instead. A
  `SIGEV_THREAD_ID` timer signals its thread only while that thread lives in the process; after it
  exits the signal is dropped, as on Linux.
- When every thread is waiting, the periodic checks (OOM, code views, load average) stop until a
  thread next enters guest code; armed timers still wake it. Stopping sets `checks_idle` and then
  looks at the threads again (`Shared::stop_checks`), while a waking thread clears its `sleeping`
  flag before reading `checks_idle` (`resume_checks`), all SeqCst, so a thread waking in between
  either wakes the timer thread or is seen running. Changing `RLIMIT_CPU` does not wake
  the timer thread; the new limit takes effect at its next pass.

### PID 1

- The virtual init is a thread group with no threads and no guest program. Host-started processes
  (each terminal tab's login shell) are its children; it adopts orphans and its children are never
  zombies.
- On request (the app always; `appine-run --init`) it boots from a subset of `/etc/inittab`:
  `sysinit` (waited), then `boot` and `bootwait` together in file order (only `bootwait` waited),
  then `wait` (waited) and `once` (not waited) for the default runlevel (`initdefault`, else 2).
  Commands run through `/bin/sh -c` with stdin from `/dev/null` and output appended to
  `/var/log/boot.log` (the first command that runs truncates it; `/dev/null` if `/var/log` is
  missing); entries whose absolute program path does not exist are skipped; `respawn` and
  `askfirst` never run.
- Shutdown (only after a boot): run `shutdown` entries and `wait`/`once` entries that list the
  requested runlevel (0 for halt and poweroff, 6 for reboot), all within one caller-supplied
  deadline (30 s from the hosts); then SIGTERM everything, wait up to 1 s, and kill the rest. The
  Android host always requests poweroff.
- From the start of boot, `kill(1, SIGTERM|SIGUSR1|SIGUSR2)` by a sender allowed to signal root is
  a reboot, halt or poweroff request handed to the host (`Machine::take_power`; only the first is
  kept until taken); senders not allowed get `EPERM`; other signals to pid 1 are ignored.
  `pidfd_send_signal` and broadcasts skip pid 1 and `rt_sigqueueinfo` to it is `ESRCH`.

### Scheduling and TLS

- nice, CPU affinity and I/O priority are per thread and applied to the host thread; raising
  priority floors at host nice -4 (never above Android's display threads); `SCHED_IDLE` maps to host
  nice 19; real-time policies are only recorded; a real-time I/O class becomes best-effort on the
  host. `sched_setattr` takes Linux 6.1's flags: `SCHED_FLAG_KEEP_POLICY` keeps the current policy,
  `SCHED_FLAG_KEEP_PARAMS` changes neither parameters nor policy (only reset-on-fork and the
  clamps), `SCHED_RESET_ON_FORK` inside the policy is `EINVAL`, a nice value beyond `RLIMIT_NICE`
  is `EPERM`, and only `SCHED_OTHER` and `SCHED_BATCH` take the nice value (`SCHED_IDLE` keeps the
  thread's); the utilization clamps are checked against 1024 and, each given or current, against
  each other, and only recorded per thread (copied at clone, reset by `SCHED_RESET_ON_FORK`, read
  back by `sched_getattr`); the deadline flags are accepted unused. Without `CAP_SYS_NICE`,
  leaving `SCHED_IDLE` needs the `RLIMIT_NICE` that allows the current nice value, and
  reset-on-fork cannot be cleared (`EPERM`, for sched_setscheduler too).
- Guest CPUs are the host's allowed CPUs (up to 64); `sched_yield` returns at once.
- The guest's `tpidr_el0` is a field of the CPU state; the host `TPIDR_EL0` is never changed, so
  ART's signal chain always sees bionic's value.

### Namespaces

- All eight types exist. UTS, IPC, net, cgroup and time live in the task's `NsProxy`; the mount
  namespace lives in `FsContext` (tasks sharing a fs share it); the pid namespace belongs to the
  thread group, with a per-task namespace for children; the user namespace lives in the credentials.
- clone and unshare: a new user namespace is created first and the other `CLONE_NEW*` flags then need
  `CAP_SYS_ADMIN` in it; unshare implies thread/fs unsharing for user and mount namespaces and is
  `EINVAL` for multithreaded callers; it copies the fd table and fs context first, then applies
  credentials, mount namespace, pid-for-children, IPC/semaphore exit and a new `NsProxy`, in that
  order; `CLONE_NEWPID` affects only children; a new mount namespace copies the whole mount tree
  (`kernel/vfs`), a new cgroup namespace is rooted at the creator's cgroup. Pid namespaces nest to
  level 32 and user namespaces to 33 (`ENOSPC` beyond). Time namespaces follow the Linux 6.2
  rule: a new one becomes time-for-children; non-`CLONE_VM` children enter it at fork; threads and
  vfork children enter at execve.
- Namespace limits (Linux's ucounts): a namespace created by effective user u in user namespace n
  (for a new user namespace, n is its parent; open_tree's anonymous mount namespaces count too)
  counts for (n, u), then for (n's parent, n's owner) up to the initial user namespace, and every
  one of those counts must stay below its user namespace's `max_<kind>_namespaces` (half of
  threads-max in the initial one, as threads-max is now where Linux fixes it at boot; `INT_MAX`
  elsewhere), else `ENOSPC`: for a user namespace after the level check and before the chroot and
  mapping checks, for the others after the `CAP_SYS_ADMIN` check and before any is created. A
  namespace counts while an `Rc` to it lives; those gone are taken off at the next check (a weak
  reference per namespace), and a user namespace's limits go with it.
- setns accepts a namespace file or a pidfd (a subset of types); every part is validated in Linux
  order (`validate_nsset`) before anything switches, then all switch together.
- User namespaces: credentials hold kernel IDs; IDs shown outward (`get*id`, stat, procfs,
  `SO_PEERCRED`, `si_uid`) are mapped through the viewer's maps, unmapped as 65534. Each map can be
  written once, and the checks use the credentials the file was opened with (as Linux's
  `file_ns_capable`): the opener must be in the namespace or its parent and have `CAP_SYS_ADMIN` in
  the namespace; mapping the parent's uid 0 needs `CAP_SETFCAP`, which the creator must have had if
  the opener is inside, or the opener must have in the parent; otherwise the opener, owning the
  namespace, may write only a single extent of its own euid (or egid after `setgroups` is set to
  deny), unless both opener and writer have `CAP_SETUID`/`CAP_SETGID` in the parent;
  `timens_offsets` checks the opener's `CAP_SYS_TIME`. Capability checks come in four forms (`capable` in the initial
  namespace, `capable_here`, `ns_capable` following Linux `cap_capable`,
  `capable_wrt_inode_uidgid`).
- Pid namespaces: kernel pids are global; each namespace keeps global↔local tables; syscall
  arguments are translated at dispatch and results (and wait, siginfo) at exit; invisible results
  become 0. The first process numbered 1 is the reaper (section 4 for its exit); registering into
  a dead namespace is `ENOMEM`; procfs instances number by the mounting process's namespace, while
  `ns_last_pid` and loadavg follow the reader.
- Time namespaces: monotonic and boottime offsets are added to clock reads (and `/proc/uptime`,
  sysinfo) and subtracted from absolute waits.
- Namespace files: `/proc/<pid>/ns/*` link into nsfs; open files and bind mounts hold the namespace
  (`NsRef`), so `setns` works after its processes exit (`ip netns add`); mqueue and procfs mounts
  also hold their namespace.

## Dependencies (inbound / outbound)

### Inbound

- Syscall dispatch (`syscall/mod.rs`: the BKL around handlers, pid translation, `Mm::sync` after
  each call), `Machine` and the hosts (CLI, Android JNI: boot, shutdown, power, spawn), signals and
  ptrace (group exit, delivery, exit hooks), procfs, every blocking syscall (via the wait API),
  `kernel/mm` (kicks through `TaskSync`, `quiesce`), mount (nsfs binds, procfs/mqueue mounts).

### Outbound

- `engine`: `Cpu::new`/`run`/`step`/`reset`, `Dbt::attach`/`run`, `Kicker`, `guest_hwcap`.
- `kernel/mm`: `fork`, `quiesce`, users, `futex_key`, `translate`, layout and mapping calls used by
  the loader, `defer_unmaps`/`release_unmapped`/`spare::refill`.
- vfs (resolution, mount flags, store, `copy_ns`), credentials, cgroups, SysV IPC
  (`kernel/ipc`), AIO (`kernel/vfs`), the OOM killer (`kernel/mm`), loadavg, the store's deferred
  commits.
- `appine_hal`: `poll`, `Event`, thread CPU clocks, host thread nice/affinity/ioprio, host CPUs,
  random bytes; `std::thread`, `std::sync::Mutex`.

## Invariants and gotchas

- Lock order is BKL, then the futex lock; never the reverse.
- Kernel objects (`Rc`/`RefCell`) may be touched, cloned or dropped only under the BKL; a thread
  must not hold a kernel-cell borrow when it releases the BKL mid-syscall.
- Wake-ups are signalled only after the BKL is released, and events are level-triggered and cleared
  only after re-taking it, so every waiter must loop and re-check its condition.
- A task leaves the kernel's task table and its address space's users under the BKL before its CPU
  state is freed; kicks rely on this.
- `begin_exit` is idempotent and runs before any `PTRACE_EVENT_EXIT`.
- Kernel pids and IDs are global inside the kernel; translation happens only at the syscall, procfs
  and signal boundaries.
- The fast path must leave guest memory untouched after it stops being "in guest", because
  `Mm::sync` waits only for threads in guest code.

- The borrow-count assertion runs only at mid-syscall releases (`block_for`, `unlocked`), not at
  every BKL release.
- A finite soft `RLIMIT_CPU` alone keeps the timer thread ticking every 10 ms even while idle.
- A deadline is `Instant::now()` plus a guest duration, which panics on overflow: new readers of
  guest times must saturate through the shared `timespec` helper in `syscall/mod.rs`.

## Confirmed bugs / technical debt

- **`unshare` is not atomic:** a network-namespace allocation failure (`ENOMEM`) leaves the earlier
  credential, mount and pid-for-children changes in place. A pidfd `setns` combining
  `CLONE_NEWNS` with other types while the fs is shared switches the mount namespace of every fs
  sharer (Linux switches only the caller's).
- **`ETXTBSY` checks only the final ELF:** executing a script, or a program whose interpreter, that is
  open for writing succeeds where Linux returns `ETXTBSY`.
- **Secure exec caps arguments at 2 MB:** the final argument check uses the clamped 8 MB stack limit,
  whereas Linux 6.1 computes the argument limit before clamping (up to 6 MB).
- **The same-owner checks count `CAP_SYS_NICE` only in the initial user namespace:**
  sched_setaffinity, sched_setscheduler/sched_setattr and setpriority let a caller act on a thread
  of another user only with `CAP_SYS_NICE` in the initial namespace, where Linux's
  `check_same_owner` and `set_one_prio` accept it over the target's user namespace; root in a
  rootless container gets `EPERM` (crun setting the CPU set of `podman run --userns keep-id`).
- **No PI futexes:** `FUTEX_LOCK_PI`, `LOCK_PI2`, `UNLOCK_PI`, `TRYLOCK_PI` and the requeue-PI
  pair are `ENOSYS`, so glibc's and musl's `PTHREAD_PRIO_INHERIT` mutexes fail to initialize
  (`ENOTSUP`).
- **Latent hazards:** matching wake-ups during BKL release runs outside `bug::catch` before the guard
  drops, so a panic there would leave the BKL locked; wake requests made without the BKL go to a
  global fallback that any kernel instance may consume.
- **The ELF loader disagrees with Linux on unusual program headers:** an executable with no
  `PT_LOAD` segment, or whose program header table lies outside every loaded segment and has no
  `PT_PHDR`, fails execve with `ENOEXEC`, where Linux 6.1 execs it (with `AT_PHDR` 0) and the new
  image then runs or faults; conversely, segments whose file offset and address disagree within the
  page are loaded by copying, where Linux kills the process with `SIGSEGV` after the point of no
  return. `Elf::parse` also fails with `ENOEXEC`, before the point of no return, for a `PT_INTERP`
  with a NUL before its last byte (Linux uses the string up to the first NUL: `"\0\0"` opens the
  cwd and fails with `EACCES`), a `PT_LOAD` whose file range runs past EOF (Linux maps it, zeros
  after EOF) and a `PT_LOAD` ending above 2^48 (Linux commits and kills with `SIGSEGV`); an
  interpreter that is not an aarch64 ELF is `ENOEXEC` where Linux returns `ELIBBAD`. Only
  hand-made ELFs and unusual linker-script output are affected (gVisor's `exec_binary_test` builds
  its ELFs with the headers outside the only `PT_LOAD`, so most of it fails here).

## Open questions

- Is the missing ASLR (fixed program, interpreter and vDSO bases) deliberate?
- Is it intended that Android always shuts down as poweroff (runlevel 6 entries never run)?
- Should `sched_yield` yield the host thread?

## To verify

- The Linux 6.1 comparisons above that rest on kernel behaviour rather than repository source
  (`eligible_child` and `copy_process` exit signals, `deny_write_access` on scripts and
  interpreters, argument limits under secure exec, `send_sigqueue` for dead threads,
  `new_idmap_permitted`, `zap_pid_ns_processes`, `load_elf_binary` without `PT_LOAD`, the keep and
  uclamp flags of `sched_setattr`, `INIT_RLIMITS`) were checked from knowledge of the kernel, not
  against its source; the items found by Starnix's syscall tests were also observed on a native
  6.8 kernel.
