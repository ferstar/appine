# Knowledge loop (knowledge-loop) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: knowledge-loop"`
>
> Last distilled: 2026-11-10 (HEAD = 300d384)

## Active

### D1: Changes are recorded in commits and module maps; `docs/rounds/` is frozen after round 208
- **What**: Each change is recorded in its `/commit-context` commit (Decision blocks; measurements, survey results and testing in the body) and in the module maps, with deferred work and unfixed problems in a map's "Confirmed bugs / technical debt" or "Open questions" and pitfalls in its "Invariants and gotchas"; the old rounds get no new rounds, index rows or "→", an old deferred item moves into its map only when it comes up again, `docs/milestones.md` keeps the cross-change reference, and work with no diff gets a short doc only when worth keeping.
- **Why**: Each change was written down three times (a round, a Decision block, a map refresh), and the round was the only copy that went stale and needed "→" marks added back into old files.
- **Tradeoffs**: The records are in English instead of Chinese; commit bodies cannot be amended, so the maps carry the live state; a change spanning many commits has no single narrative; open items in old rounds that were never moved can be forgotten.
- **Watch out**: A commit made without `/commit-context` leaves no record of why, since the user runs it by hand and no rule enforces it; old deferred items are found only by grepping; map debt sections grow if done entries are not removed.
- **Source**: 511866b (known flaky tests applied in 093cb96)

### D2: Distills turn migration scopes and implicit supersessions into entries by recorded judgment
- **What**: Each migration commit's module-scope Decision becomes one scope entry, a block replacing part of an entry is merged into the chain's latest entry, supersessions without a SUPERSEDES line or recorded under another module are listed under Superseded and flagged at review, hand-seeded entries naming removed code get minimal edits with the new hashes in Source, and "Last distilled" says which entries were seeded by hand; a fix spanning modules gets one entry per module that refer to each other, blocks sharing one approach form one cluster entry, entries a later block refines are edited with its hash added to Source, scope entries' Watch out names only bugs still open in the map, and a revert without a Decision and replaced parts of entries or clusters become Superseded lines; distills read develop only, a block that only changes a map's bug records is folded into the scope entry's Watch out and Source, and a commit recording a deviation joins the cluster of the commit that fixes it; a snapshot past about 30 full entries gets a constraint index of one-line bullets whose full text a named commit holds, while entries edited or added in a batch stay in full.
- **Why**: Five snapshots had Decision blocks committed after their hand seeding and knowledge-loop had none, distilling from commits for the first time raised cases the snapshots had no rule for, and the first distill of every module added cross-module fixes, test-only blocks and refinements of existing entries; the fourth distill (89 blocks from 44 commits) met commits only on explore/hybrid, map-only bug records and recording commits followed by their fixes.
- **Tradeoffs**: Scope entries repeat part of `MODULES.md`, one-sentence fields and cluster entries drop details that stay in the source commits, a cross-module fix is spread over several snapshots, implicit and cross-module supersessions rest on judgment rather than SUPERSEDES lines, and the explore/hybrid commits' Decisions are in no snapshot.
- **Watch out**: Edited hand-seeded entries keep their IDs, which later distills must preserve, and possible merges left separate (android/setup D13 and D5, kernel/netns D10 and D4) may want merging later, as may android/terminal D22 into D18, D14 with D15 and android/setup D15 into D12; the 2026-11-08 distill gave kernel/vfs, kernel/mm, kernel/process, android/terminal and harness constraint indexes, and the 2026-11-10 distill indexed kernel/procfs and a second kernel/process batch, both pinned to 260512d; kernel/signal and kernel/net, at 26 entries each, are next.
- **Source**: 8349911, 4b0adb9, e868881, bfb1e44, 260512d, a536142

### D3: A map's Maintained line is one sentence
- **What**: A map's Maintained line names in one sentence the areas changed since its full mapping, the latest change last, and says what the full mapping predates; map-module asks for this on every targeted refresh.
- **Why**: The terminal map's line had grown to 2626 characters over 22 targeted refreshes, a changelog repeating commit subjects and naming features later removed.
- **Tradeoffs**: The map no longer lists each refresh; its git log holds them.
- **Watch out**: None outstanding: android/terminal was fully refreshed on 2026-11-10 (d9e1f0c) and every map now uses the current wording.
- **Source**: a75981b (applied to the service map in c2f4912 and to five more maps in 3f3db11)

