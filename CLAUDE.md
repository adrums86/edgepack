# CLAUDE.md — Agent Context for edgepack

This file provides context for Claude (Opus 4.6) when working on this codebase.

## Project Priorities

These two priorities govern all development decisions:

1. **Output Integrity.** Manifest and segment output from edgepack must be 100% to spec at every level — ISOBMFF box structure, encryption transforms, DRM signaling, manifest syntax (HLS RFC 8216, DASH ISO 23009-1), and codec conformance. Every merge to main must pass all output integrity tests (`tests/output_integrity.rs`, `tests/conformance.rs`). When adding new features or encryption paths, add corresponding integrity tests before merging. Never sacrifice correctness for speed.

2. **Performance.** edgepack is designed to go from a CDN cache miss to producing a manifest and segments as fast as possible. It must be the most performant packager possible in terms of cold start times (sub-1ms WASM instantiation), processing throughput (zero-copy parsing, minimal allocations), and flexibility (any encryption/format combination in a single request). Guard binary size (WASM size limits in `tests/wasm_binary_size.rs`), avoid unnecessary dependencies, and benchmark critical paths (`benches/jit_latency.rs`). Every new feature should be evaluated for its impact on binary size and processing latency.

## Project Summary

**edgepack** is a Rust library compiled to WASM (`wasm32-wasip2`) that runs on CDN edge nodes. The ~692 KB binary instantiates in under 1 ms, enabling **just-in-time (JIT) packaging** — content is repackaged on the first viewer request rather than pre-processed at origin, eliminating storage of pre-packaged variants and packaging queues. It repackages DASH/HLS CMAF/fMP4 media between encryption schemes (CBCS ↔ CENC ↔ None) and container formats (CMAF ↔ fMP4), producing progressive HLS or DASH output. Supports **dual-format output** (simultaneous HLS and DASH from a single request, sharing format-agnostic segments), **dual-scheme output** (multiple target encryption schemes simultaneously), **multi-key DRM** (per-track keying with separate video/audio KIDs and multi-KID PSSH boxes), **advanced DRM** (ClearKey, raw key mode, key rotation, clear lead), **LL-HLS & LL-DASH** (partial segments, server control, chunk detection), **trick play & I-frame playlists** (HLS `#EXT-X-I-FRAMES-ONLY` with BYTERANGE, DASH trick play AdaptationSets), **DVR sliding window** (configurable time-shift buffer, windowed manifests for live streams, automatic live-to-VOD transitions), **content steering** (HLS `#EXT-X-CONTENT-STEERING` and DASH `<ContentSteering>` injection, DASH source pass-through, webhook override priority), **MPEG-TS input** (TS demux + CMAF transmux, feature-gated), **SCTE-35 ad marker pass-through** (emsg extraction, HLS `#EXT-X-DATERANGE`, DASH `<EventStream>`), **codec string extraction** (RFC 6381 codec strings for manifest signaling), **subtitle/text track pass-through** (WebVTT/TTML in fMP4 with HLS subtitle rendition groups, DASH subtitle AdaptationSets, and CEA-608/708 closed caption manifest signaling), and **codec/scheme compatibility validation** (pre-flight checks, HDR detection). The target encryption scheme(s) and container format are configurable per request, supporting all encryption combinations (CBCS→CENC, CENC→CBCS, CENC→CENC, CBCS→CBCS) and clear content paths (clear→CENC, clear→CBCS, encrypted→clear, clear→clear) with automatic source scheme detection, and output as either CMAF or fragmented MP4. It communicates with DRM license servers via SPEKE 2.0 / CPIX for multi-key content encryption keys (skipped when both source and target are unencrypted, or bypassed via raw key mode).

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
├── cache/              Cache backend abstraction layer (multi-provider)
│   ├── mod.rs          CacheBackend trait + CacheKeys builder + factory
│   ├── encrypted.rs    AES-256-GCM encryption layer for sensitive cache entries
│   ├── memory.rs       In-memory cache backend (sandbox feature only)
│   ├── redis_http.rs   Upstash-compatible HTTP Redis (default)
│   ├── redis_tcp.rs    TCP Redis stub (forward compatibility)
│   ├── cloudflare_kv.rs Cloudflare Workers KV REST API (cloudflare feature)
│   └── http_kv.rs      Generic HTTP KV (AWS DynamoDB, Akamai EdgeKV, custom)
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
│   ├── chunk.rs        CMAF chunk boundary detection for LL-HLS parts
│   ├── codec.rs        Codec string extraction, track metadata parsing, TrackKeyMapping
│   ├── compat.rs       Codec/scheme compatibility validation, HDR detection, init/segment structure checks
│   ├── container.rs    ContainerFormat enum (Cmaf/Fmp4) — brands, extensions, profiles
│   ├── init.rs         Init segment rewriting (sinf/schm/tenc/pssh + ftyp brand rewriting, per-track keying)
│   ├── scte35.rs       SCTE-35 splice_info_section parser (splice_insert, time_signal)
│   ├── segment.rs      Media segment rewriting (senc/mdat decrypt+re-encrypt)
│   ├── ts.rs           MPEG-TS demuxer — PAT/PMT/PES parsing, AES-128 decryption (ts feature)
│   └── transmux.rs     TS-to-CMAF transmuxer — Annex B→AVCC, init synthesis (ts feature)
├── manifest/           Manifest parsing (input) and rendering (output)
│   ├── mod.rs          render_manifest() + render_iframe_manifest() dispatchers
│   ├── types.rs        ManifestState, ManifestPhase, SegmentInfo, IFrameSegmentInfo, DrmInfo, CeaCaptionInfo, AdBreakInfo, SourceManifest
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

