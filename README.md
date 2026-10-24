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

## License

appine is free software: you can redistribute it and/or modify it under the terms of the
GNU Affero General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

```
Copyright (C) 2026 Matthew Lee
```
