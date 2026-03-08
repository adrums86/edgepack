# edgepack Full Audit Report

**Date:** 2026-03-08
**Auditor:** Claude Opus 4.6
**Codebase:** edgepack v0.1.0 (commit 4598b1c, branch main)

## Codebase Stats
- **30,000 lines** production Rust, **24,000 lines** test code (54K total)
- **1,452 tests** — all passing (0 failures)
- **~628 KB** WASM binary (limit 750 KB)
- **Zero `unsafe` blocks** in the entire codebase
- **Zero compiler warnings** on standard build
- **880 clippy pedantic/nursery suggestions** (style-level, not bugs)

---

## 1. Spec Compliance — Issues Found

### HIGH Severity

**[H1] `tenc` box uses version 0 for CBCS** — `src/media/init.rs:441`

The `build_tenc()` function hardcodes version 0 (`output.push(0)`). Per ISO/IEC 14496-12, the `tenc` box version MUST be 1 when `default_crypt_byte_block` and `default_skip_byte_block` are non-zero (CBCS pattern 1:9). Version 0 has a `reserved` byte at that offset. Strict parsers may ignore the pattern fields in a v0 tenc box. Fix: use version 1 when `pattern != (0, 0)`.

**[H2] LL-HLS `EXT-X-PART` tags emitted AFTER the segment** — `src/manifest/hls.rs:258-275`

Parts for a segment are currently rendered *after* the `#EXTINF` + URI of that segment. RFC 8216bis Section 4.4.4.9 requires `#EXT-X-PART` tags to appear *before* the `#EXTINF` of the parent segment. This will break conformant LL-HLS players.

### MEDIUM Severity

**[M1] DASH `SegmentTimeline` missing `@t` on first `<S>` for DVR** — `src/manifest/dash.rs:351-357`

When DVR sliding window is active (`startNumber > 0`), the first `<S>` element lacks a `@t` attribute. Per ISO 23009-1, the implicit start time is 0, causing a mismatch with actual segment presentation times. Fix: compute `t = sum(durations of all segments before window)`.

**[M2] HLS `EXT-X-DATERANGE` wraps at 24 hours** — `src/manifest/hls.rs:239-247`

The `START-DATE` calculation uses `(secs / 3600) % 24`, which wraps around after 24 hours. Streams longer than 24h will produce invalid ISO 8601 timestamps.

**[M3] HLS DVR doesn't use windowed iterators for ad breaks/parts** — `src/manifest/hls.rs:232,263`

The HLS renderer iterates over all ad breaks (`&state.ad_breaks`) and parts (`&state.parts`) rather than using `state.windowed_ad_breaks()` and `state.windowed_parts()`. The DASH renderer correctly uses the windowed versions.

### LOW Severity

**[L1] DASH `SegmentTimeline` doesn't use `r` (repeat) attribute** — `src/manifest/dash.rs:351-357`

Consecutive `<S>` elements with equal duration are not coalesced with `@r`. A 1000-segment VOD with uniform 6s segments emits 1000 lines instead of 1. Affects manifest size only, not correctness.

**[L2] DASH `ContentProtection` missing `value` attribute** — `src/manifest/dash.rs:278-300`

DASH-IF IOP recommends `value="Widevine"` on DRM-specific `<ContentProtection>`. Optional per ISO 23009-1 but recommended for interoperability.

### Verified Correct

- HLS: EXT-X-VERSION levels (v3/v4/v7/v9 per feature), TARGETDURATION ceil(), PLAYLIST-TYPE, ENDLIST, KEY/SESSION-KEY attributes, I-frame playlists, subtitle/CC renditions, content steering, content types
- DASH: MPD type static/dynamic, profiles (isoff-live + cmaf), ContentProtection mp4protection, PSSH/PRO signaling, EventStream SCTE-35, trick play, timeShiftBufferDepth, LL-DASH attributes
- ISOBMFF: ftyp brands (CMAF/fMP4/ISO), moov/moof/mdat structure, sinf/schm/frma hierarchy, PSSH v1 multi-KID, senc box, sample entry rename
- Encryption: CBCS 1:9 pattern with IV reset per range, CENC full-sample CTR, IV sizes (16/8), audio 0:0 pattern, FairPlay exclusion from CENC

---

## 2. Performance & Efficiency — Issues Found

