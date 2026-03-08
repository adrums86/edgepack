# Test Coverage

**Total tests:** 1,346 (base) | 1,508 (with `--features ts`)

All tests run on the native host target — WASM tests are not supported.

```bash
# Run all tests
cargo test --target $(rustc -vV | grep host | awk '{print $2}')

# With TS support
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --features ts

# Run a specific module
cargo test --target $(rustc -vV | grep host | awk '{print $2}') drm::

# Run a specific integration test suite
cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test encryption_roundtrip
```

## Unit Tests (853 base | 937 with TS)

Inline `#[cfg(test)] mod tests` blocks in every source file.

| Module | Tests | What's Covered |
|--------|-------|----------------|
| `error` | 17 | Error display strings, Result alias |
| `config` | 43 | Defaults, serde roundtrips, env var loading, PolicyConfig allowlists |
| `url` | 14 | URL parsing, join (absolute/relative/protocol-relative, normalization), serde roundtrip, authority extraction |
| `cache` | 50 | CacheKeys formatting (incl. scheme-qualified keys, format-agnostic keys), in-memory cache ops, encrypted backend (AES-128-CTR roundtrip, key sensitivity, IV uniqueness, key generation) |
| `drm` | 121 | EncryptionScheme enum (serde, bytes, from_scheme_type, from_str_value, HLS methods, IV sizes, patterns, FairPlay flags, `is_encrypted()`, None variant), SampleDecryptor/SampleEncryptor (factory dispatch, CBCS/CENC roundtrips), system IDs, CPIX XML, SPEKE client (incl. ClearKey) |
| `media` | 226 | FourCC types, ISOBMFF box parsing/building/iteration, ContainerFormat enum, init segment rewriting (scheme-aware, container-format-aware, sinf injection/stripping, ftyp rewriting, per-track tenc with TrackKeyMapping, multi-KID PSSH generation), segment rewriting (four-way dispatch), IV padding, codec string extraction (AVC/HEVC/AAC/VP9/AV1/AC-3/EC-3/Opus/FLAC/WebVTT/TTML), track metadata parsing (hdlr, mdhd timescale + language, stsd sample entries), TrackKeyMapping (single/per_type/from_tracks, serde roundtrip), emsg box parsing (v0/v1) + builder roundtrips, SCTE-35 splice_info_section parsing (splice_insert, time_signal), codec/scheme compatibility validation, HDR format detection, init/segment structure validation. With `ts` feature: chunk detection, TS demux, transmux |
| `manifest` | 231 | HLS/DASH rendering for all lifecycle phases, DRM scheme signaling, FairPlay key URI, variant streams, subtitle rendition groups (HLS `TYPE=SUBTITLES`, DASH text AdaptationSet), CEA-608/708 closed caption signaling (HLS `TYPE=CLOSED-CAPTIONS` with `INSTREAM-ID`, DASH `Accessibility` descriptors), audio/subtitle language attributes, ISO 8601 duration, KID formatting, HLS/DASH input parsing (source scheme detection, `#EXT-X-DATERANGE` SCTE-35 ad breaks, DASH `EventStream` parsing), ad break manifest rendering (`#EXT-X-DATERANGE`, DASH `EventStream`), I-frame playlist rendering (`#EXT-X-I-FRAMES-ONLY`, `#EXT-X-BYTERANGE`), master playlist I-frame stream signaling, LL-HLS/LL-DASH types and rendering |
| `repackager` | 91 | Request types/serde, progressive output state machine, cache-control headers, key set caching, pipeline execution, DRM info building (multi-KID PSSH per system), track key mapping construction, variant building from tracks, sensitive data cleanup (incl. per-scheme, target_formats), I-frame info and enable_iframe_playlist methods (incl. raw keys, key rotation, clear lead, progressive parts), multi-format output types |
| `handler` | 51 | HTTP routing, path parsing incl. scheme-qualified formats (`hls_cenc`, `dash_cbcs`), segment number parsing (all 7 extensions), I-frame manifest handler, response construction, policy enforcement |
| `http_client` | 9 | Response construction, native stub errors |

