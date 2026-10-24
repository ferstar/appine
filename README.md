# appine

Run Alpine Linux as Android App 🤖

**UNDER DEVELOPMENT.**

## Development

The architecture and milestones are described in [docs/design.md](docs/design.md).

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

## Android app

The app in [android/](android) runs the same kernel inside an Android app: each tab is a
login shell on an in-process terminal. It needs the Android SDK with NDK 29.0.14206865,
`rustup target add aarch64-linux-android` and `cargo install cargo-ndk`:

```sh
scripts/fetch-rootfs
cd android && ./gradlew installDebug   # on a connected arm64 device or emulator (Android 10+)
```

## License

appine is free software: you can redistribute it and/or modify it under the terms of the
GNU Affero General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

```
Copyright (C) 2026 Matthew Lee
```
