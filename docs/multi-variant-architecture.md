# Fix Lossy Conversion — Lossless Multi-Variant Architecture

## Context

The packager produces lossy output when converting both DASH and HLS sources with multiple video variants and text tracks. Testing with Sintel 4K DASH (`sintel-mp4-only/dash.mpd`) shows:

- **Source**: 9 video Representations (144p–2160p), 1 audio track, 10 raw WebVTT subtitle tracks
- **Output**: 1 video variant, 1 audio track, 0 subtitles, hardcoded `BANDWIDTH=2000000`, `CODECS="avc1.64001f"`

Root causes span **both core library and sandbox**, and affect **both DASH and HLS input**:

### Core problems (DASH input)
1. **DASH parser discards Representation metadata**: `dash_input.rs` ignores `bandwidth`, `width`, `height`, `codecs`, `frameRate` attributes on `<Representation>` elements
2. **No source variant metadata in SourceManifest**: `SourceManifest` has no field to carry per-variant metadata from the source manifest

### Core problems (HLS input)
3. **No HLS master playlist parser**: `hls_input.rs` explicitly rejects master playlists — there is no core function to extract variant metadata (BANDWIDTH, RESOLUTION, CODECS, FRAME-RATE) from `#EXT-X-STREAM-INF` tags
4. **No rendition metadata extraction**: Audio/subtitle rendition groups (`#EXT-X-MEDIA`) are not parsed in core

### Core problems (shared)
5. **Pipeline discards available metadata**: `build_variants_from_tracks()` in `pipeline.rs` sets `bandwidth: 0`, `resolution: None`, `frame_rate: None` — even though `tkhd` contains width/height
6. **TrackInfo missing width/height**: `extract_tracks()` parses `tkhd` for `track_id` but ignores the width/height fields at the end of the box
7. **No per-variant routing**: No route structure for independently processing/caching each variant

### Sandbox problems
8. **Single-variant processing**: Only processes one video variant (DASH or HLS), discards all others
9. **HLS variant metadata discarded**: `resolve_master_playlist()` uses bandwidth for variant selection but discards it (and never extracts RESOLUTION, CODECS, FRAME-RATE)
10. **Raw WebVTT undetected**: DASH text detection misses `mimeType="text/vtt"` (only matches fMP4-wrapped text)
11. **Hardcoded manifest metadata**: `build_progressive_combined_manifest()` hardcodes BANDWIDTH/CODECS

## Architecture: CDN Fan-Out

The solution uses a **CDN fan-out** model where each variant is an independent cache key processed by a separate WASM invocation. The CDN's native concurrency handles parallel processing — no WASM threading needed. The sandbox simulates this with `tokio::spawn()`.

### Request Flow Diagram

```
                          CDN Edge Node
                    ┌─────────────────────────┐
Player ──GET───►    │  /repackage/{id}/hls/    │
 manifest           │      manifest            │
                    │  ┌─────────────────────┐ │
                    │  │ WASM Instance #1     │ │
                    │  │  Parse source MPD    │ │
                    │  │  Extract 9 variants  │ │
                    │  │  Render master M3U8  │ │
                    │  │  (no segments)       │ │
                    │  └──────────┬──────────┘ │
                    └─────────────┼────────────┘
                                  │
         ┌────────────────────────┼────────────────────────┐
         ▼                        ▼                        ▼
  #EXT-X-STREAM-INF         #EXT-X-STREAM-INF        #EXT-X-STREAM-INF
  BANDWIDTH=100000           BANDWIDTH=2000000         BANDWIDTH=12000000
  v/0/manifest.m3u8         v/4/manifest.m3u8         v/8/manifest.m3u8


Player ABR selects variant 4 (2 Mbps):

Player ──GET───►  /repackage/{id}/hls_cenc/v/4/manifest
                    ┌─────────────────────────┐
                    │ WASM Instance #2         │
                    │  JIT: fetch variant 4    │
                    │  source segments,        │
                    │  decrypt → re-encrypt,   │
                    │  render media playlist   │
                    │  cache results           │
                    └─────────────────────────┘

Player ──GET───►  /repackage/{id}/hls_cenc/v/4/init.mp4     (cached)
Player ──GET───►  /repackage/{id}/hls_cenc/v/4/segment_0    (cached)
Player ──GET───►  /repackage/{id}/hls_cenc/v/4/segment_1    (cached)
```

