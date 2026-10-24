# appine 的 Linux arm64 开发环境（设计文档 §13.3），由 scripts/dev 自动构建。
# llvm 提供 llvm-mc，用来独立核对解释器的指令白名单。
FROM rust:1.90.0-slim-trixie
RUN rustup component add rustfmt clippy \
    && apt-get update \
    && apt-get install -y --no-install-recommends llvm clang \
    && rm -rf /var/lib/apt/lists/*
