# JNI bridge and build (android/native) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/native"`
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the retired design doc §11 and
> the round records in `docs/rounds/`; no `MODULE: android/native` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: Kernels cross JNI as handles into a table
- **What**: Kotlin holds a numeric handle; each call looks up the `Arc<Machine>` in a global table and clones it; `stop` and `shutdown` remove the entry, after which calls find nothing and return sentinels or throw.
- **Why**: The first design passed an `Arc` raw pointer and required Kotlin not to use it after `stop`; on a reset, the main thread closed a console after the worker had already freed the kernel and hung on a freed lock.
- **Tradeoffs**: Every call takes the table lock briefly.
- **Watch out**: Drop the table guard before dropping the last `Arc`, since `Machine`'s drop kills processes and joins threads.
- **Source**: rounds 07 (`07-m5`), 99 (`99-choose-rootfs`)

### D2: The native library is always a release build; debug apps add test hooks and the borrow check
- **What**: cargo-ndk builds `arm64-v8a` in release for every variant; debug variants enable `test-hooks` (the bug-simulation syscall) and `borrow-check`.
- **Why**: The instrumentation tests need the hooks to simulate kernel and engine bugs inside the app process, and the release APK must not contain them (checked in the `.so`); the BKL borrow check, once debug-only, was made a feature so release builds can run it too.
- **Tradeoffs**: Rust debug assertions never run on Android.
- **Source**: rounds 07 (`07-m5`), 21 (`21-robustness-finish`), 89 (`89-release-borrow-check`)

### D3: The APK is arm64 only and ships its native library uncompressed
- **What**: minSdk 29, target 37, `arm64-v8a` only; `useLegacyPackaging = false` so the 16 KB-aligned library maps straight from the APK; the rootfs asset is named `rootfs.tgz`.
- **Why**: Only aarch64 hosts are supported (Tier 1 translates aarch64 to aarch64); NDK 29 aligns `LOAD` segments to 16 KB, so uncompressed storage works on 16 KB-page devices; AAPT would unpack an asset ending in `.gz`.
- **Source**: round 07 (`07-m5`)