### Pre-Warm Fan-Out (Optional)

```
CDN Purge/Warm API
        │
        ├──GET──► /repackage/{id}/hls_cenc/v/0/manifest  ──► WASM #A (144p)
        ├──GET──► /repackage/{id}/hls_cenc/v/1/manifest  ──► WASM #B (240p)
        ├──GET──► /repackage/{id}/hls_cenc/v/2/manifest  ──► WASM #C (360p)
        ├──GET──► /repackage/{id}/hls_cenc/v/3/manifest  ──► WASM #D (480p)
        ├──GET──► /repackage/{id}/hls_cenc/v/4/manifest  ──► WASM #E (720p)
        ├──GET──► /repackage/{id}/hls_cenc/v/5/manifest  ──► WASM #F (1080p)
        ├──GET──► /repackage/{id}/hls_cenc/v/6/manifest  ──► WASM #G (1440p)
        ├──GET──► /repackage/{id}/hls_cenc/v/7/manifest  ──► WASM #H (1800p)
        └──GET──► /repackage/{id}/hls_cenc/v/8/manifest  ──► WASM #I (2160p)
                                                              │
                                                     9 parallel WASM
                                                     instances on CDN
```

### Cache Key Structure

```
Master manifest (all variant metadata, no segment processing):
  ep:{id}:master:{fmt}_{scheme}             → ManifestState with all variants
  ep:{id}:variants                          → Vec<SourceVariantInfo> (JSON, shared)

Per-variant (independent, cacheable per CDN request):
  ep:{id}:v{vid}:{scheme}:init              → variant init segment bytes
  ep:{id}:v{vid}:{scheme}:seg:{n}           → variant media segment bytes
  ep:{id}:v{vid}:{fmt}_{scheme}:manifest    → variant ManifestState

Shared across variants:
  ep:{id}:keys                              → DRM key set (encrypted at rest)
  ep:{id}:source_config                     → SourceConfig (shared source URL)
```

### JIT Setup Flow Diagram

```
Master manifest request:               Per-variant request:
  /repackage/{id}/hls/manifest           /repackage/{id}/hls/v/4/manifest
         │                                        │
    Cache miss?                              Cache miss?
         │                                        │
    ensure_jit_master_setup()              ensure_jit_variant_setup(vid=4)
         │                                        │
    ┌────┴────┐                            ┌──────┴──────┐
    │ Fetch   │                            │ Load variant│
    │ source  │                            │ metadata    │
    │ manifest│                            │ from cache  │
    │ (MPD/   │                            │             │
    │  M3U8)  │                            │ Build single│
    │         │                            │ variant     │
    │ Parse   │                            │ source URL  │
    │ ALL     │                            │             │
    │ variants│                            │ Fetch init  │
    │         │                            │ Fetch segs  │
    │ Cache   │                            │ Rewrite all │
    │ metadata│                            │             │
    │         │                            │ Cache init, │
    │ Render  │                            │ segs, state │
    │ master  │                            └──────┬──────┘
    └────┬────┘                                   │
         │                                   Render variant
    Master M3U8/MPD                          media playlist
    (references v/0/..., v/1/..., etc.)
```

### Route Table

```
Existing routes (backward compat, single-variant):
  GET /repackage/{id}/{format}/manifest          → master or single-variant manifest
  GET /repackage/{id}/{format}/init.mp4          → init segment
  GET /repackage/{id}/{format}/iframes           → I-frame playlist
  GET /repackage/{id}/{format}/key               → AES-128 key (TS only)
  GET /repackage/{id}/{format}/segment_{n}.{ext} → media segment

New per-variant routes:
  GET /repackage/{id}/{format}/v/{vid}/manifest          → variant media playlist
  GET /repackage/{id}/{format}/v/{vid}/init.mp4          → variant init segment
  GET /repackage/{id}/{format}/v/{vid}/iframes           → variant I-frame playlist
  GET /repackage/{id}/{format}/v/{vid}/segment_{n}.{ext} → variant media segment

{format} = hls | dash | hls_cenc | dash_cbcs | ...
{vid}    = 0-indexed variant number (sorted by bandwidth ascending)
```

