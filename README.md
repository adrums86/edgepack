<img width="460" height="340" alt="edgepack_logo" src="edgepack.png" />

# edgepack

A lightweight media repackager that runs as a WebAssembly module directly on CDN edge nodes. Compiled from Rust to `wasm32-wasip2`, the ~648 KB binary instantiates in under 1 ms — enabling **just-in-time packaging** where content is repackaged on the first viewer request rather than pre-processed in a central origin. This eliminates the origin packaging bottleneck: no batch jobs, no packaging queues, no storage of pre-packaged variants.

edgepack repackages DASH and HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4 ↔ ISO BMFF), producing progressive output manifests and segments cached at the CDN for maximum duration. Supports **dual-scheme output** (CBCS + CENC simultaneously), **multi-key DRM** with per-track keying, **SCTE-35 ad marker pass-through** (emsg extraction, HLS `#EXT-X-DATERANGE`, DASH `<EventStream>`), **subtitle/text track pass-through** (WebVTT/TTML, CEA-608/708), **advanced DRM** (ClearKey, raw key mode, key rotation, clear lead), **low-latency streaming** (LL-HLS partial segments, LL-DASH), **MPEG-TS input** (TS demux + CMAF transmux, feature-gated), and codec string extraction for manifest signaling.

## Why WASM at the Edge

Traditional media packaging happens at the origin — tools like Shaka Packager, AWS Elemental MediaPackage, or ffmpeg run in a central location, pre-packaging every format and DRM scheme combination before a single viewer requests it. This creates real problems at scale:

| Problem | Origin-Side Packaging | edgepack (Edge JIT) |
|---------|----------------------|---------------------|
| **Storage cost** | Store every format/scheme variant (HLS+DASH × CENC+CBCS = 4x) | Store source once, package on demand |
| **Time to first play** | Wait for batch packaging to complete | Package on first GET — viewer triggers it |
| **Long-tail waste** | Pre-package titles that may never be watched | Only package what's actually requested |
| **Format explosion** | Adding a new DRM scheme doubles your storage | Add a scheme with zero pre-processing |
| **Geographic latency** | Package at origin, serve globally | Package at the nearest edge node |
| **Scaling** | Provision packaging infrastructure for peak ingest | CDN edge scales horizontally, no infrastructure to manage |

The key enabler is fast cold starts. A WASM module has no process to boot, no runtime to initialize, no JVM to warm up — it's instantiated from a binary blob in under a millisecond. When a viewer in Tokyo requests an HLS/CBCS manifest that hasn't been packaged yet, the edge node in Tokyo instantiates edgepack, fetches the source segment from origin, repackages it, caches the result, and serves it — all within a typical segment request timeout. Subsequent requests for the same segment hit the CDN cache and never touch edgepack again.

## What It Does

1. **Receives a request** to repackage content — either **on-demand via HTTP GET** (JIT: package on first viewer request) or **proactively via webhook** (pre-package before viewers arrive)
2. **Fetches DRM keys** from a license server using the SPEKE 2.0 protocol and CPIX standard (supports multi-key — separate keys for video and audio tracks)
3. **Fetches source media** (CMAF init + media segments) from the origin, extracting per-track codec strings, key IDs, and language metadata
4. **Validates compatibility** — pre-flight checks catch invalid codec/scheme combinations (e.g., VP9+CBCS) before expensive crypto operations
5. **Decrypts** each segment using the source encryption scheme (CBCS or CENC, auto-detected from the init segment)
6. **Re-encrypts** each segment for one or more target schemes (CBCS, CENC, or None — configurable per request, supports dual-scheme output)
7. **Extracts SCTE-35 ad markers** from `emsg` boxes in media segments and signals them in output manifests (`#EXT-X-DATERANGE` for HLS, `<EventStream>` for DASH)
8. **Passes through subtitle tracks** — WebVTT (`wvtt`) and TTML (`stpp`) sample entries are never encrypted and flow through unchanged
9. **Rewrites** init segments per target scheme (per-track protection scheme info with track-specific KIDs, multi-KID PSSH boxes, DRM signaling, ftyp brands for container format)
10. **Outputs progressively** — writes a live manifest as soon as the first segment is ready, updates with each segment, finalises when complete. Manifests include subtitle rendition groups, CEA-608/708 closed caption signaling, and SCTE-35 ad break markers
11. **Caches aggressively** — segments are immutable with 1-year cache headers; live manifests have 1-second TTL; finalised manifests become immutable. Once cached, the edge worker is never invoked again for that segment

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- WASM target: `wasm32-wasip2`
- A cache backend: Redis (Upstash recommended), Cloudflare Workers KV (`cloudflare` feature), or generic HTTP KV
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

