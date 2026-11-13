# Program execution (kernel/exec) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/exec"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Decisions on execve and the ELF loader made before kernel/exec was split from kernel/process keep their `kernel/process` tags and live in `.agents/decisions/kernel/process.md`: D7, D17, D31, D41, D43, D46 and D51.

## Active

### D1: kernel/exec is split out of kernel/process
- **What**: kernel/exec owns `loader.rs` and the execve section of `syscall/proc.rs`: arguments, targets, `#!` scripts, `ETXTBSY`, the effects of set-id bits and file capabilities, the ELF loader with doomed images, the point of no return, `exe_file`, the executable cache and the names of the program's mappings.
- **Why**: execve and the ELF loader are a self-contained path (6.1's `fs/exec.c` and `binfmt_elf.c`) changed often on their own, and the process map could no longer be read in one Read.
- **Tradeoffs**: The point of no return calls into nine other modules (threads, credentials, time namespaces, mm, signals, fds, FUSE, AIO, IPC), all described elsewhere.
- **Watch out**: A step added to the point of no return without updating its ordered list in the map.
- **Source**: b320f8e
