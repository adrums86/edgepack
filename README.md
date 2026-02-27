# edge-packager

A Rust application compiled to WebAssembly for CDN edge environments. It repackages DASH and HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC) and container formats (CMAF ↔ fMP4), producing progressive output manifests and segments cached at the CDN for maximum duration. The target encryption scheme and container format are configurable per request, supporting all encryption scheme combinations (CBCS→CENC, CENC→CBCS, CENC→CENC, CBCS→CBCS) with automatic source scheme detection, and output as either CMAF or fragmented MP4.

## What It Does

1. **Receives a request** to repackage content (on-demand via HTTP or proactively via webhook)
2. **Fetches DRM keys** from a license server using the SPEKE 2.0 protocol and CPIX standard
3. **Fetches source media** (CMAF init + media segments) from the origin
4. **Decrypts** each segment using the source encryption scheme (CBCS or CENC, auto-detected from the init segment)
5. **Re-encrypts** each segment using the target encryption scheme (CBCS or CENC, configurable per request)
6. **Rewrites** init segments (updates protection scheme info, PSSH boxes, adjusts DRM system signaling per target scheme; rewrites ftyp brands for target container format)
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

Tests run on the native host target (not WASM), since the test harness cannot execute inside a WASI runtime:

```bash
cargo test --target $(rustc -vV | grep host | awk '{print $2}')
```

On Apple Silicon Macs, this is equivalent to:

```bash
cargo test --target aarch64-apple-darwin
```

On x86-64 Linux:

```bash
cargo test --target x86_64-unknown-linux-gnu
```

The project includes **526 tests** (452 unit tests + 74 integration tests) covering every module. To run tests for a specific module:

```bash
# Run all tests in the drm module
cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::

# Run a single test by name
cargo test --target $(rustc -vV | grep host | awk '{print $2}') handler::tests::route_health_check

# Run only integration tests
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test '*'

# Run a specific integration test suite
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test encryption_roundtrip
```

#### Unit Test Coverage (452 tests)

| Module | Tests | What's Covered |
|--------|-------|----------------|
| `error` | 16 | Error display strings, Result alias |
| `config` | 11 | Defaults, serde roundtrips, env var loading |
| `cache` | 44 | CacheKeys formatting, backend factory, Upstash JSON response parsing, in-memory cache ops, encrypted backend (AES-256-GCM roundtrip, tamper detection, key sensitivity patterns, key derivation) |
| `drm` | 100 | EncryptionScheme enum (serde roundtrips, scheme_type_bytes, from_scheme_type, HLS method strings, default IV sizes, default patterns, FairPlay support flags), SampleDecryptor/SampleEncryptor traits (factory dispatch, CBCS/CENC roundtrips), system IDs, CPIX XML roundtrips, CBCS decrypt + encrypt, CENC encrypt + decrypt, SPEKE client, auth headers |
| `media` | 85 | FourCC types, ISOBMFF box parsing/building/iteration, ContainerFormat enum (extensions, brands, ftyp building, DASH profiles, serde roundtrips, display), init segment rewriting (CBCS and CENC target schemes, tenc pattern encoding, PSSH filtering, ftyp brand rewriting per container format), segment rewriting (scheme-aware decrypt/re-encrypt), IV padding |
| `manifest` | 93 | HLS/DASH rendering for all lifecycle phases, dynamic DRM scheme signaling (SAMPLE-AES/SAMPLE-AES-CTR for HLS, cbcs/cenc for DASH), FairPlay key URI rendering, variant streams, ISO 8601 duration, KID formatting, HLS M3U8 input parsing (source scheme detection from EXT-X-KEY), DASH MPD input parsing (source scheme detection from ContentProtection) |
| `repackager` | 46 | Job types/serde, progressive output state machine, cache-control headers, key set caching, continuation params (scheme-aware serde roundtrip, container format), pipeline execution, manifest DRM info building (CBCS/CENC target scheme, FairPlay inclusion/exclusion), sensitive data cleanup |
| `handler` | 52 | HTTP routing, path parsing, format validation, segment number parsing (.cmfv and .m4s), webhook validation (target_scheme, container_format, CBCS/CENC parsing, invalid scheme/format rejection), response construction, continue endpoint |
| `http_client` | 5 | Response construction, native stub errors |