Detailed Mermaid diagrams are in [`docs/architecture.md`](docs/architecture.md). The file contains 11 diagrams: system context, data flow, module architecture, split execution sequence, progressive output state machine, cache security model, cache key layout, CDN caching strategy, per-segment encryption transform, container format comparison, and I-frame detection & trick play flow. All diagrams are Mermaid syntax, portable to Confluence, Jira, and Lucidchart.

## Key Concepts

### Two-Tier Caching

- **CDN cache** (primary): HTTP `Cache-Control` headers on responses. Default TTLs: segments and finalised manifests use `max-age=31536000, immutable`; live manifests use `max-age=1, s-maxage=1`. TTLs are configurable at three levels: env var system defaults (`CACHE_MAX_AGE_SEGMENTS`, `CACHE_MAX_AGE_MANIFEST_LIVE`, `CACHE_MAX_AGE_MANIFEST_FINAL`), per-request overrides via `CacheControlConfig` on `RepackageRequest`/`WebhookPayload`, and hardcoded safety invariants (`AwaitingFirstSegment` → always `no-cache`, status endpoint → always `no-cache`, `public` prefix → always present). Per-request overrides apply to manifests only — segments use system defaults to avoid extra Redis GET per segment request.
- **Cache backend** (application state): Stores DRM keys, job state, SPEKE response cache, progressive manifest state, and rewritten media data (init segments, media segments) for the split execution path (`execute_first()`/`execute_remaining()`). The `execute()` path (sandbox) does not cache media data — it returns output directly via `ProgressiveOutput`. Backend is configurable: Redis HTTP (default), Redis TCP, Cloudflare Workers KV (`cloudflare` feature), or generic HTTP KV (for AWS DynamoDB, Akamai EdgeKV, custom stores).

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
- Language from `mdhd` (ISO 639-2/T packed 3×5-bit chars, `None` for "und")
- RFC 6381 codec string from `stsd` sample entry config boxes:
  - H.264: `avcC` → `avc1.{profile}{constraint}{level}`
  - H.265: `hvcC` → `hev1.{profile}.{tier}{level}.{constraint}`
  - AAC: `esds` → `mp4a.40.{audioObjectType}`
  - VP9: `vpcC` → `vp09.{profile}.{level}.{bitDepth}`
  - AV1: `av1C` → `av01.{profile}.{level}{tier}.{bitDepth}`
  - WebVTT: `wvtt` → `"wvtt"`, TTML: `stpp` → `"stpp"`
  - AC-3, EC-3, Opus, FLAC → simple FourCC strings

**Pipeline integration:** The pipeline calls `extract_tracks()` on the source init segment, builds `TrackKeyMapping` from the track metadata, collects all unique KIDs for the SPEKE request, and threads the key mapping through init rewriting. Codec strings are populated into `VariantInfo` for manifest rendering (HLS `CODECS=` attribute, DASH `codecs=` attribute).

### SPEKE 2.0 / CPIX

The `drm/speke.rs` client POSTs a CPIX XML document to the license server requesting content keys for specified KIDs and DRM system IDs (Widevine, PlayReady). The response contains encrypted content keys and PSSH box data. The `drm/cpix.rs` module handles XML building and parsing. Multi-key requests are natively supported — the CPIX builder assigns `intendedTrackType` ("VIDEO"/"AUDIO") per KID.

### Advanced DRM (Phase 11)

**ClearKey DRM:** ClearKey system support with locally-built PSSH data (JSON `{"kids":["base64url-kid"]}` format). ClearKey is not sent to SPEKE — PSSH boxes are constructed from KIDs directly.

**Raw key mode:** Bypass SPEKE entirely by providing encryption keys directly via the webhook (`raw_keys` array with hex-encoded `kid`, `key`, and optional `iv`). Useful for testing and for workflows where keys are managed externally.

**Key rotation:** Rotate encryption keys at configurable segment boundaries (`key_rotation.period_segments`). Each rotation period gets its own DRM signaling — HLS emits new `#EXT-X-KEY` tags at boundaries, DASH creates new `<Period>` elements with fresh `<ContentProtection>`.

**Clear lead:** Leave the first N segments unencrypted (`clear_lead_segments`). The manifest transitions from `METHOD=NONE` to the target encryption method at the boundary, with a new `#EXT-X-MAP` pointing to the encrypted init segment.

**DRM systems override:** Explicitly select which DRM systems to include in output (`drm_systems: ["widevine", "playready", "fairplay", "clearkey"]`). Overrides the default per-scheme DRM system selection.

### Low-Latency Streaming (Phase 9)

**LL-HLS:** Low-Latency HLS with partial segments (parts). The pipeline detects CMAF chunk boundaries (moof+mdat pairs) in rewritten segments and extracts them as parts. Source LL-HLS tags are parsed (`#EXT-X-PART-INF`, `#EXT-X-PART`, `#EXT-X-SERVER-CONTROL`, `#EXT-X-PRELOAD-HINT`) and threaded through to output manifests. HLS version is bumped to 9 when parts are present.

**LL-DASH:** Low-Latency DASH with `availabilityTimeOffset` and `availabilityTimeComplete="false"` on `<SegmentTemplate>`. Source LL-DASH attributes are parsed from input MPDs and carried through to output.

**Key types:** `PartInfo` (segment_number, part_index, duration, independent, uri, byte_size), `ServerControl` (can_skip_until, hold_back, part_hold_back, can_block_reload), `LowLatencyDashInfo` (availability_time_offset, availability_time_complete).

**Chunk detection:** `detect_chunk_boundaries()` in `media/chunk.rs` finds moof+mdat pairs within a segment. `is_independent_chunk()` checks trun `first_sample_flags` for sync/IDR samples. Chunks are extracted as byte ranges and stored as parts.

### Trick Play & I-Frame Playlists (Phase 12)

**Opt-in:** Enabled via `enable_iframe_playlist: bool` on `RepackageRequest` and `WebhookPayload` (default false). When enabled, the pipeline detects I-frame byte ranges in rewritten segments and generates trick play manifests for fast-forward/rewind scrubbing.