### Sandbox Parallel Processing (tokio)

```
tokio::spawn_blocking
  │
  ├── Resolve source → extract N variants, audio, text tracks
  │
  ├── tokio::join!(
  │     variant_0_task,    ─── pipeline.execute_progressive(v0_req)
  │     variant_1_task,    ─── pipeline.execute_progressive(v1_req)
  │     ...
  │     variant_N_task,    ─── pipeline.execute_progressive(vN_req)
  │     audio_task,        ─── pipeline.execute_progressive(audio_req)
  │     text_0_task,       ─── download .vtt or pipeline for fMP4 text
  │     text_1_task,       ─── download .vtt or pipeline for fMP4 text
  │     ...
  │   )
  │
  └── Build combined master manifest from all variant results
      Write to disk
```

## Part A: Core Library Fixes

### A1. Add width/height to TrackInfo (`src/media/codec.rs`)

Add `width` and `height` fields to `TrackInfo`:
```rust
pub struct TrackInfo {
    // ... existing fields ...
    pub width: Option<u32>,   // NEW: from tkhd fixed-point 16.16
    pub height: Option<u32>,  // NEW: from tkhd fixed-point 16.16
}
```

Add `parse_tkhd_dimensions()` function to extract width/height from tkhd payload:
- tkhd v0: width at byte offset 76, height at 80 (fixed-point 16.16, take upper 16 bits)
- tkhd v1: width at byte offset 88, height at 92

Update `parse_trak()` to call it alongside `parse_tkhd_track_id()` and populate the new fields.

File: `src/media/codec.rs` (struct at lines 8-23, parse_trak at lines 136-177, parse_tkhd_track_id at lines 179-198)

### A2. Add source variant metadata to SourceManifest (`src/manifest/types.rs`)

Add a new type and field:
```rust
/// Variant metadata extracted from source manifest (DASH Representations or HLS variants).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceVariantInfo {
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub frame_rate: Option<String>,
}

pub struct SourceManifest {
    // ... existing fields ...
    #[serde(default)]
    pub source_variants: Vec<SourceVariantInfo>,  // NEW
}
```

File: `src/manifest/types.rs` (SourceManifest at lines 504-559)

### A3. DASH parser: extract all Representation metadata (`src/manifest/dash_input.rs`)

In `parse_dash_manifest()`, when encountering `<Representation>` elements inside video AdaptationSets, parse and collect:
- `bandwidth` attribute → u64
- `width` attribute → Option<u32>
- `height` attribute → Option<u32>
- `codecs` attribute → Option<String>
- `frameRate` attribute → Option<String>

Store these as `Vec<SourceVariantInfo>` on the returned `SourceManifest`. Continue using only the first video Representation for segment URL/duration extraction (the pipeline processes one variant at a time). The metadata is for manifest rendering.

The parser already has state variables tracking the current AdaptationSet context. Add a `Vec<SourceVariantInfo>` accumulator and push entries as Representations are encountered. Only collect from video AdaptationSets.

File: `src/manifest/dash_input.rs` (Representation handling around lines 56-70, 104-136)

### A4. HLS master playlist parser (`src/manifest/hls_input.rs`)

Add a new `parse_hls_master_playlist()` function that extracts variant metadata from HLS master playlists. This is separate from `parse_hls_manifest()` which handles media playlists.

```rust
/// Parsed HLS master playlist variant/rendition metadata.
pub struct HlsMasterPlaylistInfo {
    pub variants: Vec<SourceVariantInfo>,
    pub variant_uris: Vec<String>,
    pub audio_renditions: Vec<HlsRenditionInfo>,
    pub subtitle_renditions: Vec<HlsRenditionInfo>,
}

pub struct HlsRenditionInfo {
    pub uri: Option<String>,
    pub name: String,
    pub language: Option<String>,
    pub group_id: String,
    pub is_default: bool,
}
```

Parse:
- `#EXT-X-STREAM-INF`: BANDWIDTH, RESOLUTION (WxH → (u32, u32)), CODECS, FRAME-RATE, AUDIO group ref, SUBTITLES group ref
- `#EXT-X-MEDIA:TYPE=AUDIO`: URI, NAME, LANGUAGE, GROUP-ID, DEFAULT
- `#EXT-X-MEDIA:TYPE=SUBTITLES`: URI, NAME, LANGUAGE, GROUP-ID
- `#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS`: NAME, LANGUAGE, INSTREAM-ID (no URI — inline)

