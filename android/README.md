# appine Android app

The Gradle project for the Android shell (design doc §3, §9, §11):

- `app/`: the Kotlin app. `TerminalService` is a `specialUse` foreground service that
  owns the guest kernel and the terminal sessions; `MainActivity` shows them with tabs
  and a row of extra keys. `Rootfs` imports the Alpine minirootfs from the APK assets on
  first launch and picks the fastest mirror. `Native` is the JNI interface to
  `libappine.so`, which is built from `hosts/android` with cargo-ndk.
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
and a 16K-page system image.

## Vendored code

`terminal-emulator/` and `terminal-view/` come from
[termux/termux-app](https://github.com/termux/termux-app) at commit
`8629e632fcb95da272221be327db653fb24befe9`. Changes:

- `JNI.java` and the native subprocess code (`src/main/jni`) are removed.
- `TerminalSession.java` is rewritten: instead of `JNI.createSubprocess` on a host PTY,
  it talks to appine's in-process PTY through the `TerminalSession.Pty` interface. Its
  thread structure and public methods are unchanged.
- The Gradle build files are new.

License: the termux-app repository is GPLv3. Its `LICENSE.md` says these two libraries
use code from jackpal's Android-Terminal-Emulator under Apache 2.0, but does not say what
license covers the files that Termux added later (such as sixel support). This is risk R3
in the design doc and must be resolved before a public release.
