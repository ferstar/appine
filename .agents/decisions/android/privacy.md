# App-wide authentication (android/privacy) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/privacy"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1)
>
> Decisions made before android/privacy was split from android/terminal retain their `android/terminal` tags in `.agents/decisions/android/terminal.md`: the boundary in D4, lifecycle protection in D60 and export authorization in D61 (f61072f3 Decision 1 and c7309c0c Decision 3).

## Active

### D1: The authentication gate is separate from its consumers

- **What**: android/privacy owns Privacy.kt, NotificationActivity.kt and shield resources, inheriting the parent verification and residuals.
- **Why**: A gate protecting multiple activities needs a maintenance boundary independent of terminal controls.
- **Tradeoffs**: Call-site enforcement, settings, notifications, export creation/handoff, application registration and tests retain their existing module owners.
- **Watch out**: Real biometrics, concurrent windows and vendor authentication remain unverified; the split is a boundary check rather than independent full verification.
- **Source**: 9c6c51a7
