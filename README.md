<img alt="edgepack_logo" src="edgepack_logo.png" />

# edgepack

A lightweight media repackager that runs as a WebAssembly module directly on CDN edge nodes. Compiled from Rust to `wasm32-wasip2`, the ~628 KB binary instantiates in under 1 ms — enabling **just-in-time packaging** where content is repackaged on the first viewer request rather than pre-processed in a central origin. This eliminates the origin packaging bottleneck: no batch jobs, no packaging queues, no storage of pre-packaged variants.

edgepack repackages DASH and HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4 ↔ ISO BMFF), producing progressive output manifests and segments cached at the CDN with configurable TTLs. Supports **dual-format output** (simultaneous HLS and DASH from a single request, sharing format-agnostic segments), **dual-scheme output** (CBCS + CENC simultaneously), **multi-key DRM** with per-track keying, **configurable cache-control headers** (env var defaults, per-request overrides, safety invariants), **SCTE-35 ad marker pass-through** (emsg extraction, HLS `#EXT-X-DATERANGE`, DASH `<EventStream>`), **subtitle/text track pass-through** (WebVTT/TTML, CEA-608/708), **advanced DRM** (ClearKey, raw key mode, key rotation, clear lead), **low-latency streaming** (LL-HLS partial segments, LL-DASH), **trick play** (HLS I-frame playlists, DASH trick play AdaptationSets), **MPEG-TS input** (TS demux + CMAF transmux, feature-gated), **MPEG-TS output** (CMAF-to-TS muxer with AES-128-CBC encryption, HLS-TS manifests, feature-gated), and codec string extraction for manifest signaling.

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

1. **Receives a request** to repackage content — **on-demand via HTTP GET** — content is repackaged on the first viewer request
2. **Fetches DRM keys** from a license server using the SPEKE 2.0 protocol and CPIX standard (supports multi-key — separate keys for video and audio tracks)
3. **Fetches source media** (CMAF init + media segments) from the origin, extracting per-track codec strings, key IDs, and language metadata
4. **Validates compatibility** — pre-flight checks catch invalid codec/scheme combinations (e.g., VP9+CBCS) before expensive crypto operations
5. **Decrypts** each segment using the source encryption scheme (CBCS or CENC, auto-detected from the init segment)
6. **Re-encrypts** each segment for one or more target schemes (CBCS, CENC, or None — configurable per request, supports dual-scheme output)
7. **Extracts SCTE-35 ad markers** from `emsg` boxes in media segments and signals them in output manifests (`#EXT-X-DATERANGE` for HLS, `<EventStream>` for DASH)
8. **Passes through subtitle tracks** — WebVTT (`wvtt`) and TTML (`stpp`) sample entries are never encrypted and flow through unchanged
9. **Rewrites** init segments per target scheme (per-track protection scheme info with track-specific KIDs, multi-KID PSSH boxes, DRM signaling, ftyp brands for container format)
10. **Detects I-frames** — identifies IDR chunk boundaries in rewritten segments and records byte ranges for trick play (fast-forward/rewind) manifests
11. **Outputs progressively** — writes a live manifest as soon as the first segment is ready, updates with each segment, finalises when complete. Manifests include subtitle rendition groups, CEA-608/708 closed caption signaling, SCTE-35 ad break markers, and I-frame playlists for trick play
12. **Caches aggressively** — segments default to immutable with 1-year cache headers; live manifests have configurable short TTLs (default 1 second); finalised manifests become immutable. Cache-Control headers are configurable at three levels: env var system defaults, per-request overrides via `CacheControlConfig`, and hardcoded safety invariants. Once cached, the edge worker is never invoked again for that segment

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- WASM target: `wasm32-wasip2`
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

