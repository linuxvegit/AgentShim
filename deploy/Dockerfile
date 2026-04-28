FROM rust:1-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p agent-shim --bin agent-shim

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-shim /usr/local/bin/agent-shim
USER 65532:65532
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/agent-shim"]
CMD ["serve", "--config", "/etc/agent-shim/gateway.yaml"]
