# appine Android app

The Gradle project for the Android shell (design doc §3, §9, §11):

- `app/`: the Kotlin app. `TerminalService` is a `specialUse` foreground service that
  owns the guest kernel and the terminal sessions; `MainActivity` shows them with tabs
  and a row of extra keys. `Rootfs` imports Alpine from the APK assets on first launch,
  picks the fastest mirror, and can replace it with Debian (downloaded and checked
  against a pinned sha256 digest). `SettingsActivity`, opened from the tab bar's menu,
  switches the distribution, resets the rootfs, chooses the package mirror and the
  execution tier. `Native` is the JNI interface to `libappine.so`, which is built from
  `hosts/android` with cargo-ndk.
- `terminal-emulator/`, `terminal-view/`: vendored from Termux (see below).

## Building

Requirements: the Android SDK with NDK 29.0.14206865, JDK 17 or later,
`rustup target add aarch64-linux-android`, `cargo install cargo-ndk`, and the rootfs
tarball (`scripts/fetch-rootfs`, run from the repository root).

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

## Vendored code

`terminal-emulator/` and `terminal-view/` come from
[termux/termux-app](https://github.com/termux/termux-app) at commit
`8629e632fcb95da272221be327db653fb24befe9`. Changes:

- `JNI.java` and the native subprocess code (`src/main/jni`) are removed.
- `TerminalSession.java` is rewritten: instead of `JNI.createSubprocess` on a host PTY,
  it talks to appine's in-process PTY through the `TerminalSession.Pty` interface. Its
  thread structure and public methods are unchanged.
- The Gradle build files are new.

License: termux-app is released under the GPL 3.0 only (`licenses/termux-app-LICENSE.md`);
its statement says these two libraries use code from jackpal's Terminal Emulator for Android
under the Apache 2.0 (`licenses/jackpal-NOTICE.txt` is that project's NOTICE). About half of
the vendored files were written by Termux contributors after 2016 and the rest were changed
by them since, so the libraries are treated as GPL 3.0. appine itself is AGPL 3.0 or later,
which can be combined with them (section 13 of both licenses), so risk R3 in the design doc
no longer applies.

## Open source licenses

`NoticesTask` in `app/build.gradle.kts` writes the notices shown under Settings → Open source
licenses into the assets (`notices/`): appine's AGPL notice, the Termux libraries, the packages
of the bundled Alpine minirootfs (from its `lib/apk/db/installed`, with the aports commit of
each), the Android libraries on the runtime classpath, every Rust crate linked into
`libappine.so` with the license files it ships (only the MIT ones when MIT is one of the
choices), SQLite, and the full texts in `licenses/`. The build fails when a new Android library
is not in one of the groups it knows to be Apache 2.0; check its license before adding it.
