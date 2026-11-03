# appine's Linux arm64 development environment (design doc §13.3), built automatically by
# scripts/dev.
# llvm provides llvm-mc, which independently checks the interpreter's instruction whitelist;
# musl-tools provides musl-gcc, which compiles the guest test programs linked with musl
# (tests/guest/*.c).
FROM rust:1.90.0-slim-trixie
RUN rustup component add rustfmt clippy \
    && apt-get update \
    && apt-get install -y --no-install-recommends llvm clang musl-tools \
    && rm -rf /var/lib/apt/lists/*
