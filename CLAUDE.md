# CLAUDE.md — Agent Context for edge-packager

This file provides context for Claude (Opus 4.6) when working on this codebase.

## Project Summary

**edge-packager** is a Rust library compiled to WASM (`wasm32-wasip2`) that runs on CDN edge nodes. It repackages DASH/HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC) and container formats (CMAF ↔ fMP4), producing progressive HLS or DASH output. The target encryption scheme and container format are configurable per request, supporting all encryption combinations (CBCS→CENC, CENC→CBCS, CENC→CENC, CBCS→CBCS) with automatic source scheme detection, and output as either CMAF or fragmented MP4. It communicates with DRM license servers via SPEKE 2.0 / CPIX for multi-key content encryption keys.

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
├── error.rs            EdgePackagerError enum + Result<T> alias
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
│   ├── scheme.rs       EncryptionScheme enum (Cbcs/Cenc) + scheme-specific helpers
│   ├── sample_cryptor.rs  SampleDecryptor/SampleEncryptor traits + factory functions
│   ├── speke.rs        SPEKE 2.0 HTTP client
│   ├── cpix.rs         CPIX XML request builder + response parser
│   ├── cbcs.rs         AES-128-CBC pattern decryption + encryption (CBCS scheme)
│   └── cenc.rs         AES-128-CTR encryption + decryption (CENC scheme)
├── media/              ISOBMFF/CMAF/fMP4 container handling
│   ├── mod.rs          FourCC type, box_type constants, TrackType enum
│   ├── cmaf.rs         Zero-copy MP4 box parser, builders, iterators
│   ├── container.rs    ContainerFormat enum (Cmaf/Fmp4) — brands, extensions, profiles
│   ├── init.rs         Init segment rewriting (sinf/schm/tenc/pssh + ftyp brand rewriting)
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
- **Redis** (application state): Stores DRM keys, job state, SPEKE response cache, and progressive manifest state. NOT used for storing media data.

### Encryption Transform

The core transform is scheme-configurable on CMAF segments (source and target schemes determined at runtime):
1. Parse `senc` box → get per-sample IVs and subsample maps
2. Decrypt `mdat` using source scheme (`create_decryptor()` dispatches to CBCS or CENC)
3. Re-encrypt `mdat` using target scheme (`create_encryptor()` dispatches to CBCS or CENC)
4. Rewrite `senc` box with new IVs (size depends on target scheme: 16 bytes for CBCS, 8 bytes for CENC)
5. Rebuild `moof` + `mdat`

Init segments require rewriting `sinf`/`schm`/`tenc`/`pssh` boxes and `ftyp` brands. The `schm` box type and `tenc` parameters (IV size, pattern) are set based on the target `EncryptionScheme`. PSSH boxes are filtered per target scheme (FairPlay included for CBCS output, excluded for CENC output). The `ftyp` box is rewritten with compatible brands matching the target `ContainerFormat` (CMAF includes `cmfc`, fMP4 does not).

**Scheme-specific behaviour:**
- **CBCS**: AES-128-CBC, pattern encryption (1:9 video, 0:0 audio), 16-byte IVs, supports FairPlay
- **CENC**: AES-128-CTR, full encryption (no pattern), 8-byte IVs, Widevine + PlayReady only
- Source scheme auto-detected from init segment `schm` box or manifest DRM signaling

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
- Route handler accepts all three extensions (`.cmfv`, `.m4s`, `.mp4`) for media segment requests

### Progressive Manifest Output

The `ProgressiveOutput` state machine transitions:
- `AwaitingFirstSegment` → `Live` (on first segment complete, manifest written with short cache TTL)
- `Live` → `Live` (each subsequent segment updates manifest)
- `Live` → `Complete` (final segment or source EOF, manifest switches to immutable cache headers; HLS adds `#EXT-X-ENDLIST`, DASH changes `type` from `dynamic` to `static`)

### SPEKE 2.0 / CPIX

The `drm/speke.rs` client POSTs a CPIX XML document to the license server requesting content keys for specified KIDs and DRM system IDs (Widevine, PlayReady). The response contains encrypted content keys and PSSH box data. The `drm/cpix.rs` module handles XML building and parsing.

## Error Handling