# With JIT on-demand packaging
cargo build --release --features jit

# With MPEG-TS input support
cargo build --release --features ts

# All features (JIT + Cloudflare KV + MPEG-TS)
cargo build --release --features jit,cloudflare,ts
```

Output WASM binary:
```
target/wasm32-wasip2/release/edgepack.wasm
```

#### Feature Flags

| Feature | Description |
|---------|-------------|
| `jit` | JIT on-demand packaging (manifest/init/segment on GET cache miss) |
| `cloudflare` | Cloudflare Workers KV cache backend |
| `ts` | MPEG-TS input support (TS demux + CMAF transmux) |
| `sandbox` | Local development sandbox with web UI (native binary, not WASM) |

#### Binary Size by Build Variant

| Build | Command | Size | Functions | Cold Start Impact |
|-------|---------|------|-----------|-------------------|
| Base (no features) | `cargo build --release` | ~648 KB | ~1,973 | Baseline |
| JIT-only | `cargo build --release --features jit` | ~680 KB | ~2,030 | +32 KB, +57 fns |
| Full (excl. TS) | `cargo build --release --features jit,cloudflare` | ~685 KB | ~2,033 | +37 KB, +60 fns |
| TS-only | `cargo build --release --features ts` | ~720 KB | ~2,100 | +72 KB, +127 fns |
| Full (incl. TS) | `cargo build --release --features jit,cloudflare,ts` | ~725 KB | ~2,160 | +77 KB, +187 fns |

Per-feature binary size tests enforce limits (700 KB base, 750 KB JIT/full excl. TS, 800 KB TS-only, 850 KB full incl. TS) and report WASM function counts as a cold start proxy. Small binary size is critical for JIT packaging workflows where the WASM module is instantiated on every cache-miss GET request — sub-millisecond instantiation keeps first-byte latency low even when content hasn't been pre-packaged.

#### Why Binary Size Matters: Cold Start and JIT Packaging

WASM module instantiation time on CDN edge runtimes is roughly proportional to binary size and function count. This directly determines whether JIT packaging is viable — if cold starts are slow, on-demand repackaging adds unacceptable latency to viewer requests.

| Mode | Trigger | Cold Start Frequency | Latency Budget |
|------|---------|---------------------|----------------|
| **Proactive (webhook)** | `POST /webhook/repackage` | Once per content ingest | Seconds (background job) |
| **JIT (on-demand GET)** | `GET /repackage/{id}/...` on cache miss | Every uncached request | Milliseconds (user-facing) |

In JIT mode, the WASM module may be instantiated for every cache-miss request (manifest, init segment, or media segment). The ~648 KB base binary with ~1,973 functions instantiates in **under 1 ms** on modern WASI runtimes (wasmtime, V8) — fast enough that viewers don't notice the difference between a cache hit and a JIT-packaged response. Compare this to alternatives:

| Approach | Cold Start | Per-Request Overhead | Catalog Coverage |
|----------|-----------|---------------------|-----------------|
| **edgepack (WASM at edge)** | <1 ms | WASM instantiation + origin fetch + crypto | Package only what's requested |
| **Origin packager (Shaka/ffmpeg)** | N/A (pre-packaged) | None (pre-cached) | Must pre-package everything |
| **Lambda@Edge / Cloud Functions** | 50–500 ms | Container boot + runtime init | On-demand, but slow cold starts |
| **Native edge worker (JS/Rust)** | 1–5 ms | V8 isolate or native process | On-demand, CDN-specific |

edgepack's cold start is 50–500x faster than serverless functions and comparable to native edge workers, while being portable across any CDN that supports WASI Preview 2. This makes JIT packaging practical for scenarios where other approaches fall short:

- **Long-tail content** — a catalog of 100,000 titles with 4 format/scheme variants each would require 400,000 pre-packaged outputs. With JIT, you store 100,000 source assets and package on demand — the 95% of titles that get <1 request/day are never packaged at all
- **Multi-format explosion** — adding CBCS support to a CENC-only catalog doubles storage with origin packaging. With JIT, it's a config change — no reprocessing
- **Geographic efficiency** — a title popular in Japan but not Europe is packaged and cached only at Tokyo edge nodes, not replicated globally
- **Burst scaling** — cold starts during traffic spikes stay fast because there's no warm-up, no connection pool, no container to boot — just WASM instantiation + a single origin fetch

The release profile (`opt-level=z`, LTO, strip, `codegen-units=1`, `panic=abort`) and careful dependency management keep the binary well under 850 KB. Every dependency is evaluated for WASM size impact: the lightweight `src/url.rs` saves ~200 KB vs the `url` crate, there's no async runtime, no ICU/Unicode tables, and sandbox-only dependencies are completely excluded from the WASM build. Per-feature binary size tests in CI enforce these limits — a dependency that pushes the binary past the per-variant limit will fail the build.

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

The project includes **1,072 tests** (826 unit + 246 integration) with `--features jit,cloudflare`. With TS: **1,151 tests** (873 unit + 278 integration) with `--features jit,cloudflare,ts`. All tests cover every module, plus per-feature binary size guards for each build variant. To run tests for a specific module:

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

#### Unit Test Coverage (873 tests with all features)

| Module | Tests | What's Covered |
|--------|-------|----------------|
| `error` | 16 | Error display strings, Result alias |
| `config` | 17 | Defaults, serde roundtrips, env var loading |
| `url` | 14 | URL parsing, join (absolute/relative/protocol-relative, normalization), serde roundtrip, authority extraction |
| `cache` | 72 | CacheKeys formatting (incl. scheme-qualified keys), backend factory, Upstash JSON parsing, in-memory cache ops, encrypted backend (AES-256-GCM roundtrip, tamper detection, key sensitivity, key derivation) |
| `drm` | 121 | EncryptionScheme enum (serde, bytes, from_scheme_type, from_str_value, HLS methods, IV sizes, patterns, FairPlay flags, `is_encrypted()`, None variant), SampleDecryptor/SampleEncryptor (factory dispatch, CBCS/CENC roundtrips), system IDs, CPIX XML, SPEKE client (incl. ClearKey) |
| `media` | 272 | FourCC types, ISOBMFF box parsing/building/iteration, ContainerFormat enum, init segment rewriting (scheme-aware, container-format-aware, sinf injection/stripping, ftyp rewriting, per-track tenc with TrackKeyMapping, multi-KID PSSH generation), segment rewriting (four-way dispatch), IV padding, codec string extraction (AVC/HEVC/AAC/VP9/AV1/AC-3/EC-3/Opus/FLAC/WebVTT/TTML), track metadata parsing (hdlr, mdhd timescale + language, stsd sample entries), TrackKeyMapping (single/per_type/from_tracks, serde roundtrip), emsg box parsing (v0/v1) + builder roundtrips, SCTE-35 splice_info_section parsing (splice_insert, time_signal), codec/scheme compatibility validation, HDR format detection, init/segment structure validation (incl. chunk detection, TS demux, transmux -- ts feature) |
| `manifest` | 175 | HLS/DASH rendering for all lifecycle phases, DRM scheme signaling, FairPlay key URI, variant streams, subtitle rendition groups (HLS `TYPE=SUBTITLES`, DASH text AdaptationSet), CEA-608/708 closed caption signaling (HLS `TYPE=CLOSED-CAPTIONS` with `INSTREAM-ID`, DASH `Accessibility` descriptors), audio/subtitle language attributes, ISO 8601 duration, KID formatting, HLS/DASH input parsing (source scheme detection, `#EXT-X-DATERANGE` SCTE-35 ad breaks, DASH `EventStream` parsing), ad break manifest rendering (`#EXT-X-DATERANGE`, DASH `EventStream`) (incl. LL-HLS/LL-DASH types and rendering) |
| `repackager` | 91 | Job types/serde, progressive output state machine, cache-control headers, key set caching, continuation params (incl. TrackKeyMapping serialization), pipeline execution, DRM info building (multi-KID PSSH per system), track key mapping construction, variant building from tracks, sensitive data cleanup (incl. per-scheme) (incl. raw keys, key rotation, clear lead, progressive parts) |
| `handler` | 86 | HTTP routing, path parsing incl. scheme-qualified formats (`hls_cenc`, `dash_cbcs`), segment number parsing (all 7 extensions), webhook validation (target_schemes array, backward compat, duplicate/invalid rejection), response construction |
| `http_client` | 9 | Response construction, native stub errors |

