# Pipes and splicing (kernel/pipe) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/pipe"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Pipe and splice decisions made before kernel/pipe was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D31 and D54.

## Active

### D1: kernel/pipe is split out of kernel/vfs
- **What**: kernel/pipe owns `pipe.rs`, `pipe2`, `open_fifo`, the pipe-size fcntl commands, `pipe_read` and `pipe_write`, and the splice block of `syscall/fs.rs` from `splice` to `wait_splice_done`, with the pipe and splice rules and the two splice invariants.
- **Why**: Pipes and splice are Linux's `fs/pipe.c` and `fs/splice.c` and are changed on their own, and the vfs map was still 54.3 KB after overlay and locks left.
- **Tradeoffs**: A splice into or out of a file or socket uses kernel/vfs's read and write paths, so such changes read both maps.
- **Watch out**: Code that takes data out of a pipe without waiting for `wait_splice_done`, now that the rule lives here.
- **Source**: 5f327de
