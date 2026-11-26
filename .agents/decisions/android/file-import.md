# Terminal file imports (android/file-import) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/file-import"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on android/file-import made before it was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D4 and D62.

## Active

### D1: Retained file-copy work is separate from activity callbacks

- **What**: android/file-import owns the retained file-import helper, while picker callbacks, final authorization and result handling remain in android/terminal.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: Imports stage the source before publishing into the login home

- **What**: Validate the encoded home, stream into an unlinked cache file on a cancellable worker, then publish under the rootfs lock and notify the provider.
- **Why**: A slow or failing source must leave reset available and partial files invisible.
- **Tradeoffs**: A completed import passes through temporary storage.
- **Watch out**: A concurrent reset or name collision must be checked at publication.
- **Source**: 6e9bd7a7, 6f2776f3, f6a9510b, f495f2cc

## Superseded

- ~~Reading potentially slow provider input inside the publication callback~~ → replaced by **D2** in f495f2cc (2026-11-24)