#### Integration Test Coverage (278 tests with all features)

Integration tests live in `tests/` and use synthetic CMAF fixtures — no external services or network required.

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `advanced_drm` | 15 | Key rotation at segment boundaries, clear lead, ClearKey DRM, raw key mode |
| `cdn_adapters` | 24 | Backend type selection, config serde, create_backend factory, encryption token derivation (cloudflare feature) |
| `clear_content` | 10 | Clear→CENC/CBCS, encrypted→clear, clear→clear, roundtrip pipelines |
| `conformance` | 23 | Init segment structure (ftyp/sinf/pssh ordering), media segment structure (moof/mdat/senc), encryption roundtrip conformance, manifest correctness |
| `dual_scheme` | 22 | Scheme-qualified route parsing, cache key uniqueness per scheme, multi-scheme webhook payloads, backward compat, duplicate/invalid scheme rejection |
| `encryption_roundtrip` | 8 | CBCS→plaintext→CENC: full-sample, pattern, subsample, multi-sample IV, audio, cross-segment IV isolation |
| `handler_integration` | 32 | HTTP routing for all endpoints, webhook validation, HttpResponse helpers, method filtering |
| `isobmff_integration` | 18 | Init segment rewriting (scheme/container-aware), PSSH generation, senc roundtrip, segment decrypt→re-encrypt→verify |
| `jit_packaging` | 27 | JIT source config, on-demand setup, lock contention, backward compat (jit feature) |
| `ll_hls_dash` | 16 | LL-HLS partial segments, preload hints, server control, LL-DASH availability time offset, CMAF chunk boundary detection |
| `manifest_integration` | 23 | Progressive output lifecycle (HLS+DASH, all container formats), DRM signaling, cache-control headers, ManifestState serde |
| `multi_key` | 12 | Per-track tenc (video/audio KIDs), multi-KID PSSH generation, single-key backward compat, codec string extraction, TrackKeyMapping serde roundtrip, create→strip roundtrip, TrackKeyMapping::from_tracks |
| `scte35_integration` | 13 | emsg extraction, SCTE-35 parsing, HLS/DASH ad break rendering, source manifest ad marker roundtrip, AdBreakInfo serde |
| `ts_integration` | 30 | MPEG-TS demux, PES/TS packet parsing, TS-to-CMAF transmux, init segment synthesis, HLS-TS manifest parsing, AES-128 decryption (ts feature) |
| `wasm_binary_size` | 5 | Per-feature WASM binary size guards (base ≤700 KB, JIT ≤750 KB, full excl. TS ≤750 KB, TS ≤800 KB, full incl. TS ≤850 KB) with function count reporting |

