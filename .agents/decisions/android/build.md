# Android build and packaging (android/build) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/build"`
>
> Last distilled: 2026-11-16 (HEAD = 5bd37526)
>
> Decisions on android/build made before it was split from android/native keep their `android/native` tags and live in `.agents/decisions/android/native.md`: D2–D7, D10, D12–D14, D20–D22, D24–D26, D28 and the dependency portion of D32.

## Active

### D1: Android packaging is separate from runtime JNI

- **What**: android/build owns build configuration, dependencies, packaged assets and release sources, while runtime JNI remains in android/native.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14

### D2: The manifest explicitly exposes supported app languages

- **What**: The application references locale_config for en, zh-CN, zh-Hant, ko and ja, enabling the Android 13+ picker while Android 11–12 follow the system language.
- **Why**: System language choices must match the translations the app ships.
- **Tradeoffs**: The explicit list and build map require manual synchronization, with UnusedAttribute suppressed for the application declaration.
- **Watch out**: Vendor picker behavior remains unverified and the application-level suppression also covers later unused attributes on that element.
- **Source**: e406fc21, 5bd37526
