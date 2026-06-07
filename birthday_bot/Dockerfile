# Build stage. Pinned to bookworm on both stages so the glibc/OpenSSL the
# binary links against in the builder is what the runtime image provides.
FROM rust:1.94-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Cache mounts keep the registry and incremental build artifacts across
# image rebuilds; the binary is copied out because the cache is not part
# of the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release -p telegram_bot \
    && cp target/release/telegram_bot /usr/local/bin/telegram_bot

# Runtime stage: just the binary, TLS roots, and OpenSSL (SQLite is
# compiled into the binary).
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --create-home --home-dir /data bot
COPY --from=builder /usr/local/bin/telegram_bot /usr/local/bin/telegram_bot

USER bot
WORKDIR /data
# The database lives on the /data volume so it survives image rebuilds.
ENV DB_PATH=/data/birthdays.db
VOLUME /data
CMD ["telegram_bot"]
