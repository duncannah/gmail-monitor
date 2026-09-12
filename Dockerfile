FROM rust:1.85-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release && cp target/release/gmail-monitor /gmail-monitor

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --create-home monitor && \
    install -d -o monitor -g monitor /data
COPY --from=builder /gmail-monitor /usr/local/bin/gmail-monitor
USER monitor
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/gmail-monitor"]
