# kernel/cred Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/cred.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 4 research concerns; 15 claims checked by 2 independent verifiers — 11 confirmed and 4 partial; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping: the vDSO seccomp bypass also covers the alarm clocks; new files' modes and owns; ptrace access credentials and ptracer credentials; seccomp notification and TSYNC pids; v3 file capabilities last)

## Responsibilities

`kernel/cred` implements Linux credentials for guest processes:

- the credential structure (four uids, four gids, supplementary groups, five capability sets,
  securebits, user namespace) and the id syscalls (`set*uid`, `set*gid`, `setfs*`, groups);
- capability checks in their different scopes, `capget`/`capset` and the capability prctls;
- the execve credential transition (setuid/setgid bits, file capabilities, ambient sets,
  no_new_privs and tracer suppression, `AT_SECURE`);
- access-decision helpers: DAC `permission`, ownership, sticky deletion, new-file ownership,
  faccessat credentials, signal permission, ptrace access;
- guest seccomp: strict mode, cBPF filters, actions, inheritance, TSYNC, user notifications and the
  always-allow cache.

Root: `core/kernel/src/cred.rs` (except user namespaces), `syscall/cred.rs`, `seccomp.rs`.

Outside the module: user namespace creation, id maps and namespace entry (`kernel/process`); the
syscalls that call the access helpers and their error ordering, xattr storage of
`security.capability` (`kernel/vfs`); signal delivery and ptrace stops (`kernel/signal`); procfs
views of credentials (`kernel/procfs`); the cgroup device eBPF (`kernel/procfs`).

## Key types

- `Cred` — `ruid`/`euid`/`suid`/`fsuid`, the four gids, sorted `groups`, `cap_effective`,
  `cap_permitted`, `cap_inheritable`, `cap_bounding`, `cap_ambient` (u64), `securebits`,
  `user_ns`. All ids are kernel (initial-namespace) ids. Kept per thread group
  (`ThreadGroup.cred`), alongside the group's `dumpable` and `no_new_privs`; umask lives in the fs
  context, not here.
- `cap` constants (`LAST` = 40, `CAP_FULL`), `secbit` (NOROOT, NO_SETUID_FIXUP, KEEP_CAPS,
  NO_CAP_AMBIENT_RAISE and their locks), `MAY_EXEC`/`MAY_WRITE`/`MAY_READ`.
