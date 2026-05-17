# syntax=docker/dockerfile:1

FROM rust:1.95-bookworm AS build
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY rust-toolchain.toml ./
COPY gateway ./gateway
COPY worker ./worker

RUN cargo build --release --manifest-path gateway/Cargo.toml --target-dir /app/target-gateway
RUN cargo build --release --manifest-path worker/Cargo.toml --target-dir /app/target-worker

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target-gateway/release/context-gateway /app/context-gateway
COPY --from=build /app/target-worker/release/context-worker /app/context-worker
COPY docker/start-gateway-worker.sh /app/start-gateway-worker.sh

RUN chmod +x /app/start-gateway-worker.sh

ENV RUST_LOG=info \
    WORKER_PORT=18081

EXPOSE 8080

CMD ["/app/start-gateway-worker.sh"]
