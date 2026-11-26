# Native AIO (kernel/aio) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/aio"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Native AIO decisions made before kernel/aio was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D42.

## Active

### D1: kernel/aio is split out of kernel/vfs
- **What**: kernel/aio owns `syscall/aio.rs`: io_setup to io_pgetevents, contexts per thread group and their `aio-nr` charge, synchronous I/O at submission, pending `IOCB_CMD_POLL`s and context release at execve and exit.
- **Why**: Native AIO is self-contained (one file with its own contexts and limits) and changed on its own, and the vfs map could no longer be read in one Read.
- **Tradeoffs**: Pending poll completion uses kernel/poll readiness and kernel/time's check_aio_polls, while exit cleanup stays in kernel/process.
- **Watch out**: Low risk beyond the small map going stale.
- **Source**: cc160c1; 8a17e4a9; d053be24; 39e55b4b

### D2: AIO flags and completions belong to individual requests

- **What**: Pass append and nowait through request-local flags, return collected events on interruption, and release Unix references after poll completion or cancellation.
- **Why**: Concurrent requests can share an open description.
- **Tradeoffs**: File and FUSE entry points carry extra flags.
- **Watch out**: Every AIO-capable data path must honor them.
- **Source**: 15347732

### D3: Synthetic AIO eligibility follows iterator support

- **What**: Ask procfs whether the file supports an iterator for the requested direction.
- **Why**: Synthetic files implement different read and write operations.
- **Tradeoffs**: Classification resides with the pseudo-node definitions.
- **Watch out**: Classify new node types before admitting native AIO.
- **Source**: 2609ab43
