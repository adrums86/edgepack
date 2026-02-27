# edge-packager

A Rust application compiled to WebAssembly for CDN edge environments. It repackages DASH and HLS CMAF media from CBCS encryption (FairPlay/Widevine/PlayReady) into CENC encryption (Widevine/PlayReady), producing progressive output manifests and segments cached at the CDN for maximum duration.

## What It Does

1. **Receives a request** to repackage content (on-demand via HTTP or proactively via webhook)
2. **Fetches DRM keys** from a license server using the SPEKE 2.0 protocol and CPIX standard
3. **Fetches source media** (CMAF init + media segments) from the origin
4. **Decrypts** each segment using AES-128-CBC with pattern encryption (CBCS scheme)
5. **Re-encrypts** each segment using AES-128-CTR full encryption (CENC scheme)
6. **Rewrites** init segments (updates protection scheme info, PSSH boxes, removes FairPlay)
7. **Outputs progressively** — writes a live/dynamic manifest as soon as the first segment is ready, updates it with each subsequent segment, and finalises it when complete
8. **Caches aggressively** — segments are immutable with 1-year cache headers; live manifests have 1-second TTL; finalised manifests become immutable

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- WASM target: `wasm32-wasip2`
- A Redis instance (Upstash recommended for edge; any Redis for local dev)
- A SPEKE 2.0-compatible DRM license server (e.g. BuyDRM KeyOS)

### Install Rust and the WASM Target

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-wasip2
```

## Building

The project is configured to build for `wasm32-wasip2` by default (via `.cargo/config.toml`).

```bash
# Development build
cargo build

# Release build (size-optimised with LTO)
cargo build --release
```

Output WASM binary:
```
target/wasm32-wasip2/release/edge_packager.wasm
```

### Running Tests

Tests run on the native host target (not WASM):

```bash
cargo test --target $(rustc -vV | grep host | awk '{print $2}')
```

## Configuration

All configuration is via environment variables.

### Required

| Variable | Description |
|----------|-------------|
| `REDIS_URL` | Redis endpoint URL (e.g. `https://us1-xxxxx.upstash.io` for Upstash HTTP, or `redis://localhost:6379` for TCP) |
| `REDIS_TOKEN` | Redis authentication token or password |
| `SPEKE_URL` | SPEKE 2.0 license server endpoint URL |

### SPEKE Authentication (one of the following)

| Method | Variables |
|--------|-----------|
| Bearer token | `SPEKE_BEARER_TOKEN` |
| API key | `SPEKE_API_KEY` and optionally `SPEKE_API_KEY_HEADER` (default: `x-api-key`) |
| Basic auth | `SPEKE_USERNAME` and `SPEKE_PASSWORD` |

### Optional

| Variable | Default | Description |
|----------|---------|-------------|
| `REDIS_BACKEND` | `http` | Redis backend type: `http` (Upstash REST API) or `tcp` (direct connection) |

## API

### On-Demand Repackaging

Request repackaged content directly. The edge worker fetches from origin, repackages, and serves the result with CDN cache headers.

```
GET /repackage/{content_id}/{format}/manifest
GET /repackage/{content_id}/{format}/init.mp4
GET /repackage/{content_id}/{format}/segment_{n}.cmfv
```

- `{content_id}` — unique content identifier
- `{format}` — `hls` or `dash`
- `{n}` — segment number (0-indexed)

### Proactive Repackaging (Webhook)

Trigger repackaging ahead of time so content is cached before clients request it.

```
POST /webhook/repackage
Content-Type: application/json

{
  "content_id": "my-content-123",
  "source_url": "https://origin.example.com/content/master.m3u8",
  "format": "hls",
  "key_ids": ["optional-hex-kid-1"]
}
```

Returns `202 Accepted` with:
```json
{
  "status": "accepted",
  "content_id": "my-content-123",
  "format": "hls",
  "manifest_url": "/repackage/my-content-123/hls/manifest"
}
```

### Job Status

