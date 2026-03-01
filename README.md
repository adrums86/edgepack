<img width="360" height="240" alt="edgepack_logo" src="edgepack.png" />


# edgepack

A Rust application compiled to WebAssembly for CDN edge environments. It repackages DASH and HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4 ↔ ISO BMFF), producing progressive output manifests and segments cached at the CDN for maximum duration. Supports all encryption scheme combinations, clear content paths, automatic source scheme detection, and **dual-scheme output** — a single request can produce both CBCS and CENC renditions simultaneously, each with independent cache keys, manifests, and segments.

## What It Does

1. **Receives a request** to repackage content (on-demand via HTTP or proactively via webhook)
2. **Fetches DRM keys** from a license server using the SPEKE 2.0 protocol and CPIX standard
3. **Fetches source media** (CMAF init + media segments) from the origin
4. **Decrypts** each segment using the source encryption scheme (CBCS or CENC, auto-detected from the init segment)
5. **Re-encrypts** each segment for one or more target schemes (CBCS, CENC, or None — configurable per request, supports dual-scheme output)
6. **Rewrites** init segments per target scheme (protection scheme info, PSSH boxes, DRM signaling, ftyp brands for container format)
7. **Outputs progressively** — writes a live manifest as soon as the first segment is ready, updates with each segment, finalises when complete
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

# Release build (size-optimised: opt-level=z, LTO, stripped, codegen-units=1, panic=abort)
cargo build --release
```

Output WASM binary (<600 KB):
```
target/wasm32-wasip2/release/edgepack.wasm
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

The project includes **652 tests** (538 unit tests + 114 integration tests) covering every module, plus a binary size guard ensuring the release WASM stays under 600 KB. To run tests for a specific module:

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

#### Unit Test Coverage (538 tests)

| Module | Tests | What's Covered |
|--------|-------|----------------|
| `error` | 16 | Error display strings, Result alias |
| `config` | 11 | Defaults, serde roundtrips, env var loading |
| `url` | 14 | URL parsing, join (absolute/relative/protocol-relative, normalization), serde roundtrip, authority extraction |
| `cache` | 48 | CacheKeys formatting (incl. scheme-qualified keys), backend factory, Upstash JSON parsing, in-memory cache ops, encrypted backend (AES-256-GCM roundtrip, tamper detection, key sensitivity, key derivation) |
| `drm` | 115 | EncryptionScheme enum (serde, bytes, from_scheme_type, from_str_value, HLS methods, IV sizes, patterns, FairPlay flags, `is_encrypted()`, None variant), SampleDecryptor/SampleEncryptor (factory dispatch, CBCS/CENC roundtrips), system IDs, CPIX XML, SPEKE client |
| `media` | 115 | FourCC types, ISOBMFF box parsing/building/iteration, ContainerFormat enum, init segment rewriting (scheme-aware, container-format-aware, sinf injection/stripping, ftyp rewriting), segment rewriting (four-way dispatch), IV padding |
| `manifest` | 93 | HLS/DASH rendering for all lifecycle phases, DRM scheme signaling, FairPlay key URI, variant streams, ISO 8601 duration, KID formatting, HLS/DASH input parsing (source scheme detection) |
| `repackager` | 48 | Job types/serde, progressive output state machine, cache-control headers, key set caching, continuation params, pipeline execution, DRM info building, sensitive data cleanup (incl. per-scheme) |
| `handler` | 73 | HTTP routing, path parsing incl. scheme-qualified formats (`hls_cenc`, `dash_cbcs`), segment number parsing (all 7 extensions), webhook validation (target_schemes array, backward compat, duplicate/invalid rejection), response construction |
| `http_client` | 5 | Response construction, native stub errors |

#### Integration Test Coverage (114 tests)

Integration tests live in `tests/` and use synthetic CMAF fixtures — no external services or network required.

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `clear_content` | 10 | Clear→CENC/CBCS, encrypted→clear, clear→clear, roundtrip pipelines |
| `encryption_roundtrip` | 8 | CBCS→plaintext→CENC: full-sample, pattern, subsample, multi-sample IV, audio, cross-segment IV isolation |
| `isobmff_integration` | 18 | Init segment rewriting (scheme/container-aware), PSSH generation, senc roundtrip, segment decrypt→re-encrypt→verify |
| `manifest_integration` | 23 | Progressive output lifecycle (HLS+DASH, all container formats), DRM signaling, cache-control headers, ManifestState serde |
| `handler_integration` | 32 | HTTP routing for all endpoints, webhook validation, HttpResponse helpers, method filtering |
| `dual_scheme` | 22 | Scheme-qualified route parsing, cache key uniqueness per scheme, multi-scheme webhook payloads, backward compat, duplicate/invalid scheme rejection |
| `wasm_binary_size` | 1 | Release WASM binary stays under 600 KB |