#### Integration Test Coverage (74 tests)

Integration tests live in the `tests/` directory and exercise cross-module workflows with synthetic CMAF fixtures — no external services or network required.

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `encryption_roundtrip` | 8 | Full CBCS→plaintext→CENC pipeline: full-sample, pattern (1:9), subsample (NAL unit), multi-sample IV uniqueness, audio (0:0 pattern), cross-segment IV isolation |
| `isobmff_integration` | 18 | Synthetic init segment parsing and rewriting (scheme-aware: CBCS→CENC with configurable target, container-format-aware ftyp rewriting), PSSH box generation (Widevine+PlayReady, FairPlay exclusion for CENC), senc box roundtrip (with/without subsamples), media segment decrypt→re-encrypt→verify, error handling for malformed segments |
| `manifest_integration` | 20 | Progressive output lifecycle (HLS+DASH), manifest phase transitions, DRM signaling in manifests (scheme-aware: Widevine/PlayReady key URIs, dynamic METHOD selection, ContentProtection with scheme-specific value, cenc:pssh, mspr:pro), cache-control headers per phase, ManifestState serde roundtrip, cross-format consistency |
| `handler_integration` | 28 | HTTP routing for all endpoints (health, manifest, init, segment with .cmfv and .m4s extensions, status, webhook), webhook payload validation (valid/invalid JSON, missing fields), HttpResponse helpers (ok, accepted, error, cache headers), unknown routes (404), method filtering |

All integration tests use shared fixtures from `tests/common/mod.rs` that build synthetic ISOBMFF data (ftyp, moov, sinf, schm, tenc, pssh, moof, traf, trun, senc, mdat) programmatically in Rust — no external test media files needed.

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
GET /repackage/{content_id}/{format}/segment_{n}.m4s
```

- `{content_id}` — unique content identifier
- `{format}` — `hls` or `dash`
- `{n}` — segment number (0-indexed)
- Segment extension is `.cmfv` for CMAF output or `.m4s` for fMP4 output (both are accepted by the router)

### Proactive Repackaging (Webhook)

Trigger repackaging ahead of time so content is cached before clients request it.

```
POST /webhook/repackage
Content-Type: application/json

