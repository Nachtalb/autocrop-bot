# syntax=docker/dockerfile:1.7

# ── ffmpeg: johnvansickle static build (glibc-static, all codecs but ~76 MB
#    uncompressed vs ~129 MB for the mwader build). Safe on scratch because we
#    only ever do local-file remux — no NSS/DNS resolution. ──
FROM debian:trixie-slim AS ffmpeg
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl xz-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSL https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz \
        -o /tmp/ffmpeg.tar.xz \
    && mkdir /tmp/ff \
    && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp/ff --strip-components=1 --wildcards '*/ffmpeg' \
    && mv /tmp/ff/ffmpeg /ffmpeg \
    && chmod 0755 /ffmpeg

# ── build: alpine is musl-native, so ring/rustls build cleanly for the
#    fully-static x86_64-unknown-linux-musl target. ──
FROM rust:1-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
ENV RUSTFLAGS="-C target-feature=+crt-static"

# Cache the dependency layer across source-only edits.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && rm -rf src target/x86_64-unknown-linux-musl/release/deps/autocrop_bot*

COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl \
    && strip target/x86_64-unknown-linux-musl/release/autocrop-bot

# Stage to prepare a tmp dir we can hand to a non-root scratch container.
RUN mkdir -p /rootfs/tmp && chmod 1777 /rootfs/tmp

# ── runtime: scratch. Only the two static binaries + a writable /tmp. ──
FROM scratch
COPY --from=builder /rootfs/tmp /tmp
COPY --from=ffmpeg  /ffmpeg /usr/local/bin/ffmpeg
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/autocrop-bot /usr/local/bin/autocrop-bot

USER 10001:10001
ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/autocrop-bot"]