### HIGH Impact

**SencEntry.iv as `Vec<u8>` causes per-sample heap allocation** — `src/media/cmaf.rs:281`, consumed throughout `segment.rs`

IVs are always 8 or 16 bytes but stored as `Vec<u8>`. For a 128-sample segment, this is 128 heap allocations during parse and 128 more during rebuild, plus `.clone()` overhead on subsample entries. Fix: use `[u8; 16]` with a `u8` length field — eliminates heap allocation entirely and makes `SencEntry` `Copy`.

### MEDIUM Impact

**Double pass over mdat in encrypted-to-encrypted rewrite** — `src/media/segment.rs:104-155`

Decrypt loop and re-encrypt loop iterate separately over the same samples. Fusing into a single pass would halve the iteration count and improve L1 cache locality.

**`format!()` + `push_str()` pattern throughout manifest renderers** — `src/manifest/hls.rs`, `src/manifest/dash.rs`

Both renderers use `push_str(&format!(...))` on nearly every line (~78 instances), creating a temporary `String` allocation each time. Switching to `write!()` from `std::fmt::Write` writes directly into the target `String`. For a 200-segment manifest, eliminates ~400+ temporary allocations.

**Byte-by-byte copy in init segment rewriter** — `src/media/init.rs:307-329`

`rewrite_sample_entry()` copies non-sinf bytes one at a time with `push()` instead of reading box headers and copying whole boxes with `extend_from_slice()`.

### LOW Impact

- `extract_sample_sizes` allocates intermediate `Vec<u32>` — could access trun entries directly
- `build_senc_box`/`build_pssh_box` double-allocate into intermediate then final Vec
- `rebuild_moof` children Vec not pre-sized with `with_capacity`
- `windowed_segments()` called multiple times per DASH render
- O(n*m) ad break matching in HLS segment loop (sparse in practice)

---

## 3. Error Handling & Safety

### Zero `unsafe` Blocks — the entire codebase is safe Rust

### `unwrap()` Calls Needing Attention (Moderate)

6 `unwrap()` calls on `key_set`/`content_key`/`ts_config` in `pipeline.rs` (lines 178, 182, 205, 301, 324, 620, 624, 684) — logically safe due to preceding condition checks, but should use `.ok_or_else(|| EdgepackError::Drm(...))?` for defense-in-depth.

### `panic!` Paths in Public Functions (Moderate)

- `ContainerFormat::Ts` in `dash_profiles()` (`container.rs:115`) — should return `Result`
- `create_decryptor`/`create_encryptor` with `EncryptionScheme::None` (`sample_cryptor.rs:120,143`) — should return `Result`

### DoS Risk from Untrusted Input

`parse_senc` uses `Vec::with_capacity(sample_count as usize)` where `sample_count` comes from untrusted ISOBMFF input (`cmaf.rs:271`). A crafted file with `sample_count = u32::MAX` would attempt a ~4 GB allocation, causing OOM panic. Should clamp or validate against remaining data length.

### Positive

- All ISOBMFF parsers have proper bounds checking before indexing
- Encryption modules validate IV sizes before `try_into().unwrap()`
- `saturating_sub` used appropriately in timestamp arithmetic
- Zero `todo!` or `unimplemented!` macros
- All 1,452 tests pass

---

## 4. Clippy Analysis (880 Pedantic/Nursery Suggestions)

| Category | Count | Severity |
|----------|-------|----------|
| `format!()` appended to String | 78 | Medium (perf) |
| `could be const fn` | 53 | Style |
| `use Self` instead of type name | 83 | Style |
| `u64 as usize` truncation warnings | 123 | Minor (WASM is 32-bit) |
| `usize as u32` truncation | 81 | Minor |
| `map().unwrap_or()` -> `map_or()` | 21 | Style |
| `cast_lossless` (u8 -> u64 via From) | 30 | Style |
| Other style issues | 411 | Style |

The truncation warnings (`cast_possible_truncation`) are noteworthy for WASM32 where `usize` is 32-bit — most `u64 as usize` casts are for byte offsets within media segments which are bounded by practical file sizes, but malicious input could theoretically cause truncation.

---

## 5. Dead Code Analysis

