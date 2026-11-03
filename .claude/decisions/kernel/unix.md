# AF_UNIX sockets (kernel/unix) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/unix"`
>
> Last distilled: 2026-11-03 (HEAD = 5d21cc4) — seeded by hand from the retired design doc §10 and
> the round records in `docs/rounds/`; no `MODULE: kernel/unix` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: AF_UNIX is emulated entirely in process
- **What**: Stream, datagram and seqpacket sockets, `socketpair`, listen/accept and message queues are kernel-layer objects; no host socket is involved.
- **Why**: `SCM_RIGHTS` must pass guest open files and `SO_PEERCRED` must name guest processes; host sockets would carry host fds and host pids.
- **Tradeoffs**: Every AF_UNIX behaviour (merging, boundaries, readiness, credentials) has to be reimplemented after Linux.
- **Watch out**: Stream reads and poll events follow Linux's `unix_stream_read_generic` and `unix_poll` rules.
- **Source**: design review 2026-10-24 (§10), round 06 (`06-m4`)

### D2: Path names are socket nodes in the store; abstract names live in the kernel
- **What**: Binding a path creates an `S_IFSOCK` node in the store (`EADDRINUSE` if it exists) and records its inode; abstract names and autobind names are only in the kernel's name table and disappear with the socket.
- **Why**: Pathname sockets must be visible and permission-checked like files (logging through `/dev/log` was among M4's tests); abstract names need no file.
- **Tradeoffs**: Path names are global across network namespaces (only abstract names are scoped per namespace).
- **Watch out**: Look path sockets up by inode, not by path string.
- **Source**: round 06 (`06-m4`)

### D3: Cycles through in-flight messages are collected like Linux's `unix_gc`
- **What**: When a unix socket is closed (close, close_range, dup2 over it, execve) or a process exits, sockets referenced only by in-flight messages and unreachable from outside have their receive queues emptied.
- **Why**: Sockets passed through themselves formed `Rc` cycles (open file → socket → queue → message → open file) that were never freed.
- **Tradeoffs**: The collector is conservative (any temporary extra reference keeps a socket) and runs synchronously on unix socket closes; Linux 6.8 runs it asynchronously, so it cannot be compared natively and is covered by unit tests.
- **Watch out**: Only unix-socket files can be candidates.
- **Source**: round 19 (`19-deferred`)
