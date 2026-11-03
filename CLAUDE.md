## Language

- Write code comments, doc comments and developer-facing messages (assertions, panics, logs, script output) in English, using the terms of the module's map.
- `docs/rounds/` (frozen, see Records below) is in Chinese; user-facing Android strings are localized in `res/values*/strings.xml`.

## Knowledge Loop Conventions

### Before editing code

1. Resolve every module the change touches through `.claude/MODULES.md`; map `/` in an ID to subdirectories.
2. Read `.claude/maps/<module>.md` and `.claude/decisions/<module>.md` when present for every affected module. Do not load unrelated module files; if output is truncated, continue with focused ranges until each required file is complete.
3. Expand source context only to answer a named unresolved question. Stop when the behavior, change boundary, constraints, and verification path are clear.
4. Run `git log --oneline -10 -- <path>` for files about to change. Use `git show` only for recent commits relevant to the current behavior, including those with matching `MODULE: <current module>` Decision blocks.

### After finishing a task

- If implementation changes invalidate an existing module map, use `/map-module`: targeted refresh when the impact is bounded, otherwise full refresh. If unavailable, report the map as stale.
- In every `/commit-context` Decision, fill `MODULE`, `WHY`, `ALTERNATIVES`, `CHOSEN`, `TRADEOFFS`, and `RISKS`; add `SUPERSEDES` when replacing a prior Decision.

### Records

- A change is recorded in its commit (`/commit-context`: Decision blocks; measurements, survey results and how it was tested go in the body) and in the module maps. `docs/rounds/` is frozen after round 208: do not add rounds, index rows or "→" marks.
- Deferred work and problems found but not fixed go into the affected map's "Confirmed bugs / technical debt" or "Open questions" through `/map-module`, and are removed from it when done; pitfalls worth knowing later go into its "Invariants and gotchas".
- A deferred item from an old round that comes up again: check it against the current code, and if it is still open, move it into its module's map.
- Cross-change reference (measurement conditions, R1 measurements, known flaky tests) stays in `docs/milestones.md`. Work with no diff (surveys, investigations) gets a short doc under `docs/` only when worth keeping.
