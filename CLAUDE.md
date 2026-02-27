# CLAUDE.md — Agent Context for edge-packager

This file provides context for Claude (Opus 4.6) when working on this codebase.

## Project Summary

**edge-packager** is a Rust library compiled to WASM (`wasm32-wasip2`) that runs on CDN edge nodes. It repackages DASH/HLS CMAF media from CBCS encryption (FairPlay/Widevine/PlayReady) into CENC encryption (Widevine/PlayReady only), producing progressive HLS or DASH output. It communicates with DRM license servers via SPEKE 2.0 / CPIX for multi-key content encryption keys.

## Build Commands

```bash
# Development build (default target is wasm32-wasip2 via .cargo/config.toml)
cargo build

# Release build (optimised for size: opt-level=s, LTO, stripped)
cargo build --release

# Run unit tests (MUST specify native host target — tests cannot run in WASI)
cargo test --target $(rustc -vV | grep host | awk '{print $2}')

# Check without building
cargo check
```

**Important**: `cargo test` without `--target` will try to execute the WASM binary directly, which fails with a permission error. Always pass the native host target flag.

The WASM target requires `rustup target add wasm32-wasip2`. The `.cargo/config.toml` sets `wasm32-wasip2` as the default build target, so bare `cargo build` produces a `.wasm` file.

## Architecture Overview

```
src/
├── lib.rs              Module root (re-exports all submodules)
├── error.rs            EdgePackagerError enum + Result<T> alias
├── config.rs           AppConfig loaded from env vars
├── http_client.rs      Shared outgoing HTTP client (WASI wasi:http/outgoing-handler)
├── wasi_handler.rs     WASI incoming handler bridge (wasm32 only)
├── cache/              Redis-backed application state store
│   ├── mod.rs          CacheBackend trait + CacheKeys builder + factory
│   ├── redis_http.rs   Upstash-compatible HTTP Redis (primary)
│   └── redis_tcp.rs    TCP Redis stub (forward compatibility)
├── drm/                DRM key acquisition and encryption
│   ├── mod.rs          ContentKey, DrmSystemData, DrmKeySet types + system ID constants
│   ├── speke.rs        SPEKE 2.0 HTTP client
│   ├── cpix.rs         CPIX XML request builder + response parser
│   ├── cbcs.rs         AES-128-CBC pattern decryption (CBCS scheme)
│   └── cenc.rs         AES-128-CTR encryption (CENC scheme)
├── media/              ISOBMFF/CMAF container handling
│   ├── mod.rs          FourCC type, box_type constants, TrackType enum
│   ├── cmaf.rs         Zero-copy MP4 box parser, builders, iterators
│   ├── init.rs         Init segment rewriting (sinf/schm/tenc/pssh)
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

## Key Concepts

### Two-Tier Caching

- **CDN cache** (primary): HTTP `Cache-Control` headers on responses. Segments and finalised manifests use `max-age=31536000, immutable`. Live manifests use `max-age=1, s-maxage=1`.
- **Redis** (application state): Stores DRM keys, job state, SPEKE response cache, and progressive manifest state. NOT used for storing media data.

### Encryption Transform

The core transform is CBCS → CENC on CMAF segments:
1. Parse `senc` box → get per-sample IVs and subsample maps
2. Decrypt `mdat` using AES-128-CBC with pattern (crypt_byte_block:skip_byte_block)
3. Re-encrypt `mdat` using AES-128-CTR (no pattern, full sample encryption)
4. Rewrite `senc` box with new sequential IVs
5. Rebuild `moof` + `mdat`

Init segments require rewriting `sinf`/`schm`/`tenc`/`pssh` boxes and removing FairPlay PSSH.

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

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `aes` | 0.8 | AES block cipher (CBCS + CENC) |
| `cbc` | 0.1 | CBC mode for CBCS decryption |
| `ctr` | 0.9 | CTR mode for CENC encryption |
| `cipher` | 0.4 | Cipher traits shared by cbc/ctr |
| `quick-xml` | 0.37 | CPIX XML + DASH MPD parsing/generation |
| `serde` | 1 | Serialization framework |
| `serde_json` | 1 | JSON for Redis, webhooks, job state |
| `base64` | 0.22 | Key encoding in CPIX, PSSH in manifests |
| `uuid` | 1 | Content Key IDs (KIDs) |
| `url` | 2 | URL parsing for SPEKE endpoint, source URLs |
| `thiserror` | 2 | Derive macro for error types |
| `log` | 0.4 | Logging facade |
| `wasi` | 0.14 | WASI Preview 2 bindings (wasm32 target only) |

All crates are chosen for WASM compatibility (no system dependencies, no async runtime requirements).

## Tests

The project has **359 tests** total: 287 unit tests and 72 integration tests. All run on the native host target.

### Unit Tests (287)

Inlined as `#[cfg(test)] mod tests` blocks in every source file. They cover:

- **Serde roundtrips** for all serializable types (config, manifest state, job status, DRM keys, webhook payloads)
- **Encryption correctness**: CBCS decrypt and CENC encrypt/decrypt with known-answer tests and roundtrips
- **ISOBMFF box parsing**: Building binary boxes, parsing them back, verifying headers, payloads, and child iteration
- **Manifest rendering**: HLS M3U8 and DASH MPD output for every lifecycle phase (AwaitingFirstSegment, Live, Complete), DRM signaling, variant streams
- **Progressive output state machine**: Phase transitions, cache-control header generation, segment URI formatting
- **HTTP routing**: Path parsing, format validation, segment number extraction, all route dispatching
- **Webhook validation**: Valid/invalid JSON, missing fields, bad formats, empty URLs, serde roundtrips
- **Error variants**: Display output for every EdgePackagerError variant

To run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::cbcs`

### Integration Tests (72)

Located in the `tests/` directory. These exercise cross-module workflows using synthetic CMAF fixtures with no external dependencies:

```
tests/
├── common/
│   └── mod.rs                 Shared fixtures: synthetic ISOBMFF builders, test keys, DRM key sets, manifest states
├── encryption_roundtrip.rs    8 tests: CBCS→plaintext→CENC full pipeline
├── isobmff_integration.rs    18 tests: init/media segment parsing, rewriting, PSSH/senc roundtrips
├── manifest_integration.rs   20 tests: progressive output lifecycle, DRM signaling, cache headers
└── handler_integration.rs    26 tests: HTTP routing, webhook validation, response helpers
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
- **No `main.rs`**: This is a library crate (`crate-type = ["cdylib", "rlib"]`). The WASI runtime calls the exported handler functions. The `rlib` target enables integration tests to link against the crate.
- **Two test locations**: Unit tests live inline in `#[cfg(test)] mod tests` blocks within each source file. Integration tests live in the `tests/` directory with shared fixtures in `tests/common/mod.rs`.

## HTTP Route Table

| Method | Path | Handler | Description |
|--------|------|---------|-------------|
| GET | `/health` | inline | Health check, returns "ok" |
| GET | `/repackage/{id}/{format}/manifest` | `request::handle_manifest_request` | Serve repackaged manifest |
| GET | `/repackage/{id}/{format}/init.mp4` | `request::handle_init_segment_request` | Serve repackaged init segment |
| GET | `/repackage/{id}/{format}/segment_{n}.cmfv` | `request::handle_media_segment_request` | Serve repackaged media segment |
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

FairPlay is recognised in input (CBCS source) but excluded from output (CENC target).
