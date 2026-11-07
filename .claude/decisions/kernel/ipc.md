# System V IPC and message queues (kernel/ipc) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/ipc"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930)

## Active

### D1: SysV IPC and POSIX message queues are one kernel/ipc module
- **What**: kernel/ipc owns the per-namespace object model, the 12 SysV syscalls and the six `mq_*` syscalls with `MessageQueue`, while the IPC namespace lifecycle stays in kernel/process, shm host memory and VMAs in kernel/mm, mqueue node views and sysctls in kernel/procfs, and mqueue mounts and file-syscall dispatch in kernel/vfs.
- **Why**: About 2300 lines in `ipc.rs`, `syscall/ipc.rs` and `syscall/mqueue.rs` had no module, so no map described them and their bugs had nowhere to be recorded.
- **Tradeoffs**: Message-queue behaviour is spread over four maps (readiness in kernel/poll too), and the map's Linux comparisons come from recalled upstream source.
- **Watch out**: Bugs recorded at mapping stay open in the map, among them SysV capabilities ignoring user namespaces, destroyed shm segments outliving their last mapping, unshare and setns dropping sibling threads' undo records, global pids in `/proc/sysvipc`, unenforced semmns and shmall, mqueue notifications outliving their owner's fds and blocked mqueue calls never restarted.
- **Source**: 4140c75

### D2: mremap duplicates of shm segments count as attachments
- **What**: A copy made by mremap with an old length of 0 tags its shm segment with its own start, so nattch counts it and shmdt at its address detaches it (kernel/mm D21).
- **Why**: Linux counts a duplicated segment VMA as another attachment through its `vm_ops` open.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 5dcf7db

### D3: Message-queue timeouts stay on the realtime clock
- **What**: mq_timedsend and mq_timedreceive keep their absolute `CLOCK_REALTIME` timeout as an `Expiry` on the realtime base (kernel/process D19).
- **Why**: Their timeouts are absolute realtime times, which fired late once turned into monotonic instants.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 63b9e59

### D4: IPC limits stay within int, and large allocations fail with ENOMEM
- **What**: msgmax, msgmnb and the four sem values refuse values above `INT_MAX` (shmmax and shmall stay unsigned longs), msgsnd, semaphore sets and semtimedop allocate through `ipc::zeroed`, which returns `ENOMEM` when the host cannot, and semtimedop takes nsops as an unsigned int.
- **Why**: After root raised these limits, any process in the namespace could abort the app with one msgsnd, semget or semtimedop allocating by them.
- **Tradeoffs**: `zeroed` touches a large message's pages before the copy, which would touch them anyway.
- **Watch out**: Allocations the host overcommits still succeed and may later be killed by the host's low-memory killer rather than fail with `ENOMEM`.
- **Source**: 09188ec
