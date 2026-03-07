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
| 12 | Trick Play & I-Frame Playlists | ✅ |
| 13 | DVR Window & Time-Shift | ✅ |
| 14 | Content Steering & CDN Optimization | ✅ |
| 16 | Compatibility Validation & Hardening | ✅ |
| 17 | CDN Provider Adapters & Binary Optimization | ✅ |
| 19 | Configurable Cache-Control Headers | ✅ |
| 21 | Generic HLS/DASH Pipeline (Dual-Format) | ✅ |
| 22 | TS Segment Output (feature-gated) | ✅ |

# Refactoring Roadmap

The codebase is being generalized from a single-purpose CBCS→CENC converter into a generic lightweight edge repackager. Phases 1–14, 16, 17, 19, 21, and 22 are complete. All P0 and P1 items are done. Remaining phases:

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

### ~~Phase 6: Subtitle & Text Track Pass-Through~~ ✅ Complete
- WebVTT (`wvtt`) and TTML (`stpp`) sample entry pass-through in fMP4 (subtitles bypass encryption via `encrypted_sample_entry_type()` returning `None`)
- `TrackMediaType::Subtitle` enum variant, `language` field on `VariantInfo` and `TrackInfo`
- ISO 639-2/T language extraction from `mdhd` box (packed 3×5-bit chars)
- Explicit `wvtt`/`stpp` codec string detection in `extract_codec_string()`
- `CeaCaptionInfo` struct for CEA-608/708 manifest signaling (pass-through is automatic in video SEI NALs)
- HLS subtitle rendition groups (`#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"`) with `SUBTITLES="subs"` on `EXT-X-STREAM-INF`
- HLS CEA caption signaling (`#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,INSTREAM-ID=...`) with `CLOSED-CAPTIONS="cc"` on `EXT-X-STREAM-INF`
- DASH subtitle `<AdaptationSet contentType="text">` with language attribute
- DASH CEA `<Accessibility schemeIdUri="urn:scte:dash:cc:cea-608:2015">` descriptors inside video AdaptationSet
- Result: 825 tests total with `--features jit,cloudflare` (18 new subtitle/caption tests)

### ~~Phase 7: SCTE-35 Ad Markers & Multi-Period DASH~~ ✅ Complete
- `emsg` box parsing (v0/v1) with SCTE-35 splice_info_section binary parser (`src/media/scte35.rs`)
- `EmsgBox` type with builder/parser in `cmaf.rs`, `extract_emsg_boxes()` in `segment.rs`
- `AdBreakInfo` type in `manifest/types.rs`, threaded through pipeline → ProgressiveOutput → ManifestState
- HLS ad markers via `#EXT-X-DATERANGE` with `SCTE35-CMD` hex encoding
- DASH `<EventStream>` with SCTE-35 scheme URI and `<Event>` elements
- Source manifest parsing: `#EXT-X-DATERANGE` in HLS input, `<EventStream>` in DASH input
- New: `src/media/scte35.rs`, `tests/scte35_integration.rs` (13 integration tests)
- Result: 948 tests total with `--features jit,cloudflare`

### ~~Phase 8: JIT Packaging (On-Demand GET)~~ ✅ Complete
- Manifest-on-GET, Init-on-GET, Segment-on-GET (lazy repackaging on cache miss)
- Request coalescing via `set_nx` distributed locking with configurable TTL
- `POST /config/source` endpoint for per-content source configuration
- URL pattern-based source resolution with `{content_id}` placeholder
- JIT is always enabled (no feature flag required)
- Result: 762 tests total (27 new JIT integration tests)

