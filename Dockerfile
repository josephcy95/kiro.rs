# syntax=docker/dockerfile:1.7
# Multi-stage: admin-ui → Rust release → slim runtime.
# Dependency layer is cached separately so source-only changes rebuild fast.

FROM oven/bun:1-alpine AS frontend-builder

WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/bun.lock* ./
RUN bun install --frozen-lockfile --ignore-scripts
COPY admin-ui ./
RUN bun run build

FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev perl make

WORKDIR /app

# 1) Cache crate compilation on lockfile only (dummy main)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --no-default-features \
    && rm -rf src

# 2) Real sources + embedded admin UI
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# Touch so cargo sees sources as newer than the dummy build artifacts
RUN touch src/main.rs \
    && cargo build --release --no-default-features \
    && strip target/release/kiro-rs || true

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/target/release/kiro-rs /app/kiro-rs

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
