<img width="460" height="340" alt="edgepack_logo" src="edgepack.png" />

# edgepack

A lightweight media repackager that runs as a WebAssembly module directly on CDN edge nodes. Compiled from Rust to `wasm32-wasip2`, the ~628 KB binary instantiates in under 1 ms — enabling **just-in-time packaging** where content is repackaged on the first viewer request rather than pre-processed in a central origin. This eliminates the origin packaging bottleneck: no batch jobs, no packaging queues, no storage of pre-packaged variants.

edgepack repackages DASH and HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4 ↔ ISO BMFF), producing progressive output manifests and segments cached at the CDN with configurable TTLs. Supports **dual-format output** (simultaneous HLS and DASH from a single request, sharing format-agnostic segments), **dual-scheme output** (CBCS + CENC simultaneously), **multi-key DRM** with per-track keying, **configurable cache-control headers** (env var defaults, per-request webhook overrides, safety invariants), **SCTE-35 ad marker pass-through** (emsg extraction, HLS `#EXT-X-DATERANGE`, DASH `<EventStream>`), **subtitle/text track pass-through** (WebVTT/TTML, CEA-608/708), **advanced DRM** (ClearKey, raw key mode, key rotation, clear lead), **low-latency streaming** (LL-HLS partial segments, LL-DASH), **trick play** (HLS I-frame playlists, DASH trick play AdaptationSets), **MPEG-TS input** (TS demux + CMAF transmux, feature-gated), **MPEG-TS output** (CMAF-to-TS muxer with AES-128-CBC encryption, HLS-TS manifests, feature-gated), and codec string extraction for manifest signaling.

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
12. **Caches aggressively** — segments default to immutable with 1-year cache headers; live manifests have configurable short TTLs (default 1 second); finalised manifests become immutable. Cache-Control headers are configurable at three levels: env var system defaults, per-request webhook overrides, and hardcoded safety invariants. Once cached, the edge worker is never invoked again for that segment

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

On Apple Silicon Macs, this is equivalent to:

```bash
cargo test --target aarch64-apple-darwin
```

On x86-64 Linux:

```bash
cargo test --target x86_64-unknown-linux-gnu
```

The project includes **1,290 tests** without optional features. With `--features ts`: **1,452 tests**. All tests cover every module, plus per-feature binary size guards, output integrity tests validating structural correctness of every input/output lane, and 105 end-to-end tests exercising full pipeline flows and feature combinations. To run tests for a specific module:

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

#### Unit Test Coverage

| Module | Tests | What's Covered |
|--------|-------|----------------|
| `error` | 16 | Error display strings, Result alias |
| `config` | 17 | Defaults, serde roundtrips, env var loading |
| `url` | 14 | URL parsing, join (absolute/relative/protocol-relative, normalization), serde roundtrip, authority extraction |
| `cache` | 76 | CacheKeys formatting (incl. scheme-qualified keys, format-agnostic keys), in-memory cache ops, encrypted backend (AES-128-CTR roundtrip, key sensitivity, IV uniqueness, key generation) |
| `drm` | 121 | EncryptionScheme enum (serde, bytes, from_scheme_type, from_str_value, HLS methods, IV sizes, patterns, FairPlay flags, `is_encrypted()`, None variant), SampleDecryptor/SampleEncryptor (factory dispatch, CBCS/CENC roundtrips), system IDs, CPIX XML, SPEKE client (incl. ClearKey) |
| `media` | 272 | FourCC types, ISOBMFF box parsing/building/iteration, ContainerFormat enum, init segment rewriting (scheme-aware, container-format-aware, sinf injection/stripping, ftyp rewriting, per-track tenc with TrackKeyMapping, multi-KID PSSH generation), segment rewriting (four-way dispatch), IV padding, codec string extraction (AVC/HEVC/AAC/VP9/AV1/AC-3/EC-3/Opus/FLAC/WebVTT/TTML), track metadata parsing (hdlr, mdhd timescale + language, stsd sample entries), TrackKeyMapping (single/per_type/from_tracks, serde roundtrip), emsg box parsing (v0/v1) + builder roundtrips, SCTE-35 splice_info_section parsing (splice_insert, time_signal), codec/scheme compatibility validation, HDR format detection, init/segment structure validation (incl. chunk detection, TS demux, transmux -- ts feature) |
| `manifest` | 177 | HLS/DASH rendering for all lifecycle phases, DRM scheme signaling, FairPlay key URI, variant streams, subtitle rendition groups (HLS `TYPE=SUBTITLES`, DASH text AdaptationSet), CEA-608/708 closed caption signaling (HLS `TYPE=CLOSED-CAPTIONS` with `INSTREAM-ID`, DASH `Accessibility` descriptors), audio/subtitle language attributes, ISO 8601 duration, KID formatting, HLS/DASH input parsing (source scheme detection, `#EXT-X-DATERANGE` SCTE-35 ad breaks, DASH `EventStream` parsing), ad break manifest rendering (`#EXT-X-DATERANGE`, DASH `EventStream`), I-frame playlist rendering (`#EXT-X-I-FRAMES-ONLY`, `#EXT-X-BYTERANGE`), master playlist I-frame stream signaling (incl. LL-HLS/LL-DASH types and rendering) |
| `repackager` | 101 | Job types/serde, progressive output state machine, cache-control headers, key set caching, continuation params (incl. TrackKeyMapping serialization), pipeline execution, DRM info building (multi-KID PSSH per system), track key mapping construction, variant building from tracks, sensitive data cleanup (incl. per-scheme, target_formats), I-frame info and enable_iframe_playlist methods (incl. raw keys, key rotation, clear lead, progressive parts), multi-format output types |
| `handler` | 113 | HTTP routing, path parsing incl. scheme-qualified formats (`hls_cenc`, `dash_cbcs`), segment number parsing (all 7 extensions), webhook validation (target_schemes array, output_formats array, backward compat, duplicate/invalid rejection), I-frame manifest handler, response construction |
| `http_client` | 9 | Response construction, native stub errors |