### ~~Phase 9: LL-HLS & LL-DASH~~ ✅ Complete
- LL-HLS (`#EXT-X-PART`, `#EXT-X-PART-INF`, `#EXT-X-SERVER-CONTROL`, `#EXT-X-PRELOAD-HINT`)
- LL-DASH `availabilityTimeOffset` and `availabilityTimeComplete` on SegmentTemplate
- CMAF chunk boundary detection (`src/media/chunk.rs`) for partial segment extraction
- New types: `PartInfo`, `ServerControl`, `LowLatencyDashInfo`, `SourcePartInfo`
- Progressive output part support (`add_part`, `part_data`, LL setters)
- HLS version bump to 9 when LL-HLS parts present
- Pipeline integration: chunk detection after segment rewriting, source LL info threading
- New: `src/media/chunk.rs`, `tests/ll_hls_dash.rs` (16 integration tests)
- Result: 1,072 tests total with `--features jit,cloudflare` (63 new tests)

### ~~Phase 10: MPEG-TS Input~~ ✅ Complete
- TS demuxer (PES/TS packets, PAT/PMT, H.264/H.265/AAC extraction)
- TS-to-CMAF transmuxer, init segment synthesis from codec config (SPS/PPS → avcC, ADTS → esds)
- AES-128-CBC whole-segment decryption for HLS-TS
- HLS input TS detection (`.ts` extension, `#EXT-X-KEY:METHOD=AES-128` parsing, optional `#EXT-X-MAP`)
- Pipeline integration: feature-gated `process_ts_segment()` — decrypt → demux → transmux → CMAF pipeline
- All TS code behind `#[cfg(feature = "ts")]` — zero impact on non-ts builds
- New: `src/media/ts.rs`, `src/media/transmux.rs`, `tests/ts_integration.rs` (30 integration tests)
- Result: 1,151 tests total with `--features jit,cloudflare,ts` (79 new ts-gated tests)

### ~~Phase 11: Advanced DRM~~ ✅ Complete
- ClearKey DRM system ID (`e2719d58-a985-b3c9-781a-b030af78d30e`) with PSSH builder
- Raw key mode: accept encryption keys directly via webhook (bypass SPEKE)
- Key rotation: per-period key rotation at configurable segment boundaries
- Clear lead: first N segments unencrypted, then encrypted with manifest transition
- DRM systems override: explicit selection of widevine/playready/fairplay/clearkey per request
- HLS ClearKey KEY tag, DASH ClearKey ContentProtection element
- New: `tests/advanced_drm.rs` (15 integration tests)
- Result: 1,003 tests total with `--features jit,cloudflare` (55 new tests from baseline 948)

### ~~Phase 12: Trick Play & I-Frame Playlists~~ ✅ Complete
- HLS `#EXT-X-I-FRAMES-ONLY` media playlists with `#EXT-X-BYTERANGE` (byte ranges into existing rewritten segments — no duplicate storage)
- HLS master playlist `#EXT-X-I-FRAME-STREAM-INF` for each video variant
- DASH trick play `<AdaptationSet>` with `<EssentialProperty schemeIdUri="http://dashif.org/guidelines/trickmode">`
- I-frame detection reuses existing `chunk.rs` infrastructure (first IDR chunk per segment)
- `IFrameSegmentInfo` type, `enable_iframe_playlist` opt-in field (default false)
- Dedicated route: `GET /repackage/{id}/{fmt}/iframes` for HLS I-frame playlist
- DASH trick play embedded in regular MPD (no separate endpoint)
- Sandbox writes `iframes.m3u8` alongside regular HLS output
- New: `tests/trick_play.rs` (27 integration tests)
- Result: 1,111 tests total with `--features jit,cloudflare` (39 new tests)

### Phase 13: DVR Window & Time-Shift — P2 ✅
- DVR sliding window manifests for live streams (configurable `dvr_window_duration`)
- Segments filtered during rendering (ManifestState retains all segments for live-to-VOD transitions)
- HLS: omits `PLAYLIST-TYPE:EVENT` when DVR active, dynamic `MEDIA-SEQUENCE`, windowed segments/parts/iframes/ad breaks
- DASH: `timeShiftBufferDepth` attribute, dynamic `startNumber` in SegmentTimeline, windowed ad break events
- Complete phase ignores window — full VOD manifest with all segments
- Windowing helpers on ManifestState: `windowed_segments()`, `windowed_media_sequence()`, `windowed_iframe_segments()`, `windowed_parts()`, `windowed_ad_breaks()`, `is_dvr_active()`
- Webhook validation: `dvr_window_duration` must be positive when provided
- New: `tests/dvr_window.rs` (25 integration tests)
- Result: 1,154 tests total with `--features jit,cloudflare` (43 new tests)