# With MPEG-TS input/output support
cargo build --release --features ts
```

Output WASM binary:
```
target/wasm32-wasip2/release/edgepack.wasm
```

#### Feature Flags

| Feature | Description |
|---------|-------------|
| `ts` | MPEG-TS input (TS demux + CMAF transmux) and output (CMAF→TS mux + AES-128 encryption) |
| `sandbox` | Local development sandbox with web UI (native binary, not WASM) |

#### Binary Size

| Build | Command | Size |
|-------|---------|------|
| Base (no features) | `cargo build --release` | ~628 KB |
| With TS | `cargo build --release --features ts` | ~680 KB |

Per-feature binary size tests in `tests/wasm_binary_size.rs` enforce limits (base ≤750 KB, TS ≤800 KB). Small binary size is critical — the WASM module is instantiated on every cache-miss GET request, and sub-millisecond instantiation keeps first-byte latency low.

#### Why Binary Size Matters

WASM module instantiation time on CDN edge runtimes is roughly proportional to binary size. The ~628 KB binary with ~2,069 functions instantiates in **under 1 ms** on modern WASI runtimes (wasmtime, V8) — fast enough that viewers don't notice the difference between a cache hit and a JIT-packaged response.

| Approach | Cold Start | Per-Request Overhead | Catalog Coverage |
|----------|-----------|---------------------|-----------------|
| **edgepack (WASM at edge)** | <1 ms | WASM instantiation + origin fetch + crypto | Package only what's requested |
| **Origin packager (Shaka/ffmpeg)** | N/A (pre-packaged) | None (pre-cached) | Must pre-package everything |
| **Lambda@Edge / Cloud Functions** | 50–500 ms | Container boot + runtime init | On-demand, but slow cold starts |

The release profile (`opt-level=z`, LTO, strip, `codegen-units=1`, `panic=abort`) and careful dependency management keep the binary under 800 KB. Every dependency is evaluated for WASM size impact: the lightweight `src/url.rs` saves ~200 KB vs the `url` crate, there's no async runtime, no ICU/Unicode tables, and sandbox-only dependencies are completely excluded from the WASM build.

### Running Tests

Tests run on the native host target (not WASM), since the test harness cannot execute inside a WASI runtime:

```bash
cargo test --target $(rustc -vV | grep host | awk '{print $2}')
```

The project includes **1,346 tests** without optional features. With `--features ts`: **1,508 tests**. See [`docs/testing.md`](docs/testing.md) for detailed coverage tables, fixtures, and benchmarks. Quick examples:

```bash
cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::            # specific module
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test e2e        # specific integration suite
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --features ts     # include TS tests
```


## Configuration

All configuration is via environment variables.

### Required

| Variable | Description |
|----------|-------------|
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
| `CACHE_MAX_AGE_SEGMENTS` | `31536000` | Default max-age for segments and init segments (1 year) |
| `CACHE_MAX_AGE_MANIFEST_LIVE` | `1` | Default max-age for live/in-progress manifests |
| `CACHE_MAX_AGE_MANIFEST_FINAL` | `31536000` | Default max-age for finalised/VOD manifests |
| `JIT_SOURCE_URL_PATTERN` | — | URL template with `{content_id}` placeholder |
| `JIT_DEFAULT_TARGET_SCHEME` | `cenc` | Default scheme: `cenc` or `cbcs` |
| `JIT_DEFAULT_CONTAINER_FORMAT` | `cmaf` | Default format: `cmaf` or `fmp4` |
| `JIT_LOCK_TTL` | `30` | Processing lock TTL in seconds |

### Runtime Policy Controls

Restrict which encryption schemes, output formats, and container formats are available to end users. Uses a **fail-closed allowlist** — when a policy variable is set, only the listed values are permitted; everything else is denied with HTTP 403.

| Variable | Default | Values | Description |
|----------|---------|--------|-------------|
| `POLICY_ALLOWED_SCHEMES` | *(unset = all allowed)* | `cenc`, `cbcs`, `none` | Comma-separated list of permitted encryption schemes |
| `POLICY_ALLOWED_FORMATS` | *(unset = all allowed)* | `hls`, `dash` | Comma-separated list of permitted output formats |
| `POLICY_ALLOWED_CONTAINERS` | *(unset = all allowed)* | `cmaf`, `fmp4`, `iso`, `ts` | Comma-separated list of permitted container formats |

**Semantics:**
- **Unset** (default) — no restriction, all values allowed. Zero-config backward compatible.
- **Set to a list** (e.g., `cenc,cbcs`) — only the listed values are permitted. Any request for a value not in the list returns HTTP 403 Forbidden.
- **Set to empty** (e.g., `POLICY_ALLOWED_FORMATS=`) — nothing is permitted (full lockdown). All content endpoints return 403.

**Examples:**

```bash
# Only allow HLS output — DASH requests return 403
POLICY_ALLOWED_FORMATS=hls

# Only allow CENC and CBCS — clear content delivery is blocked
POLICY_ALLOWED_SCHEMES=cenc,cbcs

# Only allow CMAF containers — fMP4, ISO BMFF, and TS requests return 403
POLICY_ALLOWED_CONTAINERS=cmaf

