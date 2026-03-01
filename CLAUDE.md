# CLAUDE.md — Agent Context for edgepack

This file provides context for Claude (Opus 4.6) when working on this codebase.

## Project Summary

**edgepack** is a Rust library compiled to WASM (`wasm32-wasip2`) that runs on CDN edge nodes. It repackages DASH/HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4), producing progressive HLS or DASH output. Supports **dual-scheme output** (multiple target encryption schemes simultaneously), **multi-key DRM** (per-track keying with separate video/audio KIDs and multi-KID PSSH boxes), and **codec string extraction** (RFC 6381 codec strings for manifest signaling). The target encryption scheme(s) and container format are configurable per request, supporting all encryption combinations (CBCS→CENC, CENC→CBCS, CENC→CENC, CBCS→CBCS) and clear content paths (clear→CENC, clear→CBCS, encrypted→clear, clear→clear) with automatic source scheme detection, and output as either CMAF or fragmented MP4. It communicates with DRM license servers via SPEKE 2.0 / CPIX for multi-key content encryption keys (skipped when both source and target are unencrypted).

## Build Commands

```bash
# Development build (default target is wasm32-wasip2 via .cargo/config.toml)
cargo build

# Release build (optimised for size: opt-level=z, LTO, stripped, codegen-units=1, panic=abort)
cargo build --release

# Run unit tests (MUST specify native host target — tests cannot run in WASI)
cargo test --target $(rustc -vV | grep host | awk '{print $2}')

# Check without building
cargo check

# Build and run the local sandbox (native binary with web UI)
cargo run --bin sandbox --features sandbox --target $(rustc -vV | grep host | awk '{print $2}')
```

**Important**: `cargo test` without `--target` will try to execute the WASM binary directly, which fails with a permission error. Always pass the native host target flag.

The WASM target requires `rustup target add wasm32-wasip2`. The `.cargo/config.toml` sets `wasm32-wasip2` as the default build target, so bare `cargo build` produces a `.wasm` file.

## Architecture Overview

```
src/
├── lib.rs              Module root (re-exports all submodules)
├── error.rs            EdgepackError enum + Result<T> alias
├── config.rs           AppConfig loaded from env vars
├── url.rs              Lightweight URL parser (replaces `url` crate — saves ~200 KB in WASM)
├── http_client.rs      Shared outgoing HTTP client (WASI wasi:http/outgoing-handler)
├── wasi_handler.rs     WASI incoming handler bridge (wasm32 only)
├── bin/
│   └── sandbox.rs      Local sandbox binary (Axum web UI + API, sandbox feature only)
├── cache/              Redis-backed application state store
│   ├── mod.rs          CacheBackend trait + CacheKeys builder + factory
│   ├── encrypted.rs    AES-256-GCM encryption layer for sensitive cache entries
│   ├── memory.rs       In-memory cache backend (sandbox feature only)
│   ├── redis_http.rs   Upstash-compatible HTTP Redis (primary)
│   └── redis_tcp.rs    TCP Redis stub (forward compatibility)
├── drm/                DRM key acquisition and encryption
│   ├── mod.rs          ContentKey, DrmSystemData, DrmKeySet types + system ID constants
│   ├── scheme.rs       EncryptionScheme enum (Cbcs/Cenc/None) + scheme-specific helpers
│   ├── sample_cryptor.rs  SampleDecryptor/SampleEncryptor traits + factory functions
│   ├── speke.rs        SPEKE 2.0 HTTP client
│   ├── cpix.rs         CPIX XML request builder + response parser
│   ├── cbcs.rs         AES-128-CBC pattern decryption + encryption (CBCS scheme)
│   └── cenc.rs         AES-128-CTR encryption + decryption (CENC scheme)
├── media/              ISOBMFF/CMAF/fMP4 container handling
│   ├── mod.rs          FourCC type, box_type constants, TrackType enum
│   ├── cmaf.rs         Zero-copy MP4 box parser, builders, iterators
│   ├── codec.rs        Codec string extraction, track metadata parsing, TrackKeyMapping
│   ├── container.rs    ContainerFormat enum (Cmaf/Fmp4) — brands, extensions, profiles
│   ├── init.rs         Init segment rewriting (sinf/schm/tenc/pssh + ftyp brand rewriting, per-track keying)
│   └── segment.rs      Media segment rewriting (senc/mdat decrypt+re-encrypt)
├── manifest/           Manifest parsing (input) and rendering (output)
│   ├── mod.rs          render_manifest() dispatcher
│   ├── types.rs        ManifestState, ManifestPhase, SegmentInfo, DrmInfo, SourceManifest
│   ├── hls.rs          HLS M3U8 renderer (media + master playlists)
│   ├── dash.rs         DASH MPD renderer (SegmentTemplate + SegmentTimeline)
│   ├── hls_input.rs    HLS M3U8 input parser (source manifest extraction)
│   └── dash_input.rs   DASH MPD input parser (source manifest extraction)
├── repackager/         Orchestration layer
│   ├── mod.rs          RepackageRequest, JobStatus, JobState types
│   ├── pipeline.rs     RepackagePipeline — fetch→decrypt→re-encrypt→output flow + continuation
│   └── progressive.rs  ProgressiveOutput state machine (AwaitingFirstSegment→Live→Complete)
└── handler/            HTTP request handling
    ├── mod.rs          Router, HttpRequest/HttpResponse/HandlerContext, route() dispatcher
    ├── request.rs      On-demand GET handlers (manifest, init, segment, status)
    └── webhook.rs      POST /webhook/repackage + continue handler
```

