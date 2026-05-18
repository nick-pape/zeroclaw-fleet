# syntax=docker/dockerfile:1.7

# Build stage — debian-slim rust:1.95 with apt deps for openssl/native libs
# pulled in transitively by reqwest + bollard.
FROM rust:1.95-slim-bookworm AS builder

WORKDIR /work
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Manifest-only step so dep compilation can be cached.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs \
    && cargo build --release --locked --bin zeroclaw-fleet \
    && rm -rf target/release/zeroclaw-fleet*

# Full build.
COPY src ./src
COPY web ./web
COPY tests ./tests
RUN --mount=type=cache,target=/work/target,id=fleet-target \
    --mount=type=cache,target=/usr/local/cargo/registry,id=fleet-cargo-reg \
    cargo build --release --locked --bin zeroclaw-fleet \
    && cp /work/target/release/zeroclaw-fleet /work/zeroclaw-fleet

# Runtime stage — distroless/cc-debian12 has glibc + ca-certs + openssl.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /work/zeroclaw-fleet /usr/local/bin/zeroclaw-fleet

EXPOSE 8080

ENV FLEET_BIND=0.0.0.0:8080 \
    RUST_LOG=zeroclaw_fleet=info,info \
    RUST_BACKTRACE=1

ENTRYPOINT ["/usr/local/bin/zeroclaw-fleet"]
CMD ["serve"]