## Integration Tests (493 base | 566 with TS)

Located in `tests/`. Use synthetic CMAF fixtures from `tests/common/mod.rs` — no external services or network required.

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `advanced_drm` | 15 | Key rotation at segment boundaries, clear lead, ClearKey DRM, raw key mode |
| `cache_control` | 43 | System defaults (HLS/DASH, all phases), per-request overrides (live/final/segment max-age, s-maxage split, immutable toggle), safety invariants (AwaitingFirstSegment always no-cache), progressive output integration (HLS + DASH), backward compat, DVR + cache control, container format + cache control, system CacheConfig overrides, DASH per-request overrides |
| `clear_content` | 10 | Clear→CENC/CBCS, encrypted→clear, clear→clear, roundtrip pipelines |
| `conformance` | 23 | Init segment structure (ftyp/sinf/pssh ordering), media segment structure (moof/mdat/senc), encryption roundtrip conformance, manifest correctness |
| `content_steering` | 20 | HLS master steering tag (full, URI-only, position, backward compat), DASH steering element (full, proxy-only, qbs, position), DASH input parsing (full, minimal, backward compat), serde roundtrips, override priority |
| `dual_format` | 20 | Multi-format output (HLS+DASH), format-agnostic cache keys, dual-format manifests, output_formats parsing, serde roundtrips, container format independence |
| `dual_scheme` | 15 | Scheme-qualified route parsing, cache key uniqueness per scheme, multi-scheme parsing, backward compat, duplicate/invalid scheme rejection |
| `dvr_window` | 27 | HLS DVR window (sliding window, media sequence, playlist type, DRM, iframes, ad breaks), DASH DVR (timeShiftBufferDepth, startNumber, windowed segments), live-to-VOD, serde compat, container formats |
| `encryption_roundtrip` | 8 | CBCS→plaintext→CENC: full-sample, pattern, subsample, multi-sample IV, audio, cross-segment IV isolation |
| `handler_integration` | 26 | HTTP routing for all endpoints, HttpResponse helpers, method filtering |
| `isobmff_integration` | 18 | Init segment rewriting (scheme/container-aware), PSSH generation, senc roundtrip, segment decrypt→re-encrypt→verify |
| `jit_packaging` | 22 | JIT source config storage/retrieval, on-demand setup, lock contention, backward compat |
| `ll_hls_dash` | 16 | LL-HLS partial segments, preload hints, server control, LL-DASH availability time offset, CMAF chunk boundary detection |
| `manifest_integration` | 23 | Progressive output lifecycle (HLS+DASH, all container formats), DRM signaling, cache-control headers, ManifestState serde |
| `multi_key` | 12 | Per-track tenc (video/audio KIDs), multi-KID PSSH generation, single-key backward compat, codec string extraction, TrackKeyMapping serde roundtrip, create→strip roundtrip, TrackKeyMapping::from_tracks |
| `output_integrity` | 21 | Rewritten segment ISOBMFF structure validation (all 4 encryption lanes), mdat/trun size consistency, encrypt-decrypt plaintext recovery, I-frame BYTERANGE chunk validation, init rewrite roundtrip (clear→enc→clear), multi-KID PSSH verification, HLS/DASH manifest roundtrips (VOD, live, DVR, I-frame), cache-control body invariants, TS manifest integrity, TS encrypt-decrypt roundtrip |
| `policy` | 27 | Runtime policy controls — format denial (manifest/init/segment/iframes/key, all extensions), scheme denial (cenc/cbcs/none, qualified/unqualified URLs), combined policies, full lockdown, backward compat, health unaffected, serde roundtrips |
| `scte35_integration` | 14 | emsg extraction, SCTE-35 parsing, HLS/DASH ad break rendering, source manifest ad marker roundtrip, AdBreakInfo serde |
| `trick_play` | 27 | HLS I-frame playlist rendering (BYTERANGE, DRM, init map, endlist, disabled), HLS master I-frame stream signaling, DASH trick play AdaptationSet, manifest dispatcher, serde backward compat, container format variations, route handling |
| `e2e` | 105 | Full pipeline E2E: encryption transforms x2 formats (18), container x format x encryption matrix (18), feature combinations incl. DVR+iframes+DRM+steering+dual-format (30), lifecycle phase transitions (18), edge cases & boundary conditions (21) |
| `wasm_binary_size` | 1 | WASM binary size guard (base ≤750 KB) with function count reporting |