All modules use `crate::error::Result<T>` which aliases `std::result::Result<T, EdgePackagerError>`. The `EdgePackagerError` enum has specific variants for each subsystem (Cache, Drm, Speke, Cpix, Encryption, MediaParse, SegmentRewrite, Manifest, Http, Config, InvalidInput, NotFound, Io). Use `thiserror` derive macros. Propagation is via `?` operator throughout.

## Runtime Implementation

All HTTP transport and request handling is fully implemented:

1. **`http_client.rs`**: Shared HTTP client using `wasi:http/outgoing-handler` (wasm32) with native stub error (non-wasm32, preserves test builds).
2. **`wasi_handler.rs`**: WASI incoming handler bridge implementing `wasi:http/incoming-handler::Guest`. Converts WASI types ↔ library types and maps errors to HTTP status codes.
3. **`cache/redis_http.rs` → `execute_command()`**: Uses `http_client::get()` to make Upstash REST API calls. Parses JSON responses via extracted `parse_upstash_response()`.
4. **`drm/speke.rs` → `post_cpix()`**: Uses `http_client::post()` to POST CPIX XML to license server with auth headers.
5. **`repackager/pipeline.rs`**: `fetch_source_manifest()` auto-detects HLS vs DASH and parses. `fetch_segment()` fetches binary data. Pipeline split into `execute_first()` (through first segment + live manifest) and `execute_remaining()` (one segment per invocation with self-invocation chaining).
6. **`manifest/hls_input.rs` + `dash_input.rs`**: Source manifest input parsers extracting segment URLs, durations, init segment references, and live/VOD detection.
7. **`handler/request.rs`**: All four GET handlers query Redis for cached segment data and manifest state via `HandlerContext`.
8. **`handler/webhook.rs`**: Creates pipeline, calls `execute_first()`, fires self-invocation to `/webhook/repackage/continue`, returns 200 after first manifest publishes. Continue handler chains remaining segment processing.

## Local Sandbox

The `sandbox` feature enables a native binary (`src/bin/sandbox.rs`) that reuses the production `RepackagePipeline` with native HTTP transport and an in-memory cache.

### Architecture

- **`http_client.rs`** has a three-way `#[cfg]` dispatch: `wasm32` → WASI HTTP, `sandbox` feature → `reqwest::blocking`, neither → stub error
- **`cache/memory.rs`** implements `CacheBackend` using `Arc<RwLock<HashMap>>` (shared between pipeline thread and API server)
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

Pipeline output is written to `sandbox/output/{content_id}/{format}/` and also served via the API at `/api/output/{id}/{format}/{file}`.

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

The project has **560 tests** total: 480 unit tests and 80 integration tests. All run on the native host target. The release WASM binary is ~495 KB (guarded by a binary size test with a 600 KB threshold).

### Unit Tests (480)

Inlined as `#[cfg(test)] mod tests` blocks in every source file. They cover:

- **Serde roundtrips** for all serializable types (config, manifest state, job status, DRM keys, webhook payloads, encryption schemes, container formats, continuation params)
- **Encryption scheme abstraction**: `EncryptionScheme` enum (serde roundtrips, scheme_type_bytes, from_scheme_type, HLS method strings, default IV sizes, default patterns, FairPlay support flags), `SampleDecryptor`/`SampleEncryptor` trait dispatch via factory functions
- **Container format abstraction**: `ContainerFormat` enum with three variants (Cmaf, Fmp4, Iso) — extensions, brands, ftyp box building, DASH profile strings, serde roundtrips, display, from_str_value parsing
- **Encryption correctness**: CBCS decrypt + encrypt, CENC encrypt + decrypt, scheme-agnostic roundtrips through factory functions
- **ISOBMFF box parsing**: Building binary boxes, parsing them back, verifying headers, payloads, and child iteration
- **Init segment rewriting**: Scheme-parameterized `schm`/`tenc`/`pssh` rewriting (CBCS and CENC targets, tenc pattern encoding, PSSH filtering per scheme), ftyp brand rewriting per container format (CMAF includes `cmfc`, fMP4 does not)
- **Segment rewriting**: Scheme-aware decrypt/re-encrypt with configurable source/target scheme and pattern
- **Manifest rendering**: HLS M3U8 and DASH MPD output for every lifecycle phase, dynamic DRM scheme signaling (SAMPLE-AES/SAMPLE-AES-CTR for HLS, cbcs/cenc value for DASH), FairPlay key URI rendering
- **Source manifest parsing**: HLS M3U8 and DASH MPD input parsing including source scheme detection from `#EXT-X-KEY` METHOD and `<ContentProtection>` elements
- **Progressive output state machine**: Phase transitions, cache-control header generation, dynamic segment URI formatting per container format
- **Pipeline DRM info**: Manifest DRM info building with CBCS/CENC target scheme, FairPlay inclusion/exclusion, container format threading through ContinuationParams
- **URL parsing**: Lightweight URL parser (parse, join, component access, serde roundtrips, authority extraction, relative path resolution)
- **HTTP routing**: Path parsing, format validation, segment number extraction (.cmfv, .m4s, and .mp4), all route dispatching
- **Webhook validation**: Valid/invalid JSON, missing fields, bad formats, empty URLs, target_scheme parsing, container_format parsing (cmaf/fmp4/iso), invalid scheme/format rejection, serde roundtrips
- **Error variants**: Display output for every EdgePackagerError variant