- **Build: clean** — zero warnings, meaning no dead code detected by the compiler
- **`uuid` crate** — used only in `drm/cpix.rs:27` for `Uuid::new_v4()` (CPIX request IDs). Minimal usage but necessary.
- **`log` crate** — used for 6 `log::warn!()` calls in pipeline.rs and handler/request.rs. Minimal but appropriate.
- All `CacheKeys::*` methods are used in production code (sandbox, pipeline, handler)
- No orphaned feature flags — `ts` and `sandbox` are both actively used
- No dead code paths identified beyond compiler's own analysis

---

## 6. Feature Parity vs. Industry Leaders

### Competitors Analyzed

1. **AWS Elemental MediaPackage** — Fully managed AWS service, regional origin
2. **Unified Streaming (USP)** — C++ Apache/Nginx module, origin-tier
3. **Broadpeak BkS400** — Appliance/VM origin packager
4. **Harmonic VOS360** — Cloud SaaS platform
5. **Ateme NEA** — Server-deployed origin packager
6. **Wowza Streaming Engine** — Java media server

### Full Parity (18 capabilities)

HLS/DASH output, CENC/CBCS encryption, Widevine/PlayReady/FairPlay, multi-key DRM, key rotation, LL-HLS/LL-DASH, trick play/I-frame playlists, DVR sliding window, SCTE-35 pass-through, WebVTT/TTML subtitles, CEA-608/708 captions, CMAF/fMP4, TS input/output, CPIX key exchange, RFC 6381 codecs, content steering, simultaneous HLS+DASH, configurable cache-control

### Unique to edgepack (No Competitor Offers These)

1. **Real-time CBCS <-> CENC re-encryption** — no other packager converts between encryption schemes at runtime
2. **Dual-scheme simultaneous output** — CENC + CBCS segments from a single request
3. **CDN edge deployment as ~628 KB WASM** — every competitor runs at origin tier; edgepack is the only packager designed for CDN edge nodes
4. **Sub-1ms cold start** — per-request WASM instantiation vs. long-running server processes
5. **Zero external state dependencies** — in-process encrypted cache only; no Redis/DB/filesystem required
6. **Clear lead** — first N segments unencrypted for fast channel start
7. **DRM systems override per request** — dynamic DRM system selection
8. **Raw key mode** — bypass SPEKE for testing/external key management
9. **Pre-flight compatibility validation** — rejects invalid codec/scheme combinations before processing
10. **Encrypted in-process DRM key cache** — AES-128-CTR encryption of sensitive cache entries with minimum retention
11. **Combinatorial output matrix** — `formats x schemes` produces all permutations from a single request (e.g., 2 formats x 2 schemes = 4 outputs)

### Missing vs. Competitors

| Feature | Available In | Priority |
|---------|-------------|----------|
| Server-Side Ad Insertion (SSAI) | MediaPackage + MediaTailor, Broadpeak | High (SCTE-35 foundation already exists) |
| Smooth Streaming output | MediaPackage, USP, Wowza | Low (declining format) |
| SPEKE 1.0 | MediaPackage | Low (2.0 is current) |
| SRT/DVB subtitle formats | USP | Low |
| IMSC1 subtitle profile | MediaPackage, USP | Low |
| Full multi-period DASH | USP, MediaPackage | Medium |
| RTMP/SRT/WebRTC ingest | Wowza, others | N/A (origin concern, not edge) |
| Built-in transcoding | Harmonic, Wowza | N/A (by design) |

### DRM & Encryption Comparison

| Feature | edgepack | MediaPackage | USP | Broadpeak | Harmonic | Ateme NEA | Wowza |
|---------|----------|-------------|-----|-----------|---------|-----------|-------|
| CENC (CTR) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| CBCS (CBC pattern) | Yes | Yes | Yes | Yes | Yes | Yes | Partial |
| CBCS <-> CENC conversion | **Yes** | No | No | No | No | No | No |
| Widevine | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| PlayReady | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| FairPlay | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| ClearKey | Yes | No | Yes | No | No | No | Yes |
| Multi-key (per-track) | Yes | Yes | Yes | Partial | Yes | Partial | No |
| Key rotation | Yes | Yes | Yes | Yes | Yes | Yes | No |
| Clear lead | **Yes** | No | No | No | No | No | No |
| Raw key mode | **Yes** | No | No | No | No | No | Yes |
| SPEKE 2.0 | Yes | Yes | No | No | No | No | No |
| DRM systems override | **Yes** | No | No | No | No | No | No |