## Architecture Diagrams

Detailed Mermaid diagrams are in [`docs/architecture.md`](docs/architecture.md). The file contains 10 diagrams: system context, data flow, module architecture, split execution sequence, progressive output state machine, cache security model, cache key layout, CDN caching strategy, per-segment encryption transform, and container format comparison. All diagrams are Mermaid syntax, portable to Confluence, Jira, and Lucidchart.

## Key Concepts

### Two-Tier Caching

- **CDN cache** (primary): HTTP `Cache-Control` headers on responses. Segments and finalised manifests use `max-age=31536000, immutable`. Live manifests use `max-age=1, s-maxage=1`.
- **Redis** (application state): Stores DRM keys, job state, SPEKE response cache, progressive manifest state, and rewritten media data (init segments, media segments) for the split execution path (`execute_first()`/`execute_remaining()`). The `execute()` path (sandbox) does not cache media data in Redis — it returns output directly via `ProgressiveOutput`.

### Encryption Transform

The core transform is scheme-configurable on CMAF segments (source and target schemes determined at runtime). Four dispatch paths based on `(source_encrypted, target_encrypted)`:

- **Encrypted → Encrypted**: Parse `senc` → decrypt `mdat` with source scheme → re-encrypt with target scheme → rewrite `senc` → rebuild `moof` + `mdat`
- **Clear → Encrypted**: Parse `trun` for sample sizes → encrypt `mdat` with target scheme → inject new `senc` box → rebuild `moof` + `mdat`
- **Encrypted → Clear**: Parse `senc` + `trun` → decrypt `mdat` with source scheme → strip `senc` box → rebuild `moof` + `mdat`
- **Clear → Clear**: Byte-for-byte pass-through (no transformation)

Init segments have a corresponding four-way dispatch:
- **Encrypted → Encrypted**: Rewrite `sinf`/`schm`/`tenc`/`pssh` boxes and `ftyp` brands
- **Clear → Encrypted**: Inject `sinf` (frma + schm + tenc) into stsd, rename sample entries (`avc1`→`encv`, `mp4a`→`enca`), add PSSH boxes, rewrite `ftyp`
- **Encrypted → Clear**: Strip `sinf` from stsd, restore original sample entry names from `frma`, remove PSSH boxes, rewrite `ftyp`
- **Clear → Clear**: Rewrite `ftyp` only (format conversion)

**Scheme-specific behaviour:**
- **CBCS**: AES-128-CBC, pattern encryption (1:9 video, 0:0 audio), 16-byte IVs, supports FairPlay
- **CENC**: AES-128-CTR, full encryption (no pattern), 8-byte IVs, Widevine + PlayReady only
- **None**: Clear/unencrypted content — no encryption, no DRM, 0-byte IVs, no PSSH boxes
- Source scheme auto-detected from init segment `schm` box or manifest DRM signaling (absence of encryption info → `None`)

### Container Format

The output container format is configurable via `ContainerFormat` enum (`Cmaf`, `Fmp4`, or `Iso`):
- **CMAF** (default): Compatible brands include `cmfc`, segment extensions are `.cmfv`/`.cmfa`, DASH profile includes `cmaf:2019`
- **fMP4**: No `cmfc` brand, segment extension is `.m4s`, DASH profile is `isoff-live:2011` only
- **ISO BMFF**: No `cmfc` brand, segment extension is `.mp4`, DASH profile is `isoff-live:2011` only (same brands/profiles as fMP4, different extension)
- All formats use `.mp4` for init segments and `video/mp4`/`audio/mp4` MIME types
- The `ftyp` box in init segments is rewritten to match the target format's brands
- `ContainerFormat` flows through `RepackageRequest` → `ContinuationParams` → `ManifestState` → `ProgressiveOutput`
- Segment URIs are built dynamically using `container_format.video_segment_extension()`
- DASH renderer uses `container_format.dash_profiles()` for MPD `@profiles` attribute
- Route handler accepts all 7 CMAF (ISO 23000-19) and ISOBMFF (ISO 14496-12) segment extensions: `.cmfv`, `.cmfa`, `.cmft`, `.cmfm`, `.m4s`, `.mp4`, `.m4a`
- Extensions not in scope: `.aac` (raw ADTS, not ISOBMFF), `.m4v`/`.3gp`/`.mov` (progressive-only)

### Progressive Manifest Output

The `ProgressiveOutput` state machine transitions:
- `AwaitingFirstSegment` → `Live` (on first segment complete, manifest written with short cache TTL)
- `Live` → `Live` (each subsequent segment updates manifest)
- `Live` → `Complete` (final segment or source EOF, manifest switches to immutable cache headers; HLS adds `#EXT-X-ENDLIST`, DASH changes `type` from `dynamic` to `static`)