#### Integration Test Coverage

Integration tests live in `tests/` and use synthetic CMAF fixtures — no external services or network required.

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `advanced_drm` | 15 | Key rotation at segment boundaries, clear lead, ClearKey DRM, raw key mode |
| `cache_control` | 43 | System defaults (HLS/DASH, all phases), per-request overrides (live/final/segment max-age, s-maxage split, immutable toggle), safety invariants (AwaitingFirstSegment always no-cache), progressive output integration (HLS + DASH), backward compat, DVR + cache control, container format + cache control, system CacheConfig overrides, DASH per-request overrides, segment handler design documentation, JIT cache_control:None documentation |
| `clear_content` | 10 | Clear→CENC/CBCS, encrypted→clear, clear→clear, roundtrip pipelines |
| `conformance` | 23 | Init segment structure (ftyp/sinf/pssh ordering), media segment structure (moof/mdat/senc), encryption roundtrip conformance, manifest correctness |
| `content_steering` | 20 | HLS master steering tag (full, URI-only, position, backward compat), DASH steering element (full, proxy-only, qbs, position), DASH input parsing (full, minimal, backward compat), serde roundtrips, override priority |
| `dual_format` | 25 | Multi-format output (HLS+DASH), format-agnostic cache keys, dual-format manifests, webhook output_formats parsing, serde roundtrips, container format independence |
| `dual_scheme` | 22 | Scheme-qualified route parsing, cache key uniqueness per scheme, multi-scheme webhook payloads, backward compat, duplicate/invalid scheme rejection |
| `dvr_window` | 25 | HLS DVR window (sliding window, media sequence, playlist type, DRM, iframes, ad breaks), DASH DVR (timeShiftBufferDepth, startNumber, windowed segments), live-to-VOD, serde compat, container formats |
| `encryption_roundtrip` | 8 | CBCS→plaintext→CENC: full-sample, pattern, subsample, multi-sample IV, audio, cross-segment IV isolation |
| `handler_integration` | 32 | HTTP routing for all endpoints, webhook validation, HttpResponse helpers, method filtering |
| `isobmff_integration` | 18 | Init segment rewriting (scheme/container-aware), PSSH generation, senc roundtrip, segment decrypt→re-encrypt→verify |
| `ll_hls_dash` | 16 | LL-HLS partial segments, preload hints, server control, LL-DASH availability time offset, CMAF chunk boundary detection |
| `manifest_integration` | 23 | Progressive output lifecycle (HLS+DASH, all container formats), DRM signaling, cache-control headers, ManifestState serde |
| `multi_key` | 12 | Per-track tenc (video/audio KIDs), multi-KID PSSH generation, single-key backward compat, codec string extraction, TrackKeyMapping serde roundtrip, create→strip roundtrip, TrackKeyMapping::from_tracks |
| `scte35_integration` | 13 | emsg extraction, SCTE-35 parsing, HLS/DASH ad break rendering, source manifest ad marker roundtrip, AdBreakInfo serde |
| `trick_play` | 27 | HLS I-frame playlist rendering (BYTERANGE, DRM, init map, endlist, disabled), HLS master I-frame stream signaling, DASH trick play AdaptationSet, manifest dispatcher, serde backward compat, container format variations, route handling |
| `ts_integration` | 30 | MPEG-TS demux, PES/TS packet parsing, TS-to-CMAF transmux, init segment synthesis, HLS-TS manifest parsing, AES-128 decryption (ts feature) |
| `ts_output` | 46 | ContainerFormat::Ts (serde, extension, validation), HLS-TS manifest (no EXT-X-MAP, VERSION:3, AES-128 KEY, .ts URIs), TS muxer (PAT/PMT/PES roundtrip, AVCC↔AnnexB, ADTS, encryption), webhook TS acceptance/rejection, key endpoint routing, handler routing (ts feature) |
| `e2e` | 105 | Full pipeline E2E: encryption transforms ×2 formats (18), container×format×encryption matrix (18), feature combinations incl. DVR+iframes+DRM+steering+dual-format (30), lifecycle phase transitions (18), edge cases & boundary conditions (21) |
| `output_integrity` | 25 | Rewritten segment ISOBMFF structure validation (all 4 encryption lanes), mdat/trun size consistency, encrypt-decrypt plaintext recovery, I-frame BYTERANGE chunk validation (pre/post rewrite), init rewrite roundtrip (clear→enc→clear), multi-KID PSSH verification, HLS/DASH manifest roundtrips (VOD, live, DVR, I-frame), cache-control body invariants (manifest body unchanged with overrides, AwaitingFirstSegment safety), TS manifest integrity (no EXT-X-MAP, .ts extensions, VERSION:3), TS encrypt-decrypt roundtrip |
| `wasm_binary_size` | 1 | WASM binary size guard (base ≤750 KB) with function count reporting |

