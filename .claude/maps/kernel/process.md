# kernel/process Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/process.md` for the paired decision history (it still holds the decisions on execve, namespaces and futexes made before they were split out into `kernel/exec`, `kernel/ns` and `kernel/futex`).
>
> Verified: 2026-11-10 (full mapping: 6 research concerns; 18 claims checked by 3 independent verifiers — 10 confirmed and 8 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping: `bug.rs`, `syscall/mod.rs` and the rest of `syscall/misc.rs` registered here, the watchdog's kernel log record, failed host spawns, fast-path deadline hand-overs, then execve, namespaces and futexes split out)

## Responsibilities

`kernel/process` runs guest threads and processes inside the app's single host process:

- guest tasks (threads) and thread groups (processes): creation (`clone`/`clone3`), the run loop,
  exit, reaping and `wait`, process groups and sessions, pidfds;
- the big kernel lock (BKL) that serialises the kernel layer, its borrow checks, the syscall
  dispatcher's hooks, the no-BKL fast path, and the watchdog that reports holds of it lasting too
  long;
- the unified wait and wake-up mechanism;
- the timer thread (interval and POSIX timers, CPU-time limits, periodic checks);
- the built-in virtual init (PID 1) and inittab boot/shutdown, and the host-facing `Machine`;
- scheduling parameters applied to host threads;
- kernel-layer bugs: a panic or host fault ends only the guest process that hit it, with a kill
  report for the app, and test hooks plant such bugs;
- the other process-wide calls in `syscall/misc.rs` (clocks and CPU times, resource use and
  limits, host and domain names, reboot, sysinfo, getcpu, getrandom).

Root: `core/kernel/src/task.rs`, `process.rs`, `kernel.rs` (`Kernel`, `Shared`, `Bkl`, `Cx`, the
`wake` module), `cell.rs`, `syscall/proc.rs` (except its execve and unshare sections),
`syscall/fast.rs`, `machine.rs` (timer thread, boot/shutdown, host-facing calls), `watchdog.rs`,
`itimer.rs`, `syscall/posix_timer.rs`, `loadavg.rs`, `inittab.rs`, `syscall/sched.rs`, `bug.rs`, the
dispatcher in `syscall/mod.rs` (its restart and unimplemented-syscall handling are
`kernel/signal`'s), and `syscall/misc.rs` except the handlers `kernel/signal` lists there (uname,
syslog, adjtimex, clock setting, restart_syscall), the futex and robust-list handlers and the
time-namespace offsets. Of the rest of `syscall/misc.rs`, membarrier is described in `kernel/mmu`,
umask in `kernel/vfs`, and the capability and seccomp prctls in `kernel/cred`.

Outside the module: execve and the ELF loader (`kernel/exec`), namespaces, unshare, setns and ID
translation (`kernel/ns`), futexes and robust lists (`kernel/futex`), address spaces and fork
copy-on-write (`kernel/mm`), CPU state and translation (`engine`), signals and ptrace, credentials,
mount trees (`kernel/mount`), network namespaces (`kernel/netns`), cgroups and procfs files
(`kernel/procfs`), the OOM policy.

## Key types

### Tasks and processes

- `Task` — one guest thread: tid (a `Cell`, since a non-leader's execve takes over the tgid), its
  own credentials (`kernel/cred`), `killed`, `frozen`, `pdeathsig`, exit status and whether its
  exit started, `vfork` (its vfork completion is still pending, for the OOM killer), `syscall` (the
  BKL syscall it is in: its number in the global `SYSCALLS` count and the syscall number), CPU-time
  and context-switch bases and its host tid, nice, scheduling policy, deadline, uclamp, timer slack,
  affinity, I/O priority, its wake `event`, `sleeping`, `pid_ns_children`, `NsProxy`; `TaskRes` (a
  copy of its address space, fd table and fs that other threads read, for procfs); thread-private
  `TaskState` (address space, fd table and `FsContext`, each shared or not per clone flags; thread
  group; alternate stack; `clear_child_tid`; vfork completion; restart block; `fast_sleep`).
- `TaskSync` — lock-free `in_guest` flag and the engine `Kicker` (used by `kernel/mmu` shootdowns).
- `ThreadGroup` — one process: tgid, ppid, `parent_thread` (the creating thread, Linux
  `real_parent`), pgid, sid, live `threads` (leader first while alive), `children` (zombies
  included), `exiting` / `leader_status` / `zombie` / `leader_exit`, `exit_signal`, own and
  children's CPU time and max RSS, `child_subreaper`, rlimits, the main thread's credentials (what
  other processes go by, kept after it ends), pid namespace, cgroup, tracers of its exit, AIO,
  controlling terminal and `tty_old_pgrp`, timers.
- Default rlimits: stack 8 MB (hard unlimited), core 0, open files 1024/4096, memlock 8 MB, nproc
  half of threads-max, pending signals 16384, message queues 819200 (not enforced), nice and
  real-time priority 0, the rest unlimited.
- `ExitStatus` — exited with a code or killed by a signal (with pc, fault address, core dump);
  encoded for wait.
- `CpuTime` — total nanoseconds from the host thread CPU clock and system nanoseconds from
  `getrusage` (the calling thread) or the host's `/proc` (another thread), each kept monotonic.
- `task::pool` — pooled host threads (at most 8 idle, kept 5 s; work handed out under the BKL
  starts after it is released).
- `Kernel` — the global state behind the BKL: tasks, groups, waiters, pid allocator, namespaces,
  init state, power requests, watched processes.
- `Shared` — the BKL mutex, its holder record (`Hold`) and spinner counter, the latest stall, the
  fast-path epoch, the futex lock and queue, the timer thread's event, DNS answers waiting for it,
  the periodic checks' idle flag.
- `Hold` — one cache line written by each holder on taking the BKL: the `held` hint, an
  acquisition count, the holder's host tid, and its guest thread's `Entry` (tid, tgid, comm, what
  it entered the kernel for as `Doing`, pc); `HoldSample` — one lock-free read of it.
- `watchdog::Stall` — a hold reported as too long: id, seconds, the report, the kernel log record
  once it ended.
- `Bkl` — the held-lock guard; its `Drop` performs release-time work.
- `Cx` — the context of one syscall or signal delivery; holds the `Bkl` (absent while released).

### Waiting and the fast path

- `wake` channels: `FILE`, `OTHER`, `CHILD`, `WATCHED`, `EXITED`, `LOCK`, `SIGNAL` (plus the
  internal `OBJECTS`); `wake::Object` is the address of a value (inside a `RefCell`, or a plain
  counter).
- Waiter — (event, interest channels, optional sorted object list), stored in `Kernel::waiters`.
- `fast::Fast` — a thread's fast-path record (epoch, tracing, resolved root and cwd, tid, CPU-clock
  base, address-space id and its shared writable mappings, vDSO address, time-namespace offsets).

### Timers and init

- `itimer::ITimer` (`ITIMER_REAL`, a monotonic instant), `CpuTimer`, `PosixTimer`;
  `itimer::Expiry` — a wall-clock deadline on Linux's hrtimer base: `Mono(Instant)` (monotonic,
  absolute times converted; relative realtime; relative times of other clocks but boottime and
  TAI) or `On(host clock, time)` (absolute realtime, boottime and TAI either way; the `_ALARM`
  clocks as realtime and boottime), compared with that clock whenever looked at; the global
  wall-clock jump count (`clock_epoch`, shared by every kernel in the host process);
  `loadavg::LoadAvg`; `inittab::Entry`, `Power`.

