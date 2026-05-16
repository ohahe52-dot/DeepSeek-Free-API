# syntax=docker/dockerfile:1

FROM oven/bun:1.3.13 AS web-build
WORKDIR /app/web

COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

COPY web/ ./
RUN bun run build

FROM rust:1.95-bookworm AS rust-build
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        nasm \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY --from=web-build /app/web/dist ./web/dist

RUN cargo build --release

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/config /app/data

COPY --from=rust-build /app/target/release/ds-free-api /app/ds-free-api
COPY docker/config.example.toml /app/config/config.toml

ENV RUST_LOG=info \
    DS_DATA_DIR=/app/data \
    DS_CONFIG_PATH=/app/config/config.toml \
    DS_HOST=0.0.0.0

EXPOSE 22217

CMD ["/app/ds-free-api"]