All tests use shared fixtures from `tests/common/mod.rs` that build synthetic ISOBMFF data programmatically — no external test media files needed.

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
GET /repackage/{content_id}/{format}/segment_{n}.{ext}
```

- `{content_id}` — unique content identifier
- `{format}` — `hls`, `dash`, or scheme-qualified: `hls_cenc`, `hls_cbcs`, `dash_cenc`, `dash_cbcs`, `hls_none`, `dash_none`
- `{n}` — segment number (0-indexed)
- `{ext}` — any CMAF or ISOBMFF segment extension (see [Supported Segment Extensions](#supported-segment-extensions))

Scheme-qualified format paths (e.g., `hls_cenc`) route to scheme-specific cached data. Plain format paths (`hls`, `dash`) route to the default/sole target scheme for backward compatibility.

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
  "target_schemes": ["cenc", "cbcs"],
  "container_format": "cmaf"
}
```

Returns `200 OK` as soon as the first segment and live manifest are published per scheme:
```json
{
  "status": "processing",
  "content_id": "my-content-123",
  "format": "hls",
  "manifest_urls": {
    "cenc": "/repackage/my-content-123/hls_cenc/manifest",
    "cbcs": "/repackage/my-content-123/hls_cbcs/manifest"
  },
  "segments_completed": 1,
  "segments_total": 42
}
```

- `target_schemes` — array of `"cenc"`, `"cbcs"`, and/or `"none"`. Defaults to `["cenc"]`. Each scheme produces independent init segments, media segments, and manifests with scheme-qualified cache keys.
- `target_scheme` — (backward compat) single string; treated as `target_schemes: [value]`. If both are present, `target_schemes` takes precedence.
- `container_format` — `cmaf` (default), `fmp4`, or `iso`.

Remaining segments are processed asynchronously via self-invocation chaining (`POST /webhook/repackage/continue`). Source segments are decrypted once and re-encrypted per target scheme.

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

Keys marked with † are only written by the split execution path (`execute_first()`/`execute_remaining()`) used in WASI production. Keys marked with ‡ use scheme-qualified format (e.g., `hls_cenc` instead of `hls`) for dual-scheme output.

| Key | TTL | Sensitive | Purpose |
|-----|-----|-----------|---------|
| `ep:{id}:keys` | 24h | **Yes** | Cached DRM content keys |
| `ep:{id}:{fmt}:state` | 48h | No | Job state and progress |
| `ep:{id}:{fmt}:manifest_state` †‡ | 48h | No | Progressive manifest state (segment list, phase) |
| `ep:{id}:{fmt}:init` †‡ | 48h | No | Rewritten init segment binary data |
| `ep:{id}:{fmt}:seg:{n}` †‡ | 48h | No | Rewritten media segment binary data |
| `ep:{id}:{fmt}:source` † | 48h | No | Source manifest metadata (segment URLs, durations) |
| `ep:{id}:{fmt}:rewrite_params` †‡ | 48h | **Yes** | Continuation parameters (encryption keys, IV sizes, pattern) |
| `ep:{id}:{fmt}:target_schemes` † | 48h | No | List of target schemes for multi-scheme continuation |
| `ep:{id}:speke` | 24h | **Yes** | Cached SPEKE license server responses |

### Security Model

Sensitive cache entries (marked **Yes** above) are protected with two layers:

1. **Encryption at rest** — All sensitive values are encrypted with AES-256-GCM before being stored in Redis. The encryption key is derived from the `REDIS_TOKEN` using AES-128-ECB as a PRF on two distinct constant blocks, producing 32 bytes of key material. Wire format: `nonce (12 bytes) || ciphertext || tag (16 bytes)`. Non-sensitive keys pass through unencrypted.

2. **Immediate cleanup** — As soon as the pipeline finishes processing all segments, all sensitive cache entries (DRM keys, SPEKE responses, per-scheme rewrite parameters, target schemes list, source manifest) are explicitly deleted. For dual-scheme output, per-scheme rewrite params are deleted for each target scheme. Cleanup failures are intentionally swallowed so they cannot prevent the pipeline from reporting success.

## Architecture