To run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::cbcs`

### Integration Tests (80)

Located in the `tests/` directory. These exercise cross-module workflows using synthetic CMAF fixtures with no external dependencies:

```
tests/
├── common/
│   └── mod.rs                 Shared fixtures: synthetic ISOBMFF builders, test keys, DRM key sets, manifest states
├── encryption_roundtrip.rs    8 tests: CBCS→plaintext→CENC full pipeline
├── isobmff_integration.rs    18 tests: init/media segment parsing, rewriting (scheme + container format aware), PSSH/senc roundtrips
├── manifest_integration.rs   23 tests: progressive output lifecycle, DRM signaling, cache headers, ISO BMFF format
├── handler_integration.rs    30 tests: HTTP routing (incl. .cmfv, .m4s, and .mp4 segments), webhook validation, response helpers
└── wasm_binary_size.rs        1 test: release WASM binary stays under 600 KB size limit
```

**Key fixtures in `tests/common/mod.rs`:**
- `build_cbcs_init_segment()` — builds a synthetic CBCS init segment (ftyp + moov with stsd→encv→sinf→frma/schm/schi/tenc + pssh)
- `build_cbcs_media_segment(sample_count, sample_size)` — builds a CBCS-encrypted moof+mdat with configurable samples; returns `(segment_bytes, plaintext_samples)` for verification
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
| GET | `/repackage/{id}/{format}/segment_{n}.cmfv` | `request::handle_media_segment_request` | Serve repackaged CMAF media segment |
| GET | `/repackage/{id}/{format}/segment_{n}.m4s` | `request::handle_media_segment_request` | Serve repackaged fMP4 media segment |
| GET | `/repackage/{id}/{format}/segment_{n}.mp4` | `request::handle_media_segment_request` | Serve repackaged ISO BMFF media segment |
| POST | `/webhook/repackage` | `webhook::handle_repackage_webhook` | Trigger proactive repackaging (returns 200 after first manifest) |
| POST | `/webhook/repackage/continue` | `webhook::handle_continue` | Internal self-invocation to process remaining segments |
| GET | `/status/{id}/{format}` | `request::handle_status_request` | Query job progress |

`{format}` is `hls` or `dash`.

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

| Key Pattern | TTL | Content |
|-------------|-----|---------|
| `ep:{content_id}:keys` | 24h | Serialized DRM content keys (JSON) |
| `ep:{content_id}:{format}:state` | 48h | JobStatus JSON (state, progress) |
| `ep:{content_id}:{format}:manifest_state` | 48h | ManifestState JSON (segments, phase) |
| `ep:{content_id}:{format}:init` | 48h | Rewritten init segment binary data |
| `ep:{content_id}:{format}:seg:{n}` | 48h | Rewritten media segment binary data |
| `ep:{content_id}:{format}:source` | 48h | Source manifest metadata (segment URLs, durations, is_live) |
| `ep:{content_id}:{format}:rewrite_params` | 48h | Continuation parameters (encryption keys, IV sizes, pattern) |
| `ep:{content_id}:speke` | 24h | Cached SPEKE response (avoids duplicate calls) |

## Cache Security

Sensitive cache entries are protected with encryption at rest and explicit cleanup:

### Sensitive Keys

| Key Pattern | Contains |
|-------------|----------|
| `ep:{id}:keys` | Raw AES-128 content keys, KIDs, IVs |
| `ep:{id}:speke` | Full SPEKE CPIX XML response |
| `ep:{id}:{fmt}:rewrite_params` | Source/target encryption keys + IVs + pattern config |

### Encryption at Rest (`cache/encrypted.rs`)

`EncryptedCacheBackend` is a decorator wrapping any `CacheBackend`. It transparently encrypts values for sensitive key patterns using AES-256-GCM before storing, and decrypts on retrieval. Non-sensitive keys pass through unmodified.

- **Key derivation**: `derive_key(token)` uses AES-128-ECB as a PRF — encrypts two distinct 16-byte constant blocks with the first 16 bytes of the Redis token to produce 32 bytes of key material. No SHA-256 dependency needed.
- **Wire format**: `nonce (12 bytes) || ciphertext || tag (16 bytes)` — standard AES-GCM output.
- **Key sensitivity**: `is_sensitive_key(key)` matches keys ending in `:keys`, `:speke`, or `:rewrite_params`.
- **Wiring**: `create_backend()` in `cache/mod.rs` automatically wraps the inner backend with `EncryptedCacheBackend`. The sandbox uses `derive_key("edge-packager-sandbox")` since it has no real Redis token.

### Post-Processing Cleanup (`pipeline.rs`)

`cleanup_sensitive_data()` explicitly deletes all four sensitive cache entries after the pipeline completes. It is called at three sites:

1. **`execute()`** — after the segment loop completes
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

The codebase is being generalized from a single-purpose CBCS→CENC converter into a generic lightweight edge repackager. Phases 1 and 2 are complete. Remaining phases (3–6):

### ~~Phase 2: Container Format Flexibility (CMAF + fMP4)~~ ✅ Complete
- Created `src/media/container.rs` with `ContainerFormat` enum (`Cmaf`, `Fmp4`) — 22 tests
- Added ftyp brand rewriting in `src/media/init.rs` — 3 new tests
- Wired `container_format` through `RepackageRequest`, `WebhookPayload`, `ManifestState`, `ContinuationParams`, pipeline, progressive output, and manifest renderers
- Updated segment URI extensions dynamically, DASH profile signaling, and route handling for `.cmfv`/`.m4s`
- Result: 541 tests total (466 unit + 75 integration), including binary size guard test

### Phase 3: Unencrypted Input Support
- Add `EncryptionScheme::None` variant for clear (unencrypted) content
- Update source detection (`hls_input.rs`, `dash_input.rs`) to identify unencrypted sources
- Add `create_protection_info()` in `init.rs` to inject sinf/schm/tenc into clear init segments (clear→encrypted)
- Skip decryption in `segment.rs` when source is `None`; encrypt-only path for clear→encrypted
- Clear→clear pass-through for format-only conversion (no encryption/decryption)
- Conditional SPEKE key acquisition — skip when both source and target are unencrypted
- Update sandbox UI with "None (Clear)" target scheme and conditional SPEKE visibility
- Estimated: ~300 new LOC, ~180 modified LOC, ~25 new tests

### Phase 4: Dual-Scheme Output
- Multi-rendition: `target_schemes: Vec<EncryptionScheme>` producing separate segment sets per scheme
- Scheme-suffixed cache keys (`ep:{id}:{fmt}:cenc:seg:{n}`)
- Dual-encrypted segments: single segment encrypted with both CBCS and CENC (multiple sinf boxes)
- Multi-variant HLS master playlist and multi-AdaptationSet DASH MPD
- Estimated: ~380 new LOC, ~200 modified LOC, ~35 new tests

### Phase 5: Full Remux (Sample-Level mdat Access)
- Create `src/media/samples.rs` for sample-level parsing/rebuilding
- Segment boundary restructuring at sync points
- Timescale parsing from mdhd/mvhd boxes
- Variable segment count support in progressive output
- Estimated: ~610 new LOC, ~80 modified LOC, ~40 new tests

### Phase 6: Compatibility Validation & Hardening
- Create `src/media/compat.rs` for target compatibility checking (e.g. Chromium 53+)
- Codec detection from stsd sample entries
- Pipeline validation hooks for early rejection of incompatible configs
- New error variants: `Compatibility`, `UnsupportedCodec`
- Estimated: ~260 new LOC, ~45 modified LOC, ~30 new tests

Full plan details: `.claude/plans/radiant-plotting-badger.md`
