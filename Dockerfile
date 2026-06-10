FROM rust:latest AS builder

RUN USER=root cargo new --bin used-kinds-rs
WORKDIR /used-kinds-rs

COPY . .

RUN cargo build --release

FROM debian:bookworm-slim

# libunwind8, libstdc++6 and liblzma5 are runtime deps of the qdrant binary
RUN apt-get update \
    && apt-get install -y ca-certificates tzdata libunwind8 libstdc++6 liblzma5 \
    && rm -rf /var/lib/apt/lists/*

# Qdrant runs inside this same container: Render web services have no
# sidecars and a private service costs extra. Storage lives on the
# persistent disk; the service binds to loopback only.
COPY --from=qdrant/qdrant:v1.15.5 /qdrant /qdrant

# The tuning knobs keep qdrant small: it sizes segments, workers and
# rocksdb buffers by CPU count, which overshoots a 512MB instance.
ENV QDRANT__SERVICE__HOST=127.0.0.1 \
    QDRANT__SERVICE__MAX_WORKERS=2 \
    QDRANT__STORAGE__STORAGE_PATH=/var/data/qdrant/storage \
    QDRANT__STORAGE__SNAPSHOTS_PATH=/var/data/qdrant/snapshots \
    QDRANT__STORAGE__OPTIMIZERS__DEFAULT_SEGMENT_NUMBER=2 \
    QDRANT__STORAGE__PERFORMANCE__MAX_SEARCH_THREADS=2 \
    QDRANT__STORAGE__PERFORMANCE__MAX_OPTIMIZATION_THREADS=1 \
    QDRANT__TELEMETRY_DISABLED=true \
    MALLOC_ARENA_MAX=2 \
    QDRANT_URL=http://127.0.0.1:6334

COPY --from=builder /used-kinds-rs/target/release/used-kinds-rs .

COPY --from=builder /used-kinds-rs/templates /templates

COPY start.sh /start.sh
RUN chmod +x /start.sh

EXPOSE 8080

VOLUME ["/var/data"]

ENTRYPOINT ["/start.sh"]
