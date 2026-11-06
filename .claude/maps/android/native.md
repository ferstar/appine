# android/native Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/native.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 1 independent verifier — 6 confirmed; bugs and debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: Kotlin compiler and lint warnings fail the build, lint checks on outside data informational; route parsing)

## Responsibilities

`android/native` connects the Kotlin app to the Rust kernel and builds the APK:

- the JNI library `libappine.so` (`hosts/android`): a handle table of kernels, one static function
  per kernel or store operation, error mapping, stderr to logcat;
- `Native.kt`, the Kotlin facade;
- the Gradle build: the native library via cargo-ndk, the rootfs, doas and notice assets,
  per-variant features, SDK levels, packaging;
- building libghostty-vt, the terminal emulator, with Zig from the fetched Ghostty source;
- the manifest and debug-only resources;
- the instrumentation tests (`MachineTest`);
- the source bundles released with the APK (`scripts/release-sources`).

Root: `hosts/android/` (except `src/vt.c` and `src/vt.rs`),
`android/app/src/main/java/io/github/mthli/appine/Native.kt`, `android/app/build.gradle.kts` and
the other Gradle files, `android/app/src/main/AndroidManifest.xml`, `android/app/src/debug/`,
`android/app/src/androidTest/`, `android/licenses/`, `scripts/release-sources`,
`scripts/fetch-ghostty`.

Outside the module: the `Machine` API and the store functions (`kernel/process`, `kernel/store`);
what the app does with them (the other `android/*` modules), including the libghostty-vt
terminal's emulator shim, JNI functions, sessions and views (`android/terminal`).

## Key types

- `MACHINES` — a mutex-protected map from handle (`jlong`, monotonic from 1, never reused) to
  `Arc<Machine>`.
- `Native` — `internal object` with `@JvmStatic external` functions; loads the library and calls
  `init(BuildConfig.DEBUG)` in its initialiser.
- Gradle tasks: `CargoNdkTask`, `RootfsAssetTask`, `NoticesTask`.

## Public entry points