{
  "content_id": "my-content-123",
  "source_url": "https://origin.example.com/content/master.m3u8",
  "format": "hls",
  "key_ids": ["optional-hex-kid-1"],
  "target_scheme": "cenc",
  "container_format": "cmaf"
}
```

Returns `200 OK` as soon as the first segment and live manifest are published (clients can begin playback immediately):
```json
{
  "status": "processing",
  "content_id": "my-content-123",
  "format": "hls",
  "manifest_url": "/repackage/my-content-123/hls/manifest",
  "segments_completed": 1,
  "segments_total": 42
}
```

- `target_scheme` — `cenc` (default) or `cbcs`. Determines the output encryption scheme.
- `container_format` — `cmaf` (default) or `fmp4`. Determines the output container format. CMAF uses `.cmfv`/`.cmfa` extensions and includes the `cmfc` compatible brand; fMP4 uses `.m4s` extensions.

Remaining segments are processed asynchronously via self-invocation chaining. Each invocation processes one segment and chains the next via an internal `POST /webhook/repackage/continue` endpoint.

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

| Key | TTL | Sensitive | Purpose |
|-----|-----|-----------|---------|
| `ep:{id}:keys` | 24h | **Yes** | Cached DRM content keys |
| `ep:{id}:{fmt}:state` | 48h | No | Job state and progress |
| `ep:{id}:{fmt}:manifest_state` | 48h | No | Progressive manifest state (segment list, phase) |
| `ep:{id}:{fmt}:init` | 48h | No | Rewritten init segment binary data |
| `ep:{id}:{fmt}:seg:{n}` | 48h | No | Rewritten media segment binary data |
| `ep:{id}:{fmt}:source` | 48h | No | Source manifest metadata (segment URLs, durations) |
| `ep:{id}:{fmt}:rewrite_params` | 48h | **Yes** | Continuation parameters (encryption keys, IV sizes, pattern) |
| `ep:{id}:speke` | 24h | **Yes** | Cached SPEKE license server responses |

### Security Model

Sensitive cache entries (marked **Yes** above) are protected with two layers:

1. **Encryption at rest** — All sensitive values are encrypted with AES-256-GCM before being stored in Redis. The encryption key is derived from the `REDIS_TOKEN` using AES-128-ECB as a PRF on two distinct constant blocks, producing 32 bytes of key material. Wire format: `nonce (12 bytes) || ciphertext || tag (16 bytes)`. Non-sensitive keys pass through unencrypted.

2. **Immediate cleanup** — As soon as the pipeline finishes processing all segments (whether via `execute()`, `execute_first()` for single-segment content, or `execute_remaining()` for the final segment), all sensitive cache entries are explicitly deleted. This ensures DRM keys, SPEKE responses, and rewrite parameters do not persist in Redis beyond the active processing window. Cleanup failures are intentionally swallowed so they cannot prevent the pipeline from reporting success.

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
                    │                  │ (CMAF/  (SPEKE) │   │
                    │                  │  fMP4)          │   │
                    │                  │    ↓       ↓     │   │
                    │                  │  Manifest Redis  │   │
                    │                  └────┬───────┬─────┘   │
                    └───────────────────────┼───────┼─────────┘
                                            │       │
                              Origin ◄──────┘       └──────► License Server
                              (CBCS/CENC source)             (SPEKE 2.0)
```

### Module Dependency Graph

```
handler/ ──► repackager/ ──► media/     (CMAF/fMP4 parse + rewrite)
                         ──► drm/      (SPEKE + scheme-aware encrypt/decrypt)
                         ──► manifest/ (HLS/DASH generation)
                         ──► cache/    (Redis state)
```

### Detailed Architecture Diagrams

See [`docs/architecture.md`](docs/architecture.md) for detailed Mermaid diagrams covering:

- System context (CDN infrastructure, external dependencies)
- End-to-end data flow (configurable source → transform → configurable target encryption and container format)
- Internal module architecture and dependency graph
- Split execution model (WASI self-invocation chaining sequence)
- Progressive output state machine (AwaitingFirstSegment → Live → Complete)
- Cache security model (AES-256-GCM encryption + post-processing cleanup)
- Redis cache key layout with sensitivity classification
- CDN caching strategy per resource type
- Per-segment encryption transform detail (ISOBMFF box-level)
- Container format comparison (CMAF vs fMP4)

All diagrams use Mermaid syntax and can be imported into Confluence (Mermaid macro), Jira, and Lucidchart (File → Import → Mermaid).

## Supported Encryption Schemes

The target encryption scheme is configurable per request via the `target_scheme` field (default: `cenc`). The source scheme is auto-detected from the init segment's `schm` box or from manifest DRM signaling (`#EXT-X-KEY` in HLS, `<ContentProtection>` in DASH).

| Scheme | Mode | Pattern | IV Size | DRM Systems |
|--------|------|---------|---------|-------------|
| CBCS | AES-128-CBC | 1:9 (video), 0:0 (audio) | 16 bytes | FairPlay, Widevine, PlayReady |
| CENC | AES-128-CTR | None (full encryption) | 8 bytes | Widevine, PlayReady |

### Supported Transforms

| Source → Target | Description |
|-----------------|-------------|
| CBCS → CENC | CBC pattern → CTR full encryption (FairPlay → Widevine/PlayReady) |
| CENC → CBCS | CTR full → CBC pattern encryption (Widevine/PlayReady → FairPlay/Widevine/PlayReady) |
| CBCS → CBCS | Re-encrypt with different key, same scheme |
| CENC → CENC | Re-encrypt with different key, same scheme |

