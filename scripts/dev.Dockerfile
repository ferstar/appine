# appine 的 Linux arm64 开发环境（设计文档 §13.3），由 scripts/dev 自动构建。
FROM rust:1.90.0-slim-trixie
RUN rustup component add rustfmt clippy
