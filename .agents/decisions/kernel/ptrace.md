# Guest tracing (kernel/ptrace) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/ptrace"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on kernel/ptrace made before it was split from kernel/signal keep their `kernel/signal` tags and live in `.agents/decisions/kernel/signal.md`: D5, D6, D12, the ptrace half of D14, D17, D20 and D26.

## Active

### D1: Tracer requests, tracee stops and process_vm calls have a dedicated owner

- **What**: kernel/ptrace owns tracer requests, stop/resume state and process_vm calls, with integration call sites retained by their existing modules.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: Traced exits mark both changes to the killed flag

- **What**: Mark cgroup freezer observations when clearing the killed flag for the exit event and when restoring it. Resume delivery after trace stops, clear deferred stepping on detach, and report sibling exits except during kernel shutdown.
- **Why**: The temporary flag value can change whether a subtree counts as frozen.
- **Tradeoffs**: An exit stop can trigger a content comparison even when the value stays the same.
- **Watch out**: A checkpoint during the trace stop can observe the intermediate state.
- **Source**: 0f5c5132, 13dd5ae4

### D3: Remote memory requests validate vectors in Linux order and retry cold FUSE pages locally

- **What**: Check local vectors and zero totals before remote vectors or target lookup, and revalidate the stopped tracee and access after each unlocked fill.
- **Why**: Either side of a remote access can fault or disappear during a wait.
- **Tradeoffs**: A PEEK returns only a complete word.
- **Watch out**: Never retain a strong tracee or Mm borrow across a FUSE wait.
- **Source**: 54ce6b50, 25b0f2a0, 6084b8af, 3db35725

### D4: Ptrace restores only writable FPCR bits

- **What**: Apply the engine's write mask to restored FPCR.
- **Why**: Hardware ignores unsupported floating-point control bits.
- **Tradeoffs**: Returned state can differ from the submitted register image.
- **Watch out**: Use the same mask as execution and sigreturn.
- **Source**: 9163f493