### Multi-Key DRM & Codec Awareness

**Per-track keying:** Content can use separate encryption keys for video and audio tracks. The `TrackKeyMapping` type (in `media/codec.rs`) maps `TrackType → [u8; 16]` KIDs. Three constructors:
- `TrackKeyMapping::single(kid)` — same KID for all tracks (backward compat with single-key content)
- `TrackKeyMapping::per_type(video_kid, audio_kid)` — different KIDs per track type
- `TrackKeyMapping::from_tracks(&[TrackInfo])` — auto-detects from parsed track metadata (if all tracks share a KID, returns single)

**Init rewriting:** `rewrite_init_segment()` and `create_protection_info()` accept `&TrackKeyMapping`. Each track's `tenc` box gets the correct KID based on its `hdlr` handler type (`vide`/`soun`).

**Multi-KID PSSH:** `build_pssh_boxes()` groups DRM system entries by `system_id` and builds one PSSH v1 box per system containing all unique KIDs. The `PsshBox` struct in `cmaf.rs` already supports `key_ids: Vec<[u8; 16]>`.

**Codec string extraction:** `extract_tracks()` in `media/codec.rs` parses the moov box to extract per-track metadata (`TrackInfo`):
- Track type from `hdlr` handler type
- Track ID from `tkhd`
- Timescale from `mdhd`
- KID from `sinf → tenc` (if encrypted)
- RFC 6381 codec string from `stsd` sample entry config boxes:
  - H.264: `avcC` → `avc1.{profile}{constraint}{level}`
  - H.265: `hvcC` → `hev1.{profile}.{tier}{level}.{constraint}`
  - AAC: `esds` → `mp4a.40.{audioObjectType}`
  - VP9: `vpcC` → `vp09.{profile}.{level}.{bitDepth}`
  - AV1: `av1C` → `av01.{profile}.{level}{tier}.{bitDepth}`
  - AC-3, EC-3, Opus, FLAC → simple FourCC strings

**Pipeline integration:** The pipeline calls `extract_tracks()` on the source init segment, builds `TrackKeyMapping` from the track metadata, collects all unique KIDs for the SPEKE request, and threads the key mapping through init rewriting. Codec strings are populated into `VariantInfo` for manifest rendering (HLS `CODECS=` attribute, DASH `codecs=` attribute).

### SPEKE 2.0 / CPIX

The `drm/speke.rs` client POSTs a CPIX XML document to the license server requesting content keys for specified KIDs and DRM system IDs (Widevine, PlayReady). The response contains encrypted content keys and PSSH box data. The `drm/cpix.rs` module handles XML building and parsing. Multi-key requests are natively supported — the CPIX builder assigns `intendedTrackType` ("VIDEO"/"AUDIO") per KID.

## Error Handling

All modules use `crate::error::Result<T>` which aliases `std::result::Result<T, EdgepackError>`. The `EdgepackError` enum has specific variants for each subsystem (Cache, Drm, Speke, Cpix, Encryption, MediaParse, SegmentRewrite, Manifest, Http, Config, InvalidInput, NotFound, Io). Use `thiserror` derive macros. Propagation is via `?` operator throughout.

## Runtime Implementation

All HTTP transport and request handling is fully implemented:

1. **`http_client.rs`**: Shared HTTP client using `wasi:http/outgoing-handler` (wasm32) with native stub error (non-wasm32, preserves test builds).
2. **`wasi_handler.rs`**: WASI incoming handler bridge implementing `wasi:http/incoming-handler::Guest`. Converts WASI types ↔ library types and maps errors to HTTP status codes.
3. **`cache/redis_http.rs` → `execute_command()`**: Uses `http_client::get()` to make Upstash REST API calls. Parses JSON responses via extracted `parse_upstash_response()`.
4. **`drm/speke.rs` → `post_cpix()`**: Uses `http_client::post()` to POST CPIX XML to license server with auth headers.
5. **`repackager/pipeline.rs`**: `fetch_source_manifest()` auto-detects HLS vs DASH and parses. `fetch_segment()` fetches binary data. Two execution modes: `execute()` processes all segments synchronously and returns `(JobStatus, Vec<(EncryptionScheme, ProgressiveOutput)>)` with per-scheme output data in memory (used by sandbox). `execute_first()` + `execute_remaining()` is the split execution model for WASI — caches per-scheme init segments, media segments, and manifest state in Redis for serving via GET handlers, with self-invocation chaining. Both modes decrypt source segments once and re-encrypt for each target scheme.
6. **`manifest/hls_input.rs` + `dash_input.rs`**: Source manifest input parsers extracting segment URLs, durations, init segment references, and live/VOD detection.
7. **`handler/request.rs`**: All four GET handlers query Redis for cached segment data and manifest state via `HandlerContext`.
8. **`handler/webhook.rs`**: Creates pipeline, calls `execute_first()`, fires self-invocation to `/webhook/repackage/continue`, returns 200 after first manifest publishes. Continue handler chains remaining segment processing.

## Local Sandbox