# Full lockdown — all content endpoints return 403, only /health responds
POLICY_ALLOWED_FORMATS=
POLICY_ALLOWED_SCHEMES=
POLICY_ALLOWED_CONTAINERS=
```

**Security guarantees:**
- **Fail-closed**: Only explicitly listed values pass. No value can bypass the allowlist by default.
- **Defense in depth**: Two enforcement layers — route-level (checks format and URL-visible scheme before any processing) and JIT-setup (checks resolved scheme and container format before pipeline execution).
- **Non-bypassable**: Policy is checked on every request path — manifests, init segments, media segments, I-frame playlists, and key endpoints are all gated.
- **Health check exempt**: `GET /health` always responds regardless of policy. Operators can verify the instance is running even under full lockdown.
- **HTTP 403 Forbidden**: Denied requests return 403 with a descriptive message identifying which value was blocked, not 404 (which could be confused with missing content).

## API

On a cache miss, the edge worker instantiates (<1 ms), fetches the source segment from origin, repackages it with the target DRM scheme, caches the result in-process with immutable headers, and serves it. Subsequent requests hit the CDN cache directly.

```
GET /repackage/{content_id}/{format}/manifest
GET /repackage/{content_id}/{format}/init.mp4
GET /repackage/{content_id}/{format}/segment_{n}.{ext}
GET /repackage/{content_id}/{format}/iframes
GET /repackage/{content_id}/{format}/key
GET /health
```

- `{content_id}` — unique content identifier
- `{format}` — `hls`, `dash`, or scheme-qualified: `hls_cenc`, `hls_cbcs`, `dash_cenc`, `dash_cbcs`, `hls_none`, `dash_none`
- `{n}` — segment number (0-indexed)
- `{ext}` — any CMAF or ISOBMFF segment extension (see [Supported Segment Extensions](#supported-segment-extensions))

The `/key` endpoint serves the raw 16-byte AES-128 key for HLS-TS `#EXT-X-KEY:METHOD=AES-128` URI. Only valid when the container format is TS and content is encrypted.

The `/iframes` endpoint serves HLS I-frame-only playlists (`#EXT-X-I-FRAMES-ONLY`) for trick play. For DASH, trick play is embedded in the regular MPD.

Scheme-qualified format paths (e.g., `hls_cenc`) route to scheme-specific cached data. Plain format paths (`hls`, `dash`) route to the default/sole target scheme for backward compatibility.

Request coalescing via distributed locking (`set_nx`) ensures that concurrent requests for the same uncached segment don't trigger duplicate origin fetches.

## Caching Strategy

### CDN Layer (primary content cache)

Default Cache-Control headers (configurable via env vars and per-request overrides):

| Resource | Default Cache-Control | Configurable |
|----------|----------------------|--------------|
| Segments (once produced) | `public, max-age=31536000, immutable` | `CACHE_MAX_AGE_SEGMENTS` env var, `cache_control.segment_max_age` per-request |
| Finalised manifests (VOD) | `public, max-age=31536000, immutable` | `CACHE_MAX_AGE_MANIFEST_FINAL` env var, `cache_control.final_manifest_max_age` per-request |
| Live/in-progress manifests | `public, max-age=1, s-maxage=1` | `CACHE_MAX_AGE_MANIFEST_LIVE` env var, `cache_control.live_manifest_max_age` / `live_manifest_s_maxage` per-request |
| Awaiting first segment | `no-cache` | **Not configurable** (safety invariant) |

**Three-tier configuration:** env var system defaults → per-request overrides → hardcoded safety invariants. Per-request overrides (via `cache_control` on `RepackageRequest`) apply to manifests only — segments use system defaults. The `immutable` directive can be toggled off per-request for CDN setups that don't support it. Safety invariants are never overridable: `AwaitingFirstSegment` always returns `no-cache` and the `public` prefix is always present.

Segments never change once written. The CDN serves them without hitting the edge worker after the first request.

### In-Process Cache (application state)

The in-process cache stores DRM keys, JIT processing state, and SPEKE response cache. It persists between requests in long-running runtimes (wasmtime serve, Cloudflare Workers).

Sensitive entries (DRM content keys, SPEKE responses, encryption parameters) are encrypted at rest with a per-process AES-128-CTR key generated from process entropy. Non-sensitive entries (manifest state, segments, locks) pass through unmodified.

After processing completes, `cleanup_sensitive_data()` explicitly deletes raw DRM keys and SPEKE responses from cache — they are no longer needed once encryption parameters have been derived.

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
                    │                  │  Manifest Cache  │   │
                    │                  │         (in-proc)│   │
                    │                  └────┬─────────────┘   │
                    └───────────────────────┼─────────────────┘
                                            │
                              Origin ◄──────┘       License Server
                              (CBCS/CENC source)    (SPEKE 2.0)
