FROM rust:latest as builder

RUN USER=root cargo new --bin used-kinds-rs
WORKDIR /used-kinds-rs

COPY . .

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /used-kinds-rs/target/release/used-kinds-rs .

COPY --from=builder /used-kinds-rs/templates /templates

EXPOSE 8080

VOLUME ["/var/data"]

ENTRYPOINT ["./used-kinds-rs"]