All tests use shared fixtures from `tests/common/mod.rs` that build synthetic ISOBMFF data programmatically — no external test media files needed. Multi-key tests use separate video/audio KIDs and keys to verify per-track tenc, multi-KID PSSH, and TrackKeyMapping behavior.

> **Note:** TS-specific tests require `--features ts`. Run with `--features ts` to include all 1,452 tests. Without optional features: 1,290 tests.

#### JIT Latency Benchmarks

[Criterion](https://docs.rs/criterion) benchmarks measure the core operations that determine first-byte latency in JIT mode:

```bash
# Run all benchmarks
cargo bench --target $(rustc -vV | grep host | awk '{print $2}')

# Run a specific benchmark group
cargo bench --target $(rustc -vV | grep host | awk '{print $2}') --bench jit_latency -- segment_rewrite
```

| Benchmark Group | What's Measured |
|----------------|-----------------|
| `segment_rewrite` | Segment re-encryption at 4/32/128 samples × 1KB: CBCS→CENC, clear→CENC, passthrough |
| `init_rewrite` | Init segment DRM scheme transform: CBCS→CENC, clear→CENC |
| `manifest_render` | HLS/DASH manifest generation at 10/50/200 segments, HLS I-frame at 50 segments, HLS live at 6 segments |
| `manifest_parse` | HLS/DASH manifest input parsing at 50 segments |

Benchmarks run on native targets (not WASM). WASM performance is proportional but not identical — use binary size as the cold-start proxy for WASM instantiation latency.

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

**Three-tier configuration:** env var system defaults → per-request webhook overrides → hardcoded safety invariants. Per-request overrides (via `cache_control` on webhook payload) apply to manifests only — segments use system defaults to avoid an extra Redis GET per segment request. The `immutable` directive can be toggled off per-request for CDN setups that don't support it. Safety invariants are never overridable: `AwaitingFirstSegment` always returns `no-cache`, the status endpoint always returns `no-cache`, and the `public` prefix is always present.

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
