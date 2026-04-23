# Build stage
FROM rust:1.86-bookworm AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release --bin houndr-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    git \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash houndr

WORKDIR /app

COPY --from=builder /build/target/release/houndr-server /app/houndr-server

RUN mkdir -p /app/data && chown -R houndr:houndr /app

USER houndr

EXPOSE 6080

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -f http://localhost:6080/healthz || exit 1

ENTRYPOINT ["/app/houndr-server"]
CMD ["--config", "/app/config.toml"]