The `sandbox` feature enables a native binary (`src/bin/sandbox.rs`) that reuses the production `RepackagePipeline` with native HTTP transport and an in-memory cache. The sandbox calls `pipeline.execute()` which processes all segments synchronously and returns `(JobStatus, Vec<(EncryptionScheme, ProgressiveOutput)>)` — per-scheme output is written to disk directly from each `ProgressiveOutput` object to `sandbox/output/{content_id}/{format}_{scheme}/`, not round-tripped through cache.

### Architecture

- **`http_client.rs`** has a three-way `#[cfg]` dispatch: `wasm32` → WASI HTTP, `sandbox` feature → `reqwest::blocking`, neither → stub error
- **`cache/memory.rs`** implements `CacheBackend` using `Arc<RwLock<HashMap>>` (shared between pipeline thread and API server; used for job state and DRM key caching, not for media output)
- **`src/bin/sandbox.rs`** is a single-file Axum server with embedded HTML/CSS/JS UI

### Feature Gate

```toml
[features]
sandbox = ["dep:axum", "dep:tokio", "dep:reqwest", "dep:tower-http", "dep:tracing-subscriber"]
```

All sandbox dependencies are gated behind `cfg(not(target_arch = "wasm32"))` — they never appear in the WASM build. The `[[bin]]` entry uses `required-features = ["sandbox"]` so `cargo build` (WASM target) never compiles the sandbox.

### Build & Run

```bash
cargo run --bin sandbox --features sandbox --target $(rustc -vV | grep host | awk '{print $2}')
# Web UI at http://localhost:3333
```

### Output

Pipeline output is written to `sandbox/output/{content_id}/{format}_{scheme}/` (e.g., `sandbox/output/sb-abc123/hls_cenc/`) and served via the API at `/api/output/{id}/{format_scheme}/{file}` (reads directly from disk, not from cache). Dual-scheme requests create separate output directories per scheme.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `aes` | 0.8 | AES block cipher (CBCS + CENC) |
| `aes-gcm` | 0.10 | AES-256-GCM authenticated encryption for cache-at-rest security |
| `cbc` | 0.1 | CBC mode for CBCS decryption |
| `ctr` | 0.9 | CTR mode for CENC encryption |
| `cipher` | 0.4 | Cipher traits shared by cbc/ctr |
| `quick-xml` | 0.37 | CPIX XML + DASH MPD parsing/generation |
| `serde` | 1 | Serialization framework |
| `serde_json` | 1 | JSON for Redis, webhooks, job state |
| `base64` | 0.22 | Key encoding in CPIX, PSSH in manifests |
| `uuid` | 1 | Content Key IDs (KIDs) |
| `thiserror` | 2 | Derive macro for error types |
| `log` | 0.4 | Logging facade |
| `wasi` | 0.14 | WASI Preview 2 bindings (wasm32 target only) |
| `axum` | 0.8 | HTTP server for sandbox web UI (sandbox feature, non-wasm32 only) |
| `tokio` | 1 | Async runtime for sandbox (sandbox feature, non-wasm32 only) |
| `reqwest` | 0.12 | Native HTTP client for sandbox (sandbox feature, non-wasm32 only) |
| `tower-http` | 0.6 | Static file serving for local paths (sandbox feature, non-wasm32 only) |
| `tracing-subscriber` | 0.3 | Log output for sandbox (sandbox feature, non-wasm32 only) |

URL parsing uses a lightweight built-in module (`src/url.rs`) instead of the `url` crate, saving ~200 KB of ICU/IDNA Unicode tables in the WASM binary. Core crates are chosen for WASM compatibility (no system dependencies, no async runtime requirements). Sandbox crates are gated behind `cfg(not(target_arch = "wasm32"))` and never appear in the WASM build.

## Tests

The project has **709 tests** total: 583 unit tests and 126 integration tests. All run on the native host target. The release WASM binary is ~495 KB (guarded by a binary size test with a 600 KB threshold).

### Unit Tests (583)

Inlined as `#[cfg(test)] mod tests` blocks in every source file. They cover:

