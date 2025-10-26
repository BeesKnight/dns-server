# syntax=docker/dockerfile:1.6

FROM rust:1.82-bullseye AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY tests ./tests

RUN cargo build --release --bin dns_agent

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/dns_agent /usr/local/bin/dns_agent

EXPOSE 2053/udp

ENTRYPOINT ["/usr/local/bin/dns_agent"]
