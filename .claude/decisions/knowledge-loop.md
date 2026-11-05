# Knowledge loop (knowledge-loop) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: knowledge-loop"`
>
> Last distilled: 2026-11-06 (HEAD = afd086a)

## Active

### D1: Changes are recorded in commits and module maps; `docs/rounds/` is frozen after round 208
- **What**: Each change is recorded in its `/commit-context` commit (Decision blocks; measurements, survey results and testing in the body) and in the module maps, with deferred work and unfixed problems in a map's "Confirmed bugs / technical debt" or "Open questions" and pitfalls in its "Invariants and gotchas"; the old rounds get no new rounds, index rows or "→", an old deferred item moves into its map only when it comes up again, `docs/milestones.md` keeps the cross-change reference, and work with no diff gets a short doc only when worth keeping.
- **Why**: Each change was written down three times (a round, a Decision block, a map refresh), and the round was the only copy that went stale and needed "→" marks added back into old files.
- **Tradeoffs**: The records are in English instead of Chinese; commit bodies cannot be amended, so the maps carry the live state; a change spanning many commits has no single narrative; open items in old rounds that were never moved can be forgotten.
- **Watch out**: A commit made without `/commit-context` leaves no record of why, since the user runs it by hand and no rule enforces it; old deferred items are found only by grepping; map debt sections grow if done entries are not removed.
- **Source**: 511866b

### D2: Distills turn migration scopes and implicit supersessions into entries by recorded judgment
- **What**: Each migration commit's module-scope Decision becomes one scope entry, a block replacing part of an entry is merged into the chain's latest entry, supersessions without a SUPERSEDES line or recorded under another module are listed under Superseded and flagged at review, hand-seeded entries naming removed code get minimal edits with the new hashes in Source, and "Last distilled" says which entries were seeded by hand; a fix spanning modules gets one entry per module that refer to each other, blocks sharing one approach form one cluster entry, entries a later block refines are edited with its hash added to Source, and scope entries' Watch out names only bugs still open in the map.
- **Why**: Five snapshots had Decision blocks committed after their hand seeding and knowledge-loop had none, distilling from commits for the first time raised cases the snapshots had no rule for, and the first distill of every module added cross-module fixes, test-only blocks and refinements of existing entries.
- **Tradeoffs**: Scope entries repeat part of `MODULES.md`, one-sentence fields and cluster entries drop details that stay in the source commits, a cross-module fix is spread over several snapshots, and implicit and cross-module supersessions rest on judgment rather than SUPERSEDES lines.
- **Watch out**: Edited hand-seeded entries keep their IDs, which later distills must preserve, and possible merges left separate (android/setup D13 and D5, kernel/netns D10 and D4) may want merging later.
- **Source**: 8349911, 4b0adb9

### D3: A map's Maintained line is one sentence
- **What**: A map's Maintained line names in one sentence the areas changed since its full mapping, the latest change last, and says what the full mapping predates; map-module asks for this on every targeted refresh.
- **Why**: The terminal map's line had grown to 2626 characters over 22 targeted refreshes, a changelog repeating commit subjects and naming features later removed.
- **Tradeoffs**: The map no longer lists each refresh; its git log holds them.
- **Watch out**: android/terminal still rests on a full verification of the Termux-era terminal, so a full refresh is due.
- **Source**: a75981b (applied to the service map in c2f4912)