- **Serde roundtrips** for all serializable types (config, manifest state, job status, DRM keys, webhook payloads, encryption schemes, container formats, continuation params)
- **Encryption scheme abstraction**: `EncryptionScheme` enum (serde roundtrips, scheme_type_bytes, from_scheme_type, HLS method strings, default IV sizes, default patterns, FairPlay support flags, `is_encrypted()` for None variant), `SampleDecryptor`/`SampleEncryptor` trait dispatch via factory functions
- **Container format abstraction**: `ContainerFormat` enum with three variants (Cmaf, Fmp4, Iso) — extensions, brands, ftyp box building, DASH profile strings, serde roundtrips, display, from_str_value parsing
- **Encryption correctness**: CBCS decrypt + encrypt, CENC encrypt + decrypt, scheme-agnostic roundtrips through factory functions
- **ISOBMFF box parsing**: Building binary boxes, parsing them back, verifying headers, payloads, and child iteration
- **Init segment rewriting**: Scheme-parameterized `schm`/`tenc`/`pssh` rewriting (CBCS and CENC targets, tenc pattern encoding, PSSH filtering per scheme, per-track KID assignment via TrackKeyMapping, multi-KID PSSH v1 generation), ftyp brand rewriting per container format (CMAF includes `cmfc`, fMP4 does not), clear→encrypted sinf injection (`create_protection_info`), encrypted→clear sinf stripping (`strip_protection_info`), clear→clear ftyp-only rewrite (`rewrite_ftyp_only`)
- **Codec string extraction**: RFC 6381 codec strings from stsd config boxes (avcC, hvcC, esds, vpcC, av1C), track metadata parsing (hdlr handler type, mdhd timescale, tkhd track_id, sinf/tenc default_kid), TrackKeyMapping construction and serde roundtrips
- **Segment rewriting**: Four-way dispatch (encrypted↔encrypted, clear→encrypted, encrypted→clear, clear→clear pass-through), scheme-aware decrypt/re-encrypt with optional source/target keys
- **Manifest rendering**: HLS M3U8 and DASH MPD output for every lifecycle phase, dynamic DRM scheme signaling (SAMPLE-AES/SAMPLE-AES-CTR for HLS, cbcs/cenc value for DASH), FairPlay key URI rendering
- **Source manifest parsing**: HLS M3U8 and DASH MPD input parsing including source scheme detection from `#EXT-X-KEY` METHOD and `<ContentProtection>` elements
- **Progressive output state machine**: Phase transitions, cache-control header generation, dynamic segment URI formatting per container format
- **Pipeline DRM info**: Manifest DRM info building with CBCS/CENC target scheme (incl. multi-KID PSSH per system), FairPlay inclusion/exclusion, container format threading through ContinuationParams, TrackKeyMapping construction and serialization, variant building from track metadata
- **URL parsing**: Lightweight URL parser (parse, join, component access, serde roundtrips, authority extraction, relative path resolution)
- **HTTP routing**: Path parsing, format validation, segment number extraction (all 7 CMAF/ISOBMFF extensions: .cmfv, .cmfa, .cmft, .cmfm, .m4s, .mp4, .m4a), all route dispatching
- **Webhook validation**: Valid/invalid JSON, missing fields, bad formats, empty URLs, target_scheme/target_schemes parsing (cenc/cbcs/none, backward compat, duplicate rejection), container_format parsing (cmaf/fmp4/iso), invalid scheme/format rejection, serde roundtrips
- **Error variants**: Display output for every EdgepackError variant

To run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::cbcs`

### Integration Tests (126)

Located in the `tests/` directory. These exercise cross-module workflows using synthetic CMAF fixtures with no external dependencies:

```
tests/
├── common/
│   └── mod.rs                 Shared fixtures: synthetic ISOBMFF builders, test keys, DRM key sets, manifest states
├── clear_content.rs           10 tests: clear→CENC/CBCS, encrypted→clear, clear→clear (init + segment), roundtrips
├── dual_scheme.rs             22 tests: scheme-qualified routing, cache keys, webhook multi-scheme parsing, backward compat
├── encryption_roundtrip.rs    8 tests: CBCS→plaintext→CENC full pipeline
├── isobmff_integration.rs    18 tests: init/media segment parsing, rewriting (scheme + container format aware), PSSH/senc roundtrips
├── manifest_integration.rs   23 tests: progressive output lifecycle, DRM signaling, cache headers, ISO BMFF format
├── handler_integration.rs    32 tests: HTTP routing (all 7 CMAF/ISOBMFF segment extensions), webhook validation, response helpers
├── multi_key.rs              12 tests: per-track tenc, multi-KID PSSH, single-key backward compat, codec extraction, TrackKeyMapping serde, create→strip roundtrip
└── wasm_binary_size.rs        1 test: release WASM binary stays under 600 KB size limit
```

**Key fixtures in `tests/common/mod.rs`:**
- `build_cbcs_init_segment()` — builds a synthetic CBCS init segment (ftyp + moov with stsd→encv→sinf→frma/schm/schi/tenc + pssh)
- `build_cbcs_media_segment(sample_count, sample_size)` — builds a CBCS-encrypted moof+mdat with configurable samples; returns `(segment_bytes, plaintext_samples)` for verification
- `build_clear_init_segment()` — builds a synthetic clear init segment (ftyp + moov with stsd→avc1, no sinf, no PSSH)
- `build_clear_media_segment(sample_count, sample_size)` — builds a clear moof+mdat (trun, no senc) with plaintext samples
- `make_drm_key_set()` / `make_drm_key_set_with_fairplay()` — builds DrmKeySet with system-specific PSSH data
- `make_hls_manifest_state()` / `make_dash_manifest_state()` — builds ManifestState with DRM info and segments
- Test constants: `TEST_SOURCE_KEY`, `TEST_TARGET_KEY`, `TEST_KID`, `TEST_IV` (all `[u8; 16]`)

To run only integration tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test '*'`

To run a specific suite: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test encryption_roundtrip`

### Test Guidelines

When adding new functionality, follow the existing pattern:
- **Unit tests**: Add `#[cfg(test)] mod tests { ... }` at the bottom of the source file, import `use super::*;`, and create small focused test functions with descriptive names.
- **Integration tests**: For cross-module workflows, add tests to the appropriate file in `tests/` or create a new file. Use shared fixtures from `tests/common/mod.rs`. Add `mod common;` at the top of each integration test file.