```
GET /status/{content_id}/{format}
```

Returns JSON with job state, segments completed, and total segment count.

### Health Check

```
GET /health
```

Returns `200 OK` with body `ok`.

## Caching Strategy

### CDN Layer (primary content cache)

| Resource | Cache-Control |
|----------|---------------|
| Segments (once produced) | `public, max-age=31536000, immutable` |
| Finalised manifests (VOD) | `public, max-age=31536000, immutable` |
| Live/in-progress manifests | `public, max-age=1, s-maxage=1` |

Segments never change once written. The CDN serves them without hitting the edge worker after the first request.

### Redis (application state)

| Key | TTL | Purpose |
|-----|-----|---------|
| `ep:{id}:keys` | 24h | Cached DRM content keys |
| `ep:{id}:{fmt}:state` | 48h | Job state and progress |
| `ep:{id}:{fmt}:manifest_state` | 48h | Progressive manifest state (segment list, phase) |
| `ep:{id}:speke` | 24h | Cached SPEKE license server responses |

## Architecture

```
                    ┌─────────────────────────────────────────┐
                    │            CDN Edge Node                │
                    │                                         │
   Client ──GET──►  │  ┌──────────┐    ┌──────────────────┐   │
                    │  │ CDN Cache│◄───│  edge-packager   │   │
                    │  │ (HTTP    │    │  (.wasm module)  │   │
                    │  │  headers)│    │                  │   │
                    │  └──────────┘    │  Handler         │   │
                    │                  │    ↓             │   │
                    │                  │  Pipeline        │   │
                    │                  │    ↓       ↓     │   │
                    │                  │  Media   DRM     │   │
                    │                  │  (CMAF)  (SPEKE) │   │
                    │                  │    ↓       ↓     │   │
                    │                  │  Manifest Redis  │   │
                    │                  └────┬───────┬─────┘   │
                    └───────────────────────┼───────┼─────────┘
                                            │       │
                              Origin ◄──────┘       └──────► License Server
                              (CBCS source)                  (SPEKE 2.0)
```

### Module Dependency Graph

```
handler/ ──► repackager/ ──► media/     (CMAF parse + rewrite)
                         ──► drm/      (SPEKE + CBCS decrypt + CENC encrypt)
                         ──► manifest/ (HLS/DASH generation)
                         ──► cache/    (Redis state)
```

## Supported Encryption Schemes

| Direction | Scheme | Mode | Pattern | DRM Systems |
|-----------|--------|------|---------|-------------|
| **Input** (source) | CBCS | AES-128-CBC | 1:9 (video), 0:0 (audio) | FairPlay, Widevine, PlayReady |
| **Output** (target) | CENC | AES-128-CTR | None (full encryption) | Widevine, PlayReady |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `aes`, `cbc`, `ctr`, `cipher` | AES encryption/decryption (CBCS and CENC) |
| `quick-xml` | CPIX XML and DASH MPD parsing/generation |
| `serde`, `serde_json` | Serialization for config, Redis, webhooks |
| `base64` | Key encoding in CPIX, PSSH data in manifests |
| `uuid` | Content Key ID (KID) handling |
| `url` | URL parsing |
| `thiserror` | Error type derivation |
| `log` | Logging facade |

All dependencies are selected for WASM compatibility (no system calls, no async runtime).

## Project Status

The project scaffolding, type system, and core algorithms (ISOBMFF parsing, CBCS decryption, CENC encryption, manifest generation, progressive output) are implemented and compile to WASM. The following areas require implementation to complete the runtime:

- **WASI HTTP transport**: The `wasi:http/outgoing-handler` calls for Redis, SPEKE, and origin fetching are stubbed out
- **WASI incoming handler**: Wiring the HTTP router to `wasi:http/incoming-handler` for serving requests
- **Source manifest parsing**: HLS M3U8 and DASH MPD input parsing (the output renderers exist)
- **Request handler wiring**: Connecting the GET handlers to the cache backend and pipeline

## License

Proprietary.