## Supported Container Formats

The output container format is configurable per request via the `container_format` field (default: `cmaf`). Both formats use ISOBMFF (ISO 14496-12) box structure and `video/mp4` / `audio/mp4` MIME types.

| Format | Description | Segment Extension | Init Extension | Compatible Brands | DASH Profile |
|--------|-------------|-------------------|----------------|-------------------|--------------|
| CMAF | Common Media Application Format (ISO 23000-19) | `.cmfv` (video), `.cmfa` (audio) | `.mp4` | `isom`, `iso6`, `cmfc` | includes `urn:mpeg:dash:profile:cmaf:2019` |
| fMP4 | Fragmented MP4 (ISO 14496-12) | `.m4s` | `.mp4` | `isom`, `iso6` | `urn:mpeg:dash:profile:isoff-live:2011` only |

The init segment's `ftyp` box is rewritten at output to match the target container format. CMAF is a constrained profile of fMP4 — both are structurally identical fragmented MP4, differing only in ftyp brands, segment file extensions, and DASH profile signaling.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `aes`, `cbc`, `ctr`, `cipher` | AES encryption/decryption (CBCS and CENC) |
| `aes-gcm` | AES-256-GCM authenticated encryption for cache-at-rest security |
| `quick-xml` | CPIX XML and DASH MPD parsing/generation |
| `serde`, `serde_json` | Serialization for config, Redis, webhooks |
| `base64` | Key encoding in CPIX, PSSH data in manifests |
| `uuid` | Content Key ID (KID) handling |
| `url` | URL parsing |
| `thiserror` | Error type derivation |
| `log` | Logging facade |
| `wasi` | WASI Preview 2 bindings (wasm32 target only) |

All dependencies are selected for WASM compatibility (no system calls, no async runtime).

### Sandbox-Only Dependencies

These are only included when building with `--features sandbox` and are gated behind `cfg(not(target_arch = "wasm32"))` — they never appear in the WASM build.

| Crate | Purpose |
|-------|---------|
| `axum` | HTTP server for sandbox web UI |
| `tokio` | Async runtime for Axum |
| `reqwest` | Native HTTP client (replaces WASI HTTP transport) |
| `tower-http` | Static file serving for local manifest files |
| `tracing-subscriber` | Log output for sandbox |

## Local Sandbox

The sandbox lets you test the full repackaging pipeline locally without deploying to a CDN edge. It reuses the same `RepackagePipeline` as the production WASM build, but with `reqwest` for HTTP transport and an in-memory cache instead of Redis.

### Running

```bash
cargo run --bin sandbox --features sandbox
```

The web UI is available at **http://localhost:3333**.

### What You Need

- A source manifest URL (HLS `.m3u8` or DASH `.mpd`) pointing to CBCS- or CENC-encrypted CMAF content
- A SPEKE 2.0 license server endpoint URL and credentials (bearer token, API key, or basic auth)

You can also use a local file path (e.g. `./content/master.m3u8`) — the sandbox automatically starts a local HTTP server to serve the directory.

### How It Works

1. The web UI collects source URL, SPEKE credentials, and output format
2. The sandbox builds an `AppConfig` and `RepackageRequest`, then runs `RepackagePipeline::execute()` in a blocking thread
3. The pipeline fetches the source manifest, gets DRM keys via SPEKE, and repackages all segments
4. Progress is polled from the shared in-memory cache via `/api/status/{id}/{format}`
5. On completion, output is written to disk at `sandbox/output/{content_id}/{format}/`

### Output Structure

```
sandbox/output/{content_id}/{format}/
├── manifest.m3u8   (or manifest.mpd)
├── init.mp4
├── segment_0.cmfv  (CMAF) or segment_0.m4s (fMP4)
├── segment_1.cmfv  (CMAF) or segment_1.m4s (fMP4)
└── ...
```