### Phase 14: Content Steering & CDN Optimization — P2 ✅
- Content steering directive injection in HLS master playlists (`#EXT-X-CONTENT-STEERING`) and DASH MPDs (`<ContentSteering>`)
- Webhook-driven steering config: `server_uri`, `default_pathway_id`, `query_before_start`
- DASH source pass-through: `<ContentSteering>` elements extracted from input MPDs and preserved in output
- Override priority: webhook config takes precedence over source-extracted steering
- New type: `ContentSteeringConfig` (server_uri, default_pathway_id, query_before_start)
- New webhook field: `content_steering` on `WebhookPayload` with validation (reject empty `server_uri`)
- Pipeline threading in `execute()` path
- HLS pass-through not applicable (edgepack parses media playlists; steering tag only in master playlists)
- New: `tests/content_steering.rs` (20 integration tests)
- Result: 1,290 tests total with `--features jit,cloudflare,ts` (85 new phase tests + 18 output integrity tests)
- Output integrity tests (`tests/output_integrity.rs`): structural validation of rewritten segments across all 4 encryption lanes (enc→enc, clear→enc, enc→clear, clear→clear), mdat/trun size consistency, encrypt-decrypt plaintext recovery roundtrip, I-frame BYTERANGE chunk validation, init rewrite roundtrip (clear→enc→clear), multi-KID PSSH verification, HLS/DASH manifest roundtrips (VOD, live, DVR, I-frame)
- Criterion benchmarks (`benches/jit_latency.rs`): segment rewrite latency (CBCS→CENC, clear→CENC, passthrough at 4/32/128 samples), init rewrite latency, manifest render/parse latency (HLS/DASH at varying segment counts)

### Phase 18: Binary Size Monitoring & Selective Feature Gating — P2
The current binary (~628 KB base) is well within cold start budgets (<1 ms). Feature-gating pure Rust application logic (SCTE-35, validation, DASH rendering) yields only ~20–30 KB savings — not enough to justify the `#[cfg]` maintenance burden and test matrix explosion. The real binary size wins come from crate-level decisions (e.g., the lightweight `url.rs` saved ~200 KB vs the `url` crate).

**Policy:** Monitor binary size as new features land. Feature-gate only when a phase introduces a **heavy new dependency or parser** that meaningfully increases the binary (50+ KB). Existing examples of this approach:
- `ts` feature (Phase 10): MPEG-TS demuxer + transmuxer adds a substantial new parser — feature-gated to keep it out of builds that don't need TS input

**Action items (reactive, not pre-emptive):**
- If the binary exceeds **800 KB** with all features enabled, audit the largest new modules and consider feature-gating the heaviest one
- If a new crate dependency adds **50+ KB** to the WASM binary, it must be feature-gated
- Per-feature binary size tests in `tests/wasm_binary_size.rs` enforce limits per build variant — a failing test triggers the conversation about what to gate
- Prefer lightweight built-in implementations over crate dependencies (as with `url.rs`) when the crate adds disproportionate WASM size

