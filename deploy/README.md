# Deploying edgepack

edgepack compiles to a WASI Preview 2 component (~628 KB) that implements the `wasi:http/proxy` world. Any runtime supporting this interface can host it directly. For runtimes without native WASI P2 support, a container running `wasmtime serve` works as a universal adapter.

edgepack uses an in-process memory cache — no external state stores (Redis, KV) are needed. The CDN layer caches responses via HTTP `Cache-Control` headers, so the component only processes cache misses.

## Platform Overview

| Platform | Approach | Config |
|----------|----------|--------|
| **Akamai** | Fermyon Wasm Functions (native WASI P2) | `deploy/spin/spin.toml` |
| **Cloudflare** | Workers (WASI component) | `deploy/cloudflare/wrangler.toml` |
| **Fastly** | Compute (WASI component) | `deploy/fastly/fastly.toml` |
| **AWS CloudFront** | Container origin (wasmtime) | `deploy/docker/Dockerfile` |
| **Any CDN** | Container origin (wasmtime) | `deploy/docker/Dockerfile` |
| **Kubernetes** | SpinKube (native WASI P2) | `deploy/spin/spin.toml` |

## Prerequisites

```bash
# Rust with WASI P2 target
rustup target add wasm32-wasip2

# Build the component
make build
```

---

## Akamai (via Fermyon Wasm Functions)