The existing `parse_hls_manifest()` stays unchanged (rejects masters). The new function provides a clean API for callers (sandbox, CDN handler) to get variant metadata without also needing segment data.

File: `src/manifest/hls_input.rs` (new function, after existing `parse_hls_manifest()`)

### A5. Pipeline: use source variants + tkhd dimensions (`src/repackager/pipeline.rs`)

Update `build_variants_from_tracks()` to accept optional `SourceManifest` reference and merge metadata:

```rust
fn build_variants_from_tracks(
    tracks: &[TrackInfo],
    source: Option<&SourceManifest>,
) -> Vec<VariantInfo>
```

For each video track:
- `resolution`: use `TrackInfo.width/height` from tkhd (always available from init segment)
- `bandwidth`: use matching `SourceVariantInfo.bandwidth` if available, else 0
- `frame_rate`: use matching `SourceVariantInfo.frame_rate` if available
- `codecs`: continue using `TrackInfo.codec_string` (more accurate than manifest codecs since it's parsed from actual codec config boxes)

When `source_variants` has entries, use the first video variant's metadata to enrich the single processed variant. In a CDN multi-variant scenario, each variant would be processed independently, but the metadata from all variants should flow into `ManifestState.variants` so the master manifest lists all quality levels.

**Multi-variant ManifestState population**: When `source.source_variants` has multiple entries, create one `VariantInfo` per source variant (using source metadata for bandwidth/resolution/frame_rate, and the processed variant's codec string). This way the master manifest output will list all ABR tiers even though only one variant's segments are repackaged per request.

File: `src/repackager/pipeline.rs` (build_variants_from_tracks at lines 1073-1099, callers at ~line 251)

### A6. Per-variant route handling (`src/handler/mod.rs`, `src/handler/request.rs`)

**New routes in `mod.rs`** (placed before the catch-all segment route):
```rust
// Per-variant routes — matched before catch-all segment route
(HttpMethod::Get, ["repackage", id, fmt, "v", vid, "manifest"]) => { ... }
(HttpMethod::Get, ["repackage", id, fmt, "v", vid, "init.mp4"]) => { ... }
(HttpMethod::Get, ["repackage", id, fmt, "v", vid, "iframes"]) => { ... }
(HttpMethod::Get, ["repackage", id, fmt, "v", vid, seg_file]) => { ... }
```

**New JIT functions in `request.rs`**:
- `ensure_jit_master_setup()`: Parse source manifest, extract all variant metadata, cache at `ep:{id}:variants`. Get DRM keys (shared across variants via `ep:{id}:keys`). Render master manifest referencing `v/{vid}/...` URIs for each variant.
- `ensure_jit_variant_setup(variant_id)`: Load variant metadata from `ep:{id}:variants`, build variant-specific source config (for DASH SegmentBase: use the variant's BaseURL; for HLS: use the variant's media playlist URL). Run pipeline for that single variant. Cache init/segments/manifest at variant-qualified keys.
- `handle_variant_manifest_request()`, `handle_variant_init_request()`, `handle_variant_segment_request()`: Per-variant handler functions using variant-qualified cache keys (`ep:{id}:v{vid}:...`).

**Backward compat**: When source has only 1 variant, top-level routes work as before (master manifest = single-variant manifest). When source has multiple variants, top-level manifest returns master; per-variant routes serve individual variants.

**DRM key sharing**: All variants share the same KIDs (same content at different bitrates). SPEKE call happens once on first setup, cached at `ep:{id}:keys`. Each variant's init rewrite uses the shared keys.

Files: `src/handler/mod.rs` (route table lines 100-146), `src/handler/request.rs` (JIT handlers), `src/cache/mod.rs` (new CacheKeys for variant-qualified and master keys)

### A7. Unit + integration tests for core changes

- `media/codec.rs`: Test `extract_tracks()` returns width/height from tkhd. Update `build_cbcs_init_segment()` / `build_clear_init_segment()` fixtures in `tests/common/mod.rs` to include proper tkhd dimensions.
- `manifest/types.rs`: Serde roundtrip for `SourceVariantInfo` and `SourceManifest` with `source_variants`.
- `manifest/dash_input.rs`: Test `parse_dash_manifest()` populates `source_variants` from a multi-Representation MPD fixture.
- `manifest/hls_input.rs`: Test `parse_hls_master_playlist()` extracts variants with BANDWIDTH/RESOLUTION/CODECS/FRAME-RATE, audio renditions, and subtitle renditions.
- `repackager/pipeline.rs`: Test `build_variants_from_tracks()` merges source variant metadata.
- `handler/mod.rs`: Test per-variant route parsing (`/v/{vid}/manifest`, `/v/{vid}/init.mp4`, `/v/{vid}/segment_{n}.cmfv`).
- Integration: Test master manifest rendering includes all variants with correct metadata.

## Part B: Sandbox Fixes

### B1. New types for multi-variant resolution (`src/bin/sandbox.rs`)

Add `VideoVariantInfo` struct and expand `ResolvedSource`:

```rust
struct VideoVariantInfo {
    url: String,               // Synthetic MPD URL or direct media playlist URL
    bandwidth: u64,
    width: Option<u32>,
    height: Option<u32>,
    codecs: Option<String>,
    frame_rate: Option<String>,
}

struct ResolvedSource {
    video_url: String,                         // Primary/highest variant (backward compat)
    audio_url: Option<String>,
    text_tracks: Vec<TextTrackInfo>,
    video_variants: Vec<VideoVariantInfo>,      // NEW: all video variants
}
```

Add `is_raw_vtt: bool` to `TextTrackInfo` to distinguish raw WebVTT from fMP4-wrapped text:
```rust
struct TextTrackInfo {
    url: String,
    name: String,
    language: Option<String>,
    is_raw_vtt: bool,    // NEW
}
```

File: `src/bin/sandbox.rs` (lines 126-137)

### B2. Extract all DASH video Representations (`src/bin/sandbox.rs`)

Update `resolve_dash_tracks()`:

1. Find all video `<AdaptationSet>` blocks
2. Within each, find all `<Representation>` elements
3. For each Representation, extract `bandwidth`, `width`, `height`, `codecs`, `frameRate` attributes
4. Build a synthetic single-Representation DASH MPD (same pattern used for audio)
5. Serve via `start_local_file_server()`
6. Add to `video_variants`
7. Set `video_url` to the highest-bandwidth variant's URL

File: `src/bin/sandbox.rs`, `resolve_dash_tracks()` (lines 1489-1719)

### B3. Fix raw WebVTT detection (`src/bin/sandbox.rs`)

Update text detection in `resolve_dash_tracks()` to also match `mimeType="text/vtt"`:
```rust
let is_text = as_block.contains("contentType=\"text\"")
    || as_block.contains("contentType='text'")
    || as_block.contains("mimeType=\"application/ttml+xml\"")
    || as_block.contains("mimeType=\"text/vtt\"")
    || as_block.contains("mimeType='text/vtt'")
    || (as_block.contains("mimeType=\"application/mp4\"")
        && (as_block.contains("codecs=\"stpp") || as_block.contains("codecs=\"wvtt")));
```

For raw WebVTT tracks: extract the `<BaseURL>` content (the .vtt file URL), set `is_raw_vtt: true`, and skip the synthetic MPD pipeline — just store the direct URL.

File: `src/bin/sandbox.rs` (lines 1593-1667)

### B4. Extract all HLS variants (`src/bin/sandbox.rs`)

Update `resolve_master_playlist()` to use the new core `parse_hls_master_playlist()` function (A4) to extract ALL variant metadata from HLS master playlists:
- Call `parse_hls_master_playlist()` to get all variants with BANDWIDTH, RESOLUTION, CODECS, FRAME-RATE
- Resolve each variant's media playlist URL relative to the master URL
- Populate `video_variants` with all variants and their metadata
- `video_url` remains the highest-bandwidth variant
- Also extract audio and subtitle renditions from the parsed master info

This replaces the existing manual parsing in `resolve_master_playlist()` (which only picks highest-bandwidth and discards metadata) with the core library's parser.

File: `src/bin/sandbox.rs`, `resolve_master_playlist()` (lines 1403-1434)

### B5. Parallel multi-variant processing with tokio (`src/bin/sandbox.rs`)

Replace sequential Phase 1 with parallel variant + track processing:
```rust
// Spawn all variants + audio + text in parallel
let variant_handles: Vec<_> = video_variants.iter().enumerate()
    .map(|(vid, variant)| {
        let req = build_variant_request(variant, &content_id, vid, ...);
        tokio::task::spawn_blocking(move || {
            let pipeline = RepackagePipeline::new(config.clone());
            pipeline.execute_progressive(&req, |event| { /* write v{vid}_* files */ })
        })
    }).collect();

let audio_handle = audio_source.map(|src| tokio::task::spawn_blocking(...));

let text_handles: Vec<_> = text_tracks.iter().enumerate()
    .map(|(idx, tt)| {
        if tt.is_raw_vtt { /* spawn download task */ }
        else { /* spawn pipeline task */ }
    }).collect();

// Await all concurrently
for handle in variant_handles { handle.await??; }
if let Some(h) = audio_handle { h.await??; }
for handle in text_handles { handle.await??; }
```

- **Single variant** (empty or 1 entry): existing flow, no file naming changes
- **Multi-variant** (2+ entries): parallel `tokio::task::spawn_blocking` per variant
  - Each variant outputs to `v{vid}_init.mp4`, `v{vid}_segment_{n}.cmfv`, `v{vid}_video.m3u8`
  - `playback_ready` signals after the first variant has 2 segments + audio has 2 segments
  - Progress shows aggregate counts across all variants

File: `src/bin/sandbox.rs` (Phase 1 block, lines 357-480)

### B6. Raw WebVTT pass-through (`src/bin/sandbox.rs`)

In Phase 3 text processing, handle `is_raw_vtt` tracks:

1. Download the .vtt file via `reqwest::blocking::get(url)`
2. Write to output dir as `text_{N}.vtt`
3. Create HLS wrapper playlist `text_{N}.m3u8`:
   ```
   #EXTM3U
   #EXT-X-VERSION:7
   #EXT-X-TARGETDURATION:{ceil_duration}
   #EXT-X-PLAYLIST-TYPE:VOD
   #EXTINF:{duration},
   text_{N}.vtt
   #EXT-X-ENDLIST
   ```
4. Duration from video total duration (sum of segment durations)
5. Skip pipeline processing (no init segment, no encryption for raw WebVTT)

File: `src/bin/sandbox.rs` (Phase 3, ~line 680+)

### B7. Combined manifest with real variant metadata (`src/bin/sandbox.rs`)

Update `build_progressive_combined_manifest()` to accept variant metadata:

- Accept `&[ProcessedVariantInfo]` with bandwidth, resolution, codecs, frame_rate, manifest_filename
- For HLS multi-variant: emit one `#EXT-X-STREAM-INF` per variant with real metadata
- For HLS single-variant: use metadata from source if available, fall back to current defaults
- For DASH: merge multiple variant Representations into the output MPD

File: `src/bin/sandbox.rs`, `build_progressive_combined_manifest()` (lines 1902-1971)

## Files Modified

### Core
- `src/media/codec.rs` — TrackInfo width/height, parse_tkhd_dimensions()
- `src/manifest/types.rs` — SourceVariantInfo, HlsRenditionInfo, source_variants on SourceManifest
- `src/manifest/dash_input.rs` — parse all Representation attributes into source_variants
- `src/manifest/hls_input.rs` — new parse_hls_master_playlist() function + HlsMasterPlaylistInfo type
- `src/repackager/pipeline.rs` — build_variants_from_tracks() merges source metadata
- `tests/common/mod.rs` — update init segment fixtures with tkhd dimensions

### Sandbox
- `src/bin/sandbox.rs` — multi-variant processing, WebVTT pass-through, real metadata

## Verification

1. `cargo test --target $(rustc -vV | grep host | awk '{print $2}')` — all tests pass (new + existing)
2. `cargo build` — WASM build succeeds, binary size within limits
3. Manual test with Sintel 4K DASH: 9 variants, 10 subtitles, real metadata in output manifest
4. Manual test with HLS multi-variant source (e.g. Apple's bipbop or similar): all variants preserved with BANDWIDTH/RESOLUTION/CODECS in output master playlist
5. Single-variant regression (media playlist or single-Representation sources still work normally)
