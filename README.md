# appine

Run Alpine Linux as Android App 🤖

**UNDER DEVELOPMENT.**

## Development

The architecture and the milestone plan are described in [docs/design.md](docs/design.md);
each milestone and round of work has a record in [docs/rounds](docs/rounds), indexed in
[docs/milestones.md](docs/milestones.md).

The primary development host is Linux arm64. On macOS, `scripts/dev` runs a command in an
arm64 Linux container (colima, OrbStack or Docker Desktop):

```sh
scripts/dev cargo test --workspace     # unit tests, guest programs and instruction lockstep
scripts/dev cargo run --release -p appine-lockstep -- --iterations 1000000

# performance probe: native vs. interpreter (tier 0) vs. binary translator (tier 1)
scripts/dev sh -c 'cargo build --release -p appine-run -p appine-bench &&
                   target/linux-arm64/release/appine-bench'
```

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
scripts/fetch-rootfs                   # downloads and verifies target/rootfs/alpine-minirootfs-*.tar.gz
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

### LTP

The syscall tests of the [Linux Test Project](https://github.com/linux-test-project/ltp)
are built natively in an `alpine` arm64 container and run inside appine, one fresh
`appine-run` per test:

```sh
scripts/build-ltp                      # builds target/ltp/ltp-<version>.tar (needs Docker)
scripts/dev scripts/ltp -j 8           # all syscall tests; results in target/ltp/results.txt
scripts/dev scripts/ltp 'fcntl.*|mmap.*'   # only the tests whose name matches
```

`APPINE_STRACE=1` makes `appine-run` print every syscall with its arguments and result,
which helps when a test fails.

To profile, run `appine-run` under `perf record` with `APPINE_PERF_MAP=1`: tier 1 then writes
`/tmp/perf-<pid>.map`, and `perf report` attributes samples in translated code to guest
addresses (`g_<pc>`). `--stats` prints how many blocks were translated and why translated code
exited.

A bug in the kernel layer or the engine (a panic, or the engine faulting on host memory) kills
only the guest process that hit it in release builds, with a report on stderr; a panic in a
`Machine` method called by the host only fails that call. Debug builds abort the whole
`appine-run` instead, so the bug can be debugged where it happened; `APPINE_ON_BUG=kill` or
`APPINE_ON_BUG=abort` overrides the default. When appine's anonymous memory exceeds
`--memory-limit` (default: half the RAM), the guest process using the most memory is killed
with SIGKILL, like the Linux OOM killer.

### Differential tests

`scripts/difftest` runs each script in `tests/diff` both in appine and in a native arm64
Docker container, and compares their output and exit status. The container runs without
Docker's seccomp profile, which changes what some syscalls return (`clone3`, `pidfd_getfd`).
Both sides run the same files: the first run creates a store in `target/difftest/store` (minirootfs plus python3,
gcc, Lua, SQLite, Go, Node.js, OpenJDK 21, strace and gdb from `apk`, so it needs the internet), exports its
root filesystem from appine and imports it as a Docker image. A store made before a package was
added lacks it: install the package into the store with `apk add`, or delete the store, and pass
`-r` to export it again:

```sh
scripts/fetch-rootfs
scripts/difftest                       # all cases on tier 1
scripts/difftest -t 0 signals          # one case on the interpreter
```

Run it on the host, not in `scripts/dev`; on macOS it calls `scripts/dev` itself.

## Android app

The app in [android/](android) runs the same kernel inside an Android app: each tab is a
login shell on an in-process terminal. It needs the Android SDK with NDK 29.0.14206865,
`rustup target add aarch64-linux-android` and `cargo install cargo-ndk`:

```sh
scripts/fetch-rootfs
cd android
./gradlew installDebug                 # on a connected arm64 device or emulator (Android 10+)
./gradlew connectedDebugAndroidTest    # instrumentation tests, on the same kind of device
```

The native library is always a release build. In the debug app it also has the test hooks
(the `test-hooks` feature), which the instrumentation tests use to simulate kernel and engine
bugs inside the app process; they do nothing unless `APPINE_TEST_HOOKS=1` is set.

## License

appine is free software: you can redistribute it and/or modify it under the terms of the
GNU Affero General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

```
Copyright (C) 2026 Matthew Lee
```

The Android app also contains code under other licenses: the terminal libraries vendored from
Termux (GPL 3.0 only, partly Apache 2.0), the Alpine Linux minirootfs (busybox, apk-tools and
others under the GPL 2.0), AndroidX and Kotlin (Apache 2.0), and the Rust crates in
`Cargo.lock` (MIT and other permissive licenses). The app lists them all under
Settings → Open source licenses; see `android/README.md`.
