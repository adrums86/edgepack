# Multi-stage Dockerfile for edgepack
#
# Produces a minimal container running wasmtime serve with the edgepack component.
# Deploy behind any CDN (CloudFront, Akamai, Fastly, Cloudflare) as origin.
#
# Build:
#   make build-docker
#   # or directly:
#   docker build -t edgepack -f deploy/docker/Dockerfile .
#
# Run:
#   docker run -p 8080:8080 --env-file .env edgepack
#
# Deploy to AWS ECS/Fargate, Google Cloud Run, Azure Container Apps, etc.
# Place behind CloudFront or any CDN as the origin server.

# --- Stage 1: Build the WASM component ---
FROM rust:1.84-slim AS builder

RUN rustup target add wasm32-wasip2

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/
COPY tests/ tests/

RUN cargo build --release --target wasm32-wasip2

# --- Stage 2: Runtime with wasmtime ---
FROM debian:bookworm-slim

# Install wasmtime
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && curl https://wasmtime.dev/install.sh -sSf | bash \
    && ln -s /root/.wasmtime/bin/wasmtime /usr/local/bin/wasmtime \
    && apt-get remove -y curl && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/wasm32-wasip2/release/edgepack.wasm /app/edgepack.wasm

EXPOSE 8080

# wasmtime serve runs the wasi:http/proxy component as an HTTP server
ENTRYPOINT ["wasmtime", "serve", "--addr", "0.0.0.0:8080", "/app/edgepack.wasm"]
