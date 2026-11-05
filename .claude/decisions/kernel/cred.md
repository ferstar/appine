# Credentials and seccomp (kernel/cred) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/cred"`
>
> Last distilled: 2026-11-06 (HEAD = afd086a). D1–D6 were seeded by hand from the retired design doc
> §11.1 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Credentials live in the thread group
- **What**: One `Cred` (four uids, four gids, supplementary groups, capability sets, securebits, user namespace) per thread group; every thread reads and changes the same copy.
- **Why**: libc's setuid family already makes every thread perform the call, so one copy per group gives the result libc users expect.
- **Tradeoffs**: `setfsuid`/`setfsgid` (which libc does not broadcast) and raw per-thread id syscalls change every thread, unlike Linux; threaded file servers that switch ids per thread are the casualty.
- **Watch out**: The group also holds `dumpable` and `no_new_privs`, while seccomp filters stay per thread as in Linux.
- **Source**: round 08 (`08-m6`)

### D2: Id changes, DAC and capability fixups follow Linux 6.1 and capabilities(7)
- **What**: The set*id syscalls, `capset`, the capability prctls and securebits use Linux's rules; uid changes adjust capabilities as `cap_emulate_setxuid` does; changing effective or filesystem ids, or gaining capabilities, clears dumpable and pdeathsig as `commit_creds` does; DAC checks follow `generic_permission`, with root bypassing through capabilities.
- **Why**: M6 replaced the all-root model so that dropping to an ordinary user is enforced (a rootfs test checks DAC after `su` to `nobody`); the `commit_creds` side effects were added with user namespaces, having happened only at execve before.
- **Tradeoffs**: No ACLs, immutable or append-only attributes; `system.*` xattrs are stored but have no effect.
- **Watch out**: Securebits were recorded but ineffective until round 100; new paths must honour `NOROOT`, `NO_SETUID_FIXUP` and `NO_CAP_AMBIENT_RAISE`.
- **Source**: rounds 08 (`08-m6`), 100 (`100-file-caps`), 119 (`119-user-namespaces`)

### D3: Capability checks name their namespace, as in Linux
- **What**: Three checks: `capable` (initial user namespace only), `capable_wrt` (own namespace, with the inode's owner and group mapped — DAC overrides, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_FSETID`) and `ns_capable` (a given namespace — set*id, capset, mounts by the mount namespace's owner, ptrace and kill by the target's); "root" means uid 0 of the credential's own namespace.
- **Why**: A process in a new user namespace holds a full capability set; when user namespaces were added for bubblewrap, the 74 call sites that had checked the caller's own effective set were each switched to the scope Linux uses there.
- **Tradeoffs**: The no-BKL `stat` resolver serves only processes in the initial namespace, since an override elsewhere depends on the file.
- **Watch out**: A new check that uses the wrong scope either grants host-level privilege inside a namespace or denies what a container needs.
- **Source**: round 119 (`119-user-namespaces`)

### D4: execve transforms credentials like `cap_bprm_creds_from_file`, with file capabilities
- **What**: Setuid/setgid bits (owner and group mapped, no nosuid, no no_new_privs), file capabilities from `security.capability` (v1–v3; v3 only with rootid 0), ambient capabilities, root emulation and `AT_SECURE` for privileged execution; no_new_privs or a tracer without `CAP_SYS_PTRACE` (recorded at attach) blocks gains.
- **Why**: Ordinary users (Debian `su`, sshd logins, a non-root login account) need file capabilities such as iputils-ping's `cap_net_raw`; before round 100 the xattr was stored and ignored at exec.
- **Tradeoffs**: v3 file capabilities set inside a user namespace are not converted and do not apply; the tracer's privilege is a single flag checked in the initial namespace (Linux keeps the whole credential); sharing the fs context (`CLONE_FS`) does not suppress gains; no audit records.
- **Watch out**: File capabilities are only safe together with the xattr permission checks (`security.capability` needs `CAP_SETFCAP`, other `security.*` and `trusted.*` need `CAP_SYS_ADMIN`) and with privilege removal on write, truncate and chown; round 100 found all three missing and added them together.
- **Source**: rounds 08 (`08-m6`), 100 (`100-file-caps`), 119 (`119-user-namespaces`)

### D5: Guest seccomp is implemented in the kernel layer with Linux 6.1 semantics
- **What**: Strict mode and cBPF filters with Linux's program checks, chain size limit, action precedence, inheritance and TSYNC; filters run in an interpreter at syscall entry (after a `PTRACE_SYSCALL` entry stop); `PTRACE_EVENT_SECCOMP` and the notification fd (including `ADDFD`) are supported; `KILL_PROCESS`'s `SIGSYS` cannot be caught, as with `SA_IMMUTABLE`.
- **Why**: Programs that sandbox themselves either failed or ran without the sandbox; `strace --seccomp-bpf` and notification supervisors did not work.
- **Tradeoffs**: The interpreter costs about 4.5 ns per instruction (Linux uses a JIT); `LOG` and logged actions go to appine's debug log instead of audit; the vDSO clock stubs skip filters for every clock (Linux's vDSO handles only some); a `TRACE` re-check uses the chain from before the stop; `ADDFD` installs directly and never sees the target interrupted.
- **Watch out**: Every syscall entry, including the no-BKL fast path, must consult the filter; a fast path that misses `seccomp_on` bypasses filters silently, which the first tests did not catch.
- **Source**: round 59 (`59-seccomp`)