```
                    ┌─────────────────────────────────────────┐
                    │            CDN Edge Node                │
                    │                                         │
   Client ──GET──►  │  ┌──────────┐    ┌──────────────────┐   │
                    │  │ CDN Cache│◄───│    edgepack      │   │
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

See [`docs/architecture.md`](docs/architecture.md) for Mermaid diagrams covering system context, data flow, module architecture, split execution model, progressive output state machine, cache security, and per-segment encryption transforms.

## Supported Encryption Schemes

Target encryption scheme(s) are configurable per request via `target_schemes` (default: `["cenc"]`). Multiple schemes can be specified for dual-scheme output. The source scheme is auto-detected from the init segment's `schm` box or manifest DRM signaling.

| Scheme | Mode | Pattern | IV Size | DRM Systems |
|--------|------|---------|---------|-------------|
| CBCS | AES-128-CBC | 1:9 (video), 0:0 (audio) | 16 bytes | FairPlay, Widevine, PlayReady |
| CENC | AES-128-CTR | None (full encryption) | 8 bytes | Widevine, PlayReady |
| None | Clear (no encryption) | N/A | 0 bytes | N/A |

### Supported Transforms

| Source → Target | Description |
|-----------------|-------------|
| CBCS → CENC | CBC pattern → CTR full encryption (FairPlay → Widevine/PlayReady) |
| CENC → CBCS | CTR full → CBC pattern encryption (Widevine/PlayReady → FairPlay/Widevine/PlayReady) |
| CBCS → CBCS | Re-encrypt with different key, same scheme |
| CENC → CENC | Re-encrypt with different key, same scheme |
| Clear → CENC | Encrypt clear content with CTR full encryption |
| Clear → CBCS | Encrypt clear content with CBC pattern encryption |
| CBCS → Clear | Decrypt CBCS content to clear (strip DRM) |
| CENC → Clear | Decrypt CENC content to clear (strip DRM) |
| Clear → Clear | Format-only conversion (no encryption/decryption) |

## Supported Container Formats

The output container format is configurable per request via the `container_format` field (default: `cmaf`). Both formats use ISOBMFF (ISO 14496-12) box structure and `video/mp4` / `audio/mp4` MIME types.

| Format | Description | Segment Extension | Init Extension | Compatible Brands | DASH Profile |
|--------|-------------|-------------------|----------------|-------------------|--------------|
| CMAF | Common Media Application Format (ISO 23000-19) | `.cmfv` (video), `.cmfa` (audio) | `.mp4` | `isom`, `iso6`, `cmfc` | includes `urn:mpeg:dash:profile:cmaf:2019` |
| fMP4 | Fragmented MP4 (ISO 14496-12) | `.m4s` | `.mp4` | `isom`, `iso6` | `urn:mpeg:dash:profile:isoff-live:2011` only |
| ISO BMFF | ISO Base Media File Format (ISO 14496-12) | `.mp4` | `.mp4` | `isom`, `iso6` | `urn:mpeg:dash:profile:isoff-live:2011` only |

The init segment's `ftyp` box is rewritten to match the target container format. All three formats are structurally identical fragmented MP4, differing in ftyp brands, segment extensions, and DASH profile signaling.

### Supported Segment Extensions

The route handler accepts all standard CMAF and ISOBMFF segment extensions. The extension is stripped to extract the segment number — actual segment data is served identically regardless of extension.

| Extension | Standard | Type | Notes |
|-----------|----------|------|-------|
| `.cmfv` | ISO 23000-19 (CMAF) | Video segment | CMAF video output default |
| `.cmfa` | ISO 23000-19 (CMAF) | Audio segment | CMAF audio |
| `.cmft` | ISO 23000-19 (CMAF) | Text segment | CMAF subtitles/captions |
| `.cmfm` | ISO 23000-19 (CMAF) | Multiplexed segment | CMAF combined audio+video |
| `.m4s` | ISO 14496-12 (ISOBMFF) | Media segment | fMP4 output default |
| `.mp4` | ISO 14496-12 (ISOBMFF) | Generic container | ISO BMFF output default; also used for all init segments |
| `.m4a` | ISO 14496-12 (ISOBMFF) | Audio container | ISOBMFF audio-only |

Input parsing is extension-agnostic — the parsers fetch whatever URL the source manifest specifies.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `aes`, `cbc`, `ctr`, `cipher` | AES encryption/decryption (CBCS and CENC) |
| `aes-gcm` | AES-256-GCM authenticated encryption for cache-at-rest security |
| `quick-xml` | CPIX XML and DASH MPD parsing/generation |
| `serde`, `serde_json` | Serialization for config, Redis, webhooks |
| `base64` | Key encoding in CPIX, PSSH data in manifests |
| `uuid` | Content Key ID (KID) handling |
| `thiserror` | Error type derivation |
| `log` | Logging facade |
| `wasi` | WASI Preview 2 bindings (wasm32 target only) |

URL parsing uses a lightweight built-in module (`src/url.rs`) instead of the `url` crate, saving ~200 KB in the WASM binary. All dependencies are selected for WASM compatibility (no system calls, no async runtime).

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

The sandbox tests the full repackaging pipeline locally without deploying to a CDN edge. It reuses the same `RepackagePipeline` as the production WASM build, with `reqwest` for HTTP and an in-memory cache instead of Redis.

### Running

```bash
cargo run --bin sandbox --features sandbox --target $(rustc -vV | grep host | awk '{print $2}')
```

The web UI is available at **http://localhost:3333**.

### What You Need

- A source manifest URL (HLS `.m3u8` or DASH `.mpd`) pointing to CMAF content (encrypted or clear)
- A SPEKE 2.0 license server endpoint and credentials (if source or target is encrypted)

Local file paths (e.g. `./content/master.m3u8`) are also supported — the sandbox starts a local HTTP server automatically.

### How It Works

1. The web UI collects source URL, SPEKE credentials, and output format
2. The sandbox builds an `AppConfig` and `RepackageRequest`, then runs `RepackagePipeline::execute()` in a blocking thread
3. The pipeline fetches the source manifest, gets DRM keys via SPEKE, and repackages all segments — returning `(JobStatus, Vec<(EncryptionScheme, ProgressiveOutput)>)` with per-scheme output in memory
4. Progress is polled from the shared in-memory cache via `/api/status/{id}/{format}`
5. On completion, output is written to disk per scheme at `sandbox/output/{content_id}/{format}_{scheme}/`

### Output Structure

```
sandbox/output/{content_id}/{format}_{scheme}/
├── manifest.m3u8   (or manifest.mpd)
├── init.mp4
├── segment_0.cmfv  (or .m4s or .mp4)
├── segment_1.cmfv
└── ...
```

For dual-scheme output, each scheme gets its own directory (e.g., `hls_cenc/` and `hls_cbcs/`). Segment extensions are determined by `container_format`.

### Sandbox API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Web UI |
| POST | `/api/repackage` | Start repackaging job |
| GET | `/api/status/{id}/{format}` | Poll job progress |
| GET | `/api/output/{id}/{format}/{file}` | Serve output files (from disk) |

## Project Status

The runtime is fully implemented and compiles to a functional WASM component. All nine encryption scheme combinations, three container formats, and dual-scheme output are supported. The WASI component handles HTTP routing, source manifest parsing (HLS/DASH), DRM key acquisition (SPEKE 2.0), segment re-encryption, and progressive manifest output. Split execution via self-invocation chaining processes segments within WASI memory limits.

## Roadmap

Phases 1–4 are complete. The following phases are planned next.

### ~~Phase 2: Container Format Flexibility (CMAF + fMP4)~~ ✅

- [x] `ContainerFormat` enum (CMAF/fMP4/ISO) with brand/extension/DASH profile helpers
- [x] ftyp box rewriting, container_format wired through request/manifest pipeline
- [x] Route handler accepts all 7 CMAF/ISOBMFF segment extensions

### ~~Phase 3: Unencrypted Input Support~~ ✅

- [x] `EncryptionScheme::None` variant with four-way init/segment dispatch
- [x] sinf injection (clear→encrypted), sinf stripping (encrypted→clear), ftyp-only rewrite (clear→clear)
- [x] Conditional SPEKE key acquisition (skipped when both source and target are clear)

### ~~Phase 4: Dual-Scheme Output~~ ✅

- [x] `target_schemes: Vec<EncryptionScheme>` for multi-rendition output (one rendition per scheme)
- [x] Scheme-qualified cache keys (`ep:{id}:{fmt}_{scheme}:seg:{n}`) and route paths (`hls_cenc`, `dash_cbcs`)
- [x] Decrypt source once, re-encrypt per target scheme in pipeline (`execute`, `execute_first`, `execute_remaining`)
- [x] Per-scheme init segments, media segments, manifests, and continuation params
- [x] Backward-compatible single `target_scheme` field and plain format paths
- [x] 22 integration tests for dual-scheme routing, cache keys, webhooks, and validation

### Phase 5: Multi-Key PSSH (Layered & Per-Track Keys)

- [ ] Support multiple content key IDs per request for per-track (audio/video) or per-scheme keying
- [ ] Multi-key PSSH box generation — embed CBCS and CENC key IDs in a single init segment for layered encryption
- [ ] Multi-key SPEKE requests — fetch keys for multiple KIDs in a single CPIX exchange
- [ ] Per-track sinf/tenc — assign different keys to different sample entries (e.g., audio key ≠ video key)
- [ ] Manifest signaling for multi-key content (multiple `#EXT-X-KEY` / `<ContentProtection>` per representation)

### Phase 6: Full Remux (Sample-Level mdat Access)

- [ ] Sample-level parsing and rebuilding from mdat + trun + senc
- [ ] Segment boundary restructuring at sync points
- [ ] Timescale parsing from mdhd/mvhd boxes

### Phase 7: Compatibility Validation & Hardening

- [ ] Compatibility checker (e.g. Chromium 53: CENC-only, H.264+AAC, fMP4)
- [ ] Codec detection from stsd sample entries
- [ ] Pipeline validation hooks for incompatible configs

## License

Proprietary.
