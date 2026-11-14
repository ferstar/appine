# Native terminal implementation (android/vtcore) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtcore"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1)
>
> Decisions made before android/vtcore was split from android/vt retain their `android/vt` tags in `.agents/decisions/android/vt.md`: D1, D3, D4 and the parsing side of D5–D7; earlier `android/terminal` decisions remain in `.agents/decisions/android/terminal.md`: D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49.

## Active

### D1: Native state, locking and terminal protocols stay together

- **What**: android/vtcore owns VtNative.kt, all vt.rs and vt.c except registered interaction and frame/rendering sections, keeping prompt parsing, snapshots and compression beside shared terminal state.
- **Why**: Native implementation needs an explicit owner distinct from Kotlin session lifecycle.
- **Tradeoffs**: The map remains large because another split would duplicate shared-state and locking contracts.
- **Watch out**: Assign every new vt.c section explicitly and preserve inherited device/protocol verification gaps; the 2026-11-11 mapping is inherited through android/vt, not independently repeated.
- **Source**: 81f60bef