All tests use shared fixtures from `tests/common/mod.rs` that build synthetic ISOBMFF data programmatically — no external test media files needed. Multi-key tests use separate video/audio KIDs and keys to verify per-track tenc, multi-KID PSSH, and TrackKeyMapping behavior.

> **Note:** Some test suites require feature flags. Run with `--features jit,cloudflare,ts` to include all 1,151 tests. Without optional features: 1,015 tests.

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
| `REDIS_BACKEND` | `http` | Legacy backend type: `http` or `tcp` (overridden by `CACHE_BACKEND`) |
| `CACHE_BACKEND` | `redis_http` | Backend type: `redis_http`, `redis_tcp`, `cloudflare_kv`, `http_kv` |
| `STORE_URL` | — | Cache store endpoint URL (falls back to `REDIS_URL`) |
| `STORE_TOKEN` | — | Cache store auth token (falls back to `REDIS_TOKEN`) |
| `CACHE_ENCRYPTION_TOKEN` | `STORE_TOKEN` | Token for cache encryption key derivation |

### Cloudflare Workers KV (requires `cloudflare` feature + `CACHE_BACKEND=cloudflare_kv`)

| Variable | Required | Description |
|----------|----------|-------------|
| `CF_ACCOUNT_ID` | Yes | Cloudflare account ID |
| `CF_KV_NAMESPACE_ID` | Yes | Workers KV namespace ID |
| `CF_API_TOKEN` | Yes | Cloudflare API token with KV permissions |

