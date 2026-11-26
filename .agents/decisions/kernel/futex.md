# Futexes (kernel/futex) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/futex"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Futex decisions made before kernel/futex was split from kernel/process keep their `kernel/process` tags and live in `.agents/decisions/kernel/process.md`: D26, D44 and the futex halves of D32, D33 and D37.

## Active

### D1: kernel/futex is split out of kernel/process
- **What**: kernel/futex owns `futex.rs`, the futex and robust-list handlers in `syscall/misc.rs` and the futex call of `syscall/fast.rs`: keys, the queue the BKL and fast paths share, the commands with `FUTEX_WAKE_OP` and `futex_waitv`, PI locks and their hand-over at exit, and robust lists, every kernel write to a futex word going through `writable_word`.
- **Why**: The process map was still 52.4 KB after exec and ns left, and futexes are self-contained, as Linux's `kernel/futex` is.
- **Tradeoffs**: Fast-path waits and deadline handover stay in kernel/process, while Expiry and clock jumps belong to kernel/time, so timed futex changes read both maps.
- **Watch out**: Low risk beyond the small map going stale.
- **Source**: b320f8e; 8a17e4a9; d053be24; 39e55b4b

### D2: Exec releases PI ownership under the retiring task identity

- **What**: Pass the released tid to the PI-owner exit scan during exec takeover.
- **Why**: The task can still be present when its old ownership ends.
- **Tradeoffs**: The exit scan accepts an explicit retiring owner.
- **Watch out**: Owner-death handoff must still cover absent tasks and zero owners.
- **Source**: 15347732
