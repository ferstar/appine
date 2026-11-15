# Guest tracing (kernel/ptrace) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/ptrace"`
>
> Last distilled: 2026-11-16 (HEAD = 5bd37526)
>
> Decisions on kernel/ptrace made before it was split from kernel/signal keep their `kernel/signal` tags and live in `.agents/decisions/kernel/signal.md`: D5, D6, D12, the ptrace half of D14, D17, D20 and D26.

## Active

### D1: Tracer requests, tracee stops and process_vm calls have a dedicated owner

- **What**: kernel/ptrace owns tracer requests, stop/resume state and process_vm calls, with integration call sites retained by their existing modules.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14