### ~~Phase 19: Configurable Cache-Control Headers~~ ✅ Complete
- Three-tier cache-control configuration: env var system defaults → per-request webhook overrides → hardcoded safety invariants
- `CacheControlConfig` struct: `segment_max_age`, `final_manifest_max_age`, `live_manifest_max_age`, `live_manifest_s_maxage`, `immutable` (all `Option`)
- `CacheConfig` extended with `final_manifest_max_age` field + env var loading (`CACHE_MAX_AGE_SEGMENTS`, `CACHE_MAX_AGE_MANIFEST_LIVE`, `CACHE_MAX_AGE_MANIFEST_FINAL`)
- `ManifestState.manifest_cache_header()` and `segment_cache_header()` methods — phase-based with per-request override → system default fallback
- Per-request overrides apply to manifests only (segments use system defaults to avoid overhead per segment request)
- Safety invariants: `AwaitingFirstSegment` → always `no-cache`, `public` prefix → always present
- Separate immutable flag control (default: true)
- Separate `max-age` and `s-maxage` for live manifests (CDN vs browser caching)
- `CacheControlInput` webhook type (separate from internal `CacheControlConfig`)
- Pipeline threading: `RepackageRequest` → `execute()` → `ProgressiveOutput.set_cache_control()` → `ManifestState`
- Request handlers simplified: inline phase-matching replaced with `state.manifest_cache_header(&ctx.config.cache)`
- Sandbox UI: collapsible "Cache-Control Overrides" section with all 5 config fields
- New: `tests/cache_control.rs` (43 integration tests), 3 new output integrity tests, 12 new unit tests (webhook + progressive)
- Result: 1,291 tests total with `--features jit,cloudflare` (80 new tests)

### Phase 20: Multi-Source Manifest Merging — P2
- Combine multiple source manifests (HLS/DASH) into a single unified output manifest
- Feature-gated behind `#[cfg(feature = "merge")]` to keep it modular and avoid binary impact on builds that don't need it
- Accept an array of source manifest URLs in the webhook payload (`source_urls: Vec<String>`) instead of a single `source_url`
- Mixed-format input: each source can be a different manifest format (e.g., HLS M3U8 + DASH MPD) — auto-detected per URL via the existing `hls_input`/`dash_input` parsers, then normalized into `SourceManifest` before merging
- Each source is fetched independently, parsed into `SourceManifest`, and its variants/tracks are merged into a unified manifest
- Variant deduplication: detect overlapping bitrates/resolutions across sources and apply configurable conflict resolution (prefer first, prefer highest quality, error)
- Track type merging: combine video variants from one source with audio/subtitle tracks from another (e.g., separate audio-only and video-only CMAF sources, or an HLS video source merged with a DASH audio source)
- Per-source encryption: each source may have a different encryption scheme — decrypt each independently, re-encrypt all to the target scheme(s)
- Per-source container format: sources may use different container formats (CMAF, fMP4, TS) — each is parsed/transmuxed independently before merging into the target output format
- Per-source init segments: each variant retains its own init segment (no re-muxing across sources)
- Unified DRM signaling: merged manifest gets a single consistent set of DRM tags/ContentProtection elements
- HLS: merged master playlist with all `#EXT-X-STREAM-INF` entries, unified `#EXT-X-MEDIA` groups for audio/subtitle renditions
- DASH: merged MPD with multiple `<AdaptationSet>` elements, one per source track type
- Segment URIs remain source-specific (each segment is fetched and repackaged from its original source)
- Pipeline changes: `RepackagePipeline` accepts `Vec<SourceManifest>`, iterates sources to build merged `ManifestState` before entering the segment processing loop
- Sandbox UI: multi-URL input field for testing merged output
- New: `src/manifest/merge.rs` for manifest merging logic

### ~~Phase 21: Generic HLS/DASH Pipeline (Dual-Format)~~ ✅ Complete
- Changed `RepackageRequest.output_format` to `output_formats: Vec<OutputFormat>` with backward-compatible webhook API (`format` singular still accepted, `output_formats` array takes precedence)
- Format-agnostic segment cache keys: `ep:{id}:{scheme}:init` and `ep:{id}:{scheme}:seg:{n}` (no format prefix — segments are identical for HLS and DASH)
- Per-format manifest state: `ep:{id}:{format}_{scheme}:manifest_state` stays format-qualified (manifests differ between HLS and DASH)
- `execute()` returns `Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>` — one output per (format, scheme) pair
- Webhook API: `output_formats: ["hls", "dash"]` for dual-format, `format` (singular) for backward compat; `resolved_output_formats()` mirrors `resolved_target_schemes()` pattern
- Dual-format + dual-scheme: `output_formats: [Hls, Dash]` × `target_schemes: [Cenc, Cbcs]` = 4 outputs (HLS+CENC, HLS+CBCS, DASH+CENC, DASH+CBCS)
- Result: 1,331 tests total (924 unit + 407 integration), including 25 new dual_format integration tests

