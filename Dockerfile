# syntax=docker/dockerfile:1

# ── Dependency cache (cargo-chef) ─────────────────────────────────────────
FROM rust:1.98.1-slim-trixie AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p uptime-server
COPY . .
RUN cargo build --release --locked -p uptime-server --bin uptimestatus

# ── Runtime ───────────────────────────────────────────────────────────────
FROM debian:trixie-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home app \
 && mkdir -p /data \
 && chown app:app /data
COPY --from=builder /app/target/release/uptimestatus /app/uptimestatus
# Starts as root only to hand the storage volume to `app`, then drops to it.
COPY deploy/entrypoint.sh /app/entrypoint.sh
# Dual-stack bind: some platforms' proxies reach the app over IPv6.
ENV UPTIMESTATUS_HTTP__HOST=:: \
    UPTIMESTATUS_LOG__FORMAT=json
EXPOSE 8080 9090
ENTRYPOINT ["/app/entrypoint.sh"]
CMD ["serve"]
