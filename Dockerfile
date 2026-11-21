# syntax=docker/dockerfile:1.7
# AirRadar multi-stage image: glibc release build on bookworm, slim non-root runtime.

FROM rust:1-bookworm AS builder
WORKDIR /src

# 1) Cache dependency compilation with a stub crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# 2) Compile the real crate.
COPY src ./src
# The stub binary from the dependency-cache stage must not survive into the real
# build. BuildKit preserves source mtimes, so Cargo can otherwise consider the
# stub up to date and ship a binary whose main() is empty.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    rm -f target/release/airradar \
    && touch src/main.rs src/lib.rs \
    && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 airradar \
    && useradd --uid 10001 --gid airradar --no-create-home --shell /usr/sbin/nologin airradar \
    && mkdir -p /app /data/cache \
    && chown -R 10001:10001 /app /data

COPY --from=builder /src/target/release/airradar /usr/local/bin/airradar

WORKDIR /app
ENV PORT=8080 \
    DATA_DIR=/data \
    CACHE_DIR=/data/cache \
    LOG_LEVEL=info \
    MALLOC_ARENA_MAX=2

EXPOSE 8080
VOLUME ["/data"]
USER 10001:10001

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/airradar", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/airradar"]