### D4: Files with no module are registered when found
- **What**: A kernel file without a module gets a registry line in the module that owns its behaviour (`syscall/posix_timer.rs`, `syscall/mod.rs`, `bug.rs` and `syscall/misc.rs` outside kernel/signal's handlers in kernel/process, `errno.rs` and `debug.rs` in kernel/signal, native AIO in kernel/vfs, `oom.rs` and `uaccess.rs` in kernel/mm, SysV IPC and message queues in a new kernel/ipc), its module doc comment names that map, and maps written before later splits are repointed at the current owners.
- **Why**: `MODULES.md` named a `posix_timer.rs` that never existed, about 2300 lines of IPC plus AIO, the OOM killer and the dispatcher's, bug handling's, errno's and debug log's files resolved to no module, so their behaviour and defects were recorded nowhere, and the registry, maps and doc comments named different owners for some.
- **Tradeoffs**: Message queues and cgroup memory enforcement are spread over several maps, and `syscall/misc.rs`'s owner is a default whose membarrier, umask and capability and seccomp prctls other maps describe.
- **Watch out**: A new kernel file needs a registry line or no map covers it, and owners split inside one file (`syscall/misc.rs`, `syscall/mod.rs`, `vfs/mod.rs`, `syscall/mount.rs`) exist only in the registry's wording.
- **Source**: 4140c75, 3f3db11, d2b1dc3

### D5: Survey conclusions without a diff go into one doc that points at the maps
- **What**: The two Starnix surveys live in one English `docs/starnix.md` whose ranked ideas point at the deviations recorded in the maps, with a "Taken so far" table updated with the fixing commits and no milestones index row, since rounds are frozen.
- **Why**: The first survey's conclusions lived only outside the repository, and the second survey's ideas had no code change to carry them.
- **Tradeoffs**: The ranked ideas and run counts are a snapshot at 8dc166b, and ideas that become work must move into maps or commits.
- **Watch out**: The doc and the maps drift unless "Taken so far" is updated each time an idea is taken, and that update relies on exact matches of map lines.
- **Source**: 612d238, 2d78930

### D6: Prose names the system Appine; the frozen rounds keep the lowercase name
- **What**: Every sentence or title naming the system in `docs/design`, `docs/milestones.md`, `docs/starnix.md`, `android/README.md`, the maps, the decisions, source comments, crate descriptions and `appine-run`'s help says Appine, identifiers and labels stay lowercase (harness D23), and `docs/rounds` stays as written.
- **Why**: The app had been renamed Appine with developer docs and comments left for a separate pass, so the README said Appine while the design doc, maps and decisions still said appine.
- **Tradeoffs**: The frozen rounds and old commit messages keep the lowercase name, so a search for the product name has to be case-insensitive.
- **Watch out**: New prose, maps and Decision blocks must say Appine, and a distill copying old commit text into `.claude/decisions` can bring the lowercase name back.
- **Source**: 248d79f

### D7: Subsystems that outgrow the vfs map become modules of their own
- **What**: FUSE became kernel/fuse with a fully verified map of its own, and the mount table, mount syscalls and mount namespaces became kernel/mount, split out of the vfs map with the facts its full mapping had verified that day; the registry now has 24 modules.
- **Why**: FUSE is a self-contained subsystem of about 3,500 lines, and the vfs map, read whole before every VFS change, was near its limit and then reached 1426 lines.
- **Tradeoffs**: More modules to keep in step with kernel/vfs, and boundaries that run inside `vfs/mod.rs` and `syscall/mount.rs`.
- **Watch out**: The mount decisions made before the split stay in kernel/vfs's snapshot, which kernel/mount's map points to until a kernel/mount distill.
- **Source**: c008749, d2b1dc3

## Superseded

- ~~`errno.rs`, `uaccess.rs`, `debug.rs`, `bug.rs` and `syscall/mod.rs` have no module (D4's watch-out, 4140c75)~~ → replaced by **D4** in d2b1dc3 (2026-11-10)