### Generic HTTP KV (requires `CACHE_BACKEND=http_kv`)

| Variable | Required | Description |
|----------|----------|-------------|
| `HTTP_KV_BASE_URL` | Yes | KV API base URL |
| `HTTP_KV_AUTH_HEADER` | No | Auth header name (default: `Authorization`) |
| `HTTP_KV_AUTH_VALUE` | Yes | Auth header value |

### JIT Packaging (requires `jit` feature)

| Variable | Default | Description |
|----------|---------|-------------|
| `JIT_ENABLED` | `false` | Enable JIT on-demand packaging |
| `JIT_SOURCE_URL_PATTERN` | — | URL template with `{content_id}` placeholder |
| `JIT_DEFAULT_TARGET_SCHEME` | `cenc` | Default scheme: `cenc` or `cbcs` |
| `JIT_DEFAULT_CONTAINER_FORMAT` | `cmaf` | Default format: `cmaf` or `fmp4` |
| `JIT_LOCK_TTL` | `30` | Processing lock TTL in seconds |

## API

### On-Demand Repackaging (JIT)

The core JIT packaging API. On a cache miss, the edge worker instantiates (<1 ms), fetches the source segment from origin, repackages it with the target DRM scheme, caches the result with immutable headers, and serves it — all in a single request. Subsequent requests hit the CDN cache directly without invoking edgepack.

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

Request coalescing via distributed locking (`set_nx`) ensures that concurrent requests for the same uncached segment don't trigger duplicate origin fetches — the first request does the work, others wait for the cached result.

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

The runtime is fully implemented and compiles to a ~648 KB WASM component that instantiates in under 1 ms — production-ready for JIT packaging at the edge. All P0 and P1 phases are complete. All nine encryption scheme combinations, three container formats, dual-scheme output, multi-key DRM with per-track keying, subtitle/text track pass-through, SCTE-35 ad marker pass-through, codec/scheme compatibility validation, advanced DRM (ClearKey, raw key mode, key rotation, clear lead), low-latency streaming (LL-HLS partial segments, LL-DASH), and MPEG-TS input (feature-gated TS demux + CMAF transmux) are supported. The WASI component handles HTTP routing, source manifest parsing (HLS/DASH/HLS-TS), JIT on-demand packaging with request coalescing, DRM key acquisition (SPEKE 2.0 with multi-KID CPIX, ClearKey, or raw keys), codec string extraction, per-track init segment rewriting, segment re-encryption, SCTE-35 ad break signaling (emsg extraction, HLS `#EXT-X-DATERANGE`, DASH `EventStream`), subtitle pass-through with manifest signaling (HLS rendition groups, DASH AdaptationSets, CEA-608/708 captions), and progressive manifest output with codec signaling. Split execution via self-invocation chaining processes segments within WASI memory limits. Portable across any CDN supporting WASI Preview 2 — multiple cache backends are supported (Redis HTTP, Cloudflare Workers KV, generic HTTP KV).

## Roadmap

Phases 1–11, 16, and 17 are complete. All P0 and P1 items are done. The roadmap targets feature parity with Shaka Packager and AWS Elemental MediaPackage, optimized for CDN edge deployment.

### Completed

| Phase | Name | Status |
|-------|------|--------|
| 1 | Core CBCS→CENC Conversion | ✅ |
| 2 | Container Format Flexibility (CMAF + fMP4 + ISO) | ✅ |
| 3 | Unencrypted Input Support (clear content paths) | ✅ |
| 4 | Dual-Scheme Output (multi-rendition per request) | ✅ |
| 5 | Multi-Key DRM & Codec Awareness | ✅ |
| 6 | Subtitle & Text Track Pass-Through | ✅ |
| 7 | SCTE-35 Ad Markers & Ad Break Signaling | ✅ |
| 8 | JIT Packaging (On-Demand GET) | ✅ |
| 9 | LL-HLS & LL-DASH | ✅ |
| 10 | MPEG-TS Input (feature-gated) | ✅ |
| 11 | Advanced DRM | ✅ |
| 16 | Compatibility Validation & Hardening | ✅ |
| 17 | CDN Provider Adapters & Binary Optimization | ✅ |