### Output Formats Comparison

| Feature | edgepack | MediaPackage | USP | Broadpeak | Harmonic | Ateme NEA | Wowza |
|---------|----------|-------------|-----|-----------|---------|-----------|-------|
| HLS (M3U8) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| DASH (MPD) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| CMAF segments | Yes | Yes | Yes | Yes | Yes | Yes | Partial |
| fMP4 segments | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| TS input | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| TS output | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| Smooth Streaming | No | Yes | Yes | No | No | No | Yes |
| Simultaneous HLS+DASH | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| Simultaneous CENC+CBCS | **Yes** | No | No | No | No | No | No |

### Codec Support Comparison

| Feature | edgepack | MediaPackage | USP | Broadpeak | Harmonic | Ateme NEA | Wowza |
|---------|----------|-------------|-----|-----------|---------|-----------|-------|
| H.264 (AVC) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| H.265 (HEVC) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| VP9 | Yes | No | Yes | No | No | No | No |
| AV1 | Yes | Partial | Yes | No | Partial | No | No |
| AAC | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| AC-3 (Dolby Digital) | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| EC-3 (Dolby Digital Plus) | Yes | Yes | Yes | Yes | Yes | Yes | Partial |
| Opus | Yes | No | Yes | No | No | No | No |
| FLAC | **Yes** | No | No | No | No | No | No |
| HDR detection (HDR10, DV, HLG) | Yes | Partial | Yes | No | Yes | No | No |

---

## 7. Performance Advantages Over Competitors

| Metric | edgepack | AWS MediaPackage | USP | Wowza |
|--------|----------|-----------------|-----|-------|
| Binary size | ~628 KB | N/A (managed) | ~10-50 MB | ~200+ MB |
| Cold start | <1 ms | N/A | ~seconds | ~5-15s (JVM) |
| Runtime | WASM (sandboxed) | AWS managed | C++ (native) | Java (JVM) |
| Deployment | CDN edge | AWS region | Origin server | Origin server |
| External deps | None | AWS infra | Apache/Nginx + storage | JVM + filesystem |
| State | In-process encrypted | AWS managed | Shared storage | Filesystem |

edgepack's architectural advantage is fundamental: it pushes packaging from the origin tier to the CDN edge. This eliminates origin as a bottleneck, reduces latency to the first byte, and enables per-request encryption flexibility that origin-tier packagers cannot offer without maintaining multiple pre-packaged variants.

The ~628 KB binary with sub-1ms instantiation means the cost of a cache miss is measured in milliseconds, not round-trips to an origin server. Combined with dual-scheme output, a single edgepack instance at the edge replaces what would require 2-4 separate packaging pipelines at origin in competing architectures.

---

## 8. Recommended Action Items (Priority Order)

### P0 — Fix Before Production

1. **[H1]** Set tenc box version to 1 for CBCS (pattern != 0:0) — `init.rs:441`
2. **[H2]** Move EXT-X-PART tags before EXTINF+URI — `hls.rs:258-275`

### P1 — Fix Soon

3. **[M1]** Add `@t` attribute to first SegmentTimeline `<S>` for DVR — `dash.rs:351`
4. **[M2]** Fix EXT-X-DATERANGE START-DATE to handle >24h streams — `hls.rs:239`
5. **[M3]** Use `windowed_ad_breaks()`/`windowed_parts()` in HLS renderer — `hls.rs:232,263`
6. Replace `unwrap()` with `ok_or_else()?` in pipeline.rs (6 instances)
7. Clamp `parse_senc` sample_count against remaining data length — `cmaf.rs:271`

### P2 — Performance Optimization

8. Change `SencEntry.iv` from `Vec<u8>` to `[u8; 16]` + length byte
9. Fuse decrypt+encrypt loops in segment rewrite
10. Switch `format!()` + `push_str()` to `write!()` in manifest renderers
11. Fix byte-by-byte copy in `rewrite_sample_entry()`

### P3 — Nice to Have

12. **[L1]** Coalesce SegmentTimeline `<S>` elements with `@r` attribute
13. **[L2]** Add `value` attribute to DASH ContentProtection elements
14. Replace `panic!` with `Result` in `dash_profiles()` and crypto factory functions
15. Add `with_capacity` to Vec allocations in rebuild functions