**I-frame detection:** Reuses existing `chunk.rs` infrastructure. After segment rewriting, `detect_chunk_boundaries()` finds moof+mdat pairs. The first independent (IDR) chunk's byte offset and size are recorded as an `IFrameSegmentInfo`. CMAF segments always start with an IDR frame, so every segment contributes one I-frame entry. Chunk detection is consolidated — runs once per segment when either LL-HLS parts or I-frame playlists need it.

**HLS I-frame playlists:** `render_iframe_playlist()` in `manifest/hls.rs` produces `#EXT-X-I-FRAMES-ONLY` playlists with `#EXT-X-VERSION:4` (required for BYTERANGE), `#EXT-X-BYTERANGE:length@offset` pointing into existing segment files (no duplicate storage), DRM KEY tags, and init MAP. The master playlist includes `#EXT-X-I-FRAME-STREAM-INF` entries per video variant (bandwidth/10, codecs, resolution, `URI="iframes"`).

**DASH trick play:** `render()` in `manifest/dash.rs` adds a separate `<AdaptationSet>` with `<EssentialProperty schemeIdUri="http://dashif.org/guidelines/trickmode" value="1"/>` referencing the main video AdaptationSet by `id="1"`. Trick play Representations use `_trick` suffix and bandwidth/10.

**Dedicated route:** `GET /repackage/{id}/{fmt}/iframes` serves the HLS I-frame playlist. For DASH, trick play is embedded in the regular MPD — the iframes endpoint returns 404. The route is placed before the catch-all segment route to prevent wildcard matching.

**Key types:** `IFrameSegmentInfo` (segment_number, byte_offset, byte_length, duration, segment_uri). `ManifestState` extended with `iframe_segments: Vec<IFrameSegmentInfo>` and `enable_iframe_playlist: bool` (both `#[serde(default)]` for backward compat).

### DVR Sliding Window (Phase 13)

**Configurable window:** Enabled via `dvr_window_duration: Option<f64>` on `RepackageRequest`, `WebhookPayload`, and `ManifestState`. When set, only the most recent N seconds of segments are rendered in live manifests. Older segments remain accessible by direct URL — they are not pruned from `ManifestState`.

**Filter-during-rendering:** Segments are filtered at render time, not removed from state. This preserves full segment history for live-to-VOD transitions (Complete phase renders all segments regardless of window). Trade-off: ManifestState grows with stream length (~1.5 MB for 24h at 6s segments — acceptable for Redis).

**Windowing helpers** on `ManifestState`:
- `windowed_segments()` — returns slice of segments within the DVR window from live edge
- `windowed_media_sequence()` — first segment number in the window (for HLS `#EXT-X-MEDIA-SEQUENCE`)
- `windowed_iframe_segments()` — filters I-frame segments by windowed segment numbers
- `windowed_parts()` — filters LL-HLS parts by windowed segment numbers
- `windowed_ad_breaks()` — filters SCTE-35 ad breaks by windowed segment numbers
- `is_dvr_active()` — true when window is set and phase is Live

**HLS behavior:** When DVR active, omits `#EXT-X-PLAYLIST-TYPE:EVENT` (allows segments to slide out of window). Without DVR, keeps `EVENT`. Complete phase stays `VOD`. Media sequence and segment list use windowed values.

**DASH behavior:** Adds `timeShiftBufferDepth` attribute (ISO 8601 duration) to MPD element when DVR active. `startNumber` in `<SegmentTemplate>` is dynamic (first windowed segment number). `<SegmentTimeline>` only includes windowed entries. Complete phase omits `timeShiftBufferDepth` and renders all segments.

### Content Steering (Phase 14)

**Content steering** allows a steering server to dynamically direct players between CDNs or content pathways at runtime. The player periodically queries a steering server URL, which returns JSON with pathway priorities.

**Core type:** `ContentSteeringConfig` in `manifest/types.rs` — `server_uri: String`, `default_pathway_id: Option<String>`, `query_before_start: Option<bool>`. Fields on both `ManifestState` and `SourceManifest` (both `#[serde(default)]` for backward compat).

**HLS output:** `#EXT-X-CONTENT-STEERING:SERVER-URI="...",PATHWAY-ID="..."` tag in master playlists only (after `#EXT-X-INDEPENDENT-SEGMENTS`, before `#EXT-X-SESSION-KEY`). Media playlists never contain steering tags.

**DASH output:** `<ContentSteering proxyServerURL="..." defaultServiceLocation="..." queryBeforeStart="..."/>` element in MPD (after `minBufferTime>` close, before `<Period>`).