- `FileCaps` — parsed `security.capability` (permitted, inheritable, effective).
- `Seccomp` — per-thread `mode` (disabled, strict, filter, dead) and `chain`; `Filter` (checked
  program, converted length, `allow` bitmap, original bytes, log and wait-killable flags, `prev`,
  user count, optional `Notif`); `Chain` (a thread's handle on its newest filter); `Notif`/
  `Knotif` (notification queue; states Init, Sent, Replied); `Listener` (the notify fd's object);
  `Verdict` (`Run(nr)`, `Skip`, `Exit`). `Task::seccomp_on` and `seccomp_allow` are lock-free copies
  for the fast path.

## Public entry points

- Id syscalls: `getuid`…`getresgid`, `setuid`, `setreuid`, `setresuid`, `setfsuid` and the gid
  forms, `getgroups`, `setgroups`; `capget`, `capset`; prctl `PR_*KEEPCAPS`, `PR_*SECUREBITS`,
  `PR_CAPBSET_READ/DROP`, `PR_CAP_AMBIENT`, `PR_*NO_NEW_PRIVS`.
- Checks: `Cred::capable` (initial namespace), `capable_here` (own namespace), `ns_capable`,
  `capable_wrt` (with respect to an inode's owner), `permission`, `owns`, `may_delete`,
  `new_owner`, `for_access`, `searches_all`, `in_group`, `may_signal`, `may_ptrace`; id
  translation `uid_in`/`uid_out`/`gid_in`/`gid_out`.
- Exec: `Cred::exec` with `FileCaps` from the program file.
- Seccomp: `seccomp(2)` (277), prctl `PR_SET/GET_SECCOMP`, `seccomp::check` (syscall entry),
  `always_allowed` (fast path), the notify-fd ioctls, `get_filter`/`get_metadata` (ptrace),
  `actions_avail`/`actions_logged` sysctls.

## Data flow / lifecycle

### Credentials and id syscalls

- The first process gets root credentials (all ids 0, permitted/effective/bounding full) in the
  initial user namespace. Readers take a cheap by-value snapshot; fork copies the group's
  credentials (moved into a new user namespace with `CLONE_NEWUSER`); all threads of a group share
  one credential, so even raw per-thread id syscalls affect every thread.
- Set syscalls map guest ids through the caller's user namespace (`-1` means unchanged; unmapped
  `EINVAL`), apply Linux 6.1's rules with `capable_here` (`CAP_SETUID`/`CAP_SETGID` in the caller's
  own namespace), and commit through `update`, which — like `commit_creds` — clears dumpable and
  every thread's pdeathsig when effective or filesystem ids change or permitted grows, and
  refreshes the fast path. `unshare(CLONE_NEWUSER)` and setns into a user namespace replace the
  credential directly, without these side effects (correct for unshare, as in Linux).
- Capability fixups on uid changes (`cap_emulate_setxuid`, root meaning uid 0 of the credential's
  namespace): leaving root on all three uids clears permitted and effective unless `KEEP_CAPS`
  and always clears ambient; euid root → non-root clears effective, non-root → root sets
  effective = permitted; `setfsuid` drops or raises the filesystem capabilities;
  `SECBIT_NO_SETUID_FIXUP` disables all of it.
- `setfsuid`/`setfsgid` never fail: bad values are ignored and the old value is returned.
  `getgroups` (negative size `EINVAL`, 0 returns the count, too small `EINVAL`); `setgroups` (size
  0..65536, mapped ids, then `CAP_SETGID` and the namespace's setgroups permission with a written
  gid map, else `EPERM`; stored sorted).

### Capabilities

- Scopes: `capable` only in the initial user namespace; `capable_here` in the caller's own;
  `ns_capable` in a given namespace (including the parent-owner rule); `capable_wrt` additionally
  requires the inode's uid and gid to be mapped in the caller's namespace.
- `capset`: v1 (one word) or v2/v3 (two); unknown versions write v3 back and fail `EINVAL`; null
  data is a probe; another pid `EPERM`; new permitted ⊆ old permitted, effective ⊆ new permitted,
  inheritable ⊆ old inheritable ∪ (`CAP_SETPCAP` ? all : old permitted) and ⊆ old inheritable ∪
  bounding; ambient is then masked to permitted ∩ inheritable.
- prctl: bounding drop needs `CAP_SETPCAP`; ambient raise needs the capability in permitted and
  inheritable and no `NO_CAP_AMBIENT_RAISE`; securebits need `CAP_SETPCAP`, cannot change locked
  bits or unlock; keepcaps fails when locked.

### execve

- Inputs: the final interpreter's stat; setuid/setgid apply only with the mode bit, no
  no_new_privs, a mount without nosuid, owner and group mapped in the caller's namespace (setgid
  also needs group exec); file caps are read unless the mount is nosuid (parse errors fail exec with
  `EINVAL`) and apply only when their root (kernel uid 0 for v1/v2, the rootid for v3) is the root
  of the caller's user namespace or an ancestor's (`rootid_owns_currentns`); "traced" means a tracer whose credentials when tracing began (Linux's
  `ptracer_cred`) lack `CAP_SYS_PTRACE` over the caller's user namespace (`ptracer_capable`).
- `Cred::exec` (Linux 6.1 `bprm_fill_uid` + `cap_bprm_creds_from_file`): euid/egid from the file;
  permitted = (bounding ∩ file permitted) | (inheritable ∩ file inheritable), `EPERM` when the file's
  effective bit is set and not all file permitted capabilities were granted; root emulation unless
  `NOROOT` (and not for a non-root running a setuid-root binary that has file caps): a root euid or
  ruid gives permitted = bounding | inheritable, a root euid sets the effective bit; gains (setid or
  more permitted) under no_new_privs or an unprivileged tracer reset euid/egid and limit permitted
  to the old set; saved and filesystem ids follow the effective ones; ambient is cleared for setid
  or file-caps programs, then permitted |= ambient and effective = file effective ? permitted :
  ambient; `KEEP_CAPS` is cleared; secureexec = setid or (non-root real uid and (file effective or
  permitted ⊄ ambient)).
- After the transition: secureexec sets `AT_SECURE` and caps the stack rlimit at 8 MB; dumpable is
  cleared when ids differ or permitted grew, else set from whether the caller can read the file;
  pdeathsig is cleared in those cases.
- Writes, truncation and chown remove file caps (and set-id bits) from the inode (in the store).

### Access decisions

