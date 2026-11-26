# Clocks, sleeps and timers (kernel/time) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/time"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions made before kernel/time was split from kernel/process retain their `kernel/process` tags in `.agents/decisions/kernel/process.md`: D11, D15, D19, D25, D54 and the periodic-check part of D30; D6 and D52 retain the fast-path and deadline-handover contracts.

## Active

### D1: Time owns deadlines, clocks, timers and periodic checks

- **What**: kernel/time owns clock and sleep handlers, deadline helpers, interval and POSIX timers, load averages and the timer worker with stop_checks/resume_checks.
- **Why**: Time handling forms a distinct maintenance boundary within the oversized process map.
- **Tradeoffs**: CPU accounting, generic waits, fast-path dispatch, time namespaces and timerfd retain their other owners.
- **Watch out**: Preserve deadline handover and the idle handshake; the exited-leader clock debt and Linux verification residual are inherited, not independently reverified.
- **Source**: 8a17e4a9; d053be24; 39e55b4b

### D2: CPU clocks retain exited leader identity and usage

- **What**: Save the leader serial with its exit record and use saved CPU time when resolving the departed leader's clock or timer.
- **Why**: Surviving threads can still reference the leader's clock.
- **Tradeoffs**: Leader takeover must reset the old serial.
- **Watch out**: Avoid assigning an old timer to a reused task identity.
- **Source**: 13dd5ae4