- Init: `init(verbose)` (stderr redirect, debug log).
- Store: `importRootfs`, `syncRootfs`, `writeFile`, `readFile` (null when missing).
- Browse: `browseStat`, `browseList`, `browseOpen`, `browseSniff`, `browseSearch` (read);
  `browseCreate`, `browseRemove`, `browseRename`, `browseMove`, `browseOpenWrite`, `browseWritten`
  (through the store's change path).
- Kernel: `start(store, tier)` (1 = Tier 1, else Tier 0), `stop`, `boot`, `shutdown`, `takePower`,
  `processes`, `trimMemory`, `setRoutes`, `takeKills`, `takeListens`, `takeChangedDirs`.
- Consoles and processes: `openConsole`, `spawn` (negative console = the app's stdio), `read`,
  `write` (until all bytes are written), `resize`, `closeConsole`, `waitFor`, `consoleCwd`.

## Data flow / lifecycle

### Handles

- `start` creates a `Machine` and registers it; every call clones the `Arc` under the table lock,
  drops the lock and works, so a blocked read keeps its kernel alive across a concurrent `stop`.
- `stop` removes the handle and then kills everything; `shutdown` runs the inittab shutdown
  (poweroff, 30 s) and then removes it. Both drop the removed `Arc` only after the table lock is
  released, since the last reference's drop syncs the store and joins threads. Later calls find
  nothing: reads and writes return −1, `waitFor` −9, `openConsole`/`spawn` throw, the rest do
  nothing.
- Errors become `IOException` (`FileNotFoundException` for browse calls) or sentinel values.
- Callers are JVM threads (the service's worker and boot threads, the terminal's reader, writer and
  waiter threads, the main thread, provider binder threads, the libghostty-vt tabs' pump
  threads, which `vt.rs` calls back into Java from inside `pump`); guest and timer threads never
  call back into Java.

### stderr and panics

- `init` replaces fd 2 with a pipe read by a thread that writes lines to logcat (tag `appine`) and
  sets `APPINE_LOG` when verbose.
- Panics are contained only where the kernel wraps the call (`host_call`, on whenever debug
  assertions are off, which is always on Android); the JNI layer has no `catch_unwind`, so a panic
  elsewhere aborts the app (unwinding out of `extern "system"`). Unwrapped: `Machine::wait`,
  `try_wait`, `processes`, `take_power`, the wait loops of console reads and writes, `boot`,
  `shutdown` and `kill_all`, `Machine`'s drop, `sync_store`, and the JNI code itself.

### Build

- cargo-ndk builds `arm64-v8a` for API 29, always in release, into `target/android`; debug variants
  add the `test-hooks` and `borrow-check` features.
- On an Android target, `build.rs` runs `zig build -Demit-lib-vt` (baseline CPU, ReleaseFast) on
  the Ghostty 1.4.0 source in `target/ghostty` (or `APPINE_GHOSTTY_SRC`) into `OUT_DIR`, with the
  proxy variables removed, then compiles `src/vt.c` with `cc` and links `libghostty-vt.a`
  statically into `libappine.so` (with uucode, simdutf, Highway, Wuffs and Zig's compiler_rt;
  about +2 MB stripped). Zig is `zig` on `PATH` or `ZIG` (0.17.0, which Ghostty 1.4.0 requires);
  Ghostty's build finds the NDK through `ANDROID_NDK_HOME`. Every Android build, CI's included,
  therefore needs Zig and the fetched source.
- `scripts/fetch-ghostty [dir]` downloads the source release, checks its pinned sha256, unpacks it
  and runs the build's configure phase with `build.rs`'s options (`--help`), which fetches the Zig
  packages that build uses into the source's `zig-pkg/` (Zig checks them against the
  `build.zig.zon` hashes), so the cargo build needs no network.
- Android libraries: `androidx.core` and Material Components 1.14.0 (which brings AppCompat,
  androidx.activity, CoordinatorLayout and the Error Prone annotations).
- Assets: the newest Alpine minirootfs tarball (as `rootfs.tgz`, since AAPT would unpack `.gz`),
  `doas.apk`, generated `notices/`, and the terminal font (`fonts/`, `android/terminal`);
  `BuildConfig` carries the Alpine branch.
- Notices (`NoticesTask`): appine's notice, then the terminal's components with their license
  texts from `android/licenses/` (Ghostty, uucode with the Unicode and Hoehrmann licenses, simdutf,
  Highway and Wuffs under their MIT or BSD option, Zig's compiler_rt, JetBrains Mono's OFL),
  Alpine, the Android libraries, the crates, SQLite and full texts. Every Android library on the
  runtime classpath must belong to a group known to be Apache-2.0 (AndroidX, Kotlin, kotlinx,
  JetBrains annotations, Guava's listenablefuture, Material Components, Error Prone annotations,
  JSpecify), or the task fails. The component texts and
  versions are copies kept by hand and change with the Ghostty version.
- SDK: min 29, compile and target 37; uncompressed native libraries (`useLegacyPackaging = false`)
  so 16 KB-aligned `LOAD` segments (NDK 29's default) map directly from the APK.
- Warnings fail: every Kotlin compilation (the app and the instrumentation tests, local builds
  included) treats compiler warnings as errors, and lint treats its warnings as errors, except
  the checks that follow data outside the repository (newer libraries or AGP, the newest SDK
  level, Play's target SDK deadlines, libraries marked outdated in the Play SDK Index), which are
  informational.
- Manifest: launcher, settings and licenses activities (the application theme `Theme.Appine`,
  Material 3; the launcher overrides it with `Theme.Appine.Terminal`; settings and licenses leave
  `uiMode` out of their configuration changes), the `specialUse` foreground service with its subtype
  property, the documents provider, a handful of normal permissions (no storage permission); the
  debug network security config allows cleartext to 127.0.0.1 only.

### Released sources

- `scripts/release-sources [dir]` (default `target/release-sources`) refuses to run with tracked
  changes in the work tree and writes `appine-<rev>.tar.gz` (`git archive HEAD`),
  `appine-<rev>-vendor.tar.gz` (`cargo vendor`), `ghostty-<version>.tar.gz` (the release as
  published) with `ghostty-<version>-zig-packages.tar.gz` (the source's `zig-pkg/` after
  `fetch-ghostty` with an empty Zig global cache, so only what the build fetches: uucode, Highway,
  Wuffs with its test images, zlib and Ghostty's themes (configured, not linked), and the build
  tools aro and translate-c), and
  `alpine-<version>-gpl-sources.tar`: for each
  GPL origin listed in the bundled minirootfs's `lib/apk/db/installed`, the aports directory at its
  commit (GitLab archive by subdirectory) plus the upstream files named in the APKBUILD's
  `sha512sums`, fetched from Alpine's distfiles mirror and checked against those sums; APKBUILDs
  are never executed.

### Tests

- `MachineTest` runs on emulators with 4 KB and 16 KB pages: boot and shutdown by inittab, consoles,
  login and doas, restore, documents, mirrors, downloads (some opt-in with network), memory trim,
  and kernel or engine bugs injected through the test hooks.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`, `ConsolePty`, `Rootfs`, `Documents`, `MachineTest`; the terminal
  (`android/terminal`) through the libghostty-vt build.

### Outbound

- `appine_kernel` (`Machine`, store and browse functions), `jni` 0.21, `libc`, Android `liblog`;
  cargo-ndk and NDK 29; AGP; `androidx.core` and Material Components from Google's Maven
  repository.
- Zig 0.17.0, the Ghostty 1.4.0 source release and its Zig packages, the `cc` crate.

## Invariants and gotchas

- JNI symbol names hard-code `io.github.mthli.appine.Native` (and `VtNative` for the libghostty-vt
  terminal's functions).
- The crate is empty on non-Android hosts; `build.rs` does nothing there.
- Zig's HTTP client fails through some proxies (HTTP 400), so `scripts/fetch-ghostty` and `build.rs`
  remove the proxy variables.
- `build.rs` and `scripts/fetch-ghostty` pass the same `zig build` options and change together:
  lazy Zig packages are fetched only for the options the configure phase sees.
- Zig 0.17 unpacks every package already in its global cache into `zig-pkg/`, needed or not; only
  an empty cache shows what a build uses.
- The K2 compiler reports unused variables and never-read assignments only with its extra
  warnings, which are off: they flag a variable read only by a local function as never read.

## Confirmed bugs / technical debt

- **Panics outside the kernel's wrapped calls abort the app** (see the list above).
- The release build is signed with the debug key and not minified; the Alpine tarball is picked by
  the lexically greatest file name (3.9 sorts after 3.10), with the checksum checked only in
  `scripts/fetch-rootfs`.
- Local references are never deleted in loops that build arrays (harmless with ART's large table
  on API 29+); a failed string conversion with no pending exception makes void calls return
  silently (unreachable from the non-null Kotlin signatures).
- `scripts/fetch-ghostty` checks only the pinned sha256; the release's minisign signature is to be
  checked by hand once when upgrading, before pinning the new sha256.

## Open questions

- Should the JNI layer catch panics itself?

## To verify

none