## Coding Conventions

- **No `async`/`await`**: WASI Preview 2 doesn't have a standard async runtime. All I/O is synchronous (blocking WASI calls).
- **Zero-copy parsing where possible**: The ISOBMFF parser works with byte slices and offsets rather than allocating per-box.
- **Trait-based abstraction**: `CacheBackend` trait allows swapping Redis implementations without changing business logic.
- **Explicit state machines**: `ManifestPhase` and `JobState` enums drive control flow rather than implicit boolean flags.
- **`#[derive(Serialize, Deserialize)]`** on all types that cross the Redis boundary.
- **No `main.rs`**: This is a library crate (`crate-type = ["cdylib", "rlib"]`). The WASI runtime calls the exported handler functions. The `rlib` target enables integration tests to link against the crate. The sandbox binary (`src/bin/sandbox.rs`) is a separate binary target gated behind `required-features = ["sandbox"]`.
- **Two test locations**: Unit tests live inline in `#[cfg(test)] mod tests` blocks within each source file. Integration tests live in the `tests/` directory with shared fixtures in `tests/common/mod.rs`.

## HTTP Route Table

| Method | Path | Handler | Description |
|--------|------|---------|-------------|
| GET | `/health` | inline | Health check, returns "ok" |
| GET | `/repackage/{id}/{format}/manifest` | `request::handle_manifest_request` | Serve repackaged manifest |
| GET | `/repackage/{id}/{format}/init.mp4` | `request::handle_init_segment_request` | Serve repackaged init segment |
| GET | `/repackage/{id}/{format}/segment_{n}.{ext}` | `request::handle_media_segment_request` | Serve repackaged media segment (accepts all 7 CMAF/ISOBMFF extensions) |
| POST | `/webhook/repackage` | `webhook::handle_repackage_webhook` | Trigger proactive repackaging (returns 200 after first manifest) |
| POST | `/webhook/repackage/continue` | `webhook::handle_continue` | Internal self-invocation to process remaining segments |
| GET | `/status/{id}/{format}` | `request::handle_status_request` | Query job progress |

`{format}` is a plain format (`hls`, `dash`) or a scheme-qualified format (`hls_cenc`, `hls_cbcs`, `dash_cenc`, `dash_cbcs`, `hls_none`, `dash_none`). Scheme-qualified routes are produced by dual-scheme requests; plain routes still work for backward compatibility (single-scheme requests).

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `REDIS_URL` | Yes | — | Redis endpoint (e.g. `https://us1-xxx.upstash.io`) |
| `REDIS_TOKEN` | Yes | — | Redis auth token |
| `REDIS_BACKEND` | No | `http` | Backend type: `http` or `tcp` |
| `SPEKE_URL` | Yes | — | SPEKE 2.0 license server endpoint |
| `SPEKE_BEARER_TOKEN` | One of three | — | Bearer token auth |
| `SPEKE_API_KEY` | One of three | — | API key auth (pair with `SPEKE_API_KEY_HEADER`) |
| `SPEKE_API_KEY_HEADER` | No | `x-api-key` | Header name for API key |
| `SPEKE_USERNAME` | One of three | — | Basic auth username |
| `SPEKE_PASSWORD` | One of three | — | Basic auth password |

## Redis Key Schema

Keys marked with † are only written by the split execution path (`execute_first()`/`execute_remaining()`). The `execute()` path (sandbox) keeps output in memory via `ProgressiveOutput` and does not cache media data in Redis. Keys marked with ‡ are scheme-qualified (one key per target scheme, using `{format}_{scheme}` e.g. `hls_cenc`).

| Key Pattern | TTL | Content |
|-------------|-----|---------|
| `ep:{content_id}:keys` | 24h | Serialized DRM content keys (JSON) |
| `ep:{content_id}:{format}:state` | 48h | JobStatus JSON (state, progress) |
| `ep:{content_id}:{format}_{scheme}:manifest_state` †‡ | 48h | ManifestState JSON (segments, phase) |
| `ep:{content_id}:{format}_{scheme}:init` †‡ | 48h | Rewritten init segment binary data |
| `ep:{content_id}:{format}_{scheme}:seg:{n}` †‡ | 48h | Rewritten media segment binary data |
| `ep:{content_id}:{format}:source` † | 48h | Source manifest metadata (segment URLs, durations, is_live) |
| `ep:{content_id}:{format}_{scheme}:rewrite_params` †‡ | 48h | Continuation parameters (encryption keys, IV sizes, pattern) |
| `ep:{content_id}:{format}:target_schemes` † | 48h | Target schemes list (JSON array of EncryptionScheme) |
| `ep:{content_id}:speke` | 24h | Cached SPEKE response (avoids duplicate calls) |

## Cache Security

Sensitive cache entries are protected with encryption at rest and explicit cleanup:

### Sensitive Keys

| Key Pattern | Contains |
|-------------|----------|
| `ep:{id}:keys` | Raw AES-128 content keys, KIDs, IVs |
| `ep:{id}:speke` | Full SPEKE CPIX XML response |
| `ep:{id}:{fmt}_{scheme}:rewrite_params` | Source/target encryption keys + IVs + pattern config (per scheme) |

