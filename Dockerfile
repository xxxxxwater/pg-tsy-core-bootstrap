FROM rust:1.98-bookworm AS builder

WORKDIR /src
COPY rust ./rust
WORKDIR /src/rust
RUN cargo build --release -p pg-core

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --uid 10001 pgtsy
WORKDIR /app

COPY --from=builder /src/rust/target/release/pg-core /usr/local/bin/pg-core
COPY strategies /app/strategies

RUN mkdir -p /app/data \
    && chown -R pgtsy:pgtsy /app

USER pgtsy
EXPOSE 8080

ENV PG_RUN_MODE=shadow \
    PG_LIVE_TRADING=false \
    PG_STRATEGY_DIR=/app/strategies \
    PG_HEALTH_ADDR=0.0.0.0:8080 \
    RUST_LOG=info

ENTRYPOINT ["/usr/local/bin/pg-core"]
CMD ["--serve"]