### ~~Phase 16: Compatibility Validation & Hardening~~ ✅ Complete
- Codec/scheme compatibility validation (`src/media/compat.rs`): VP9+CBCS error, HEVC+CENC subsample warning, AV1+CBCS warning, DV RPU warning, text track encryption error
- HDR format detection (HDR10, HDR10+, Dolby Vision, HLG) from codec strings
- Init segment structure validation (ftyp ordering, sinf/schm/tenc presence, PSSH well-formedness)
- Media segment structure validation (moof/mdat presence, senc sample count, IV size)
- `validate_repackage_request()` pre-flight check in pipeline entry (errors → reject, warnings → log)
- Post-rewrite debug validation (init + segment structure checks)
- New: `src/media/compat.rs` (28 unit tests), `tests/conformance.rs` (23 integration tests)

### ~~Phase 17: CDN Provider Adapters & Binary Optimization~~ ✅ Simplified
- External cache backends (Redis HTTP, Redis TCP, Cloudflare KV, HTTP KV) have been removed
- Cache is now in-process `EncryptedCacheBackend<InMemoryCacheBackend>` with AES-128-CTR encryption for sensitive entries
- No external state store dependencies — the CDN layer caches responses via HTTP headers

### CDN Platform Deployment

edgepack compiles to a portable WASI P2 component — no CDN-specific APIs or external state stores needed.

Build commands:
```bash
cargo build --release                  # Base (no TS)
cargo build --release --features ts    # With MPEG-TS input/output
```

### ~~Phase 22: TS Segment Output~~ ✅ Complete
- CMAF-to-TS muxer (`src/media/ts_mux.rs`): extract samples from CMAF moof/mdat, convert AVCC→Annex B (H.264), raw AAC→ADTS, build PAT/PMT/PES, packetize into 188-byte TS packets
- `ContainerFormat::Ts` variant behind `#[cfg(feature = "ts")]` — `.ts` extension, no init segment (PAT/PMT embedded in each segment), HLS-only (DASH+TS rejected at validation)
- AES-128-CBC whole-segment encryption (`encrypt_ts_segment()`) — reverse of Phase 10's `decrypt_ts_segment()`
- HLS manifest rendering: no `#EXT-X-MAP` tag, `#EXT-X-KEY:METHOD=AES-128,URI="{key_uri}"` instead of SAMPLE-AES/SAMPLE-AES-CTR, `#EXT-X-VERSION:3`, `.ts` segment URIs
- Key delivery endpoint: `GET /repackage/{id}/{format}/key` serves raw 16-byte AES key for HLS-TS `#EXT-X-KEY` URI
- Pipeline integration: `TsMuxConfig` extracted from init segment, segments muxed via `mux_to_ts()` then optionally encrypted
- Webhook validation: accepts `"ts"` as `container_format`, rejects TS+DASH combination
- Sandbox UI: TS container format option, `.ts` output files, no `init.mp4` for TS
- All code behind existing `#[cfg(feature = "ts")]` gate — zero impact on non-TS builds
- New: `src/media/ts_mux.rs`, `tests/ts_output.rs` (46 integration tests), 4 new output integrity tests
- Result: 1,603 tests total with `--features jit,cloudflare,ts` (88 new TS output tests)

### Phase 23: MoQ Ingest — P3 (feature-gated, requires research)

**Goal:** Accept Media over QUIC (MoQ) streams from an upstream MoQ relay as a source input format, converting them to HLS/DASH output with encryption transforms — analogous to how edgepack currently ingests HLS/DASH manifests and CMAF/fMP4/TS segments.