### Encryption at Rest (`cache/encrypted.rs`)

`EncryptedCacheBackend` is a decorator wrapping any `CacheBackend`. It transparently encrypts values for sensitive key patterns using AES-256-GCM before storing, and decrypts on retrieval. Non-sensitive keys pass through unmodified.

- **Key derivation**: `derive_key(token)` uses AES-128-ECB as a PRF — encrypts two distinct 16-byte constant blocks with the first 16 bytes of the Redis token to produce 32 bytes of key material. No SHA-256 dependency needed.
- **Wire format**: `nonce (12 bytes) || ciphertext || tag (16 bytes)` — standard AES-GCM output.
- **Key sensitivity**: `is_sensitive_key(key)` matches keys ending in `:keys`, `:speke`, or `:rewrite_params` (including scheme-qualified keys like `hls_cenc:rewrite_params`).
- **Wiring**: `create_backend()` in `cache/mod.rs` automatically wraps the inner backend with `EncryptedCacheBackend`. The sandbox uses `derive_key("edgepack-sandbox")` since it has no real Redis token.

### Post-Processing Cleanup (`pipeline.rs`)

`cleanup_sensitive_data()` explicitly deletes all sensitive cache entries (DRM keys, SPEKE response, per-scheme rewrite params, target schemes list, source manifest) after the pipeline completes. It accepts `&[EncryptionScheme]` and deletes per-scheme rewrite params for each scheme. It is called at three sites:

1. **`execute()`** — after the segment loop completes (cleans up DRM keys and SPEKE response cached during key acquisition)
2. **`execute_first()`** — inside the `if is_last` block (single-segment content)
3. **`execute_remaining()`** — inside the `if is_last` block (final segment in chained processing)

Cleanup errors are swallowed with `let _ =` so they cannot prevent the pipeline from returning success.

## ISOBMFF Box Types

The parser handles these box types (defined in `media::box_type`):

- **Container**: `moov`, `trak`, `mdia`, `minf`, `stbl`, `moof`, `traf`, `sinf`, `schi`, `mvex`, `edts`
- **Full boxes**: `mvhd`, `tkhd`, `mdhd`, `hdlr`, `stsd`, `tfhd`, `tfdt`, `mfhd`, `trex`
- **Encryption**: `schm`, `tenc`, `pssh`, `senc`, `saiz`, `saio`, `frma`
- **Fragment**: `trun`, `mdat`
- **Grouping**: `sbgp`, `sgpd`
- **Top-level**: `ftyp`

## DRM System IDs

| System | UUID | Constant |
|--------|------|----------|
| Widevine | `edef8ba9-79d6-4ace-a3c8-27dcd51d21ed` | `drm::system_ids::WIDEVINE` |
| PlayReady | `9a04f079-9840-4286-ab92-e65be0885f95` | `drm::system_ids::PLAYREADY` |
| FairPlay | `94ce86fb-07ff-4f43-adb8-93d2fa968ca2` | `drm::system_ids::FAIRPLAY` |

FairPlay is recognised in both input and output. For CENC target output, FairPlay PSSH boxes are excluded (FairPlay does not support CENC). For CBCS target output, FairPlay PSSH boxes are included alongside Widevine and PlayReady.

## Refactoring Roadmap

The codebase is being generalized from a single-purpose CBCS→CENC converter into a generic lightweight edge repackager. Phases 1–5 are complete. Remaining phases:

### ~~Phase 2: Container Format Flexibility (CMAF + fMP4)~~ ✅ Complete
- Created `src/media/container.rs` with `ContainerFormat` enum (`Cmaf`, `Fmp4`) — 22 tests
- Added ftyp brand rewriting in `src/media/init.rs` — 3 new tests
- Wired `container_format` through `RepackageRequest`, `WebhookPayload`, `ManifestState`, `ContinuationParams`, pipeline, progressive output, and manifest renderers
- Updated segment URI extensions dynamically, DASH profile signaling, and route handling for `.cmfv`/`.m4s`
- Result: 541 tests total (466 unit + 75 integration), including binary size guard test

### ~~Phase 3: Unencrypted Input Support~~ ✅ Complete
- Added `EncryptionScheme::None` variant with `is_encrypted()` method and all match arms in `scheme.rs`
- Added panic arms in `sample_cryptor.rs` factory functions for None (should never be called)
- Accepted `"none"` target_scheme in `webhook.rs`, enabled sandbox for clear content
- Added `create_protection_info()` in `init.rs` — inject sinf/schm/tenc/pssh into clear init segments (clear→encrypted)
- Added `strip_protection_info()` in `init.rs` — remove sinf/pssh and restore original sample entries (encrypted→clear)
- Added `rewrite_ftyp_only()` in `init.rs` — format-only conversion for clear→clear
- Added four-way segment dispatch in `segment.rs` with optional source/target keys
- Updated pipeline with conditional SPEKE, four-way init/segment dispatch, optional DRM info
- Updated `ProgressiveOutput::new()` to accept `Option<ManifestDrmInfo>`
- Result: 614 tests total (522 unit + 92 integration), including 10 new clear_content integration tests

