# android/native Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/native.md` for the paired decision history.
>
> Verified: 2026-11-10 (full mapping: 3 research concerns; 16 claims checked by 2 independent verifiers — 14 confirmed and 2 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-14 (targeted verification only since the full mapping; latest change: file-import store and lifecycle regression coverage)

## Responsibilities

`android/native` connects the Kotlin app to the Rust kernel and builds the APK:

- the JNI library `libappine.so` (`hosts/android`): a handle table of kernels, one static function
  per kernel or store operation, error mapping, panic containment, stderr to logcat;
- `Native.kt`, the Kotlin facade;
- the Gradle build: the native library via cargo-ndk, the rootfs, bundled package and notice assets,
  per-variant features, SDK levels, packaging, warnings as errors;
- building libghostty-vt, the terminal emulator, with Zig from its fetched source release;
- the manifest and the debug source set;
- the instrumentation tests (`MachineTest`);
- the source bundles released with the APK (`scripts/release-sources`) and the licence texts in
  `android/licenses/`.

Root: `hosts/android/` (except `src/vt.c` and `src/vt.rs`),
`android/app/src/main/java/io/github/mthli/appine/Native.kt` and `MainApplication.kt`, `android/app/build.gradle.kts` and
the other Gradle files, `android/app/src/main/AndroidManifest.xml`, `android/app/src/debug/`,
`android/app/src/androidTest/`, `android/licenses/`, `android/app/src/main/assets/notices/`, `scripts/release-sources`,
`scripts/fetch-ghostty`.

Outside the module: the `Machine` API and the store functions (`kernel/process`, `kernel/store`);
what the app does with them (the other `android/*` modules), including the libghostty-vt
terminal's emulator shim, its JNI functions and sessions (`android/vt`) and its view
(`android/vtview`).

## Key types

- `MACHINES` — a mutex-protected map from handle (`jlong`, from `NEXT_HANDLE`, counting up from 1
  and never reused in a process) to `Arc<Machine>`; a poisoned lock is recovered. `vt.rs` looks
  kernels up in the same table.
- `Native` — `internal object` with 37 `@JvmStatic external` functions, 17 of them
  `@Throws(IOException::class)`; its initialiser loads the library and calls the private
  `init(BuildConfig.DEBUG)`.
- `guarded!` — the macro every JNI body runs in: `(name, fallback, body)`, or
  `(name, throws env, fallback, body)` for the functions Kotlin declares `@Throws`.
- Gradle tasks: `CargoNdkTask` (one per variant, `buildRust<Variant>`), `RootfsAssetTask` (one
  `rootfsAsset` shared by all variants), the first on `CargoTask` (the workspace and which cargo
  to run); helpers `byVersion` (compares file names by their runs of digits, then by how many
  runs), `newestFetched` (the newest file in `target/rootfs` whose name matches), `bashPackages`
  (the newest of bash, readline, libncursesw and ncurses-terminfo-base, null when any is missing)
  and `procpsPackages` (procps-ng, libproc2, libintl, utmps-libs, skalibs-libs and the two ncurses
  packages, null when any is missing), `bubblewrapPackages` (bubblewrap and libcap2, null when either
  is missing), and `alpineBranch` (from the tarball's name, `edge` without one).
- `MachineTest` — kernel and app instrumentation tests; `RunPolicyTest` — isolated preferences and idle-grace tests;
  `LicenseTest` — static notice coverage and offline reader rendering/restoration, without opening the store.

## Public entry points

- Init: `init(verbose)` (private; stderr redirect, debug log).
- Store: `importRootfs`, `syncRootfs`, `writeFile`, `readFile` (null on `ENOENT`).
- Browse: `browseStat`, `browseList`, `browseOpen`, `browseSniff`, `browseSearch` (read);
  `browseCreate`, `browseRemove`, `browseRename`, `browseMove`, `browseOpenWrite`, `browseWritten`
  (through the store's change path; `browseWritten` is not `@Throws` and ignores every error).
- Kernel: `start(store, tier)` (1 = Tier 1, else Tier 0; the kernel starts with
  `debug.exception-trace` at 1, so guest crashes reach the kernel log and with it the bug reports;
  0 and an `IOException("cannot start: …")` on failure), `boot`, `setRoutes`, `takeListens`,
  `takeKills`, `bklStall` (the latest too-long hold of the BKL as
  `<id>\t<seconds>\t<report>\t<end record or empty>`, tabs and line breaks in the last two turned
  into spaces, or null; it does not take the BKL), `takePower`, `processes`, `guestCpuTime`
  (live-plus-exited guest CPU nanoseconds under the BKL; −1 for a stopped handle or caught panic), `takeChangedDirs`,
  `trimMemory`, `shutdown`, `stop`.
- Consoles and processes: `openConsole`, `spawn` (negative console = the app's stdio), `read`,
  `write` (until all bytes are written), `resize`, `closeConsole`, `waitFor`, `consoleCwd`. The app
  itself no longer calls `read` or `resize` (the terminal's pump and resizes go through `vt.rs`);
  `MachineTest` does.
- Build: `./gradlew assembleDebug lintDebug` (what CI runs), `connectedDebugAndroidTest` (with
  `-Pandroid.testInstrumentationRunnerArguments.network=true` for the download of a distribution),
  `scripts/fetch-ghostty [dir]` (default `target/ghostty`), `scripts/release-sources [dir]` (default
  `target/release-sources`).

## Data flow / lifecycle

### Handles

- `start` builds `MachineOptions` (tier, exception trace), starts a `Machine` and registers it;
  every call clones the `Arc` under the table lock and releases the lock before working, so a
  blocked call keeps its kernel alive across a concurrent `stop`.
- `stop` removes the handle and then kills everything; `shutdown` runs the inittab shutdown
  (poweroff, 30 s) while the handle is still registered and then removes it. Both drop the removed
  `Arc` only after the table lock is released, since the last reference's drop (`kill_all`, sync,
  joining the timer thread, stopping the watchdog) can take long. Whichever thread finishes the
  last call on a removed kernel runs that drop.
- After the kernel is gone: `read` and `write` return −1, `waitFor` −9, `openConsole` and `spawn`
  throw "the machine is stopped", `takeListens`, `takeKills` and `takeChangedDirs` return empty
  arrays, `bklStall` and `consoleCwd` null, `takePower` and `processes` 0, `guestCpuTime` −1, and the rest
  (`boot`, `setRoutes`, `resize`, `closeConsole`, `trimMemory`, `shutdown`, `stop`) do nothing.
- Errors become `IOException`, or `FileNotFoundException` for every browse error whatever its
  errno, or sentinel values. `openConsole` and `resize` clamp rows and columns to 1–65535.
- `setRoutes` parses lines of `<hex destination>/<prefix> <hex gateway or -> <interface>` (later
  fields ignored); hex fields must be ASCII and even, the destination 4 or 16 bytes and the gateway
  the same length; the metric is 0 for IPv4, and for IPv6 1024 for a default route, else 256.
  Malformed lines are dropped silently.

### Callers

- All callers are JVM threads; the bridge attaches no native thread to the JVM, and guest and timer
  threads never call into Java. The main thread (`bklStall`, and `openConsole`/`spawn` from a tab's
  start), `appine-start` (`start`), the service's worker (the poll's calls, `setRoutes`,
  `consoleCwd`, `trimMemory`, `stop`, `shutdown`, the rules refresh's `spawn`), `appine-boot`
  (`spawn`, `waitFor`, `boot`), each tab's `vt-writer` (`write`) and `vt-waiter` (`waitFor`), the
  provider's binder threads (browse functions, `readFile`), and each tab's `vt-pump`, which reads
  the console through `Machine::read_console` in `vt.rs` and calls back into Java from inside
  `pump`.

### stderr and panics

- `init` redirects stderr once: fd 2 becomes a pipe read by a `stderr-log` thread that writes each
  line to logcat (tag `appine`, priority INFO; lines containing NUL are dropped); a failing `pipe`
  or `dup2` leaves stderr as it was. `verbose` sets `APPINE_LOG=1`.
- Panics are contained at two layers. The kernel wraps most `Machine` methods in `host_call`,
  which turns a panic into an error of the call when containment is on (`APPINE_ON_BUG=kill`, or
  by default whenever debug assertions are off, which is always on Android) and aborts the process
  otherwise. Everything else is caught by `guarded!`: `Machine::wait`, `processes`, `cpu_time_ns`, `take_power`,
  `stall`, `sync_store`, the Tier 1 pool trim, the wait loops of console reads and writes, the
  unwrapped parts of `boot`, `shutdown`, `kill_all` and `Machine`'s drop (their wait loops, flags,
  inittab plan parsing and thread joins; their signals, inittab reads, spawns and syncs run in
  `host_call`), and the JNI code itself, `vt.rs` included. `guarded!` runs the body in
  `catch_unwind`: the panic message reaches logcat through the panic hook's stderr, a
  line names the function, and the call returns its fallback (−1 for console reads and writes,
  `openConsole`, `spawn`, `browseOpen` and `browseOpenWrite`; 0 for `start`, `takePower` and
  `processes`; −9 from `waitFor`; null for objects; nothing for unit). The 17 `@Throws` functions
  also throw `IOException("internal error in …")` unless an exception is already pending. Kernel
  state a panic left half changed (a poisoned lock, a held BKL) is not repaired, so later calls
  may fail or wait.

### Build

- `CargoNdkTask` runs `cargo ndk -t arm64-v8a --platform 30 -o <build/generated/rust/<variant>/jniLibs>
  build --release -p appine-android`, with `CARGO_TARGET_DIR=target/android` and both
  `ANDROID_NDK_HOME` and `ANDROID_NDK_ROOT` set to AGP's NDK, using `~/.cargo/bin/cargo` when it is
  executable; it is never up to date. Debug variants add the `test-hooks` and `borrow-check`
  features; the Rust side is always a release build.
- On an Android target `build.rs` asserts the libghostty-vt source (`build.zig` in
  `target/ghostty/libghostty-vt-1.0.0`, or `APPINE_GHOSTTY_SRC`) and the release's Zig packages
  beside it (the same path with `-deps`), panics on any architecture but aarch64, and runs `$ZIG`
  (or `zig` on `PATH`) `build -Demit-lib-vt -Dcpu=baseline -Doptimize=ReleaseFast
  -Dtarget=aarch64-linux-android --system <deps>` into `OUT_DIR`, so Zig takes the packages from
  that directory and never fetches. Its global cache is the source's `.zig-global-cache`, beside
  Zig's local `.zig-cache` there, so builds of every profile (Gradle's, clippy's) share both. It
  then compiles `src/vt.c` (C11, extra warnings) with `cc`, against the installed headers and
  Wuffs's single-file header among the packages (`<deps>/<hash>/release/c`, the hash read from the
  source's `pkg/wuffs/build.zig.zon`; vt.c decodes PNG images with the Wuffs in the archive), and
  links `libghostty-vt.a` statically into `libappine.so` (with uucode, simdutf, Highway, Wuffs, the
  translate-c helpers and Zig's compiler_rt); `vt.rs` also links the system's libjnigraphics. The
  build finds the NDK through `ANDROID_NDK_HOME`, and libghostty-vt 1.0.0 requires Zig 0.17.x,
  which its own build checks (`minimum_zig_version`). Every Android build, CI's included,
  therefore needs Zig and the fetched release.
- `scripts/fetch-ghostty [dir]` handles the release's source tarball and its dependency tarball
  (`release.files.ghostty.org/libghostty-vt/<version>/`, each with its own pinned sha256): it keeps
  a tarball whose sha256 matches, else downloads it (`curl --retry 3` to a `.part` file, checked,
  renamed), and unpacks each into `<name>.part` and moves its directory into place whole, only
  when that directory is missing. It runs no Zig and needs no proxy handling.
- Toolchain: AGP 9.4.1 with its built-in Kotlin, Gradle 9.9.1, Java 17, NDK 29.0.14206865; plugins
  and dependencies from `google()` and `mavenCentral()` (project repositories fail the build). The
  version catalog (`android/gradle/libs.versions.toml`) holds the libraries' versions, and AGP's
  is in `android/build.gradle.kts`. Android libraries: `androidx.core` 1.19.1,
  `androidx.biometric:biometric` 1.1.0 (including its merged biometric permission), `androidx.preference:preference` 1.2.1, `androidx.browser:browser` 1.10.0,
  `androidx.webkit:webkit` 1.18.0 and Material Components 1.14.0 (which
  brings AppCompat and androidx.activity); tests use androidx.test's runner 1.7.0 and ext.junit
  1.3.0.
- Assets: the newest Alpine minirootfs tarball in `target/rootfs` by version (as `rootfs.tgz`,
  since AAPT would unpack `.gz`), the newest `doas.apk`, and bash's four packages under `bash/`
  with their own file names, plus procps-ng's seven packages under `procps/` (including copies of
  ncurses so either bundle installs independently), and bubblewrap/libcap2 under `bubblewrap/`
  (all by `byVersion`; `rootfsAsset` fails with "run
  scripts/fetch-rootfs first" when any is missing), checked-in `notices/`, and
  the terminal font (`fonts/`, `android/vtview`); `BuildConfig.ALPINE_BRANCH` carries the
  branch.
- SDK: min 30, compile and target 37; no ABI filter (only `arm64-v8a` is built); uncompressed
  native libraries (`useLegacyPackaging = false`) so 16 KB-aligned `LOAD` segments (NDK 29's
  default; the build sets no page-size flag) map directly from the APK.
- Warnings fail: every Kotlin compilation (the app and the instrumentation tests, local builds
  included) treats compiler warnings as errors, and lint treats its warnings as errors, except
  `GradleDependency`, `AndroidGradlePluginVersion`, `OldTargetApi`, `ExpiringTargetSdkVersion` and
  `OutdatedLibrary`, which follow data outside the repository and are informational.
- Release: signed with the debug key, not minified, versionCode 1 and versionName 1.0.0.

### Notices

- `app/src/main/assets/notices/` holds static HTML, a shared stylesheet and `index.json`, packaged
  without a Gradle generation task. The catalog lists, in order: Appine (its notice and the
  repository's `LICENSE`); Fuchsia's
  BSD-style licence for the line-discipline parts adapted from Starnix (`kernel/tty`); the
  terminal's components with their texts from `android/licenses/` (libghostty-vt 1.0.0, uucode
  with the Unicode and Hoehrmann licences, simdutf, Highway and Wuffs under their MIT or BSD option,
  translate-c, Zig's compiler_rt, JetBrains Mono's OFL); Alpine (each package's version, licence, origin and commit
  from the tarball's `lib/apk/db/installed`, then the bundled packages' from their `.PKGINFO`, doas
  and bash's four and procps-ng's five additional packages in sections of their own (shared ncurses
  counted only once), plus bubblewrap and libcap2, all counted in the entry's package count (28) and
  licences); the Android libraries on the
  runtime classpath; the Rust crates; SQLite (its version from libsqlite3-sys's `sqlite3.h`); full
  texts of GPL-3.0, GPL-2.0, LGPL-2.1, Apache-2.0 and MPL-2.0. `LicenseCatalog` (android/terminal)
  reads stable IDs, names, versions, chosen licenses and HTML file names, plus optional package
  counts and standalone-license kinds. Detail pages retain upstream alternatives and notices.
- Dependency and bundled-package upgrades must update these files in the same change; Gradle
  no longer discovers licenses or rejects unknown Android groups. Compare both runtime
  classpaths, the Android-target normal Rust dependency graph (excluding workspace members and
  proc-macros), and the selected Alpine artifacts. The initial debug/release inventories agree.
- `LicenseTest` checks the catalog's unique IDs, readable files and component notice coverage;
  it is not a general dependency freshness check. `android/README.md` describes maintenance.
- The component texts in `android/licenses/` (16 files) remain upstream reference copies,
  maintained with their corresponding HTML notices; the release's
  `deps/libghostty-vt-third-party.json` lists which projects are linked (translate-c among them,
  though nothing of it shows as an object of its own in `libghostty-vt.a`).

### Manifest

- Manifest application `MainApplication` lives in its own file and installs the shared page gate with `Privacy.install` in `onCreate`; non-exported `NotificationActivity` authenticates private report-copy actions, while non-exported `Notifications.ReportReceiver` copies ordinary reports without opening a page. AndroidX Biometric is listed in the static Android library notice; both runtime dependency inventories were checked.
- `MainApplication` also installs `Diagnostics` after the privacy gate. Non-exported
  `DiagnosticProvider` uses the existing AndroidX FileProvider with manifest path metadata,
  granting only generated archives from `cache/diagnostic-exports`; no new permission or
  dependency is required. Retained diagnostic files use the app's no-backup directory.
- Launcher, settings, licenses list and license reader activities (the application theme
  `Theme.Appine`, Material 3; the
  launcher overrides it with `Theme.Appine.Terminal`; settings and licenses leave `uiMode` out of
  their configuration changes; the non-exported reader handles no configuration changes itself),
  the `specialUse` foreground service with its subtype property, the
  documents provider, backup turned off (`allowBackup`, `fullBackupContent`, data extraction
  rules), normal permissions plus `POST_NOTIFICATIONS`, which `MainActivity` requests at run time;
  no storage permission. The debug source set's network security config allows cleartext to
  127.0.0.1 only (for the download tests).

### Released sources

- `scripts/release-sources` refuses to run with tracked changes in the work tree and writes, under
  the short `HEAD` revision: `appine-<rev>.tar.gz` (`git archive HEAD`), `appine-<rev>-vendor.tar.gz`
  (`cargo vendor --locked` of the whole workspace), `libghostty-vt-<version>.tar.gz` and
  `libghostty-vt-<version>-deps.tar.gz` (the release and its Zig packages, copied as
  `fetch-ghostty` checked them; together about 10 MB), and
  `alpine-<version>-gpl-sources.tar` (uncompressed, with a README listing the packages).
- The Alpine part takes the tarball `scripts/fetch-rootfs` prints last and the bundled packages
  it prints before it, picks packages whose licence (the tarball's `lib/apk/db/installed`, each
  package's `.PKGINFO`) contains `GPL` (LGPL included; so bash, readline, procps-ng/libproc2 and
  libintl, bubblewrap and libcap2, not doas, ncurses, utmps-libs or skalibs-libs),
  downloads each origin's
  aports directory at its commit (GitLab archive of `main/<origin>`), and for every file in the
  APKBUILD's `sha512sums` hashes the copy in that directory or fetches it from Alpine's distfiles
  mirror (the branch's directory, then `edge`); any miss or mismatch stops the script. APKBUILDs
  are never executed.

### Guest browser client

- `BrowserAssetTask` builds `hosts/android/open-url.c` with Zig as static aarch64-linux-musl,
  packaging `appine-open-url` as a generated asset for either distro; no guest package is needed.
  The bundled musl copyright is in `android/licenses/` and the offline notices.
- The client rereads `/etc/appine/browser`, connects to IPv4 loopback, sends token and URL lines
  and waits up to 5 s total for an `OK` reply. Invalid arguments/configuration, connection failures,
  refusal or a missing reply print an error and exit 1; URLs and tokens are never logged.
  Only HTTP(S) URLs up to 8192 bytes are accepted; the service performs strict UTF-8/URI checks.
  A timeout means no acknowledgment: it cannot prove no launch occurred or cancel buffered TCP requests.

### Tests

- `TerminalFilesTest` uses isolated stores for binary/empty imports, collisions, name/home
  escaping, read/close failure cleanup and missing homes; a real guest shell checks reads and
  clipboard quoting. A locked MainActivity checks pending-picker retention across recreation;
  all seven cases passed on API 34.
- `DiagnosticsTest` checks two-part byte limits and previous-run rotation, ZIP entries and
  report retention, own-PID capture excluding another process, read-only share intents and
  provider root confinement, plus rejection of locked/stale export callbacks and recreation.
  Save coverage includes ViewModel ownership across recreation, one-shot consumption and owner
  cleanup, plus isolation from share pruning and FileProvider access.
  It uses API 34 and does not bypass biometric authentication.
- `TerminalInputTest` uses an isolated store and native terminal, a shell-free session and a
  capturing view host to exercise IME replacement/commit/composition and clipboard menu callbacks.
  It checks confirmation and cancellation, no delayed replay, bracketed paste and its terminator,
  ordinary Chinese composition/Enter, and trailing-newline removal without trimming other
  whitespace (including unsafe/empty remainders and changed terminal modes) on API 34. The
  `screenshots=true` runner argument saves `cache/paste-preview.png` for visual inspection of
  the terminal-themed dialog; the tests do not drive Gboard's UI.
- `PrivacyTest` covers default-off/mutually exclusive file access, cold and recreated locked pages, pause-only visibility versus stop-and-return locking, secure dialog flags, non-exported action targets, service/broadcast/activity routing and locked-report clipboard denial without an authentication bypass. `MachineTest.documentsShowTheHomeDirectory` also checks every provider denial with privacy on and a stale true file-access preference.
- `BrowserTest` imports separate temporary stores and checks custom launcher preservation,
  launching without a TTY or stdio, root and `su` users, endpoint replacement with a surviving
  shell, authentication and URL validation, refusal/timeout, queued UI cancellation, and Custom Tab intents.
  Its `network=true` test installs tmux/screen and checks reattach on another console and nesting.
  It does not perform a real OAuth login.
- `RunPolicyTest` uses a separate preference file for legacy migration and remembered toggle
  targets, and checks idle expiry, activity, failed samples and delayed delivery with a fake clock.
  `MachineTest.guestCpuTimeTracksRunningAndExitedThreads` checks both tiers, exited and pooled
  threads, idle stability and stopped handles.
- `MachineTest` runs in the debug app's own process against the app's real store and
  preferences: `@Before` imports the bundled rootfs when no store is ready, `start(tier)` records
  each handle and `@After` stops them; tests change guest files, accounts and preferences and put
  them back in `finally`. It covers both tiers (guest page size 4096), calls after stop, memory
  trim, boot and shutdown by inittab (under 10 s), osclock, consoles (input, Ctrl-C, concurrency,
  the foreground cwd), login from `/etc/passwd`, the saved directory, accounts and doas (two logins installing it at once, and one waiting for apk's lock, included), NOPASSWD
  rules following a password set later, saved tabs, the Files provider (default-off access including
  legacy preferences, disabled queries/reads/changes and file links, authority-wide grant revocation,
  home, no home, writes,
  changes both ways, grants below a directory as a pure check of `Documents.grantedUnder`), manual
  mirrors, verified, resumed and cancelled downloads (`file:` URLs and a local server), routes, low
  ports, bugs injected through the test hooks killing only the guest process on both tiers, kill
  reports (including an OOM from a cgroup `memory.max`), a guest crash reaching the kernel log, and
  prompt marks (busybox ash's with root's shell set to `/bin/sh` for the test), and a
  new Alpine import logging in with bash (`newSystemsLogInWithBash`, its own store: the offline
  install, the `/bin/sh` fallback, bash's marks and nested shells'). `newSystemsInstallProcpsOffline`
  uses its own store and ash, omits a dependency to check failure/retry, retries in two concurrent
  logins without repositories, runs ps/top/free/vmstat/uptime/pgrep, and checks that user removal
  persists. `newSystemsInstallBubblewrapOffline` similarly uses its own store without bash/procps
  or repositories, checks a missing libcap dependency, concurrent retries and user removal, and
  runs root and unprivileged sandboxes with namespaces, read-only root and private tmpfs.
  Only `downloadedDistributionsRun` (about 50 MB of Debian, its own store) downloads
  from the internet, behind the `network=true` argument; `routesComeFromLinkProperties` has no
  gate but fails without a default network with an IPv4 gateway.
- The hooks need the debug variant (`test-hooks`) and `APPINE_TEST_HOOKS=1`, which the tests set
  and never unset. The KDoc and README ask for runs on 4 KB and 16 KB-page arm64 emulators; CI does
  not run the tests (they need KVM). Nothing here covers `Native.bklStall` (the stall itself is
  tested in `hosts/cli`), terminal gestures/rendering, real IME UI or persisted grants end to end.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`, `VtSession`, `ConsolePty`, `Rootfs`, `Documents`, `MachineTest`; `vt.rs`
  (`guarded!`, `machine`, `string`, `throw`, the handle table); `Rootfs` through the assets and
  `BuildConfig`; `LicenseCatalog` and `LicenseActivity` read the static notices;
  CI's `android.yml` (`assembleDebug lintDebug`, clippy
  through cargo-ndk); the terminal (`android/vt`) through the libghostty-vt build.

### Outbound

- `appine_kernel` (`Machine`, `MachineOptions`, routes, store and browse functions), `jni` 0.21,
  `libc`, Android `liblog` and `libjnigraphics`; cargo-ndk and NDK 29; AGP, Gradle;
  `androidx.core`, AndroidX Preference, AndroidX Browser, AndroidX WebKit, Material Components and androidx.test from Google's Maven repository and Maven
  Central.
- Zig 0.17.x, the libghostty-vt 1.0.0 source release and its dependency tarball, the `cc` crate;
  `scripts/fetch-rootfs`'s output in `target/rootfs`.
- `scripts/release-sources`: git, `cargo vendor`, `scripts/fetch-ghostty` and `fetch-rootfs`,
  gitlab.alpinelinux.org and distfiles.alpinelinux.org, curl, tar, awk and a SHA-512 tool.

## Invariants and gotchas

- JNI symbol names hard-code `io.github.mthli.appine.Native` (and `VtNative` for the libghostty-vt
  terminal's functions); each `external fun` has exactly one `Java_…` function.
- The crate is `#![cfg(target_os = "android")]`, so it is empty on other hosts, where `build.rs`
  only prints its rerun lines.
- Every JNI function's body goes in `guarded!` (with `throws env` exactly when Kotlin declares
  `@Throws`); the macro is defined before `mod vt` so `vt.rs` can use it, and its closure moves the
  parameters, so a `return` inside returns from the body only.
- The global cache a `--system` build uses must never have held a package's archive: the user's
  `~/.cache/zig` usually has, and Zig 0.17 then panics ("reached unreachable code"), so `build.rs`
  gives the source a cache of its own, which nothing fetches into.
- `build.rs` reruns only when `vt.c`, the source's `LIB_VERSION` or `build.zig.zon`,
  `APPINE_GHOSTTY_SRC` or `ZIG` change, not for other libghostty-vt files or `ANDROID_NDK_HOME`.
- vt.c's Wuffs header is found through the release's layout: the dependency named `wuffs` in
  `pkg/wuffs/build.zig.zon`, the dependency tarball's `<hash>` directories and the file
  `wuffs-v0.4.c`. An upgrade that changes any of them fails the build ("no Wuffs …" or no hash)
  until `build.rs` follows; the package is there whenever libghostty-vt builds, since it links Wuffs
  for Kitty graphics.
- The K2 compiler reports unused variables and never-read assignments only with its extra
  warnings, which are off: they flag a variable read only by a local function as never read.
- The instrumentation tests are not isolated from the app: they use its store, preferences and
  saved tabs (a store switched to another distribution breaks the Alpine assertions). Root logs
  in with bash on a store this version imported and with busybox ash on an older one, so
  assertions on a command's output in a login go through `plain`, which drops control sequences
  and CRs (bash puts its marks and bracketed paste right before the output).
- `release-sources` relies on `fetch-ghostty` printing `fetch-ghostty: <source>` with the two
  tarballs at `<source>.tar.gz` and `<source>-deps.tar.gz`, and on `fetch-rootfs` printing the
  bundled packages' paths (ending in `.apk`) and the tarball's path last.

## Confirmed bugs / technical debt

- The release build is signed with the debug key and not minified; the Alpine tarball's checksum
  is checked only in `scripts/fetch-rootfs`.
- Local references are never deleted in lib.rs's loops that build or read arrays (`string_array`,
  `takeListens`, `takeKills`, `strings`; harmless with ART's large table on every supported
  version); a failed string conversion with no pending exception makes void calls return silently
  (unreachable from the non-null Kotlin signatures). If the `stderr-log` thread fails
  to start, fd 2 is already the pipe and nobody reads it, so stderr writes block once it fills.
- `scripts/fetch-ghostty` checks only the pinned sha256s; the release's minisign signatures are to
  be checked by hand once when upgrading, before pinning the new sha256s.
- `scripts/release-sources` takes the Alpine sources from `fetch-rootfs`'s pinned tarball and
  packages while the APK bundles the newest ones in `target/rootfs`, so the two can differ; its
  aports path is fixed to `main/`; and it vendors the whole workspace although its header speaks
  of the crates linked into the app.

## Open questions

- Should browse errors other than a missing file (`EEXIST`, `EACCES`, `ENOSPC`) surface as
  `FileNotFoundException`?
- Is the release variant meant to ship as configured?

## To verify

- Diagnostic exports passed six instrumentation cases and seven privacy cases on API 34;
  debug/release assembly and lint passed. Manual checks covered authentication before picker
  launch, saved ZIP integrity, picker cancellation cleanup, locked return without another prompt,
  and process-death orphan cleanup. Real biometric devices, API 30 trace attachments
  and 16 KB-page device behavior remain unverified.
- The static browser launcher on Debian and a 16 KB-page device; current bridge tests use Alpine on API 34.

- That NDK 29's default really gives 16 KB-aligned `LOAD` segments in `libappine.so` and that the
  test suite passes on a 16 KB-page image today; ART's local-reference limit on API 30 and later.