## Public entry points

- **Syscalls**: `clone` (220), `clone3`, `exit`, `exit_group`, `set_tid_address`, `wait4`, `waitid`,
  `nanosleep`/`clock_nanosleep`, `setitimer`/`getitimer`, `timer_*`, `getpriority`/`setpriority`,
  `ioprio_*`, `sched_*`, `setsid`/`setpgid`/`getpgid`/`getsid`, `pidfd_open`, `pidfd_getfd`, `kcmp`,
  `prctl` (`PR_SET/GET_PDEATHSIG`, `PR_SET/GET_CHILD_SUBREAPER`), `kill` to pid 1 (power requests),
  `clock_gettime`/`clock_getres`/ `gettimeofday`, `times`, `getrusage`, `prlimit`, `reboot`,
  `personality`, `sethostname`/ `setdomainname`, `getcpu`, `sysinfo`, `getrandom`, and the test
  syscall 0xa991 (debug and `test-hooks` builds).
- **Kernel bugs**: `bug::catch`, `host_call`, `kill`/`kill_process`, `record_kill` (also the OOM
  killer's), `report`, `install_panic_hook`, `trap`/`trap_finish`; `Machine::take_kills`.
- **Kernel-internal**: `task::start`, `thread_main` → `run` → `exit`, `process::finish_group`,
  `Shared::lock`, `block`/`block_for`, `Cx::wait*`, `Cx::unlocked`, `request_wake*`,
  `signal_later`/`flush_signals`, `Shared::fast_changed`, `kernel::doing`/`done`.
- **Host-facing** (`Machine`): `start` (creates PID 1, the timer thread and the BKL watchdog; a
  panic is an internal error), `stall` (the latest too-long hold; takes no BKL), `spawn`, `boot`,
  `shutdown`, `kill_all`, `take_power`, `processes`, `wait`/`try_wait`/`wait_or_power`, `signal`
  (a host signal to a guest process, `SI_KERNEL`); `run` (`appine-run`), which with
  `RunOptions::stop` passes the first stop signal the host got on to the program and ends every
  process at one arriving at least a second later, then finishes as usual.
- **procfs**: `threads-max`, `/proc/<pid>/task/<tid>/children`.

## Data flow / lifecycle

### Tasks and host threads

- Each guest thread runs on its own host pthread, scheduled by the host kernel. A finished thread's
  host thread is pooled (at most 8 idle, 5 s) and its engine CPU state goes to the engine's spare
  list (up to 8), so creating threads rarely creates host threads.
- A thread starting on a host thread records the host CPU-clock, system-time and context-switch
  bases and its host tid, and re-applies its nice and I/O priority, and its affinity when that is
  restricted or the host thread was reused.

### clone

- Global ids come from `alloc_tid`: the numbers after the last one, up to `PID_MAX - 1`, then from
  300 again (Linux's `RESERVED_PIDS`); `PID_MAX` itself is never handed out, and `ns_last_pid`
  writes move the start (the global one at level 0, else the namespace's own).
- `clone` takes the low 32 bits of the flags (`CLONE_PIDFD` with `CLONE_PARENT_SETTID` is
  `EINVAL`, as the pidfd shares that slot); `clone3` refuses a size below 64 (`EINVAL`), above a
  page or with a non-zero tail (`E2BIG`), unknown flags, `CLONE_DETACHED`, exit signals above 64 or
  with `CLONE_THREAD`/`CLONE_PARENT`, a stack without a size, and bad `set_tid` or cgroup
  arguments. There are no separate fork or vfork syscalls on aarch64.
- Invalid combinations are `EINVAL` (a thread without shared signal handlers, shared handlers
  without shared memory, `CLONE_PIDFD` with a thread or `CLONE_DETACHED`, `CLONE_PARENT` from a
  pid-namespace init, a thread with `CLONE_NEWPID` or another pid namespace for children,
  `CLONE_NEWIPC` with `CLONE_SYSVSEM`, `CLONE_NEWNS` or `CLONE_NEWUSER` with a shared fs,
  `CLONE_NEWUSER` with a thread); a thread put into another cgroup is `EOPNOTSUPP`.
- Limits: `RLIMIT_NPROC` counts live threads per kernel real uid across all user namespaces (not
  for real uid 0 or with `CAP_SYS_RESOURCE`/`CAP_SYS_ADMIN`); `threads-max` applies to root too
  (`EAGAIN`) and counts live tasks only (default: physical memory in MB, at most half the host fd
  limit, at least 20, 4096 if memory is unknown; writable in 20..=0x3fffffff); the cgroup
  `pids.max` and freezer are checked; then a pid is chosen. Host-launched processes
  (`Machine::spawn`) are a trusted path and skip these limits.
- The child's address space is shared (`CLONE_VM`) or quiesced and forked (`kernel/mm`); fds and fs
  are shared or copied. A new process gets `ppid` and `parent_thread` from the caller (or the
  caller's parent with `CLONE_PARENT`, which also gives it the caller's process's exit signal, as
  Linux's `copy_process` does). A fresh CPU state copies the parent's registers (x0 = 0), the new
  stack and TLS; a `CLONE_VM` thread that is not a vfork child gets no alternate stack. The task
  inherits the creator's signal mask, comm, credentials, scheduling, seccomp and ptrace options.
- Registration: the pid is registered (in every pid-namespace level), the parent/child tid and
  pidfd are written, then `task::start` queues the host thread and inserts the task into the group,
  the address space's users and the kernel's task table (the thread runs once the BKL is
  released); a failure releases the pid and the pidfd. Then the group is registered with its
  parent, the ptrace fork event reported and, for vfork, the parent waits (interest none) until the
  child execs or exits, or it is killed; until the completion is signalled (`release_mm`, at
  execve or exit) the child's `Task::vfork` is set, so the OOM killer can pass it over
  (`kernel/mm`). The waiting parent counts as frozen for `cgroup.events` (`Task::frozen`, as
  Linux's `wait_for_vfork_done`), and stays counted after the wait while its cgroup is freezing,
  until it stops on the way out (`kernel/procfs`).

### Run loop

- Each pass: let the periodic checks resume, mark the thread in guest code, act on generation
  changes (a new code space re-attaches the translator, a TLB generation flushes the TLB, a code
  generation invalidates translations), then run: `Cpu::step` while single-stepping, the
  translator when attached and no hardware breakpoints are set, else `Cpu::run`.
- An `Svc` exit first tries the fast path (below) without the BKL; everything else takes the BKL
  and is handled: a killed thread exits; code invalidations are applied; syscalls are dispatched;
  kicks deliver signals; undefined instructions raise `SIGILL`, breakpoints and watchpoints
  `SIGTRAP`, alignment faults `SIGBUS`, host faults kill the process; page faults go to
  `kernel/mm` (copy-on-write break, already present, fault in, FUSE fill, past-EOF `SIGBUS` or
  remap, stack growth, else `SIGSEGV`). A panic on either path kills the process and the thread
  exits.
- The result (`Continue`, `Exec`, `Exit`) decides the next step; `Exec` re-attaches the translator
  for the new address space. After each BKL-path handling the fast-path record is rebuilt if the
  epoch moved. On `Exit`, still under the BKL, the last thread using its fd table flushes the FUSE
  files in it (`kernel/fuse`) before `task::exit` drops the table.

### Syscall dispatch

- Under the BKL: the ptrace entry stop and seccomp, pid arguments translated into global pids
  (invisible ones become "no pid"), a fresh FUSE token, the buffers of read, write, pread64,
  pwrite64 and the vector forms read into FUSE mappings (`kernel/fuse`'s `prefault`, since those
  consume input before copying), then the handler. A handler failing with the internal
  `FUSE_RETRY` (a walk needed a FUSE daemon's reply, or a copy met a page of a FUSE mapping not yet
  read, `kernel/mm`'s `Fill`) makes the dispatcher ask for it with the BKL released and run the
  handler again from the start, at most 16384 times (then `EIO`); a page that cannot be read ends
  the syscall with `EFAULT` (`ERESTARTSYS` when a signal took the read back). ptrace's stops and
  seccomp see one syscall.
- From the token until the sweeps the thread records the syscall in `Task::syscall`
  (`InSyscall`, numbered by the global `SYSCALLS` count, cleared on every way out), so the overlay
  sweep can tell which syscalls began before a removal.
- After the handler: released FUSE files are flushed, the FUSE node cache may be swept
  (`kernel/fuse`'s `sweep`), detached mounts whose last use the syscall let go of are collected
  and removed overlay nodes swept (`kernel/overlay`), and watched cgroup2 files compared for changes
  (`cgroup::notify_changes`, also run at thread exit, freezer stops and by the timer thread); then
  results are translated out (invisible pids become 0), edge-triggered readiness is re-armed after
  reads, x0 is set, `Mm::sync` runs, then the exit stop and signal delivery. `exit`, `exit_group`,
  `rt_sigreturn` and a successful `execve` return before the sweeps and `Mm::sync`.
- A guest page fault on a FUSE mapping's page not yet read reads it with the BKL released before
  re-executing (`kernel/mm`'s fault order).

### Kernel bugs and kill reports

- Containment (`bug::contain`) is on with `APPINE_ON_BUG=kill`, off with `abort`, and otherwise on
  in release builds and off in debug builds. `catch` runs kernel code under `catch_unwind`; with
  containment off a panic aborts the host process (`appine: internal error, aborting`).
- Where a panic is caught and what it ends: every entry of a guest thread into the kernel, thread
  start and each step of thread exit end that thread's process; each step of process finish is
  only reported and the next step goes on; the timer thread's handling of one process's timers
  ends that process, while a panic in the memory-limit check or signal-driven I/O turns that
  feature off; `host_call` around `Machine` methods and the library's other public functions makes
  the call return an error (a console's call also hangs up that console). Taking the BKL after a
  caught panic ignores its poison flag (`Shared::lock`). An engine host fault (`Exit::HostFault`)
  ends the process the same way.
- `kill` (a thread's own bug) and `kill_process` (the timer thread's) write one report line to
  stderr (pid, comm, the thread and its last guest pc for `kill`, what failed, and the panic's
  message and location) and to the kernel log at `KERN_ERR`, one record per line, record a kill
  report, and end the whole thread group with `SIGSEGV` (no core dump). The panic hook
  (`install_panic_hook`, once per host process, chained to the previous hook) keeps each thread's
  latest panic message (up to 1000 bytes) and location for that report.
- Kill reports (`Kernel::kills`, at most 32, the oldest dropped) carry the pid, comm, the console
  number when the process's controlling terminal is one of the app's consoles, and the reason:
  `Bug` (the report, the 10 kernel log records before it plus itself, and the most-reached
  unimplemented paths from `stub::top`) or `OutOfMemory` (from the OOM killer, `kernel/oom`). The
  host takes them with `Machine::take_kills` (`android/service`).
- Test hooks exist only in debug builds and with the `test-hooks` feature, and answer `ENOSYS`
  unless `APPINE_TEST_HOOKS=1`. Syscall 0xa991 with `what` 1 to 13 panics at once, points a page
  at an inaccessible host page so the engine faults on the host, or plants a panic that fires once
  (`trap`) at the thread's exit, the next thread start, the next timer handling, the next host
  call, a step of process finish, right after releasing the BKL, the next no-BKL syscall or a futex
  wait; it can also act as if the host's wall clock jumped, hold the BKL for `addr`
  milliseconds for the watchdog's tests, or make the next host thread for a guest thread fail to
  be created (`fails`: `task::start` returns `EAGAIN` for a clone or a host spawn). Any other
  `what` is `EINVAL`.

### Exit and reaping

- `exit` sets the thread's status (the leader's also records `leader_status`); `exit_group` and
  fatal signals set the group's `exiting` status once, mark every thread killed and wake them.
  Before any `PTRACE_EVENT_EXIT`, `begin_exit` (idempotent) makes the last thread to start exiting
  fill in the group status (Linux 6.1 `synchronize_group_exit`).
- `task::exit` runs in stages, each contained by `bug::catch`: core-dump registers, statistics and
  CPU time, the leader's `leader_exit`, sample RSS if last, release the address space (robust list,
  `clear_child_tid` wake when the address space is shared, vfork completion, leave its users),
  ptrace and seccomp cleanup; when that stage panics, vfork completion and leaving the users run
  again on their own (both are idempotent) before the process is killed, so no stale user is
  kicked and no vfork parent waits; remove the task and its pid, hand PI locks it owned with
  waiters to their first waiters (`futex::pi_owners_exited`), and reap SysV shm segments marked for
  removal whose last attachment that was (`kernel/ipc`); a non-last thread hands its children to
  another live thread (sending their pdeath signals) and retargets pending group signals, the last
  thread finishes the group; flush message-queue notifications, reset its namespaces and drop
  references under the BKL; flush released FUSE files; collect unix sockets in flight (last
  thread), IPC namespaces and the mounts of dead mount namespaces or detached mounts the thread was
  the last user of; compare watched cgroup files; request an `EXITED` wake. The engine CPU state is
  freed after the BKL is released.
- `finish_group`, each step contained: release AIO; reparent children to the nearest
  `PR_SET_CHILD_SUBREAPER` ancestor in the same pid-namespace level that still has live threads
  (one in the middle of exiting can be chosen, as on Linux), else the pid-namespace init while it
  has not exited and the namespace is not dead, else the virtual init (children get `exit_signal =
  SIGCHLD`; zombies handed to the virtual init are freed at once, others reported to their new
  parent, and zombies still held for a tracer only moved); send `PR_SET_PDEATHSIG` signals (keyed to the creating thread); release SysV IPC and,
  for a session leader, the controlling terminal (`kernel/tty`).
- When a pid-namespace init exits, the namespace is marked dead (registering a pid in it is then
  `ENOMEM`) and every other member gets `SIGKILL`; the init's own children, and the orphans other
  members leave while they die (unless a live subreaper in the level takes them), go to the
  virtual init, which frees them at once, as Linux's exiting init reaps them, and its exit is
  reported only once all other members are reaped (it stays registered meanwhile).
- Reporting: the status is `exiting` or the leader's; it is reported to a tracer or the parent (a
  process traced by another process is held as a zombie for the tracer and handed to its parent
  once the tracer takes it). Handed to the parent, the group is reaped immediately when the parent
  is the virtual init, gone, ignores `SIGCHLD` or has `SA_NOCLDWAIT`, whatever its exit signal;
  otherwise it becomes a zombie and the parent gets `exit_signal`. Orphaned process
  groups with a stopped member get SIGHUP/SIGCONT. A `CHILD` wake follows (`WATCHED` too for a
  process the host watches).
- `wait4`/`waitid` (option checks; `waitid` takes `P_PIDFD`, where a reaped process is `ECHILD`
  and a nonblocking pidfd without `WNOHANG` `EAGAIN`, and `WNOWAIT`) scan `children` (filtered by
  the target and `__WNOTHREAD`, and by exit signal as Linux's `eligible_child`: without `__WALL`,
  only children reporting with `SIGCHLD`, or with `__WCLONE` only the others; a child the caller
  traces always counts): a zombie is reaped (its CPU time and max RSS added to the waiter's
  children totals; one whose exit a tracer has yet to take counts for any wait but is not shown),
  then stop and
  continue reports, then tracees; nothing matching is `ECHILD`, and a zombie counts only for a wait
  that takes exits (so waiting for stops and continues of children that have all exited is
  `ECHILD`); otherwise the waiter blocks on `CHILD` and restarts if interrupted.
- An exited leader keeps its tgid registered until the group is reaped. Its task leaves the task
  table like any thread, but the group records its name and own CPU time (`leader_exit`, cleared
  when a non-main thread's execve takes the tgid), and procfs shows it as a zombie thread until
  the group is reaped, as Linux keeps the leader as a zombie task.

### The BKL

- One mutex over `Kernel`. Kernel objects are shared with `Rc` and the kernel's own `RefCell`; their
  counts and borrow flags are not atomic, so even cloning or dropping them needs the BKL.
- Held: for every guest exit except fast-path syscalls (syscalls, faults, kicks, signal
  delivery), by the timer thread while it works, and by host-facing `Machine` calls. Not held while
  running guest code.
- Released inside a syscall only by the wait primitives and `Cx::unlocked`, whose closure must be
  `Send` and so cannot touch kernel objects. Uses of `unlocked`: host fd and host stdio I/O,
  regular-file reads and writes other than `O_APPEND` writes (calls without an explicit offset
  one per open file at a time, like `f_pos_lock`), fsync, sync and syncfs (the store's WAL wait
  included) and `O_SYNC`/`O_DSYNC` write syncs, `sync_file_range`, `MS_SYNC` msync,
  copy_file_range/sendfile copies between regular files, giving a held store file an object of
  its own, the execve image build, `sched_yield` when the fast path cannot take it, and a test
  hook. A guard re-takes the BKL
  on return and on unwinding.
- Acquisition: try once; otherwise up to half the host CPUs (1 to 4) spin for at most about 30 µs,
  reading only the `held` flag; the rest sleep on the mutex (a poisoned one is accepted). The new
  holder sets `held`, records itself in `Hold` (relaxed stores on the same cache line, no clock
  read), marks the thread as holding this kernel's BKL and starts deferring host unmaps
  (`kernel/mm`).
- Release (`Drop for Bkl`), in order: hang up closed terminals (contained), match pending wake
  requests against waiters, clear `held`, unlock, clear the thread's held marker, notify pooled
  host threads, signal the matched events, release deferred host memory and refill fork spares
  (`kernel/mm`).
- Waits outside a syscall context (the timer thread, `Machine::wait_*`, the terminal bridge) call
  `block`/`block_for` directly; they hold no kernel objects while the BKL is released.
- Borrow checks: the kernel's `RefCell` counts borrows per thread; every `block_for` (also the
  timer thread's and host waits) and `Cx::unlocked` assert the count is zero. Always on in debug
  builds; in release builds with the `borrow-check` feature (enabled for `appine-run` and the debug
  app). The inode store also checks that its owning kernel's BKL is held.

### Fast path (`syscall::fast`)

- Without the BKL, a thread can run `futex` (WAIT, WAKE and the BITSET forms with a non-zero
  bitset, `FUTEX_CLOCK_REALTIME` only with WAIT_BITSET, on a 4-byte-aligned address that
  translates writable and is not in a shared mapping that may be written), `nanosleep`,
  `clock_nanosleep` (REALTIME, MONOTONIC, BOOTTIME), `clock_gettime` (host clocks, with the time
  namespace's offsets, or its own CPU clock), `clock_getres`, `gettimeofday`, `sched_yield`
  (yields the host thread, not counted as running guest code meanwhile), and `newfstatat`/`statx`
  (absolute or `AT_FDCWD`-relative paths, when the root is the root mount's store directory, the
  root mount is usable, the walk stays in the store's metadata cache and crosses no other mount,
  and the credentials can skip search-permission checks).
- It is off while strace-tracing, under `PTRACE_SYSCALL`, single-stepping, or with a pending
  `ivau_page`; with seccomp only for numbers the filter always allows, and for the vDSO stubs'
  reads of clocks Linux's vDSO reads in user space (`kernel/signal`).
- The per-thread record is built under the BKL and rebuilt when a global epoch moves, bumped by
  `fast_changed` (chdir, chroot, credential and fs changes, mounts, setns, execve, time-namespace
  entry); a call that cannot complete falls back to the BKL path (writes to guest memory it made
  stay). A panic on the fast path kills the process.
- Fast futex (`kernel/futex`): it shares the BKL path's queue and lock. A timed wait
  woken early (futex, `nanosleep`, `clock_nanosleep`; relative or absolute) hands its deadline over
  in `fast_sleep`: the BKL handler takes it instead of reading the time argument again (Linux's
  restart never reads it again either, and the memory may be gone), and a relative sleep's
  remaining time comes from it; the run loop clears it once the BKL path has dispatched that same
  call, so a deadline its handler never took (the call skipped or renumbered by seccomp or a
  tracer) is dropped. Wakes are signalled through the deferred list and flushed at once.

### Waiting and waking

- A guest thread never blocks in a host syscall: every wait is one host `poll()` on the thread's own
  event (an eventfd) plus the host fds it waits on, with the deadline as timeout. Wall-clock
  deadlines (sleeps, futex and mqueue timeouts, timers) are `Expiry` values turned into an instant
  at each wait and checked with their own clock, so a wait woken after the realtime or boottime
  clock jumped (the host's clock set, or the device resuming) waits again for the right time;
  relative `CLOCK_BOOTTIME` sleeps count the time the device slept.
- `block_for`: register the waiter (unless its interest is empty), release the BKL, poll, re-take
  the BKL, clear the event, deregister. Host `EINTR` counts as a wake. Callers re-check their
  condition, deadline and interruption.
- Timeouts and timer values read from the guest (every `timespec` reader, `itimerval`) saturate at
  `KTIME_MAX` (about 292 years) as Linux's `timespec64_to_ktime` does, so GNU `sleep infinity`
  waits until interrupted and timers report the saturated time left; a socket timeout of
  `KTIME_MAX` or more means none.
- Wake requests made under the BKL are recorded per thread (several kernel instances may share the
  host process) as channels and objects; requests made without the BKL go to a global fallback.
  At BKL release, a waiter fires if its interest shares a channel with the request, or if objects
  changed and it is `FILE`-interested with no object list or a matching object. Matched events are
  signalled after unlocking, so woken threads do not immediately sleep on the BKL (a wall-clock
  jump signals every task's event directly, and the fast path's sleeps clear their own).
- Objects: waiters register what they wait on (pipes, PTYs, eventfd, timerfd, inotify, signalfd,
  epoll, socket queues, message queues, seccomp notifications, kernel log, FUSE connections, host
  consoles); poll/select/epoll register each watched file's objects, and AIO's io_getevents its
  context and its pending polls' files; a wait on host fds alone registers an empty list. SysV IPC,
  SIGCONT of a stopped group, a host terminal closing and machine teardown still wake everyone.
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

### Timer thread

- One host thread ("timers") loops, holding the BKL while it works, each step but the clock check
  contained (interval timers per process): if its host `ClockWatch` reports a wall-clock jump, run `clock_jumped` (bump the jump count, signal
  every task's event: BKL waiters, futex waiters and no-BKL sleepers alike look at their deadlines
  again); expire interval timers, POSIX timers and CPU-time limits; run the OOM check (only with a
  memory limit or a cgroup `memory.max`), compare watched cgroup files, the translator code-view
  release, load-average sampling, deferred store commits, `O_ASYNC` and `EPOLLEXCLUSIVE` checks,
  AIO polls that signal eventfds (`kernel/aio`), DNS stub answers (`kernel/net`) and raw ICMP taps;
  then wait (BKL released) until the earliest deadline (timerfds of those AIO polls and of
  exclusive gates included), its event or the host fds of those checks and the clock watch. It
  listens on `OTHER` (or all channels while `O_ASYNC` files, exclusive gates or such AIO polls
  exist). A panic in a step turns that feature off for the kernel's life (the memory limit, `O_ASYNC`
  signals, exclusive gates, AIO polls), or kills only the process whose timers it was handling.
- Setting a timer writes the deadline into the thread group and signals the timer thread's event.
  POSIX timers on wall clocks keep an `Expiry`, so absolute realtime and boottime timers fire by
  their own clock; the watch's fd is in the timer thread's poll.
- `ITIMER_REAL` is a monotonic instant (wall-clock jumps leave it); `ITIMER_VIRTUAL`/`PROF` and
  process CPU-time timers use total host thread CPU time and wake a running thread. Armed CPU
  timers or a finite `RLIMIT_CPU` make it tick every 10 ms. Over the soft `RLIMIT_CPU` (whole
  seconds) sends `SIGXCPU` and raises the soft limit by 1 s; over the hard limit `SIGKILL`.
- POSIX timers: the `_ALARM` clocks need `CAP_WAKE_ALARM`, CPU-clock ids of other threads and
  processes are accepted, at most 4096 per process with the lowest free id; without a `sigevent`
  the signal is `SIGALRM` with the id as value; a `SIGEV_THREAD_ID` target must be in the process.
  A timer whose previous signal is still pending counts an overrun instead (`timer_getoverrun`
  reports the count when the last signal was dequeued). A `SIGEV_THREAD_ID` timer signals its
  thread only while that thread lives in the process; after it exits the signal is dropped, as on
  Linux.
- When every thread is waiting, the periodic checks (OOM, code views, load average) stop until a
  thread next enters guest code; armed timers still wake it. Stopping sets `checks_idle` and then
  looks at the threads again (`Shared::stop_checks`), while a waking thread clears its `sleeping`
  flag before reading `checks_idle` (`resume_checks`), all SeqCst, so a thread waking in between
  either wakes the timer thread or is seen running. Changing `RLIMIT_CPU` does not wake the timer
  thread; the new limit takes effect at its next pass.

### BKL watchdog

- A guest thread records what it is in the kernel for in a thread-local `Entry` (`kernel::doing`):
  starting, a syscall number (low 56 bits of x8), a memory fault, a kick, another exception, or
  ending; with its tid, tgid, comm and pc at that entry. It is set before each BKL-path entry (and
  before the fast path's panic kill), kept across the BKL re-takes inside that entry, and cleared
  when the host thread finishes the guest thread. Threads running no guest thread (the timer
  thread, host calls, the watchdog) record none.
- `Machine::start` starts a host thread "bkl-watchdog" that never takes the BKL to look: every
  fifth of the threshold (10 s; `APPINE_BKL_WATCHDOG` in seconds, 0 turns it off) it samples
  `Hold`; the same sample (count, host tid, entry) seen for the threshold is one hold that lasted
  too long. A sample torn by a new holder differs and only looks like another hold.
- On reaching the threshold, without the BKL: one line on stderr naming the guest process (pid and
  comm), thread, what it entered for and its pc, or else the host thread, plus the holder's host
  thread name and activity (running, in host syscall n, or its scheduler state); the same line
  becomes the latest `Stall` (ids count from 1 per kernel; seconds updated at each look).
- When a later look sees the lock free or another hold: at once a line naming the holder and the
  total seconds on stderr and in the `Stall`, and the same line as a KERN_ERR kernel log record,
  which a short-lived thread (`bkl-watchdog-log`) writes once it gets the BKL, so the watchdog
  goes on looking, and reports the next hold, while that holder keeps it (a panic writing it is
  only reported; when the thread cannot start the record is lost). Durations count from the first
  look, so they are right to within one look. Nothing is ended.
- `Machine::stall` returns the latest `Stall` without the BKL; the watchdog is stopped and joined
  last in `Machine`'s drop.

### PID 1

- The virtual init is a thread group with no threads and no guest program. Host-started processes
  (each terminal tab's login shell) are its children; it adopts orphans and its children are never
  zombies. `Machine::spawn` registers such a process (in the group table, init's children and the
  watched table) only once its host thread has started; when its task or thread cannot be
  created, it gives the console it attached back (`kernel/tty`) and leaves nothing registered.
- On request (the app always; `appine-run --init`) it boots from a subset of `/etc/inittab` (a
  store file; anything else is an empty table): `sysinit` (waited), then `boot` and `bootwait`
  together in file order (only `bootwait` waited), then `wait` (waited) and `once` (not waited) for
  the default runlevel (`initdefault`, else 2). Commands run through `/bin/sh -c` with stdin from
  `/dev/null` and output appended to `/var/log/boot.log` (the first command that runs truncates
  it, and the log is emptied when none ran; `/dev/null` if `/var/log` is missing); entries whose
  absolute program path does not exist are skipped; `respawn` and `askfirst` never run; a shutdown
  stops a boot in progress.
- Shutdown: after a boot, run `shutdown` entries and `wait`/`once` entries that list the requested
  runlevel (0 for halt and poweroff, 6 for reboot) in order, all within one caller-supplied
  deadline (30 s from the hosts; entries not started by then are skipped); then, always, SIGTERM
  everything, wait up to 1 s, and kill the rest. The Android host always requests poweroff.
- From the start of boot, `kill(1, SIGTERM|SIGUSR1|SIGUSR2)` by a sender allowed to signal root is
  a reboot, halt or poweroff request handed to the host (`Machine::take_power`; only the first is
  kept until taken); senders not allowed get `EPERM`; other signals to pid 1 are ignored.
  `pidfd_send_signal` to it checks permission and delivers nothing, broadcasts skip it and
  `rt_sigqueueinfo` to it is `ESRCH`.

### Scheduling and TLS

- nice, CPU affinity and I/O priority are per thread (I/O priority shared under `CLONE_IO`) and
  applied to the host thread; raising priority floors at host nice -4 (never above Android's
  display threads); `SCHED_IDLE` maps to host nice 19; real-time policies are only recorded and
  always need `CAP_SYS_NICE` in the initial namespace (`RLIMIT_RTPRIO` is not consulted),
  `SCHED_DEADLINE` also the same owner; a real-time I/O class (needing `CAP_SYS_ADMIN` or
  `CAP_SYS_NICE`) becomes best-effort on the host. Reset-on-fork turns a real-time child into
  `SCHED_OTHER` with nice 0 and clamps others' nice to at least 0.
- `sched_setattr` reads structures of 48 to 4096 bytes (0 meaning 48; a larger one with non-zero
  bytes past 56 is `E2BIG` with the size written back) and takes Linux 6.1's flags:
  `SCHED_FLAG_KEEP_POLICY` keeps the current policy, `SCHED_FLAG_KEEP_PARAMS` changes neither
  parameters nor policy (only reset-on-fork and the clamps), `SCHED_RESET_ON_FORK` inside the
  policy is `EINVAL`, a nice value beyond `RLIMIT_NICE` is `EPERM` (`EACCES` from setpriority),
  and only `SCHED_OTHER` and `SCHED_BATCH` take the nice value (`SCHED_IDLE` keeps the thread's);
  the utilization clamps are checked against 1024 and, each given or current, against each other,
  and only recorded per thread (copied at clone, reset by `SCHED_RESET_ON_FORK`, read back by
  `sched_getattr`, which always reports the nice value); the deadline flags are accepted unused.
  Without `CAP_SYS_NICE`, leaving `SCHED_IDLE` needs the `RLIMIT_NICE` that allows the current
  nice value, and reset-on-fork cannot be cleared (`EPERM`, for sched_setscheduler too).
- Changing another user's thread: setpriority and sched_setaffinity need the same effective user
  ID (as the target's effective or real one) or `CAP_SYS_NICE` over the target's user namespace
  (`set_one_prio`, `sched_setaffinity`), so root of a container may change its processes';
  sched_setscheduler and sched_setattr need the same owner or `CAP_SYS_NICE` in the initial
  namespace (`__sched_setscheduler`), as does raising priority beyond `RLIMIT_NICE` (`can_nice`);
  ioprio_set needs the target's real uid to be the caller's effective or real one, or initial
  `CAP_SYS_NICE`. `PRIO_USER` translates its uid through the caller's user namespace and skips
  processes the caller cannot see; `KCMP_IO` compares the shared I/O contexts.
- Guest CPUs are the host's allowed CPUs (up to 64); `sched_yield` yields the host thread, on the
  fast path or, while tracing or with a seccomp filter to run, with the BKL released.
- The guest's `tpidr_el0` is a field of the CPU state; the host `TPIDR_EL0` is never changed, so
  ART's signal chain always sees bionic's value.

### Other calls in `syscall/misc.rs`

- `sethostname`/`setdomainname` change the caller's UTS namespace: `CAP_SYS_ADMIN` over its owner
  (`EPERM`), then a negative length or one over 64 is `EINVAL`, then the name is copied in.
- `getcpu` reports the lowest CPU in the thread's affinity mask (not where its host thread runs)
  and node 0, skipping null pointers.
- `getrandom`: flags other than `GRND_NONBLOCK`, `GRND_RANDOM` and `GRND_INSECURE`, or `RANDOM`
  with `INSECURE`, are `EINVAL`; the bytes come from the host (`fill_random`) 64 KiB at a time,
  never blocking, and a fault in a later piece returns the bytes of the pieces before it.

## Dependencies (inbound / outbound)

### Inbound

- Syscall dispatch (`syscall/mod.rs`), `Machine` and the hosts (CLI, Android JNI: boot, shutdown,
  power, spawn, stall), signals and ptrace (group exit, delivery, exit hooks), procfs, every
  blocking syscall (via the wait API), `kernel/mmu` (kicks through `TaskSync`, `quiesce`),
  `kernel/mount` (`fast_changed`), `kernel/exec` (killing the other threads, the tgid takeover,
  vfork completion, POSIX timers), `kernel/ns` (thread groups, ptrace access for setns), the
  terminal bridge (`block_for`).

### Outbound

- `engine`: `Cpu::new`/`run`/`step`/`reset`/`translate`, `Dbt::attach`/`run`, `Kicker`,
  `guest_hwcap`, the spare CPU list.
- `kernel/mm`: `fork`, `translate`, `defer_unmaps`/`release_unmapped`/`spare::refill`;
  `kernel/mmu`: `quiesce`, users, `Mm::sync`, generations and code spaces in the run loop.
- `kernel/exec` (execve, `loader::load` for `Machine::spawn`), `kernel/ns` (namespaces at clone,
  pid translation at dispatch and wait), `kernel/futex` (the futex syscalls and their fast path,
  robust lists and PI owners at exit).
- vfs (resolution, store, the metadata cache's `Cached`), `kernel/mount` (mount flags),
  `kernel/fuse` (serving retries, prefault, sweeps, the exit flush), credentials, cgroups
  (charging, freezer, `notify_changes`), SysV IPC (`kernel/ipc`), AIO (`kernel/aio`), the OOM
  killer (`kernel/oom`), loadavg, the store's deferred commits, signals (`send_to_group`,
  `send_cpu_timer`, `force_fatal`), the kernel log.
- `appine_hal`: `poll`, `Event`, thread CPU clocks and system time, host thread
  nice/affinity/ioprio, host thread activity (watchdog reports), host CPUs, `ClockWatch`, random
  bytes; `std::thread`, `std::sync::Mutex`.

## Invariants and gotchas

- Lock order is BKL, then the futex lock; never the reverse.
- Kernel objects (`Rc`/`RefCell`) may be touched, cloned or dropped only under the BKL; a thread
  must not hold a kernel-cell borrow when it releases the BKL.
- Wake-ups are signalled only after the BKL is released, and events are level-triggered and cleared
  only after re-taking it, so every waiter must loop and re-check its condition. A wait with no
  interest is woken only by a direct signal (futex wakes, signals, clock jumps).
- A task leaves the kernel's task table and its address space's users under the BKL before its CPU
  state is freed; kicks rely on this.
- `begin_exit` is idempotent and runs before any `PTRACE_EVENT_EXIT`.
- The fast path must leave guest memory untouched after it stops being "in guest", because
  `Mm::sync` waits only for threads in guest code; the futex and clock fast paths do not check the
  epoch, relying on the thread that changed state being the one that refreshes.
- `Hold` is written on every BKL acquisition: anything added to it must stay on its cache line and
  cost a plain store, or every syscall pays. A thread that takes the BKL outside an entry after
  running guest code (the run loop's translator re-attach) is attributed to its previous entry.
- A finite soft `RLIMIT_CPU` alone keeps the timer thread ticking every 10 ms even while idle.
- A deadline is `Instant::now()` plus a guest duration, which panics on overflow: new readers of
  guest times must saturate through the shared `timespec` helper in `syscall/mod.rs`.
- A syscall handler may run several times for one syscall (FUSE retries): whatever it does before
  a walk that can fail with `FUSE_RETRY` is done again, so handlers change nothing before their
  last walk (`kernel/exec`'s execve changes no process state before killing the other threads).
  Copies to and from
  guest memory can fail with it too (FUSE mappings), which only the read/write buffers read in
  first make safe.
- Between syscalls no FUSE node is held by the finishing thread, which is when `sweep` may forget
  nodes; a node another thread holds must be found by `kernel/fuse`'s `in_use` scan, an open
  handle or a request in flight.
- Removed overlay nodes are kept for every syscall in progress that began before the removal, unless
  its number is in `kernel/overlay`'s `waits_on_files_only`: a syscall that waits with the BKL
  released while keeping a node it looked up must never be added there.

## Confirmed bugs / technical debt

- **POSIX timers on a CPU clock of a thread or process that is gone** never fire, yet keep the
  timer thread ticking every 10 ms while their owner lives, report their whole interval as time
  left, and follow a new task that reuses the number.
- **`sched_setattr` changes the policy before the nice check**, so a refused nice value can leave
  the policy (and reset-on-fork) changed.
- **Latent hazards:** matching wake-ups during BKL release runs outside `bug::catch` before the guard
  drops, so a panic there would leave the BKL locked; wake requests made without the BKL go to a
  global fallback that any kernel instance may consume.

## Open questions

- Should a parent's exit signal other than `SIGCHLD`, or a `SA_NOCLDWAIT` handler, change how a
  child is reaped and signalled (Linux reaps directly only for `SIGCHLD` and still signals the
  handler)?
- Is counting `RLIMIT_NPROC` by kernel uid across user namespaces, and never consulting
  `RLIMIT_RTPRIO`, intended?
- Is it intended that Android always shuts down as poweroff (runlevel 6 entries never run)?

## To verify

- The Linux 6.1 comparisons above that rest on kernel behaviour rather than repository source
  (`eligible_child`, `do_notify_parent` and `copy_process` exit signals, `send_sigqueue` for dead
  threads, `zap_pid_ns_processes`, the keep and uclamp flags of `sched_setattr`, `INIT_RLIMITS`)
  were checked from knowledge of the kernel, not against its source; the items found by Starnix's
  syscall tests were also observed on a native 6.8 kernel.
