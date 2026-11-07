# appine Android app

The Gradle project for the Android shell (design doc §3 and the `android/*` maps; terminals in the `kernel/tty` map):

- `app/`: the Kotlin app. `TerminalService` is a `specialUse` foreground service that
  owns the guest kernel and the terminal sessions; `MainActivity` shows them with tabs
  and a row of extra keys. The terminal emulator is libghostty-vt from Ghostty, linked into
  `libappine.so` (`hosts/android/src/vt.c`, `vt.rs`), and drawn by `VtView` with the bundled
  JetBrains Mono font. `Rootfs` imports Alpine from the APK assets on first launch,
  picks the fastest mirror, and can replace it with Debian (downloaded and checked
  against a pinned sha256 digest). `SettingsActivity`, opened from the tab bar's menu,
  switches the distribution, resets the rootfs, chooses the package mirror, the login
  account and, in debug builds only, the execution tier (release builds always run Tier 1).
  Each tab runs `assets/login.sh` as root: it picks the
  login account (the first regular user by default), gives a regular user doas or sudo the
  first time it logs in (the doas package from `scripts/fetch-rootfs` is in the assets),
  and switches to it with `su` (`--session-command` where su is util-linux's, so that the
  shell keeps its controlling terminal and job control). `Native` is the JNI interface to
  `libappine.so`, which is built from `hosts/android` with cargo-ndk.

## Building

Requirements: the Android SDK with NDK 29.0.14206865, JDK 17 or later,
`rustup target add aarch64-linux-android`, `cargo install cargo-ndk`, Zig 0.17.0 on `PATH`
(or in `ZIG`), the rootfs tarball (`scripts/fetch-rootfs`) and the Ghostty source with the
Zig packages its build uses (`scripts/fetch-ghostty`), both run from the repository root.
Behind a proxy, `fetch-ghostty` and the build drop the proxy variables for Zig, whose HTTP
client fails through some proxies.

```sh
./gradlew installDebug                 # build and install on a connected arm64 device
./gradlew connectedDebugAndroidTest    # instrumentation tests on a device or emulator
```

The native library is always built in release mode: the interpreter is too slow in a
debug build. Only `arm64-v8a` is built. Run the instrumentation tests on both a 4K-page
and a 16K-page system image. `downloadedDistributionsRun` downloads Debian
(about 50 MB) and is skipped unless
`-Pandroid.testInstrumentationRunnerArguments.network=true` is given. An emulator started
with `-http-proxy` sends connections to 127.0.0.1 to the proxy too, which breaks
`downloadsResumeWhereTheyBrokeOff`. Setting the system proxy instead
(`adb shell settings put global http_proxy 10.0.2.2:<port>`) only affects the app's own
HTTP downloads and leaves 127.0.0.1 alone; clear it afterwards with
`adb shell settings put global http_proxy :0`. Deleting the setting instead leaves
`global_http_proxy_host` and `global_http_proxy_port` behind, and the proxy stays in effect
(with an empty exclusion list, so 127.0.0.1 goes to the proxy too) until the next reboot.

## Open source licenses

`NoticesTask` in `app/build.gradle.kts` writes the notices shown under Settings → Open source
licenses into the assets (`notices/`): appine's AGPL notice, Fuchsia's BSD-style license for
the parts of the line discipline adapted from Starnix (`licenses/fuchsia-LICENSE`, the same
commit `core/kernel/src/tty.rs` names), Ghostty's libghostty-vt with the
libraries it links (uucode, simdutf, Highway, Wuffs, Zig's compiler_rt) and the JetBrains Mono
font (their texts are in `licenses/`, taken from the sources `scripts/fetch-ghostty` downloads;
update them with its version), the packages of the bundled Alpine minirootfs (from its `lib/apk/db/installed`, with the aports commit of
each), the Android libraries on the runtime classpath, every Rust crate linked into
`libappine.so` with the license files it ships (only the MIT ones when MIT is one of the
choices), SQLite, and the full texts in `licenses/`. The build fails when a new Android library
is not in one of the groups it knows to be Apache 2.0; check its license before adding it.