Segment file extensions are determined by the `container_format` setting (`.cmfv` for CMAF, `.m4s` for fMP4).

### Sandbox API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Web UI |
| POST | `/api/repackage` | Start repackaging job |
| GET | `/api/status/{id}/{format}` | Poll job progress |
| GET | `/api/output/{id}/{format}/{file}` | Serve output files |

## Project Status

The runtime is fully implemented and compiles to a functional WASM component:

- **WASI HTTP transport**: Shared HTTP client (`http_client.rs`) uses `wasi:http/outgoing-handler` for all outbound requests (Redis, SPEKE, origin fetching). Native builds return a stub error to preserve test builds.
- **WASI incoming handler**: `wasi_handler.rs` bridges `wasi:http/incoming-handler` to the library router. Converts WASI request/response types and maps errors to appropriate HTTP status codes.
- **Source manifest parsing**: HLS M3U8 (`manifest/hls_input.rs`) and DASH MPD (`manifest/dash_input.rs`) input parsers extract segment URLs, durations, init segment references, live/VOD detection, and source encryption scheme (from `#EXT-X-KEY` METHOD in HLS or `<ContentProtection>` elements in DASH).
- **Request handler wiring**: All GET handlers query Redis for cached segment data and manifest state. The webhook creates a `RepackagePipeline`, processes the first segment to produce a live manifest, and chains remaining processing via self-invocation.
- **Configurable encryption**: Target encryption scheme (CBCS or CENC) is set per request. Source scheme auto-detected.
- **Configurable container format**: Output container format (CMAF or fMP4) is set per request. Controls ftyp brands, segment extensions, and DASH profile signaling.

## Roadmap

Phase 1 (encryption scheme generalization) and Phase 2 (container format flexibility) are complete. The following phases are planned:

### ~~Phase 2: Container Format Flexibility (CMAF + fMP4)~~ ✅ Complete

- [x] Created `src/media/container.rs` — `ContainerFormat` enum (`Cmaf`, `Fmp4`) with brand/extension/DASH profile helpers
- [x] Added ftyp box rewriting in `src/media/init.rs` for output container format
- [x] Wired `container_format` through `RepackageRequest`, `WebhookPayload`, `ManifestState`, `ContinuationParams`, and manifest renderers
- [x] Updated segment URI extensions dynamically based on container format (`.cmfv` for CMAF, `.m4s` for fMP4)
- [x] Updated route handler to accept both `.cmfv` and `.m4s` segment file extensions
- [x] Updated DASH renderer with dynamic profile string and segment template extension

### Phase 3: Dual-Scheme Output

- [ ] Support `target_schemes: Vec<EncryptionScheme>` for multi-rendition output (one rendition per scheme)
- [ ] Implement scheme-suffixed cache keys (`ep:{id}:{fmt}:cenc:seg:{n}`, `ep:{id}:{fmt}:cbcs:seg:{n}`)
- [ ] Implement dual-encrypted segments (single segment with both CBCS and CENC applied)
- [ ] Multi-variant HLS master playlist and multi-AdaptationSet DASH MPD for dual-scheme output

### Phase 4: Full Remux (Sample-Level mdat Access)

- [ ] Create `src/media/samples.rs` — sample-level parsing and rebuilding from mdat + trun + senc
- [ ] Segment boundary restructuring: split/merge samples at sync points to target duration
- [ ] Timescale parsing from mdhd/mvhd boxes
- [ ] Update progressive output to handle variable segment counts (remux may change segment boundaries)

### Phase 5: Compatibility Validation & Hardening

- [ ] Create `src/media/compat.rs` — compatibility checker (e.g. Chromium 53: CENC-only, H.264+AAC, fMP4)
- [ ] Codec detection from stsd sample entries (avc1/hev1/mp4a)
- [ ] Pipeline validation hooks: reject incompatible configs early
- [ ] Add `Compatibility` and `UnsupportedCodec` error variants

## License

Proprietary.