### ~~Phase 4: Dual-Scheme Output~~ ✅ Complete
- Result: 652 tests total (538 unit + 114 integration), including 22 new dual_scheme integration tests
- Changed `RepackageRequest.target_scheme` to `target_schemes: Vec<EncryptionScheme>` with backward-compatible webhook API (`target_scheme` singular still accepted)
- Scheme-qualified cache keys using `{format}_{scheme}` pattern (e.g. `ep:{id}:hls_cenc:seg:{n}`)
- Scheme-qualified URL routes (e.g. `/repackage/{id}/hls_cenc/manifest`)
- Pipeline `execute()` returns `Vec<(EncryptionScheme, ProgressiveOutput)>` — one output per scheme
- Split execution (`execute_first()`/`execute_remaining()`) stores per-scheme continuation params, init segments, manifest state
- Source segments decrypted once, re-encrypted for each target scheme
- `cleanup_sensitive_data()` accepts `&[EncryptionScheme]` and deletes per-scheme rewrite params
- Sandbox UI supports "Both (Dual-Scheme)" option, writes output per scheme
- Webhook response includes `manifest_urls: HashMap<String, String>` mapping scheme names to URLs

### ~~Phase 5: Multi-Key DRM & Codec Awareness~~ ✅ Complete
- Multi-key SPEKE requests for multiple KIDs in a single CPIX exchange
- Per-track sinf/tenc via `hdlr` box parsing (video vs audio keying) with `TrackKeyMapping`
- Multi-key PSSH v1 generation (grouped by system_id, all KIDs per system)
- Codec string extraction from stsd (`avcC`, `hvcC`, `vpcC`, `av1C`, `esds` → codec strings)
- Timescale parsing from `mdhd`
- `TrackKeyMapping` threaded through pipeline, init rewriting, PSSH building, and ContinuationParams
- Codec strings populated into `VariantInfo` for HLS/DASH manifest signaling
- New: `src/media/codec.rs` (34 unit tests), `tests/multi_key.rs` (12 integration tests)
- Result: 709 tests total (583 unit + 126 integration)

### Phase 6: Subtitle & Text Track Pass-Through — P0
- WebVTT (`wvtt`) and TTML (`stpp`) sample entry pass-through in fMP4
- CEA-608/708 manifest signaling (pass-through is automatic)
- HLS subtitle rendition groups, DASH subtitle AdaptationSets

### Phase 7: SCTE-35 Ad Markers & Multi-Period DASH — P1
- Parse `emsg` boxes for SCTE-35 splice info
- HLS ad markers (`#EXT-X-DATERANGE` or `#EXT-X-CUE-OUT/IN`)
- Multi-period DASH at SCTE-35 boundaries
- New: `src/media/scte35.rs`

### Phase 8: JIT Packaging (On-Demand GET) — P0
- Manifest-on-GET, Init-on-GET, Segment-on-GET (lazy repackaging)
- Request coalescing via Redis locking
- Hybrid mode (JIT + proactive webhook coexist)
- Configuration endpoint for source resolution

### Phase 9: LL-HLS & LL-DASH — P1
- LL-HLS (`#EXT-X-PART`, `#EXT-X-PRELOAD-HINT`, `#EXT-X-SERVER-CONTROL`, `#EXT-X-SKIP`)
- LL-DASH chunked transfer with `availabilityTimeOffset`
- New: `src/media/chunk.rs`

### Phase 10: MPEG-TS Input — P1
- TS demuxer (PES/TS packets, PAT/PMT, H.264/H.265/AAC extraction)
- TS-to-CMAF transmuxer, init segment synthesis from codec config
- AES-128 segment-level decryption for HLS-TS
- New: `src/media/ts.rs`, `src/media/transmux.rs`
- Binary size trigger: feature-gated with `--features ts`

### Phase 11: Advanced DRM — P1
- Key rotation, clear lead, ClearKey DRM, raw key mode

### Phase 12: Trick Play & I-Frame Playlists — P2
- HLS `#EXT-X-I-FRAMES-ONLY`, DASH trick play Representation

### Phase 13: DVR Window & Time-Shift — P2
- Sliding window manifests, DVR start-over, live-to-VOD

### Phase 14: Content Steering & CDN Optimization — P2
- HLS/DASH content steering, edge location awareness

### Phase 15: TS Segment Output — P2
- CMAF-to-TS muxer, HLS-TS manifests, AES-128 segment encryption
- New: `src/media/ts_mux.rs`

### Phase 16: Compatibility Validation & Hardening — P1 (parallel)
- Codec compatibility matrix, pipeline validation hooks
- HDR metadata preservation validation
- New: `src/media/compat.rs`, conformance test suite

### Phase 17: CDN Provider Adapters & Binary Optimization — P0
- Cloudflare Workers, Fastly Compute, AWS Lambda@Edge, Vercel adapters
- WASI Preview 1 fallback shim
- Binary profiling with `twiggy` + `wasm-opt`

Critical path: **Phase 8 → Phase 17**
Full roadmap plan: `.claude/plans/crystalline-singing-bee.md`
