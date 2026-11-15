# Extra keys (android/keys) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/keys"`
>
> Last distilled: 2026-11-16 (HEAD = 5bd37526)
>
> Decisions on android/keys made before it was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D4, D12, D14, D15 and D19–D23.

## Active

### D1: Extra-key views own their layouts, gestures and modifiers

- **What**: android/keys owns extra-key rendering, gestures, layouts and shared indicator helpers, while activity integration remains in android/terminal.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14