### TS-Feature Integration Tests (requires `--features ts`)

| Test Suite | Tests | What's Covered |
|------------|-------|----------------|
| `ts_integration` | 30 | MPEG-TS demux, PES/TS packet parsing, TS-to-CMAF transmux, init segment synthesis, HLS-TS manifest parsing, AES-128 decryption |
| `ts_output` | 43 | ContainerFormat::Ts (serde, extension, validation), HLS-TS manifest (no EXT-X-MAP, VERSION:3, AES-128 KEY, .ts URIs), TS muxer (PAT/PMT/PES roundtrip, AVCC↔AnnexB, ADTS, encryption), TS validation, key endpoint routing, handler routing |

## Benchmarks

[Criterion](https://docs.rs/criterion) benchmarks in `benches/jit_latency.rs` measure JIT-critical latencies:

```bash
cargo bench --target $(rustc -vV | grep host | awk '{print $2}')
cargo bench --target $(rustc -vV | grep host | awk '{print $2}') --bench jit_latency -- segment_rewrite
```

| Benchmark Group | What's Measured |
|----------------|-----------------|
| `segment_rewrite` | Segment re-encryption at 4/32/128 samples x 1KB: CBCS→CENC, clear→CENC, passthrough |
| `init_rewrite` | Init segment DRM scheme transform: CBCS→CENC, clear→CENC |
| `manifest_render` | HLS/DASH manifest generation at 10/50/200 segments, HLS I-frame at 50 segments, HLS live at 6 segments |
| `manifest_parse` | HLS/DASH manifest input parsing at 50 segments |

Benchmarks run on native targets. WASM performance is proportional but not identical — use binary size as the cold-start proxy for WASM instantiation latency.

## Test Fixtures

All tests use shared fixtures from `tests/common/mod.rs` that build synthetic ISOBMFF data programmatically — no external test media files needed.

**Key fixtures:**
- `build_cbcs_init_segment()` / `build_cenc_init_segment()` / `build_clear_init_segment()` — synthetic init segments
- `build_cbcs_media_segment()` / `build_cenc_media_segment()` / `build_clear_media_segment()` — media segments with configurable sample count/size
- `make_drm_key_set()` / `make_drm_key_set_with_fairplay()` — DRM key sets
- `make_hls_manifest_state()` / `make_dash_manifest_state()` — manifest states with DRM and segments
- `make_hls_iframe_manifest_state()` / `make_dash_iframe_manifest_state()` — I-frame manifest states
- `make_hls_dvr_manifest_state()` / `make_dash_dvr_manifest_state()` — DVR windowed manifest states
- `full_segment_rewrite()` / `full_init_rewrite()` — encryption transform convenience wrappers
- `assert_valid_hls()` / `assert_valid_dash()` — structural validation helpers
- `assert_valid_segment_structure()` — moof/mdat/trun/senc validation

**Test constants:** `TEST_SOURCE_KEY`, `TEST_TARGET_KEY`, `TEST_KID`, `TEST_IV` (all `[u8; 16]`)

## Binary Size Guards

Per-feature binary size tests in `tests/wasm_binary_size.rs` prevent dependency bloat:

| Test | Features | Limit | Current Size |
|------|----------|-------|-------------|
| `wasm_base_binary_size` | none | 750 KB | ~628 KB |