**Primary use case:** edgepack subscribes to a MoQ relay as a MOQT subscriber, receives media groups/objects, and produces repackaged HLS/DASH + CMAF/fMP4 output with configurable encryption (CBCS/CENC/clear). The MoQ relay handles fan-out from the publisher; edgepack handles the MoQ-to-HLS/DASH bridge at the edge.

**Feature gate:** `#[cfg(feature = "moq")]` — heavy dependency surface (QUIC stack, async runtime, WebTransport) must not impact non-MoQ builds.

#### Relevant Specifications

| Spec | Draft | Purpose |
|------|-------|---------|
| MOQT (Media over QUIC Transport) | `draft-ietf-moq-transport` (v16+) | Core pub/sub transport: tracks, groups, objects, subscriptions |
| LOC (Low Overhead Container) | `draft-ietf-moq-loc` | Lightweight media container for MOQT objects |
| MoQ Catalog Format | `draft-ietf-moq-catalogformat` | JSON catalog for track discovery and codec signaling |
| MSF (MOQT Streaming Format) | `draft-ietf-moq-msf` | Media packaging over MOQT (successor to WARP) |
| Secure Objects | `draft-jennings-moq-secure-objects` | E2E encryption (SFrame-based) for MOQT objects |

**Note:** All specs are active IETF drafts — none have reached RFC status. Implementation should track the latest drafts and be prepared for breaking changes.

#### Rust Ecosystem

| Crate | Purpose | Notes |
|-------|---------|-------|
| `moq-lite` | Core MOQT pub/sub (broadcasts, tracks, groups, frames) | Active development (moq-dev/moq) |
| `hang` | Media layer atop moq-lite (catalog, codecs, LOC) | Active development (moq-dev/moq) |
| `moq-native` | Quinn QUIC + rustls endpoint config helpers | Active development |
| `web-transport-quinn` | WebTransport over Quinn | Active development |
| `quinn` | Async Rust QUIC implementation | Mature, widely used |
| `wtransport` | Pure Rust async WebTransport (alternative) | Active development |

All MoQ crates require `tokio` async runtime and native QUIC (UDP sockets).

#### Architecture Constraint: WASI P2 and QUIC

**The fundamental constraint:** MOQT runs over QUIC or WebTransport, both of which require UDP sockets and an async runtime. WASI P2 only exposes `wasi:http` in CDN edge runtimes (Cloudflare Workers, Fastly Compute, etc.) — `wasi:sockets/udp` is specified but not implemented by CDN providers. Additionally, edgepack uses synchronous blocking I/O, while MOQT requires persistent bidirectional async connections.

**Implication:** The MOQT transport layer (QUIC subscriber, WebTransport session) cannot run inside the WASM binary on current CDN edge runtimes. Two architectural approaches:

**Approach A — Native MoQ subscriber sidecar:**
- A native binary (using `moq-native` + `moq-lite` + `hang`) runs alongside the WASM binary
- The sidecar subscribes to the MoQ relay, reassembles groups, writes CMAF segments + catalog-derived metadata to the cache backend (Redis/KV)
- The WASM binary reads from cache and applies encryption transforms + manifest generation as it does today for HLS/DASH sources
- Catalog metadata is converted to `SourceManifest` format in the cache
- Pro: cleanest separation, reuses existing edgepack pipeline unchanged
- Con: requires deploying and managing a separate process

**Approach B — Native binary with embedded pipeline (no WASM):**
- A single native binary combines the MoQ subscriber and the edgepack repackaging pipeline
- Uses `moq-native` for transport and links against edgepack as an `rlib`
- Pro: single deployment unit, direct in-process data flow
- Con: loses WASM portability, ties deployment to a specific platform

**Future: WASI P3** may add native `async`, `stream<T>`, and `future<T>` types, potentially enabling a WASM-native QUIC stack if CDN runtimes also implement `wasi:sockets/udp`. This would unify the architecture but is not available today.

#### Components