### Phase 6: Subtitle & Text Track Pass-Through ✅

- [x] WebVTT in fMP4 (`wvtt` sample entries) and TTML/EBU-TT (`stpp` sample entries) — pass-through via `encrypted_sample_entry_type()` returning `None` for subtitle codecs
- [x] `TrackMediaType::Subtitle` enum variant, `language` field on `VariantInfo` and `TrackInfo`
- [x] ISO 639-2/T language extraction from `mdhd` box (packed 3×5-bit chars)
- [x] CEA-608/708 manifest signaling (`CeaCaptionInfo` struct) — pass-through is automatic in video SEI NALs
- [x] HLS subtitle rendition groups (`#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"`) with `SUBTITLES="subs"` on `EXT-X-STREAM-INF`
- [x] HLS CEA caption signaling (`#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,INSTREAM-ID=...`) with `CLOSED-CAPTIONS="cc"` on `EXT-X-STREAM-INF`
- [x] DASH subtitle `<AdaptationSet contentType="text" mimeType="application/mp4">` with `lang` attribute
- [x] DASH CEA `<Accessibility schemeIdUri="urn:scte:dash:cc:cea-608:2015">` descriptors inside video AdaptationSet

### Phase 7: SCTE-35 Ad Markers & Ad Break Signaling ✅

- [x] `emsg` box parsing (version 0 and 1) with builder for roundtrip fidelity
- [x] SCTE-35 `splice_info_section` binary parser — `splice_insert` (0x05) and `time_signal` (0x06) commands
- [x] `extract_emsg_boxes()` scans media segments for event message boxes
- [x] `AdBreakInfo` type threaded through pipeline → `ProgressiveOutput` → `ManifestState` → renderers
- [x] HLS ad break signaling via `#EXT-X-DATERANGE` with `SCTE35-CMD` hex encoding and `PLANNED-DURATION`
- [x] DASH ad break signaling via `<EventStream schemeIdUri="urn:scte:scte35:2013:bin">` with `<Event>` elements
- [x] Source manifest ad marker parsing — `#EXT-X-DATERANGE` (HLS) and `<EventStream>` (DASH) roundtrip through `SourceManifest`

### Phase 8: JIT Packaging (On-Demand GET) ✅

- [x] Manifest-on-GET — fetch source manifest, rewrite segment URLs, return immediately
- [x] Init-on-GET / Segment-on-GET — fetch, transform, cache, serve on first request
- [x] Request coalescing — `set_nx` distributed locking with configurable TTL
- [x] Hybrid mode — JIT (GET-triggered) and proactive (webhook) coexist
- [x] `POST /config/source` endpoint and URL pattern-based source resolution per content_id
- [x] All JIT code behind `#[cfg(feature = "jit")]` feature flag

### Phase 9: LL-HLS & LL-DASH ✅

- [x] LL-HLS partial segments (`#EXT-X-PART`, `#EXT-X-PRELOAD-HINT`, `#EXT-X-SERVER-CONTROL`)
- [x] LL-DASH chunked transfer with `availabilityTimeOffset`
- [x] CMAF chunk boundary detection
- [x] New: `src/media/chunk.rs`, `tests/ll_hls_dash.rs`

### Phase 10: MPEG-TS Input ✅

- [x] MPEG-TS demuxer — PES/TS packet parser for H.264/AAC
- [x] TS-to-CMAF transmuxer — elementary streams to fMP4 fragments
- [x] Init segment synthesis from codec config (SPS/PPS, AudioSpecificConfig)
- [x] HLS-TS manifest parsing and AES-128 segment-level decryption
- [x] Feature-gated: `#[cfg(feature = "ts")]`
- [x] New: `src/media/ts.rs`, `src/media/transmux.rs`, `tests/ts_integration.rs`