Akamai runs [Fermyon Wasm Functions](https://www.fermyon.com) natively at the edge — WASI P2 components execute with sub-millisecond cold starts on Akamai's global network.

### Setup

```bash
# Install Spin CLI
curl -fsSL https://developer.fermyon.com/downloads/install.sh | bash

# Build
make build

# Test locally
spin up --from deploy/spin/spin.toml

# Deploy to Akamai via Fermyon
spin deploy --from deploy/spin/spin.toml
```

### Environment Variables

Set via the Fermyon dashboard or `spin deploy --variable`:

```bash
spin deploy --from deploy/spin/spin.toml \
  --variable SPEKE_URL=https://drm.example.com/speke/v2 \
  --variable SPEKE_BEARER_TOKEN=your-drm-token
```

---

## Cloudflare Workers

Cloudflare Workers runs WASI components at the edge.

### Setup

```bash
# Install Wrangler
npm install -g wrangler
wrangler login

# Build
make build

# Set secrets
wrangler secret put SPEKE_URL
wrangler secret put SPEKE_BEARER_TOKEN

# Deploy
cd deploy/cloudflare && npx wrangler deploy
```

---

## Fastly Compute

Fastly Compute runs WebAssembly at the edge. WASI Preview 2 component model support is being added — check [Fastly Community](https://community.fastly.com/t/wasm-support-for-component-model-wasip2/3642) for the latest status.

### Setup

```bash
# Install Fastly CLI
brew install fastly/tap/fastly

# Build
make build-fastly

# Deploy
cd deploy/fastly && fastly compute deploy
```

### Backend Configuration

Edit `deploy/fastly/fastly.toml` to configure backend addresses for your origin and DRM license server. Environment variables are set via Fastly config stores.

---

## AWS CloudFront (Container Origin)

CloudFront doesn't natively run WASM. Deploy edgepack as a container origin using `wasmtime serve`, then place CloudFront in front for global edge caching.

### Architecture

```
Client → CloudFront (CDN cache) → ECS/Fargate (wasmtime serve)
                                                 → SPEKE DRM server
                                                 → Media origin
```

CloudFront caches edgepack's HTTP responses (manifests, segments) at the edge. The container only processes cache misses.

### Setup

```bash
# Build Docker image
make build-docker

# Run locally
docker run -p 8080:8080 \
  -e SPEKE_URL=https://drm.example.com/speke/v2 \
  -e SPEKE_BEARER_TOKEN=your-token \
  edgepack:latest

# Push to ECR
aws ecr get-login-password --region us-east-1 | docker login --username AWS --password-stdin <account>.dkr.ecr.us-east-1.amazonaws.com
docker tag edgepack:latest <account>.dkr.ecr.us-east-1.amazonaws.com/edgepack:latest
docker push <account>.dkr.ecr.us-east-1.amazonaws.com/edgepack:latest
```

### Deploy to ECS/Fargate

Create an ECS service with the container image, then configure CloudFront with the ECS service as origin. Set environment variables in the ECS task definition.

---

## Generic Container Deployment

For any CDN or self-hosted setup, use the Docker image with `wasmtime serve`.

### Setup

```bash
# Build
make build-docker

# Run
docker run -p 8080:8080 \
  -e SPEKE_URL=https://drm.example.com/speke/v2 \
  -e SPEKE_BEARER_TOKEN=your-token \
  edgepack:latest

# Or with docker-compose
docker compose -f deploy/docker/docker-compose.yml up
```

### Use as CDN Origin

Point any CDN (Akamai, CloudFront, Fastly, Cloudflare, Varnish, etc.) at the container's HTTP endpoint. edgepack sets appropriate `Cache-Control` headers on all responses:

- **Segments**: `public, max-age=31536000, immutable` (1 year)
- **Live manifests**: `public, max-age=1, s-maxage=1`
- **VOD manifests**: `public, max-age=31536000, immutable`
- **In-progress**: `no-cache` (until first segment completes)

These TTLs are configurable per-request and via environment variables.

---

## Kubernetes (SpinKube)

[SpinKube](https://spinkube.dev) runs Spin applications natively in Kubernetes using the containerd-shim-spin runtime.

### Setup

```bash
# Install SpinKube operator (one-time cluster setup)
kubectl apply -f https://github.com/spinkube/spin-operator/releases/latest/download/spin-operator.yaml

# Build and push to OCI registry
make build
spin registry push ttl.sh/edgepack:latest --from deploy/spin/spin.toml

# Deploy
kubectl apply -f - <<EOF
apiVersion: core.spinoperator.dev/v1alpha1
kind: SpinApp
metadata:
  name: edgepack
spec:
  image: ttl.sh/edgepack:latest
  replicas: 3
  variables:
    - name: SPEKE_URL
      valueFrom:
        secretKeyRef:
          name: edgepack-secrets
          key: speke-url
    - name: SPEKE_BEARER_TOKEN
      valueFrom:
        secretKeyRef:
          name: edgepack-secrets
          key: speke-bearer-token
EOF
```

---

## Environment Variables Reference

### Required (all platforms)

| Variable | Description |
|----------|-------------|
| `SPEKE_URL` | SPEKE 2.0 DRM license server endpoint |
| `SPEKE_BEARER_TOKEN` | DRM auth (or `SPEKE_API_KEY` / `SPEKE_USERNAME`) |

### Optional

| Variable | Default | Description |
|----------|---------|-------------|
| `CACHE_MAX_AGE_SEGMENTS` | `31536000` | Segment cache TTL (seconds) |
| `CACHE_MAX_AGE_MANIFEST_LIVE` | `1` | Live manifest TTL (seconds) |
| `CACHE_MAX_AGE_MANIFEST_FINAL` | `31536000` | Final/VOD manifest TTL (seconds) |
| `JIT_SOURCE_URL_PATTERN` | — | URL template with `{content_id}` placeholder |
| `JIT_DEFAULT_TARGET_SCHEME` | `cenc` | Default scheme: `cenc` or `cbcs` |
| `JIT_DEFAULT_CONTAINER_FORMAT` | `cmaf` | Default format: `cmaf` or `fmp4` |
| `JIT_LOCK_TTL` | `30` | Processing lock TTL in seconds |

---

## Verifying Deployment

After deploying, verify the health endpoint:

```bash
curl https://your-deployment.example.com/health
# Expected: "ok"
```

Request a JIT-repackaged manifest (content is repackaged on-demand on the first request):

```bash
curl https://your-deployment.example.com/repackage/test-content/hls_cenc/manifest
```
