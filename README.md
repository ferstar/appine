# <img src="website/dist/favicon.svg" width="36" height="36" align="absmiddle" alt=""> Appine

Appine is a high-performance Linux terminal for Android. Install packages, edit
code and build projects on your phone or tablet without rooting it. Alpine Linux
comes bundled with the app. Debian is available from **Settings**.

**[Get Appine on Google Play](https://play.google.com/store/apps/details?id=io.github.mthli.appine)** · **[Download the APK from GitHub Releases](https://github.com/mthli/appine/releases)**

Requires Android 11 or later on a 64-bit ARM device.

With Appine, you can:

- Use **Codex CLI** to work on a local project from your Android terminal.
- Open source files and configuration in **Vim**.
- Run Python scripts, compile C and C++, or develop with Java, Node.js, Go and Rust.
- Run **Docker** containers on your device, for example an Nginx web server.
- View images in terminal programs that use the **Kitty graphics protocol**.
- And more...

## Try Codex CLI

**Alpine Linux** (default)

```sh
apk update
apk add nodejs npm
npm install -g @openai/codex
```

**Debian**

```sh
apt update
apt install nodejs npm
npm install -g @openai/codex
```

## How it works

Appine runs unmodified ARM64 Linux binaries inside the Android app process.
Its CPU engine translates ARM64 instructions into ARM64 code and uses an
interpreter for instructions it does not translate. A user-space kernel handles
Linux system calls, and the Android terminal uses Ghostty's libghostty-vt.
The Linux command-line runner uses the same engine and kernel.

## Development

### Build the Android app

Install JDK 17 or later, Android SDK Platform 37, NDK 29.0.14206865, Rust
with rustup, and Zig 0.17.0. From the repository root, fetch the build inputs
and install a debug build on a connected ARM64 device or emulator.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked
scripts/fetch-rootfs
scripts/fetch-ghostty
cd android
./gradlew installDebug
```

Run `./gradlew connectedDebugAndroidTest` from the same directory for the
instrumentation tests. See the [Android build guide](android/README.md#building)
for Android Studio setup, release builds and device tests.

### Develop and test the runtime

The primary development host is ARM64 Linux. On macOS, start a Docker-compatible
runtime such as Colima, OrbStack or Docker Desktop. From the repository root,
`scripts/dev` runs each command in the project's ARM64 Linux container.

```sh
scripts/fetch-rootfs
scripts/dev cargo test --workspace
```

The workspace tests cover the kernel, guest programs and CPU instruction execution.

#### Run a guest program or an Alpine shell

To run a hand-written guest program:

```sh
scripts/dev sh -c 'cc -nostdlib -static -no-pie -o /tmp/hello tests/guest/hello.S &&
                   cargo run -q -p appine-run -- --tier 1 /tmp/hello'
```

The C guest programs in `tests/guest` are built with `musl-gcc` from musl-tools, which the
`scripts/dev` container provides; the tests that need it are skipped when it is missing.

To run busybox from the Alpine minirootfs (the rootfs tests in `cargo test` need the
tarball too, and are skipped without it):

```sh
scripts/fetch-rootfs # downloads and verifies target/rootfs/alpine-minirootfs-*.tar.gz
scripts/dev sh -c 'cargo build -q -p appine-run && run=target/linux-arm64/debug/appine-run &&
                   $run --rootfs target/store --import target/rootfs/alpine-minirootfs-*.tar.gz &&
                   $run --rootfs target/store /bin/busybox ls -l / &&
                   $run --rootfs target/store --env PATH=/bin:/usr/bin /bin/sh -c "ls / | wc -l"'
```

When stdin and stdout are a terminal, `appine-run` runs the guest on an in-process
terminal, so an interactive login shell has job control (Ctrl-C, Ctrl-Z, `fg`) and
full-screen programs such as `vi` and `top` work:

```sh
scripts/dev target/linux-arm64/debug/appine-run --rootfs target/store \
    --env TERM=xterm-256color --env HOME=/root /bin/sh -l
```

The guest uses the host's network, so `apk` and `git` work. The guest only sees the
environment given with `--env`; if the host needs a proxy, pass it through:

```sh
scripts/dev sh -c 'target/linux-arm64/debug/appine-run --rootfs target/store \
    --env PATH=/usr/sbin:/usr/bin:/sbin:/bin --env HOME=/root \
    --env http_proxy=$http_proxy --env https_proxy=$https_proxy \
    /bin/sh -c "apk update && apk add git python3"'
```

The tests that need the internet run only with `APPINE_NETWORK_TESTS=1`.

#### Run Linux Test Project syscall tests

The syscall tests of the [Linux Test Project](https://github.com/linux-test-project/ltp)
are built natively in an `alpine` arm64 container and run inside Appine, one fresh
`appine-run` per test:

```sh
scripts/dev cargo build --release -p appine-run
scripts/build-ltp                        # builds target/ltp/ltp-<version>.tar (needs Docker)
scripts/dev scripts/ltp -j 8             # all syscall tests; results in target/ltp/results.txt
scripts/dev scripts/ltp 'fcntl.*|mmap.*' # only the tests whose name matches
```

#### Compare Appine with native Linux

`scripts/difftest` runs each script in `tests/diff` both in Appine and in a native arm64
Docker container, and compares their output and exit status. The container runs without
Docker's seccomp profile, which changes what some syscalls return (`clone3`, `pidfd_getfd`).
Both sides run the same files: the first run creates a store in `target/difftest/store` (minirootfs plus python3,
gcc, Lua, SQLite, Go, Node.js, OpenJDK 21, strace and gdb from `apk`, so it needs the internet), exports its
root filesystem from Appine and imports it as a Docker image. A store made before a package was
added lacks it: install the package into the store with `apk add`, or delete the store, and pass
`-r` to export it again:

```sh
scripts/fetch-rootfs
scripts/dev cargo build --release -p appine-run
scripts/difftest              # all cases on tier 1
scripts/difftest -t 0 signals # one case on the interpreter
```

Run it on the host, not in `scripts/dev`; on macOS it calls `scripts/dev` itself.

#### Benchmark, trace and profile the runtime

For a longer lockstep comparison, run `appine-lockstep`. To compare
the interpreter and binary translator against native execution, run `appine-bench`.

```sh
scripts/dev cargo run --release -p appine-lockstep -- --iterations 1000000
scripts/dev sh -c 'cargo build --release -p appine-run -p appine-bench &&
                   target/linux-arm64/release/appine-bench'
```

To debug a failing test, rerun it with `APPINE_STRACE=1`. `appine-run` prints each syscall
with its arguments and result, so you can check which calls failed or returned unexpected
values.

To profile, run `appine-run` under `perf record` with `APPINE_PERF_MAP=1`: tier 1 then writes
`/tmp/perf-<pid>.map`, and `perf report` attributes samples in translated code to guest
addresses (`g_<pc>`). `--stats` prints how many blocks were translated and why translated code
exited. `tier 0 fallback` counts instructions the translator leaves to the interpreter.
The `interp` count on the `exits` line records each runtime exit to the interpreter,
including exits caused by failed grouped memory checks.

A bug in the kernel layer or the engine (a panic, or the engine faulting on host memory) kills
only the guest process that hit it in release builds, with a report on stderr; a panic in a
`Machine` method called by the host only fails that call. Debug builds abort the whole
`appine-run` instead, so the bug can be debugged where it happened; `APPINE_ON_BUG=kill` or
`APPINE_ON_BUG=abort` overrides the default. When Appine's anonymous memory exceeds
`--memory-limit` (default: half the RAM), the guest process using the most memory is killed
with SIGKILL, like the Linux OOM killer.

## Project documentation

- [Architecture overview](docs/design/README.md#3-overall-architecture) and
  [module index](.agents/MODULES.md) for navigating the source.
- [Module maps](.agents/maps) for current behavior and tests, and
  [design decisions](.agents/decisions) for the reasoning behind changes.
- [Development milestones](docs/milestones.md) for earlier measurements and implementation history.
- Coding agents should start with [AGENTS.md](AGENTS.md).

## License

Copyright (C) 2026 Matthew Lee.

Appine is licensed under the [GNU Affero General Public License, version 3 or later](LICENSE).

Parts of the terminal line discipline in `core/kernel/src/tty.rs` are adapted
from Fuchsia's Starnix and retain [The Fuchsia Authors' BSD-style license](android/licenses/fuchsia-LICENSE).

The app includes separate notices for libghostty-vt and its dependencies,
JetBrains Mono, Alpine Linux packages, Android libraries and Rust crates.
Read them in **Settings → Open source licenses** or browse the
[bundled notices](android/app/src/main/assets/notices).