- `permission(stat, mask)`: owner class if fsuid matches, group if fsgid or a supplementary group
  matches, else other; if the class allows everything requested, allow; otherwise directories
  allow with `CAP_DAC_READ_SEARCH` (no write) or `CAP_DAC_OVERRIDE`, non-directories allow with
  `CAP_DAC_READ_SEARCH` (read only) or `CAP_DAC_OVERRIDE` (no exec, or some exec bit set), all via
  `capable_wrt`; else `EACCES`. No ACLs, immutable or append flags.
- `owns`: fsuid is the owner, or `CAP_FOWNER` in the caller's namespace with the owner mapped there
  (Linux's `inode_owner_or_capable`; the group need not be mapped). `may_delete`: write and search
  on the directory, then for sticky directories the owner of file or directory or `CAP_FOWNER`
  (`EPERM`). `new_owner` (Linux 6.1's `vfs_prepare_mode` and `inode_init_owner`) takes the mode
  and the umask: uid fsuid; gid the directory's when it is setgid, else fsgid; in a setgid
  directory a non-member without `CAP_FSETID` loses `S_ISGID` on a group-executable file before
  the umask applies; a directory keeps only its permissions and the sticky bit and inherits the
  setgid bit of a setgid parent. `for_access`: filesystem ids set to the real ids, effective capabilities
  to permitted only when the real uid is root in the namespace. `searches_all`: `CAP_DAC_READ_SEARCH`
  or `CAP_DAC_OVERRIDE` in the initial namespace (gates the no-BKL resolver).
- `may_signal` (Linux `kill_ok_by_cred`): sender's real or effective uid equals the target's real
  or saved uid, or `CAP_KILL` over the target's namespace; callers add same group and SIGCONT
  within the session.
- `may_ptrace(target, dumpable, fscreds)`: privileged (`CAP_SYS_PTRACE` over the target's
  namespace) or (all three target uids and gids equal the caller's filesystem or real ids, the target
  is dumpable, same namespace and the target's permitted ⊆ the caller's effective set with
  filesystem credentials, its permitted set with real ones); procfs links and the opens of
  `/proc/<pid>/mem`, `auxv`, `environ`, `maps`, `smaps` and `pagemap` use filesystem credentials,
  ptrace attach, process_vm, pidfd_getfd, kcmp and setns through a pidfd real ones.

### seccomp

- Installing (`seccomp(2)`/prctl, Linux order): flag combinations `EINVAL`; length 0 or over 4096
  `EINVAL`; without no_new_privs (read from the thread group) or `CAP_SYS_ADMIN` in the caller's
  namespace `EACCES`; the classic and seccomp checkers (aligned loads of `seccomp_data`, forward
  jumps, packet loads and MOD rejected); mode must be disabled or filter; a second listener
  `EBUSY`; total converted length plus 4 per filter over 32768 `ENOMEM`; TSYNC requires every other
  thread to be disabled or on an ancestor chain (else that thread's tid in the caller's pid
  namespace, or `ESRCH` with `TSYNC_ESRCH` or when the caller cannot see it). The always-allow bitmap is built (ANDed with the previous filter's) and the filter
  pushed; TSYNC copies the chain to other threads; the listener fd is close-on-exec.
- Filters belong to threads, are copied by clone/fork and kept across execve; an exiting leader's
  chain is held until the group is reaped.
- At syscall entry (after a `PTRACE_SYSCALL` entry stop): disabled, `PTRACE_O_SUSPEND_SECCOMP` and
  the vDSO clock stubs (clock_gettime, clock_getres and gettimeofday issued from the vDSO, for any
  clock id) skip; strict mode allows read, write, exit and rt_sigreturn and kills otherwise;
  in filter mode a number in the top filter's bitmap runs at once, otherwise every filter runs
  (newest first) on `seccomp_data` (nr, `AUDIT_ARCH_AARCH64`, pc, original x0 and x1..x5) and the
  most severe action wins (ties: the newer filter). Actions: ERRNO (capped at 4095), TRAP (`SIGSYS`
  with syscall/arch/address), TRACE (`PTRACE_EVENT_SECCOMP` stop and re-check, or `ENOSYS` without
  a tracer), USER_NOTIF, LOG, ALLOW, KILL_THREAD (or KILL_PROCESS when it is the last live
  thread), KILL_PROCESS and unknown actions (fatal `SIGSYS` with core dump). Logged actions go to
  Appine's debug log only (stderr with `APPINE_LOG`); USER_NOTIF is never logged.
- User notifications: the waiting thread is woken by replies, ends on fatal signals, and returns
  `ERESTARTSYS` on other signals (unless `WAIT_KILLABLE_RECV` and already received, where a
  signal costs one spurious wakeup before blocking again). RECV (zeroed struct, blocks even when
  nonblocking, `ENOENT` for withdrawn; the `pid` field is the notified thread as the receiver's pid
  namespace sees it, 0 when invisible), SEND (flags, `CONTINUE`, `ENOENT`,
  `EINPROGRESS` for a second answer), ID_VALID, ADDFD (installs into the target's table, optionally
  replacing an fd and answering); closing the listener answers waiters with `ENOSYS`; poll readiness
  and `POLLHUP` when no thread uses the filter.
- The cache: for numbers 0..511, a filter whose evaluation with only nr/arch known reaches
  `RET ALLOW` marks the number always allowed; such calls skip the interpreter and keep the no-BKL
  fast path.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs` (every permission and ownership check, xattr capability rules, file-cap removal),
  `kernel/process` (fork/clone copies, execve, namespaces, setpriority/ioprio `PRIO_USER`,
  `RLIMIT_NPROC`), `kernel/signal` (kill, ptrace access), `kernel/procfs` (status lines, ownership,
  sysctls), `kernel/unix` (credentials), `kernel/net`/`kernel/netns` (capability checks), the syscall
  dispatcher and fast path (seccomp).

### Outbound

- User namespaces and id maps (`kernel/process`); `Stat` and modes (`kernel/store`); ptrace stops,
  `signal::force`/`force_fatal` (`kernel/signal`); the vDSO stub check; fd tables; the kernel's
  waits and wake objects; `appine_hal::fill_random` (notification ids); the debug log.

## Invariants and gotchas

- Stored ids are kernel ids; translation happens only at syscall and procfs boundaries (unmapped
  ids show as 65534).
- Capability checks must use the right scope: `capable` is false inside any child user namespace
  even with a full effective set.
- `Seccomp` is touched only under the BKL; the fast path reads the published atomics (a stale
  bitmap during TSYNC belongs to the older, looser chain).
- `in_group` relies on `setgroups` keeping the list sorted.

## Confirmed bugs / technical debt

- **Per-thread id changes leak to every thread.** Credentials are per thread group, and libc does
  not broadcast `setfsuid`/`setfsgid` or raw id syscalls; threaded file servers that switch
  filesystem ids per thread (Samba, nfs-ganesha, FUSE servers, Go raw syscalls) would have one
  thread's checks run as another thread's user.
- **A shared fs context does not block privilege gains.** Nothing models Linux's
  `LSM_UNSAFE_SHARE`: a process sharing `CLONE_FS` with a setuid program can still change its cwd,
  root or umask.
- **The vDSO bypass covers every clock.** Linux's vDSO falls back to a real (filtered) syscall for
  CPU-time clocks, the alarm clocks (`CLOCK_REALTIME_ALARM`, `CLOCK_BOOTTIME_ALARM`) and invalid
  ids; here filters restricting clock ids (Chromium's) never fire, and
  a guest can jump to the vDSO's `svc` to skip its own filter for these three syscalls.
- **seccomp logging is incomplete.** Logged actions reach only Appine's debug stderr (not the guest
  kernel log, where Linux's audit records appear without auditd), and USER_NOTIF is never logged
  although `SECCOMP_FILTER_FLAG_LOG` asks for it.
- **`for_access` ignores `SECBIT_NO_SETUID_FIXUP`**, so access(2) answers differ for
  capability-only sandboxes.
- **Errno order differences.** setgroups reports `EINVAL`/`EFAULT` before the permission check's
  `EPERM`; capset does not mask bits above `CAP_LAST_CAP` (`EPERM` instead of silently ignoring
  them); `PR_CAPBSET_DROP` beyond 40 is `EINVAL` before `EPERM`.
- **setns into a foreign user namespace keeps dumpable.** Entering a namespace not owned by the
  caller's euid skips Linux's dumpable and pdeathsig reset (until the next execve).
- `PER_CLEAR_ON_SETID` personality flags are not cleared on privileged exec (the flags have no
  effect, so only `personality(0xffffffff)` shows it).

## Open questions

- Are per-thread-group credentials a permanent simplification even for raw per-thread id
  syscalls (threaded file servers using `setfsuid`, Go's `AllThreadsSyscall`)?

## To verify

- The Linux 6.1 comparisons above (`access_override_creds`, `cap_ptrace_access_check`,
  `inode_owner_or_capable`, `vfs_prepare_mode`, `ptracer_capable`, the vDSO fallbacks) come from the
  verifiers' knowledge of the kernel source, not from a side-by-side reading or a differential test.
