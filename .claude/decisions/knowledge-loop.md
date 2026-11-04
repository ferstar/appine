# Knowledge loop (knowledge-loop) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: knowledge-loop"`
>
> Last distilled: 2026-11-04 (HEAD = b38d29c)

## Active

### D1: Changes are recorded in commits and module maps; `docs/rounds/` is frozen after round 208
- **What**: Each change is recorded in its `/commit-context` commit (Decision blocks; measurements, survey results and testing in the body) and in the module maps, with deferred work and unfixed problems in a map's "Confirmed bugs / technical debt" or "Open questions" and pitfalls in its "Invariants and gotchas"; the old rounds get no new rounds, index rows or "→", an old deferred item moves into its map only when it comes up again, `docs/milestones.md` keeps the cross-change reference, and work with no diff gets a short doc only when worth keeping.
- **Why**: Each change was written down three times (a round, a Decision block, a map refresh), and the round was the only copy that went stale and needed "→" marks added back into old files.
- **Tradeoffs**: The records are in English instead of Chinese; commit bodies cannot be amended, so the maps carry the live state; a change spanning many commits has no single narrative; open items in old rounds that were never moved can be forgotten.
- **Watch out**: A commit made without `/commit-context` leaves no record of why, since the user runs it by hand and no rule enforces it; old deferred items are found only by grepping; map debt sections grow if done entries are not removed.
- **Source**: 511866b