**1. MoQ Catalog Parser** (`src/moq/catalog.rs`)
- Parse JSON catalog format (`draft-ietf-moq-catalogformat`)
- Extract track namespaces, names, codec parameters, selection properties (bitrate, resolution, framerate)
- Handle `initData` (base64 codec config) and `initTrack` references
- Support delta updates via JSON Patch
- Convert to edgepack's `SourceManifest` / `VariantInfo` types
- **This component is pure JSON parsing — could run in WASM**

**2. LOC Container Parser** (`src/moq/loc.rs`)
- Parse LOC header extensions (timestamp, timescale, video config, frame marking)
- Extract raw codec bitstream from LOC payload
- Map to existing codec handling (H.264/H.265/AAC/VP9/AV1)
- Handle LOC encryption (Secure Objects) if present
- **Pure byte manipulation — could run in WASM**

**3. LOC-to-CMAF Transmuxer** (`src/moq/transmux.rs`)
- Convert LOC-packaged media objects to CMAF moof+mdat segments
- Similar pattern to existing TS-to-CMAF transmux (`media/transmux.rs`)
- Synthesize init segments from LOC codec config
- Build trun/mdat from LOC frame payloads
- **Pure byte manipulation — could run in WASM**

**4. CMAF-over-MoQ Passthrough**
- When upstream MoQ content uses `packaging: "cmaf"` (objects are CMAF chunks)
- Reuse existing ISOBMFF parser (`media/cmaf.rs`) directly
- Most natural integration path — only transport changes, not container
- **Already implemented for CMAF — just needs data flow wiring**

**5. MOQT Subscriber** (`src/moq/subscriber.rs`)
- MOQT session setup (SETUP message exchange, version negotiation)
- SUBSCRIBE to video/audio tracks by namespace + name
- Group/object reassembly (ordered by Group ID, Object ID)
- Handle subgroup dependencies, join points (group boundaries = keyframes)
- FETCH for past groups (DVR/catch-up)
- Session lifecycle (GOAWAY, reconnection)
- **Requires QUIC/WebTransport — must run as native code**

**6. MoQ-to-HLS/DASH Bridge** (pipeline integration)
- Convert reassembled MoQ groups into the segment processing pipeline
- Map MoQ track metadata to `ManifestState` / `VariantInfo`
- Generate progressive HLS/DASH manifests as groups arrive (live streaming)
- Apply encryption transforms (CBCS/CENC) via existing pipeline
- Thread `dvr_window_duration`, content steering, SCTE-35 (if signaled in catalog)

#### Research Required Before Implementation

This phase requires significant research and prototyping before implementation begins:

- [ ] **Spec stability assessment:** Monitor MOQT transport spec progression toward RFC. Breaking changes between drafts may invalidate implementation work
- [ ] **Relay compatibility testing:** Test against live MoQ relays (moq-relay, Cloudflare's relay infrastructure) to understand real-world protocol behavior
- [ ] **LOC vs CMAF packaging prevalence:** Determine which packaging format is more common from MoQ publishers to prioritize parser implementation order
- [ ] **Catalog format maturity:** The catalog spec is still evolving — assess whether the JSON schema is stable enough to build against
- [ ] **WASI P3 timeline:** Track WASI P3 async support and CDN runtime adoption — this determines whether a WASM-native approach becomes viable
- [ ] **Binary size impact:** Profile the dependency tree of `moq-lite` + `hang` + `quinn` to estimate WASM binary size impact (expected to be very large — likely requires native-only build)
- [ ] **Sidecar vs embedded architecture decision:** Prototype both approaches to evaluate operational complexity, latency, and deployment patterns
- [ ] **E2E encryption interop:** Understand how MoQ Secure Objects (SFrame) interacts with edgepack's DRM encryption — potential double-encryption or key management complexity
- [ ] **Live-to-VOD with MoQ:** Design how MoQ's group-based delivery maps to edgepack's `ManifestPhase` state machine (AwaitingFirstSegment → Live → Complete)

**All P0 and P1 items are complete.** No P0 or P1 phases remain in the roadmap. Remaining phases (18, 20) are P2, Phase 23 is P3.

Full roadmap plan: `.claude/plans/crystalline-singing-bee.md`
