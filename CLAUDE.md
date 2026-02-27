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

# Run unit tests (uses native host target, not WASM)
cargo test

# Check without building
cargo check
```

The WASM target requires `rustup target add wasm32-wasip2`. The `.cargo/config.toml` sets `wasm32-wasip2` as the default build target, so bare `cargo build` produces a `.wasm` file.

## Architecture Overview

```
src/
├── lib.rs              Module root (re-exports all submodules)
├── error.rs            EdgePackagerError enum + Result<T> alias
├── config.rs           AppConfig loaded from env vars
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
├── manifest/           Output manifest generation
│   ├── mod.rs          render_manifest() dispatcher
│   ├── types.rs        ManifestState, ManifestPhase, SegmentInfo, DrmInfo, etc.
│   ├── hls.rs          HLS M3U8 renderer (media + master playlists)
│   └── dash.rs         DASH MPD renderer (SegmentTemplate + SegmentTimeline)
├── repackager/         Orchestration layer
│   ├── mod.rs          RepackageRequest, JobStatus, JobState types
│   ├── pipeline.rs     RepackagePipeline — full fetch→decrypt→re-encrypt→output flow
│   └── progressive.rs  ProgressiveOutput state machine (AwaitingFirstSegment→Live→Complete)
└── handler/            HTTP request handling
    ├── mod.rs          Router, HttpRequest/HttpResponse types, route() dispatcher
    ├── request.rs      On-demand GET handlers (manifest, init, segment, status)
    └── webhook.rs      POST /webhook/repackage handler
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

## Unimplemented Areas (marked with TODO)

These sections compile but return placeholder errors and need WASI HTTP transport:

1. **`cache/redis_http.rs` → `execute_command()`**: Needs `wasi:http/outgoing-handler` to make Upstash REST API calls.
2. **`drm/speke.rs` → `post_cpix()`**: Needs `wasi:http/outgoing-handler` to POST CPIX XML to license server.
3. **`repackager/pipeline.rs` → `fetch_source_manifest()` and `fetch_segment()`**: Need `wasi:http/outgoing-handler` to fetch origin content.
4. **`handler/request.rs`**: All four request handlers need to be wired up to the cache backend and pipeline.
5. **`handler/webhook.rs`**: Async pipeline kickoff needs WASI task scheduling or self-referential HTTP chaining.
6. **Source manifest parsing**: The pipeline currently stubs out source manifest fetching. Needs an HLS M3U8 and DASH MPD *input* parser (the `manifest/` module only handles *output* generation).

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

All crates are chosen for WASM compatibility (no system dependencies, no async runtime requirements).

## Coding Conventions

- **No `async`/`await`**: WASI Preview 2 doesn't have a standard async runtime. All I/O is synchronous (blocking WASI calls).
- **Zero-copy parsing where possible**: The ISOBMFF parser works with byte slices and offsets rather than allocating per-box.
- **Trait-based abstraction**: `CacheBackend` trait allows swapping Redis implementations without changing business logic.
- **Explicit state machines**: `ManifestPhase` and `JobState` enums drive control flow rather than implicit boolean flags.
- **`#[derive(Serialize, Deserialize)]`** on all types that cross the Redis boundary.
- **No `main.rs`**: This is a library crate (`crate-type = ["cdylib"]`). The WASI runtime calls the exported handler functions.

## HTTP Route Table

| Method | Path | Handler | Description |
|--------|------|---------|-------------|
| GET | `/health` | inline | Health check, returns "ok" |
| GET | `/repackage/{id}/{format}/manifest` | `request::handle_manifest_request` | Serve repackaged manifest |
| GET | `/repackage/{id}/{format}/init.mp4` | `request::handle_init_segment_request` | Serve repackaged init segment |
| GET | `/repackage/{id}/{format}/segment_{n}.cmfv` | `request::handle_media_segment_request` | Serve repackaged media segment |
| POST | `/webhook/repackage` | `webhook::handle_repackage_webhook` | Trigger proactive repackaging |
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