```

### Module Dependency Graph

```
handler/ ──► repackager/ ──► media/     (CMAF/fMP4 parse + rewrite)
                         ──► drm/      (SPEKE + scheme-aware encrypt/decrypt)
                         ──► manifest/ (HLS/DASH generation)
                         ──► cache/    (in-process encrypted state)
```

### Detailed Architecture Diagrams

See [`docs/architecture.md`](docs/architecture.md) for Mermaid diagrams covering system context, data flow, module architecture, progressive output state machine, cache security, and per-segment encryption transforms.

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
| TS | MPEG Transport Stream (`ts` feature) | `.ts` | N/A (no init) | N/A | HLS only (DASH unsupported) |

The init segment's `ftyp` box is rewritten to match the target container format (CMAF/fMP4/ISO). TS output has no init segment — PAT/PMT tables are embedded in each TS segment. TS uses AES-128-CBC whole-segment encryption (`#EXT-X-KEY:METHOD=AES-128`) instead of per-sample CENC/CBCS.

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
| `.ts` | ISO 13818-1 (MPEG-TS) | Transport stream | TS output (`ts` feature) |

Input parsing is extension-agnostic — the parsers fetch whatever URL the source manifest specifies.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `aes`, `cbc`, `ctr`, `cipher` | AES encryption/decryption (CBCS and CENC) |
| `quick-xml` | CPIX XML and DASH MPD parsing/generation |
| `serde`, `serde_json` | Serialization for config, cache state, JIT params |
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

The sandbox tests the full repackaging pipeline locally without deploying to a CDN edge. It reuses the same `RepackagePipeline` as the production WASM build, with `reqwest` for HTTP and an in-memory encrypted cache.

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
3. The pipeline fetches the source manifest, gets DRM keys via SPEKE, and repackages all segments — returning `Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>` with per-(format, scheme) output in memory
4. On completion, output is written to disk per scheme at `sandbox/output/{content_id}/{format}_{scheme}/`

### Output Structure

```
sandbox/output/{content_id}/{format}_{scheme}/
├── manifest.m3u8   (or manifest.mpd)
├── iframes.m3u8    (HLS only, when enable_iframe_playlist=true)
├── init.mp4
├── segment_0.cmfv  (or .m4s or .mp4 or .ts)
├── segment_1.cmfv
└── ...
```

For dual-scheme output, each scheme gets its own directory (e.g., `hls_cenc/` and `hls_cbcs/`). For dual-format output, each format gets its own directory (e.g., `hls_cenc/` and `dash_cenc/`). Dual-format + dual-scheme produces directories for each combination (e.g., `hls_cenc/`, `hls_cbcs/`, `dash_cenc/`, `dash_cbcs/`). Segment extensions are determined by `container_format`.

### Sandbox API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Web UI |
| POST | `/api/repackage` | Start repackaging job |
| GET | `/api/status/{id}/{format}` | Poll job progress |
| GET | `/api/output/{id}/{format}/{file}` | Serve output files (from disk) |

## Project Status

The runtime is fully implemented and compiles to a ~628 KB WASM component that instantiates in under 1 ms — production-ready for JIT packaging at the edge. All nine encryption scheme combinations, four container formats (CMAF, fMP4, ISO BMFF, MPEG-TS), dual-format output (simultaneous HLS + DASH sharing format-agnostic segments), dual-scheme output, multi-key DRM with per-track keying, subtitle/text track pass-through, SCTE-35 ad marker pass-through, codec/scheme compatibility validation, advanced DRM (ClearKey, raw key mode, key rotation, clear lead), low-latency streaming (LL-HLS partial segments, LL-DASH), trick play (HLS I-frame playlists, DASH trick play AdaptationSets), MPEG-TS input/output (feature-gated), and configurable cache-control headers are supported. The in-process cache encrypts sensitive DRM data at rest with AES-128-CTR and cleans up keys after processing completes. Portable across any CDN supporting WASI Preview 2.

## Roadmap

Phases 1–14, 16, 17, 19, 21, and 22 are complete. All P0 and P1 items are done. The roadmap targets feature parity with Shaka Packager and AWS Elemental MediaPackage, optimized for CDN edge deployment. See [`docs/roadmap.md`](docs/roadmap.md) for detailed phase descriptions.

## License

Proprietary.
