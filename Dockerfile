# syntax = docker/dockerfile:1
#
# ============================================================================
#  Build: musl static binary
# ============================================================================
#  Build commands (run from repo root):
#
#    docker build -t linewatch .
#    docker run --rm -p 9980:9980 \
#      -v /var/lib/linewatch:/var/lib/linewatch \
#      linewatch run
#
#  For ICMP ping support (unprivileged SOCK_DGRAM) no extra flags are needed
#  on modern kernels where net.ipv4.ping_group_range covers all GIDs.
#  If your kernel restricts ping_group_range, add:
#      --cap-add=NET_RAW
#  or set sysctl before running:
#      sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
# ============================================================================

FROM rust:alpine AS builder

# musl target + static build prerequisites
RUN apk add --no-cache musl-dev g++ cmake make perl
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Dependency caching layer (only Cargo files change rarely)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    mkdir -p src/core src/shell assets && \
    cargo build --release --target x86_64-unknown-linux-musl --bin linewatch 2>/dev/null; \
    true

# Real source + assets (font embedded via include_bytes!)
COPY src/ src/
COPY assets/ assets/

# Touch to force recompilation of the real binary
RUN touch src/main.rs && \
    cargo build --release --target x86_64-unknown-linux-musl --bin linewatch

# Strip debug symbols
RUN strip target/x86_64-unknown-linux-musl/release/linewatch

# ============================================================================
#  Runtime: minimal Alpine (libcap for setcap, otherwise only the binary)
# ============================================================================
FROM alpine:latest

RUN adduser -D -u 1000 linewatch && \
    apk add --no-cache libcap && \
    mkdir -p /var/lib/linewatch && \
    chown -R linewatch:linewatch /var/lib/linewatch

COPY --from=builder \
    /app/target/x86_64-unknown-linux-musl/release/linewatch \
    /usr/local/bin/linewatch

# Minimal default config (can be overridden via env vars LINEWATCH_*).
COPY linewatch.toml /etc/linewatch/linewatch.toml

# Grant CAP_NET_RAW for ICMP raw-socket fallback (unprivileged SOCK_DGRAM
# does not need this, but the surge-ping client tries raw first on some
# systems).
RUN setcap cap_net_raw+ep /usr/local/bin/linewatch

USER linewatch
WORKDIR /etc/linewatch

ENV LINEWATCH_DATA_DIR=/var/lib/linewatch

EXPOSE 9980

ENTRYPOINT ["/usr/local/bin/linewatch"]
CMD ["run"]