**DASH source pass-through:** `dash_input.rs` parser extracts `<ContentSteering>` elements from source MPDs into `SourceManifest.content_steering`. HLS input parser does not extract steering (media playlists don't contain it).

**Override priority:** Webhook `content_steering` config takes precedence over source-extracted steering: `request.content_steering.clone().or_else(|| source.content_steering.clone())`.

**Webhook input:** `ContentSteeringInput` struct on `WebhookPayload` with validation (reject empty `server_uri`). Converted to `ContentSteeringConfig` before threading into pipeline.

### MPEG-TS Input (Phase 10)

**Feature-gated:** All TS code is behind `#[cfg(feature = "ts")]` — zero binary impact on non-TS builds.

**TS demuxer** (`media/ts.rs`): Parses 188-byte TS packets, PAT/PMT tables for stream discovery, and reassembles PES packets. The `TsDemuxer` is a stateful accumulator that produces `DemuxedSegment` with separated video and audio PES data. Supports H.264 video and AAC audio codec detection from PMT stream types.

**Transmuxer** (`media/transmux.rs`): Converts TS elementary streams to CMAF. For video: extracts H.264 NAL units from Annex B byte streams, parses SPS for resolution/profile, converts to AVCC format, and builds avcC config boxes. For audio: parses ADTS headers for AAC config and builds esds boxes. `synthesize_init_segment()` creates ftyp+moov from codec config. `transmux_to_cmaf()` creates moof+mdat fragments.

**AES-128 decryption:** `decrypt_ts_segment()` handles whole-segment AES-128-CBC decryption (as used by HLS-TS with `#EXT-X-KEY:METHOD=AES-128`), reusing the existing `aes`/`cbc` crates.

**HLS-TS detection:** The HLS input parser detects TS sources by `.ts` segment extension, parses `#EXT-X-KEY:METHOD=AES-128` with URI and IV, and relaxes the `#EXT-X-MAP` requirement (TS sources don't have init segments — they're synthesized by the transmuxer).

### Dual-Format Output (Phase 21)

**Core insight:** CMAF/fMP4 segments are format-agnostic — the same encrypted bytes serve both HLS and DASH. Only manifests differ between formats.

**`RepackageRequest.output_formats: Vec<OutputFormat>`:** Replaces the old `output_format` (singular). Webhook API accepts `output_formats: ["hls", "dash"]` for dual-format output; legacy `format` field still accepted for backward compat. `resolved_output_formats()` mirrors the `resolved_target_schemes()` pattern. `primary_format()` returns the first format (fallback: `Hls`).

**Format-agnostic segment caching:** Init and media segments are cached with scheme-only keys (`ep:{id}:{scheme}:init`, `ep:{id}:{scheme}:seg:{n}`) — no format prefix. Manifest state remains per-(format, scheme) since HLS M3U8 and DASH MPD have entirely different structures.

**Pipeline execution:** `execute()` returns `Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>` — one output per (format, scheme) pair. Re-encryption runs once per scheme, then results are distributed to all format outputs. `execute_first()`/`execute_remaining()` store `target_formats` alongside `target_schemes` for continuation chaining.

**Backward compatibility:** Request handlers try format-agnostic cache keys first, then fall back to legacy format-qualified keys (`ep:{id}:{format}_{scheme}:init`) for content cached before Phase 21.

**Combinatorial output:** `output_formats: [Hls, Dash]` × `target_schemes: [Cenc, Cbcs]` = 4 outputs (HLS+CENC, HLS+CBCS, DASH+CENC, DASH+CBCS).

## Error Handling

All modules use `crate::error::Result<T>` which aliases `std::result::Result<T, EdgepackError>`. The `EdgepackError` enum has specific variants for each subsystem (Cache, Drm, Speke, Cpix, Encryption, MediaParse, SegmentRewrite, Manifest, Http, Config, InvalidInput, NotFound, Io). Use `thiserror` derive macros. Propagation is via `?` operator throughout.

## Runtime Implementation

All HTTP transport and request handling is fully implemented:

1. **`http_client.rs`**: Shared HTTP client (GET, POST, PUT, DELETE) using `wasi:http/outgoing-handler` (wasm32) with native stub error (non-wasm32, preserves test builds).
2. **`wasi_handler.rs`**: WASI incoming handler bridge implementing `wasi:http/incoming-handler::Guest`. Converts WASI types ↔ library types and maps errors to HTTP status codes.
3. **`cache/redis_http.rs` → `execute_command()`**: Uses `http_client::get()` to make Upstash REST API calls. Parses JSON responses via extracted `parse_upstash_response()`.
4. **`drm/speke.rs` → `post_cpix()`**: Uses `http_client::post()` to POST CPIX XML to license server with auth headers.
5. **`repackager/pipeline.rs`**: `fetch_source_manifest()` auto-detects HLS vs DASH and parses. `fetch_segment()` fetches binary data. Two execution modes: `execute()` processes all segments synchronously and returns `(JobStatus, Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>)` with per-(format, scheme) output data in memory (used by sandbox). `execute_first()` + `execute_remaining()` is the split execution model for WASI — caches format-agnostic init/media segments and per-(format, scheme) manifest state in Redis for serving via GET handlers, with self-invocation chaining. Both modes decrypt source segments once and re-encrypt for each target scheme, then distribute to all output formats.
6. **`manifest/hls_input.rs` + `dash_input.rs`**: Source manifest input parsers extracting segment URLs, durations, init segment references, and live/VOD detection.
7. **`handler/request.rs`**: All four GET handlers query Redis for cached segment data and manifest state via `HandlerContext`.
8. **`handler/webhook.rs`**: Creates pipeline, calls `execute_first()`, fires self-invocation to `/webhook/repackage/continue`, returns 200 after first manifest publishes. Continue handler chains remaining segment processing.

## Local Sandbox

The `sandbox` feature enables a native binary (`src/bin/sandbox.rs`) that reuses the production `RepackagePipeline` with native HTTP transport and an in-memory cache. The sandbox calls `pipeline.execute()` which processes all segments synchronously and returns `(JobStatus, Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>)` — per-(format, scheme) output is written to disk directly from each `ProgressiveOutput` object to `sandbox/output/{content_id}/{format}_{scheme}/`, not round-tripped through cache.

### Architecture

- **`http_client.rs`** has a three-way `#[cfg]` dispatch: `wasm32` → WASI HTTP, `sandbox` feature → `reqwest::blocking`, neither → stub error
- **`cache/memory.rs`** implements `CacheBackend` using `Arc<RwLock<HashMap>>` (shared between pipeline thread and API server; used for job state and DRM key caching, not for media output)
- **`src/bin/sandbox.rs`** is a single-file Axum server with embedded HTML/CSS/JS UI

### Feature Gate

```toml
[features]
jit = []                  # Phase 8: JIT on-demand packaging
cloudflare = []           # Phase 17: Cloudflare Workers KV cache backend
ts = []                   # Phase 10: MPEG-TS input demuxer + transmuxer
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
| `criterion` | 0.5 | Benchmark framework for JIT latency measurement (dev-dependency only) |

URL parsing uses a lightweight built-in module (`src/url.rs`) instead of the `url` crate, saving ~200 KB of ICU/IDNA Unicode tables in the WASM binary. Core crates are chosen for WASM compatibility (no system dependencies, no async runtime requirements). Sandbox crates are gated behind `cfg(not(target_arch = "wasm32"))` and never appear in the WASM build.

## Tests

The project has **1,436 tests** total (with `--features jit,cloudflare`): 924 unit tests and 512 integration tests. With `--features jit,cloudflare,ts`: **1,515 tests** (971 unit + 544 integration). Without optional features: **1,379 tests**. All run on the native host target.

#### WASM Binary Size Guards

Per-feature binary size tests in `tests/wasm_binary_size.rs` prevent dependency bloat for each build variant:

| Test | Features | Limit | Current Size | Functions |
|------|----------|-------|-------------|-----------|
| `wasm_base_binary_size` | none | 720 KB | ~692 KB | ~2,069 |
| `wasm_jit_binary_size` | `jit` | 750 KB | ~725 KB | ~2,030 |
| `wasm_full_binary_size` | `jit,cloudflare` | 750 KB | ~730 KB | ~2,033 |

JIT adds ~33 KB (60 functions) over base. Cloudflare adds only ~5 KB (11 functions). Binary size is the primary cold start proxy — WASM instantiation time is proportional to module size and function count. Function counts are reported via `wasm-tools objdump` if installed (informational, not enforced).

### Unit Tests (971 with all features incl. ts)

Inlined as `#[cfg(test)] mod tests` blocks in every source file. They cover:

- **Serde roundtrips** for all serializable types (config, manifest state, job status, DRM keys, webhook payloads, encryption schemes, container formats, continuation params)
- **Encryption scheme abstraction**: `EncryptionScheme` enum (serde roundtrips, scheme_type_bytes, from_scheme_type, HLS method strings, default IV sizes, default patterns, FairPlay support flags, `is_encrypted()` for None variant), `SampleDecryptor`/`SampleEncryptor` trait dispatch via factory functions
- **Container format abstraction**: `ContainerFormat` enum with three variants (Cmaf, Fmp4, Iso) — extensions, brands, ftyp box building, DASH profile strings, serde roundtrips, display, from_str_value parsing
- **Encryption correctness**: CBCS decrypt + encrypt, CENC encrypt + decrypt, scheme-agnostic roundtrips through factory functions
- **ISOBMFF box parsing**: Building binary boxes, parsing them back, verifying headers, payloads, and child iteration
- **Init segment rewriting**: Scheme-parameterized `schm`/`tenc`/`pssh` rewriting (CBCS and CENC targets, tenc pattern encoding, PSSH filtering per scheme, per-track KID assignment via TrackKeyMapping, multi-KID PSSH v1 generation), ftyp brand rewriting per container format (CMAF includes `cmfc`, fMP4 does not), clear→encrypted sinf injection (`create_protection_info`), encrypted→clear sinf stripping (`strip_protection_info`), clear→clear ftyp-only rewrite (`rewrite_ftyp_only`)
- **Codec string extraction**: RFC 6381 codec strings from stsd config boxes (avcC, hvcC, esds, vpcC, av1C, wvtt, stpp), track metadata parsing (hdlr handler type, mdhd timescale + language, tkhd track_id, sinf/tenc default_kid), TrackKeyMapping construction and serde roundtrips
- **Segment rewriting**: Four-way dispatch (encrypted↔encrypted, clear→encrypted, encrypted→clear, clear→clear pass-through), scheme-aware decrypt/re-encrypt with optional source/target keys
- **Manifest rendering**: HLS M3U8 and DASH MPD output for every lifecycle phase, dynamic DRM scheme signaling (SAMPLE-AES/SAMPLE-AES-CTR for HLS, cbcs/cenc value for DASH), FairPlay key URI rendering, subtitle rendition groups (HLS `TYPE=SUBTITLES`, DASH text AdaptationSet), CEA-608/708 closed caption signaling (HLS `TYPE=CLOSED-CAPTIONS` with `INSTREAM-ID`, DASH `Accessibility` descriptors), I-frame playlist rendering (HLS `#EXT-X-I-FRAMES-ONLY` with BYTERANGE), master playlist I-frame stream signaling (`#EXT-X-I-FRAME-STREAM-INF`), DASH trick play AdaptationSet with EssentialProperty trickmode, DVR sliding window (windowed segments/media sequence/playlist type for HLS, timeShiftBufferDepth/startNumber for DASH, live-to-VOD transitions)
- **Source manifest parsing**: HLS M3U8 and DASH MPD input parsing including source scheme detection from `#EXT-X-KEY` METHOD and `<ContentProtection>` elements, `#EXT-X-DATERANGE` SCTE-35 ad break extraction, DASH `<EventStream>` SCTE-35 event parsing
- **SCTE-35 parsing**: `emsg` box parsing (v0/v1), SCTE-35 splice_info_section binary parsing (splice_insert, time_signal), scheme URI detection, emsg builder roundtrips
- **Compatibility validation**: Codec/scheme compatibility checks (VP9+CBCS error, HEVC+CENC warning, etc.), HDR format detection (HDR10, Dolby Vision, HLG), init/media segment structure validation, repackage request pre-flight validation
- **Progressive output state machine**: Phase transitions, cache-control header generation, dynamic segment URI formatting per container format
- **Pipeline DRM info**: Manifest DRM info building with CBCS/CENC target scheme (incl. multi-KID PSSH per system), FairPlay inclusion/exclusion, container format threading through ContinuationParams, TrackKeyMapping construction and serialization, variant building from track metadata
- **URL parsing**: Lightweight URL parser (parse, join, component access, serde roundtrips, authority extraction, relative path resolution)
- **HTTP routing**: Path parsing, format validation, segment number extraction (all 7 CMAF/ISOBMFF extensions: .cmfv, .cmfa, .cmft, .cmfm, .m4s, .mp4, .m4a), all route dispatching
- **Webhook validation**: Valid/invalid JSON, missing fields, bad formats, empty URLs, target_scheme/target_schemes parsing (cenc/cbcs/none, backward compat, duplicate rejection), output_formats parsing (hls/dash, backward compat, duplicate rejection), container_format parsing (cmaf/fmp4/iso), enable_iframe_playlist parsing, invalid scheme/format rejection, serde roundtrips
- **Error variants**: Display output for every EdgepackError variant

To run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::cbcs`

### Integration Tests (544 with all features incl. ts)

Located in the `tests/` directory. These exercise cross-module workflows using synthetic CMAF fixtures with no external dependencies:

```
tests/
├── common/
│   └── mod.rs                 Shared fixtures: synthetic ISOBMFF builders, test keys, DRM key sets, manifest states
├── cdn_adapters.rs            18+ tests: backend type selection, config serde, create_backend factory, encryption token derivation
├── clear_content.rs           10 tests: clear→CENC/CBCS, encrypted→clear, clear→clear (init + segment), roundtrips
├── dual_format.rs             25 tests: multi-format output, format-agnostic cache keys, dual-format manifests, webhook output_formats, serde roundtrips
├── dual_scheme.rs             22 tests: scheme-qualified routing, cache keys, webhook multi-scheme parsing, backward compat
├── encryption_roundtrip.rs    8 tests: CBCS→plaintext→CENC full pipeline
├── isobmff_integration.rs    18 tests: init/media segment parsing, rewriting (scheme + container format aware), PSSH/senc roundtrips
├── jit_packaging.rs           27 tests: JIT source config, on-demand setup, lock contention, backward compat (jit feature)
├── manifest_integration.rs   23 tests: progressive output lifecycle, DRM signaling, cache headers, ISO BMFF format
├── handler_integration.rs    32 tests: HTTP routing (all 7 CMAF/ISOBMFF segment extensions), webhook validation, response helpers
├── multi_key.rs              12 tests: per-track tenc, multi-KID PSSH, single-key backward compat, codec extraction, TrackKeyMapping serde, create→strip roundtrip
├── conformance.rs            23 tests: init/media segment structure validation, roundtrip conformance, manifest conformance
├── scte35_integration.rs     13 tests: emsg extraction, SCTE-35 parsing, HLS/DASH ad rendering, source manifest roundtrip, serde
├── advanced_drm.rs           15 tests: ClearKey, raw key mode, key rotation, clear lead, DRM systems override
├── ll_hls_dash.rs            16 tests: chunk detection, LL-HLS/LL-DASH parsing+rendering, progressive parts, serde
├── trick_play.rs             27 tests: HLS I-frame playlist (BYTERANGE, DRM, endlist, disabled), master I-frame stream, DASH trick play, serde compat, container formats, route handling
├── dvr_window.rs             25 tests: HLS DVR window (sliding window, media sequence, playlist type, DRM, iframes, ad breaks), DASH DVR (timeShiftBufferDepth, startNumber, windowed segments), live-to-VOD, serde compat, container formats
├── content_steering.rs       20 tests: HLS master steering tag (full, URI-only, position, backward compat), DASH steering element (full, proxy-only, qbs, position), DASH input parsing (full, minimal, backward compat), serde roundtrips, override priority
├── cache_control.rs          43 tests: system defaults (HLS/DASH, all phases), per-request overrides (live/final/segment max-age, s-maxage split, immutable toggle), safety invariants, progressive output integration (HLS + DASH), backward compat, DVR + cache control, container format + cache control, system CacheConfig overrides, DASH per-request overrides, segment handler design documentation, JIT cache_control:None documentation
├── e2e.rs                   105 tests: full pipeline E2E — encryption transforms ×2 formats (18), container×format×encryption matrix (18), feature combinations incl. DVR+iframes+DRM+steering+dual-format (30), lifecycle phase transitions (18), edge cases & boundary conditions (21)
├── ts_integration.rs         30 tests: TS demux, transmux, AES-128, HLS TS detection, full pipeline (ts feature)
├── output_integrity.rs       21 tests: segment structure validation, encrypt-decrypt roundtrip, I-frame BYTERANGE, init rewrite roundtrip, multi-KID PSSH, manifest roundtrips (HLS/DASH, live, DVR, I-frame), cache-control body invariants
└── wasm_binary_size.rs        5 tests: per-feature WASM binary size guards (base, jit, full, ts, full+ts)
```

**Key fixtures in `tests/common/mod.rs`:**
- `build_cbcs_init_segment()` — builds a synthetic CBCS init segment (ftyp + moov with stsd→encv→sinf→frma/schm/schi/tenc + pssh)
- `build_cbcs_media_segment(sample_count, sample_size)` — builds a CBCS-encrypted moof+mdat with configurable samples; returns `(segment_bytes, plaintext_samples)` for verification
- `build_clear_init_segment()` — builds a synthetic clear init segment (ftyp + moov with stsd→avc1, no sinf, no PSSH)
- `build_clear_media_segment(sample_count, sample_size)` — builds a clear moof+mdat (trun, no senc) with plaintext samples
- `make_drm_key_set()` / `make_drm_key_set_with_fairplay()` — builds DrmKeySet with system-specific PSSH data
- `make_hls_manifest_state()` / `make_dash_manifest_state()` — builds ManifestState with DRM info and segments
- `make_hls_iframe_manifest_state()` / `make_dash_iframe_manifest_state()` — builds ManifestState with DRM info, segments, and I-frame segment info (enable_iframe_playlist=true)
- `make_hls_dvr_manifest_state()` / `make_dash_dvr_manifest_state()` — builds ManifestState with DVR window duration and exact 6.0s segment durations for precise windowing math
- `build_cenc_init_segment()` — builds a synthetic CENC init segment (schm=cenc, 8-byte IV, 0:0 pattern)
- `build_cenc_media_segment(sample_count, sample_size, key, iv_size)` — builds a CENC-encrypted media segment using AES-128-CTR
- `make_manifest_state_with_container(format, container, segment_count, phase)` — generic ManifestState builder for any OutputFormat/ContainerFormat combination
- `assert_valid_hls(manifest, expected_segments)` / `assert_valid_dash(manifest, expected_segments)` — structural validation + parse roundtrip helpers
- `full_segment_rewrite(source, source_scheme, target_scheme, source_key, target_key)` — convenience wrapper for `rewrite_segment()` with auto IV size/pattern
- `full_init_rewrite(source, source_scheme, target_scheme, key_set, container)` — 4-way init rewrite dispatcher
- `assert_valid_segment_structure(segment, expected_samples, expect_senc)` — moof/mdat structure + trun/senc validation
- Test constants: `TEST_SOURCE_KEY`, `TEST_TARGET_KEY`, `TEST_KID`, `TEST_IV` (all `[u8; 16]`)

To run only integration tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test '*'`

To run a specific suite: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test encryption_roundtrip`

### Test Guidelines

When adding new functionality, follow the existing pattern:
- **Unit tests**: Add `#[cfg(test)] mod tests { ... }` at the bottom of the source file, import `use super::*;`, and create small focused test functions with descriptive names.
- **Integration tests**: For cross-module workflows, add tests to the appropriate file in `tests/` or create a new file. Use shared fixtures from `tests/common/mod.rs`. Add `mod common;` at the top of each integration test file.
- **Output integrity tests** (mandatory for merges): `tests/output_integrity.rs` validates structural correctness of every input/output lane. When adding new encryption paths, container format support, or manifest features, add corresponding integrity tests **before merging**. Every new output path must have at least one integrity test proving it produces spec-compliant output.
- **Conformance tests**: `tests/conformance.rs` validates init/media segment structure and manifest conformance. Extend these when modifying box structure or manifest rendering.
- **Performance regression**: Run `cargo bench` before and after changes that touch hot paths (segment rewriting, init rewriting, manifest rendering/parsing). Note any regressions in the PR description.
- **Binary size checks**: `tests/wasm_binary_size.rs` enforces per-feature WASM binary size limits. New dependencies must not push the binary past these limits.
- **ISOBMFF parsing in tests**: When calling `parse_trun`/`parse_senc`/`parse_pssh`, pass the box **payload** (after the box header), not the full box including the header. The `header_size` field on `BoxHeader` is `u8`.

### Benchmarks

Criterion benchmarks in `benches/jit_latency.rs` measure JIT-critical latencies:

```bash
# Run all benchmarks
cargo bench --target $(rustc -vV | grep host | awk '{print $2}')

# Run a specific benchmark group
cargo bench --target $(rustc -vV | grep host | awk '{print $2}') --bench jit_latency -- segment_rewrite
```

| Benchmark Group | What's Measured |
|----------------|-----------------|
| `segment_rewrite` | Segment re-encryption: CBCS→CENC, clear→CENC, passthrough (4/32/128 samples × 1KB) |
| `init_rewrite` | Init segment DRM scheme transform: CBCS→CENC, clear→CENC |
| `manifest_render` | HLS/DASH manifest generation (10/50/200 segments), HLS I-frame (50 segments), HLS live (6 segments) |
| `manifest_parse` | HLS/DASH manifest input parsing (50 segments) |

Benchmarks use synthetic fixtures from the bench file (not from `tests/common/mod.rs`). They run on native targets — WASM performance is proportional but not identical.

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
| GET | `/repackage/{id}/{format}/iframes` | `request::handle_iframe_manifest_request` | Serve HLS I-frame playlist (DASH returns 404 — trick play embedded in MPD) |
| GET | `/repackage/{id}/{format}/segment_{n}.{ext}` | `request::handle_media_segment_request` | Serve repackaged media segment (accepts all 7 CMAF/ISOBMFF extensions) |
| POST | `/webhook/repackage` | `webhook::handle_repackage_webhook` | Trigger proactive repackaging (returns 200 after first manifest) |
| POST | `/webhook/repackage/continue` | `webhook::handle_continue` | Internal self-invocation to process remaining segments |
| GET | `/status/{id}/{format}` | `request::handle_status_request` | Query job progress |

`{format}` is a plain format (`hls`, `dash`) or a scheme-qualified format (`hls_cenc`, `hls_cbcs`, `dash_cenc`, `dash_cbcs`, `hls_none`, `dash_none`). Scheme-qualified routes are produced by dual-scheme requests; plain routes still work for backward compatibility (single-scheme requests).

## Environment Variables

### Cache Store

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `STORE_URL` | Yes* | — | Cache store endpoint URL |
| `STORE_TOKEN` | Yes* | — | Cache store auth token |
| `CACHE_BACKEND` | No | `redis_http` | Backend type: `redis_http`, `redis_tcp`, `cloudflare_kv`, `http_kv` |
| `CACHE_ENCRYPTION_TOKEN` | No | `STORE_TOKEN` | Token for cache encryption key derivation |
| `CACHE_MAX_AGE_SEGMENTS` | No | `31536000` | Default max-age for segments and init segments (1 year) |
| `CACHE_MAX_AGE_MANIFEST_LIVE` | No | `1` | Default max-age for live manifests |
| `CACHE_MAX_AGE_MANIFEST_FINAL` | No | `31536000` | Default max-age for finalized/VOD manifests |
| `REDIS_URL` | Yes* | — | Redis endpoint (backward compat alias for `STORE_URL`) |
| `REDIS_TOKEN` | Yes* | — | Redis auth token (backward compat alias for `STORE_TOKEN`) |
| `REDIS_BACKEND` | No | `http` | Legacy backend type: `http` or `tcp` (overridden by `CACHE_BACKEND`) |

*`STORE_URL`/`STORE_TOKEN` fall back to `REDIS_URL`/`REDIS_TOKEN` for backward compatibility.

### Cloudflare Workers KV (requires `cloudflare` feature + `CACHE_BACKEND=cloudflare_kv`)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CF_ACCOUNT_ID` | Yes | — | Cloudflare account ID |
| `CF_KV_NAMESPACE_ID` | Yes | — | Workers KV namespace ID |
| `CF_API_TOKEN` | Yes | — | Cloudflare API token with KV permissions |
| `CF_API_BASE_URL` | No | `https://api.cloudflare.com/client/v4` | API base URL |

### Generic HTTP KV (requires `CACHE_BACKEND=http_kv`)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `HTTP_KV_BASE_URL` | Yes | — | KV API base URL (e.g. `https://xxx.execute-api.us-east-1.amazonaws.com/prod`) |
| `HTTP_KV_AUTH_HEADER` | No | `Authorization` | Auth header name (e.g. `x-api-key`) |
| `HTTP_KV_AUTH_VALUE` | Yes | — | Auth header value (e.g. `Bearer xxx` or API key) |

### DRM / SPEKE

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SPEKE_URL` | Yes | — | SPEKE 2.0 license server endpoint |
| `SPEKE_BEARER_TOKEN` | One of three | — | Bearer token auth |
| `SPEKE_API_KEY` | One of three | — | API key auth (pair with `SPEKE_API_KEY_HEADER`) |
| `SPEKE_API_KEY_HEADER` | No | `x-api-key` | Header name for API key |
| `SPEKE_USERNAME` | One of three | — | Basic auth username |
| `SPEKE_PASSWORD` | One of three | — | Basic auth password |

### JIT Packaging (requires `jit` feature)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `JIT_ENABLED` | No | `false` | Enable JIT on-demand packaging |
| `JIT_SOURCE_URL_PATTERN` | No | — | URL template with `{content_id}` placeholder |
| `JIT_DEFAULT_TARGET_SCHEME` | No | `cenc` | Default scheme: `cenc` or `cbcs` |
| `JIT_DEFAULT_CONTAINER_FORMAT` | No | `cmaf` | Default format: `cmaf` or `fmp4` |
| `JIT_LOCK_TTL` | No | `30` | Processing lock TTL in seconds |

## Redis Key Schema

Keys marked with † are only written by the split execution path (`execute_first()`/`execute_remaining()`). The `execute()` path (sandbox) keeps output in memory via `ProgressiveOutput` and does not cache media data in Redis. Keys marked with ‡ are scheme-qualified (one key per target scheme, using `{format}_{scheme}` e.g. `hls_cenc`). Keys marked with § are format-agnostic (Phase 21 — shared across HLS/DASH).

| Key Pattern | TTL | Content |
|-------------|-----|---------|
| `ep:{content_id}:keys` | 24h | Serialized DRM content keys (JSON) |
| `ep:{content_id}:{format}:state` | 48h | JobStatus JSON (state, progress) |
| `ep:{content_id}:{format}_{scheme}:manifest_state` †‡ | 48h | ManifestState JSON (segments, phase) |
| `ep:{content_id}:{scheme}:init` †§ | 48h | Rewritten init segment binary data (format-agnostic) |
| `ep:{content_id}:{scheme}:seg:{n}` †§ | 48h | Rewritten media segment binary data (format-agnostic) |
| `ep:{content_id}:{format}_{scheme}:init` †‡ | 48h | Legacy format-qualified init segment (pre-Phase 21, fallback) |
| `ep:{content_id}:{format}_{scheme}:seg:{n}` †‡ | 48h | Legacy format-qualified media segment (pre-Phase 21, fallback) |
| `ep:{content_id}:{format}:source` † | 48h | Source manifest metadata (segment URLs, durations, is_live) |
| `ep:{content_id}:{format}_{scheme}:rewrite_params` †‡ | 48h | Continuation parameters (encryption keys, IV sizes, pattern) |
| `ep:{content_id}:{format}:target_schemes` † | 48h | Target schemes list (JSON array of EncryptionScheme) |
| `ep:{content_id}:target_formats` † | 48h | Target formats list (JSON array of OutputFormat) |
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
- **Event**: `emsg`
- **Top-level**: `ftyp`

## DRM System IDs

| System | UUID | Constant |
|--------|------|----------|
| Widevine | `edef8ba9-79d6-4ace-a3c8-27dcd51d21ed` | `drm::system_ids::WIDEVINE` |
| PlayReady | `9a04f079-9840-4286-ab92-e65be0885f95` | `drm::system_ids::PLAYREADY` |
| FairPlay | `94ce86fb-07ff-4f43-adb8-93d2fa968ca2` | `drm::system_ids::FAIRPLAY` |
| ClearKey | `e2719d58-a985-b3c9-781a-b030af78d30e` | `drm::system_ids::CLEARKEY` |

FairPlay is recognised in both input and output. For CENC target output, FairPlay PSSH boxes are excluded (FairPlay does not support CENC). For CBCS target output, FairPlay PSSH boxes are included alongside Widevine and PlayReady.

ClearKey is used for testing and development — its PSSH data is built locally (JSON format with base64url-encoded KIDs) without requiring a SPEKE license server call.

## Refactoring Roadmap

See [`docs/roadmap.md`](docs/roadmap.md) for the full roadmap. Phases 1–14, 16, 17, 19, and 21 are complete (all P0/P1 done). Remaining P2 phases: 18 (Binary Size Monitoring), 20 (Multi-Source Manifest Merging). P3: 22 (TS Segment Output), 23 (MoQ Ingest — feature-gated, requires research).
