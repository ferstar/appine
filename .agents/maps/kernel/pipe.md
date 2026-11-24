# kernel/pipe Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/pipe.md` for the paired decision history (decisions from before the split are D31 and D54 in `.agents/decisions/kernel/vfs.md`).
>
> Verified: 2026-11-11 (split out of the kernel/vfs full mapping of 2026-11-10; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-24 (targeted verification only since the full mapping; latest change: socket input offsets and error ordering in splice)

## Responsibilities

`kernel/pipe` is pipes, FIFOs and splicing (6.1's `fs/pipe.c` and `fs/splice.c`):

- anonymous pipes from `pipe2` and named pipes (FIFOs) shared per inode: buffers, capacity and
  `F_SETPIPE_SZ`, atomic writes, readers and writers, `SIGPIPE`;
- opening a FIFO and its waits;
- `splice`, `tee` and `vmsplice`: argument checks, when they wait, and how data leaves a pipe while
  a splice writes it elsewhere.

Root: `core/kernel/src/pipe.rs`; in `syscall/fs.rs` `pipe2`, `open_fifo`, the pipe-size fcntl
commands, `pipe_read`, `pipe_write` and `broken_pipe`, and the splice block from `splice` to
`wait_splice_done`.

Outside the module: fd tables and open files (`kernel/files`); the read, write and vectored paths
that reach pipes (`kernel/fileio`); the wait mechanism and wake objects (`kernel/process`); readiness and `EPOLLET`
(`kernel/poll`); `/dev/fuse` and AF_UNIX sockets as splice ends (`kernel/fuse`, `kernel/unix`); the
`pipe-max-size` sysctl (`kernel/sysctl`).

## Key types

- `Pipe` / `PipeEnd` — payload bytes and occupied page slots, capacity, reader and writer counts, EPOLLET
  counters, `splicing`; FIFOs by inode in `Kernel::fifos` (weak).
- `FileKind::Pipe` (`kernel/files`).

## Public entry points

- **Syscalls**: `pipe2`, `splice`, `tee`, `vmsplice`; `F_GETPIPE_SZ`/`F_SETPIPE_SZ` through fcntl;
  `FIONREAD` on a pipe through ioctl.
- **Kernel-internal**: `open_fifo` (opening a FIFO node), `pipe_read`/`pipe_write` (the read and
  write paths), `wait_splice_done` (every consumer of pipe data).

## Data flow / lifecycle

### Pipes and FIFOs

- Data lives in process memory. `pipe2` accepts `O_CLOEXEC|O_NONBLOCK|O_DIRECT` (`O_DIRECT`
  ignored: no packet mode, not in `F_GETFL`). Capacity 64 KiB, capped at `pipe-max-size` (1 MiB)
  without `CAP_SYS_RESOURCE`; `F_SETPIPE_SZ` rounds up to a power of two of at least a page
  (`EPERM` when growing beyond the max, `EBUSY` when the new capacity cannot hold all occupied
  and reserved slots).
- Pipe reads (`read`, `readv`, read-side `vmsplice`) peek and copy into guest memory via
  `pipe_read_user` (`write_user_partial`), draining only the bytes actually copied so a faulting
  user buffer (`EFAULT`) leaves unread bytes in the pipe.
- Ordinary writes may merge the initial length modulo 4096 into the last anonymous write slot
  if it all fits after the last byte. The remaining data occupies new slots. Reading a prefix
  keeps its slot occupied until the final byte leaves. Freeing a slot from a full pipe wakes
  blocked writers and advances the write-end edge counter.
- FIFO open: one `Pipe` per inode; write-only nonblocking without a reader `ENXIO`; read-only
  nonblocking succeeds; read-write never waits; blocking one-ended opens wait for a peer present
  or opened since (`ERESTARTSYS` on a signal); access mode 3 `EINVAL` (`fifo_open`).
- No readers: `SIGPIPE`+`EPIPE` (or a short count); writes up to `PIPE_BUF` (4096) are atomic,
  larger ones partial; nonblocking `EAGAIN`. Reads return 0 without writers. `FIONREAD` reports
  the buffered bytes.

### splice, tee and vmsplice

- `splice`, `tee` and `vmsplice` copy through buffers. One side of `splice` must be a pipe; data
  leaves the pipe only after the write succeeds, and meanwhile every other consumer of that pipe
  (read, readv, vmsplice, another splice out) waits whatever `O_NONBLOCK` says, as for Linux's pipe
  lock; writers do not wait. Unknown flags `EINVAL`; a zero length returns 0 first in splice,
  after the flags in tee, and in vmsplice after the flags, access mode and iovecs (a non-pipe fd
  is `EBADF` only after that). splice checks access modes (`EBADF`), then its `EINVAL` cases (no
  pipe, an `O_APPEND` output including FIFOs, an end that cannot splice that way), then offsets on
  a pipe (`ESPIPE`). An explicit socket input offset gives `EINVAL` before waiting for room or
  consuming data. Other unseekable inputs can return `ESPIPE` after the wait for room. `/dev/fuse` splices
  one whole request into a pipe (waiting as the device's own `O_NONBLOCK` says, `EIO` without
  room) and takes a reply of exactly the length asked, setting its header length to it (libfuse
  fixes the length in user memory after vmsplicing the header, which a copying vmsplice does not
  see).
- Which files splice at each end follows Linux's `splice_read` and `splice_write`
  (`splice_reads`, `splice_writes`, also used by sendfile): pipes, regular and FUSE files,
  sockets, terminals, host fds, `/dev/fuse`, procfs and sysfs files, and the devices, except that
  `/dev/null` cannot be read from, `/dev/full` cannot be written to, and `/dev/kmsg` and
  `/dev/loop-control` neither; directories, eventfd, epoll and the other anonymous files cannot
  splice at all.
- When splice waits follows 6.1: between two pipes and for tee, for data (going on at end of file)
  and then room, unless `SPLICE_F_NONBLOCK` or either end's `O_NONBLOCK`, then moving or copying
  what is there and fits (splice waits again if a race took it, tee returns 0); from a pipe, for
  data unless `SPLICE_F_NONBLOCK` or the input's `O_NONBLOCK`, writing as the output's own
  `O_NONBLOCK` says; into a pipe, for room unless `SPLICE_F_NONBLOCK` or the output's
  `O_NONBLOCK` (`EPIPE` with `SIGPIPE` without readers), reading at most the room as the input's
  `O_NONBLOCK` says, except that an AF_UNIX stream socket also stops waiting for the splice flag or
  the pipe's `O_NONBLOCK` (TCP does not). vmsplice writes when the fd is open for writing (a FIFO
  opened read-write too), reads otherwise, and waits unless `SPLICE_F_NONBLOCK`, whatever the
  pipe's `O_NONBLOCK`. Regular-file splice and sendfile split at file page boundaries, vmsplice
  at each iovec's guest page boundaries. Each fragment occupies a separate slot. A whole slot moved between
  pipes keeps its merge flag; tee and partial moves create nonmergeable destination slots.
- vmsplice's write side reads guest memory directly; every other copy to or from a guest buffer
  first grows a grows-down mapping below it (`kernel/fileio`).

## Dependencies (inbound / outbound)

### Inbound

- `kernel/files`: opening FIFO nodes, fcntl, ioctl, close; `kernel/fileio`: the read, write and
  vectored paths, and sendfile asking `splice_reads`/`splice_writes` about its ends.
- `kernel/poll` (pipe readiness, `EPOLLET` counters), `kernel/fuse` (`/dev/fuse` as a splice end),
  `kernel/unix` (stream sockets as splice inputs).

### Outbound

- `kernel/fileio` (the read and write paths of the other end, file checks, `RLIMIT_FSIZE`),
  `kernel/files` (fd tables and `FileKind::Pipe`),
  `kernel/process` (waits and wake objects), `kernel/signal` (`SIGPIPE`), `kernel/sysctl`
  (`pipe-max-size`), `kernel/cred` (`CAP_SYS_RESOURCE`).

## Invariants and gotchas

- A splice reading into a pipe reserves slots across unlocked input I/O. A short read or error
  releases unused slots for other writers. Poll sees published slots.
- A splice out of a pipe into a file, socket or FUSE file drains what it wrote only after the
  write, which can release the BKL: code that takes data out of a pipe must first wait while
  `Pipe::splicing` is set (`wait_splice_done`), or the late drain removes bytes it never wrote.
  Closing an end does not wait (Linux's close takes the pipe lock too).

## Confirmed bugs / technical debt

- **splice's checks differ:** access modes (`EBADF`) come before a pipe given an offset
  (`ESPIPE`), where 6.1 returns `ESPIPE` first; some unseekable non-socket inputs return `ESPIPE`
  after waiting for room where Linux returns `EINVAL`.
- **Pipe write I/O with faulting buffers:** a writev whose later iovec faults still writes what
  precedes it (Linux commits only whole copied pages; a sub-page writev writes nothing); a write
  copies up to 64 KiB at a time, so a fault in the first chunk is `EFAULT` and a later one
  returns the preceding chunks (Linux returns the bytes up to the fault).
- **Fidelity gaps**, one per line:
  - `O_NOTIFICATION_PIPE` is `EINVAL` (6.1 makes one, or `ENOPKG` without watch queues);
  - vmsplice copies at the call, so later changes to the buffer are not seen by the reader;
  - every procfs and sysfs file is taken as splicing both ways, so splice reads a per-process
    file such as `/proc/<pid>/status` into a pipe, which has no `splice_read` on Linux (`EINVAL`
    on a native 6.8 kernel).

## Open questions

- None recorded.

## To verify

- The 6.1 attribution of splice's `SPLICE_F_NONBLOCK` conversion, per-page pipe copies and
  `O_NOTIFICATION_PIPE`, seen only on a native 6.8 kernel.