### Phase 11: Advanced DRM ✅

- [x] Key rotation at segment boundaries with per-period DRM signaling
- [x] Clear lead — configurable unencrypted lead-in segments
- [x] ClearKey DRM system support with local PSSH generation
- [x] Raw key mode — accept keys directly without SPEKE

### Phase 12: Trick Play & I-Frame Playlists — P2

- [ ] HLS `#EXT-X-I-FRAMES-ONLY` playlists from trun sync sample flags
- [ ] DASH trick play Representation with `@maxPlayoutRate`

### Phase 13: DVR Window & Time-Shift — P2

- [ ] Sliding window manifests with configurable time-shift buffer
- [ ] `#EXT-X-PROGRAM-DATE-TIME` / `@availabilityStartTime` for DVR
- [ ] Live-to-VOD manifest freezing

### Phase 14: Content Steering & CDN Optimization — P2

- [ ] HLS `#EXT-X-CONTENT-STEERING` with per-CDN pathway IDs
- [ ] DASH Content Steering (`ServiceDescription`, `ContentSteering`)
- [ ] Edge location awareness via CDN headers

### Phase 15: TS Segment Output — P2

- [ ] CMAF-to-TS muxer (PES packets, PAT/PMT, 188-byte TS)
- [ ] HLS-TS manifests (no `#EXT-X-MAP`, `.ts` extensions, `AES-128` encryption)

### Phase 18: Binary Size Monitoring & Selective Feature Gating — P2

The current binary (~648 KB base, ~685 KB full excl. TS, ~725 KB with all features) is well within cold start budgets (<1 ms). Feature-gating existing Rust application logic yields only ~20–30 KB — not worth the `#[cfg]` maintenance burden. Real binary size wins come from crate-level decisions (e.g., the lightweight `url.rs` saved ~200 KB vs the `url` crate). The `ts` feature (MPEG-TS demuxer + transmuxer) adds ~72 KB and is correctly feature-gated as predicted.

- [ ] Monitor binary size as new features land — per-feature size tests in CI enforce limits per build variant
- [ ] Feature-gate only when a phase introduces a heavy new dependency or parser (50+ KB), as done for the `ts` feature (Phase 10: MPEG-TS demuxer)
- [ ] If the binary exceeds ~900 KB with all features, audit and selectively gate the heaviest new module
- [ ] Prefer lightweight built-in implementations over crate dependencies when the crate adds disproportionate WASM size

### Phase 16: Compatibility Validation & Hardening ✅

- [x] Codec/scheme compatibility matrix — VP9+CBCS rejected, HEVC+CENC warned (subsample required), AV1+CBCS warned (limited support), Dolby Vision RPU preservation warned, text track encryption rejected
- [x] HDR format detection from codec strings — HDR10, HDR10+, Dolby Vision (`dvhe`/`dvav`), HLG
- [x] Init segment structure validation — ftyp ordering, sinf/schm/tenc presence (encrypted) or absence (clear), PSSH well-formedness
- [x] Media segment structure validation — moof/mdat presence, senc sample count matching trun, IV size correctness
- [x] `validate_repackage_request()` pre-flight hook in pipeline entry — errors reject before SPEKE, warnings logged
- [x] Post-rewrite debug validation (init + segment structure checks, logged as warnings)
- [x] Conformance test suite (`tests/conformance.rs`) — 23 tests covering init/segment structure, encryption roundtrips, manifest correctness

### Phase 17: CDN Provider Adapters & Binary Optimization ✅

- [x] Generalized config: `RedisConfig` → `StoreConfig`, `RedisBackendType` → `CacheBackendType`
- [x] Cloudflare Workers KV backend (`cloudflare` feature) via REST API
- [x] Generic HTTP KV backend (always available) for AWS DynamoDB, Akamai EdgeKV, custom stores
- [x] HTTP client extended with `PUT` and `DELETE` methods
- [x] Backward compatible: `REDIS_URL`/`REDIS_TOKEN` still work
- [x] `CACHE_BACKEND` env var override, `CACHE_ENCRYPTION_TOKEN` for custom key derivation
- [x] `set_nx()` best-effort (GET then PUT) on non-Redis backends

## License

Proprietary.
