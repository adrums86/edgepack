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
| 16 | Compatibility Validation & Hardening | ✅ |
| 17 | CDN Provider Adapters & Binary Optimization | ✅ |

# Refactoring Roadmap

The codebase is being generalized from a single-purpose CBCS→CENC converter into a generic lightweight edge repackager. Phases 1–12, 16, and 17 are complete. All P0 and P1 items are done. Remaining phases:

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
- Hybrid mode (JIT + proactive webhook coexist — webhook detects JIT setup marker)
- `POST /config/source` endpoint for per-content source configuration
- URL pattern-based source resolution with `{content_id}` placeholder
- All JIT code behind `#[cfg(feature = "jit")]` feature flag
- Result: 762 tests total with `--features jit` (27 new JIT integration tests)

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

### Phase 13: DVR Window & Time-Shift — P2
- Sliding window manifests, DVR start-over, live-to-VOD

### Phase 14: Content Steering & CDN Optimization — P2
- HLS/DASH content steering, edge location awareness

### Phase 15: TS Segment Output — P2
- CMAF-to-TS muxer, HLS-TS manifests, AES-128 segment encryption
- New: `src/media/ts_mux.rs`

### Phase 18: Binary Size Monitoring & Selective Feature Gating — P2
The current binary (~648 KB base, ~685 KB full) is well within cold start budgets (<1 ms). Feature-gating pure Rust application logic (SCTE-35, validation, DASH rendering) yields only ~20–30 KB savings — not enough to justify the `#[cfg]` maintenance burden and test matrix explosion. The real binary size wins come from crate-level decisions (e.g., the lightweight `url.rs` saved ~200 KB vs the `url` crate).

**Policy:** Monitor binary size as new features land. Feature-gate only when a phase introduces a **heavy new dependency or parser** that meaningfully increases the binary (50+ KB). Existing examples of this approach:
- `ts` feature (Phase 10): MPEG-TS demuxer + transmuxer adds a substantial new parser — feature-gated to keep it out of builds that don't need TS input
- `cloudflare` feature (Phase 17): Cloudflare KV backend — only needed on Cloudflare Workers deployments

**Action items (reactive, not pre-emptive):**
- If the binary exceeds **800 KB** with all features enabled, audit the largest new modules and consider feature-gating the heaviest one
- If a new crate dependency adds **50+ KB** to the WASM binary, it must be feature-gated
- Per-feature binary size tests in `tests/wasm_binary_size.rs` enforce limits per build variant — a failing test triggers the conversation about what to gate
- Prefer lightweight built-in implementations over crate dependencies (as with `url.rs`) when the crate adds disproportionate WASM size

### Phase 19: Configurable Cache-Control Headers — P2
- Allow per-request configuration of `Cache-Control` max-age for segments and manifests
- Currently hardcoded: segments and finalised manifests use `max-age=31536000, immutable`; live manifests use `max-age=1, s-maxage=1`
- Add `cache_control` config to `RepackageRequest` / webhook payload with separate overrides for segments, live manifests, and finalised manifests
- Support both `max-age` and `s-maxage` (shared/CDN cache vs private/browser cache) independently
- Thread cache TTL config through `ContinuationParams` → `ProgressiveOutput` → HTTP response headers
- Env var defaults (`CACHE_MAX_AGE_SEGMENTS`, `CACHE_MAX_AGE_MANIFEST_LIVE`, `CACHE_MAX_AGE_MANIFEST_FINAL`) with per-request override via webhook/JIT query params
- Sandbox UI controls for cache header tuning

### ~~Phase 16: Compatibility Validation & Hardening~~ ✅ Complete
- Codec/scheme compatibility validation (`src/media/compat.rs`): VP9+CBCS error, HEVC+CENC subsample warning, AV1+CBCS warning, DV RPU warning, text track encryption error
- HDR format detection (HDR10, HDR10+, Dolby Vision, HLG) from codec strings
- Init segment structure validation (ftyp ordering, sinf/schm/tenc presence, PSSH well-formedness)
- Media segment structure validation (moof/mdat presence, senc sample count, IV size)
- `validate_repackage_request()` pre-flight check in pipeline entry (errors → reject, warnings → log)
- Post-rewrite debug validation (init + segment structure checks)
- New: `src/media/compat.rs` (28 unit tests), `tests/conformance.rs` (23 integration tests)

### ~~Phase 17: CDN Provider Adapters & Binary Optimization~~ ✅ Complete
- Generalized config: `RedisConfig` → `StoreConfig`, `RedisBackendType` → `CacheBackendType`
- **Cloudflare Workers KV** backend (`cloudflare` feature) via REST API
- **Generic HTTP KV** backend (always available) for AWS DynamoDB via API Gateway, Akamai EdgeKV via proxy, or custom KV stores
- HTTP client extended with `PUT` and `DELETE` methods
- Backward compatible: `REDIS_URL`/`REDIS_TOKEN` env vars still work unchanged
- `CACHE_BACKEND` env var override for backend selection
- `CACHE_ENCRYPTION_TOKEN` env var for custom key derivation source
- No new crate dependencies — all backends use existing `http_client.rs` and `serde_json`
- `set_nx()` is best-effort (GET then PUT) on non-Redis backends (acceptable for JIT lock coalescing)
- Result: 807 tests total with `--features jit,cloudflare` (18 new CDN adapter integration tests)

### CDN Platform Compatibility

| CDN Platform | WASI P2 Support | Recommended Backend | Alternative |
|---|---|---|---|
| Generic WASI P2 | Native | Redis HTTP (default) | — |
| Cloudflare Workers | Via component model | **Cloudflare KV** (`cloudflare` feature) | Redis HTTP |
| Fastly Compute | Native | Redis HTTP (existing) | HTTP KV |
| AWS CloudFront / Lambda@Edge | Via wasmtime in Lambda | **HTTP KV** (DynamoDB via API Gateway) | Redis HTTP |
| Akamai EdgeWorkers / EdgeCompute | Via WASI runtime | **HTTP KV** (EdgeKV via auth proxy) | Redis HTTP |
| Vercel Edge Functions | Via V8 WASI shim | Redis HTTP (existing) | HTTP KV |

Build commands per platform:
```bash
cargo build --release                           # Generic WASI P2 (Redis HTTP)
cargo build --release --features cloudflare     # Cloudflare Workers
cargo build --release --features jit            # JIT only
cargo build --release --features jit,cloudflare # All features (excl. TS)
cargo build --release --features jit,cloudflare,ts # All features (incl. TS input)
```

**All P0 and P1 items are complete.** No P0 or P1 phases remain in the roadmap. Remaining phases (13–15, 18–19) are P2.

Full roadmap plan: `.claude/plans/crystalline-singing-bee.md`