### D6: An always-allow cache keeps filtered threads on the fast path
- **What**: Installing a filter emulates it for syscall numbers 0..511 with only the number and architecture known (`seccomp_is_const_allow`); numbers that always reach `ALLOW` form a bitmap ANDed with the previous filter's; the thread publishes it atomically, and those numbers plus the vDSO clock stubs skip the interpreter and keep the no-BKL fast path.
- **Why**: Without it every syscall ran the whole chain under the BKL, so futex wakes and clock reads in multithreaded sandboxed programs queued on the lock; with it, filtered threads run at unfiltered speed for libseccomp-style filters.
- **Tradeoffs**: Only the program shapes Linux recognises are cached; other numbers still take the BKL and the interpreter.
- **Watch out**: During TSYNC another thread may read the old bitmap; this is acceptable only because a new chain is never looser, so the call counts as happening before the sync.
- **Source**: round 60 (`60-seccomp-cache`)

### D7: Credentials, capabilities and seccomp are a module of their own
- **What**: kernel/cred covers `cred.rs` except user namespaces, `syscall/cred.rs` and `seccomp.rs`, while user namespaces stay in kernel/process.
- **Why**: §11.1's credential and seccomp design had to leave the retired design doc, and the user asked for a module of its own.
- **Tradeoffs**: User namespaces sit in kernel/process although their code is in `cred.rs`, and the Linux 6.1 comparisons rest on the verifiers' knowledge of the kernel source rather than a side-by-side reading.
- **Watch out**: Divergences recorded at the split await a separate review, among them per-thread id changes leaking to every thread, global pids in seccomp notifications, privilege gains despite a shared fs context, untranslated `PRIO_USER`, new-file modes and the ptrace access checks.
- **Source**: 12c4afa

### D8: Credential and seccomp files read in the order their code runs
- **What**: cred.rs runs credentials and file capabilities with the user namespace types (owned by kernel/process) last and `impl Cred`'s entry points before id translation and scope checks; syscall/cred.rs runs id syscalls, capabilities, then committing; seccomp.rs goes from installing filters through syscall entry, the sysctls, user notifications, ptrace and thread exit to the always-allow cache, cBPF and shared helpers; constants several sections use stay at the top.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; cred.rs opened with the user namespace types and seccomp.rs mixed the cBPF interpreter into the filter paths.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; two seccomp sections (thread exit, shared helpers) hold one function each.
- **Watch out**: Code appended at the end of a file drifts from this order.
- **Source**: 159d4f4
