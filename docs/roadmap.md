# edgepack Roadmap

## Completed Phases

| Phase | Name | Status |
|-------|------|--------|
| 1 | Core CBCS→CENC Conversion | Done |
| 2 | Container Format Flexibility (CMAF + fMP4 + ISO) | Done |
| 3 | Unencrypted Input Support (clear content paths) | Done |
| 4 | Dual-Scheme Output (multi-rendition per request) | Done |
| 5 | Multi-Key DRM & Codec Awareness | Done |
| 6 | Subtitle & Text Track Pass-Through | Done |
| 7 | SCTE-35 Ad Markers & Ad Break Signaling | Done |
| 8 | JIT Packaging (On-Demand GET) | Done |
| 9 | LL-HLS & LL-DASH | Done |
| 10 | MPEG-TS Input (feature-gated) | Done |
| 11 | Advanced DRM | Done |
| 12 | Trick Play & I-Frame Playlists | Done |
| 13 | DVR Window & Time-Shift | Done |
| 14 | Content Steering & CDN Optimization | Done |
| 16 | Compatibility Validation & Hardening | Done |
| 17 | CDN Provider Adapters & Binary Optimization | Done |
| 19 | Configurable Cache-Control Headers | Done |
| 21 | Generic HLS/DASH Pipeline (Dual-Format) | Done |
| 22 | TS Segment Output (feature-gated) | Done |
| 24 | Spec Compliance Fixes | Done |
| 25 | Manifest Correctness Fixes | Done |
| 26 | Runtime Policy Controls | Done |

---

## Active Roadmap

Derived from the full audit conducted 2026-03-08 (see `edgepack-audit-2026-03-08.md`). Phases are ordered by priority.

---

### ~~Phase 24: Spec Compliance Fixes — P0~~ Done

- **[H1]** `build_tenc()` now emits version 1 for CBCS (non-zero pattern), version 0 for CENC — per ISO/IEC 14496-12. Unit tests verify version byte for both schemes.
- **[H2]** `#EXT-X-PART` tags now emitted before `#EXTINF` of the parent segment — per RFC 8216bis Section 4.4.4.9. Unit test renamed and assertions corrected; integration test ordering assertion added.

---

### ~~Phase 25: Manifest Correctness Fixes — P1~~ Done

- **[M1]** DASH `<SegmentTimeline>` first `<S>` now includes `@t` attribute when DVR window is active (`startNumber > 0`) — per ISO 23009-1. Computes cumulative duration of all segments before the window. Unit and integration tests added in `dash.rs`, `dvr_window.rs`.
- **[M2]** HLS `#EXT-X-DATERANGE` `START-DATE` no longer wraps at 24 hours. Properly computes full days for streams longer than 24h. Unit tests in `hls.rs`, integration test in `scte35_integration.rs`.
- **[M3]** HLS renderer now uses `windowed_ad_breaks()` and `windowed_parts()` for DVR — matching DASH renderer behavior. Unit tests for DVR windowed ad breaks and parts added in `hls.rs`.

---

### ~~Phase 26: Runtime Policy Controls — P1~~ Done

- `PolicyConfig` with fail-closed allowlist model (`Option<Vec<_>>`: `None` = no restriction, `Some(vec![])` = full lockdown).
- `Forbidden(String)` error variant → HTTP 403 in WASI handler.
- **Route-level enforcement**: `parse_and_check_policy()` checks output format and explicit encryption scheme before any cache lookup or JIT setup.
- **JIT setup enforcement**: Scheme checked after JIT default resolution; container format checked after source config resolution.
- Env vars: `POLICY_ALLOWED_SCHEMES`, `POLICY_ALLOWED_FORMATS`, `POLICY_ALLOWED_CONTAINERS` (comma-separated allowlists).
- Health check and source config registration unaffected by policy.
- 28 integration tests in `tests/policy.rs`, 32 unit tests across `config.rs` and `handler/mod.rs`.

---

### Phase 27: Error Handling Hardening — P1

Replace panicking code with fallible error handling for defense-in-depth.

**[E1] Replace unwrap() with ok_or_else(?) in pipeline.rs**
- Lines 178, 182, 205, 301, 324, 620, 624, 684 — `key_set.as_ref().unwrap()` and similar.
- Logically safe due to preceding conditions, but should use `.ok_or_else(|| EdgepackError::Drm("...".into()))?` so that logic changes don't introduce panics.

**[E2] Replace panic! with Result in public functions**
- `container.rs:115` — `dash_profiles()` panics for `ContainerFormat::Ts`. Should return `Result`.
- `sample_cryptor.rs:120,143` — `create_decryptor`/`create_encryptor` panic for `EncryptionScheme::None`. Should return `Result`.

**[E3] Clamp parse_senc sample_count against data length**
- `cmaf.rs:271` — `Vec::with_capacity(sample_count as usize)` where `sample_count` comes from untrusted ISOBMFF input. A crafted file with `sample_count = u32::MAX` causes OOM.
- Fix: validate `sample_count * min_entry_size <= remaining_data_length` before allocating.

---

### Phase 28: Hot Path Performance Optimization — P2

Performance improvements to the segment rewriting and manifest rendering hot paths.

**[P1] SencEntry.iv inline representation**
- File: `src/media/cmaf.rs` (SencEntry struct), consumed throughout `segment.rs`
- `SencEntry.iv` is `Vec<u8>` but IVs are always 8 or 16 bytes. For a 128-sample segment, this causes 128 heap allocations during parse and 128 during rebuild.
- Fix: change to `[u8; 16]` with a `u8` iv_len field. Eliminates heap allocation and makes IVs `Copy`. Also eliminates `.clone()` overhead on subsamples in the encrypt-to-encrypt path.
- Impact: HIGH — innermost loop of the hottest path (per-sample crypto).

**[P2] Fuse decrypt+encrypt loops in segment rewrite**
- File: `src/media/segment.rs:104-155`
- `rewrite_encrypted_to_encrypted` has separate decrypt (104-127) and re-encrypt (141-155) loops over the same samples. Fusing into a single pass halves iteration count and improves L1 cache locality.
- Impact: MEDIUM — most impactful for large segments exceeding cache size.

**[P3] Replace format!()+push_str() with write!() in manifest renderers**
- Files: `src/manifest/hls.rs`, `src/manifest/dash.rs`
- ~78 instances of `push_str(&format!(...))` creating temporary `String` allocations. `write!()` from `std::fmt::Write` writes directly into the target `String`.
- Impact: MEDIUM — eliminates ~400+ temporary allocations for a 200-segment manifest.

**[P4] Box-level copy in init segment rewriter**
- File: `src/media/init.rs:307-329`
- `rewrite_sample_entry()` copies non-sinf bytes one at a time with `push()`. Should read box headers and copy whole boxes with `extend_from_slice()`.
- Impact: MEDIUM — called per track per init rewrite.

**[P5] Minor allocation optimizations**
- `extract_sample_sizes` intermediate `Vec<u32>` — access trun entries directly
- `build_senc_box`/`build_pssh_box` double-allocate via intermediate Vec — compute size upfront
- `rebuild_moof` children Vec not pre-sized — use `with_capacity`
- `windowed_segments()` called multiple times per DASH render — cache or pass as parameter
- Impact: LOW individually, cumulative improvement.

---

### Phase 29: DASH Manifest Polish — P2

Low-severity DASH spec improvements.

**[D1] SegmentTimeline repeat coalescing**
- File: `src/manifest/dash.rs:351-357`
- Consecutive `<S>` elements with equal duration are not coalesced with `@r` (repeat) attribute. A 1000-segment VOD with uniform 6s segments emits 1000 lines instead of `<S d="6000" r="999"/>`.
- Fix: track previous duration, increment repeat counter, emit `r="N"` when duration changes.
- Impact: Manifest size reduction only, not correctness.

**[D2] ContentProtection value attribute**
- File: `src/manifest/dash.rs:278-300`
- DASH-IF IOP recommends `value="Widevine"` / `value="PlayReady"` on DRM-specific `<ContentProtection>`. Optional per ISO 23009-1 but recommended for interoperability.
- Impact: Interoperability improvement only.

---

### Phase 18: Binary Size Monitoring — P2

(Unchanged from previous roadmap)

Monitor binary size as new features land. Feature-gate only when a phase introduces a heavy new dependency or parser that meaningfully increases the binary (50+ KB).

**Policy:**
- If the binary exceeds **800 KB** with all features enabled, audit the largest new modules and consider feature-gating the heaviest one
- If a new crate dependency adds **50+ KB** to the WASM binary, it must be feature-gated
- Per-feature binary size tests in `tests/wasm_binary_size.rs` enforce limits per build variant
- Prefer lightweight built-in implementations over crate dependencies (as with `url.rs`)

---

### Phase 30: Feature Gaps — P2

Features present in competing JIT packagers (AWS MediaPackage, USP, Broadpeak) but missing from edgepack.

**[F1] Full multi-period DASH**
- edgepack creates new `<Period>` elements for key rotation but does not support arbitrary multi-period DASH manifests from source.
- Competitors (USP, MediaPackage) support full multi-period pass-through.
- Scope: DASH input parser + renderer multi-period support, period-aware segment numbering.

**[F2] Server-Side Ad Insertion (SSAI) — research phase**
- SCTE-35 pass-through foundation is already in place (emsg extraction, HLS DATERANGE, DASH EventStream).
- Basic ad conditioned manifest manipulation (e.g., signaling to an SSAI decision server) could be added without full ad content splicing.
- This is the most commercially significant feature gap.
- Scope: research SSAI integration patterns compatible with edge deployment, prototype manifest conditioning.

---

### Phase 23: MoQ Ingest — P3

(Unchanged from previous roadmap — feature-gated, requires research)

Accept Media over QUIC (MoQ) streams from an upstream MoQ relay as a source input format, converting them to HLS/DASH output with encryption transforms.

**Architecture constraint:** MOQT runs over QUIC/WebTransport requiring UDP sockets and async runtime. WASI P2 only exposes `wasi:http` in CDN edge runtimes. The MOQT transport layer cannot run inside the WASM binary on current CDN runtimes.

**Research required before implementation:**
- Spec stability assessment (MOQT transport spec progression toward RFC)
- Relay compatibility testing (moq-relay, Cloudflare relay infrastructure)
- LOC vs CMAF packaging prevalence
- Catalog format maturity
- WASI P3 timeline (async support, CDN runtime adoption)
- Binary size impact of `moq-lite` + `hang` + `quinn`
- Sidecar vs embedded architecture decision
- E2E encryption interop (MoQ Secure Objects / SFrame vs edgepack DRM)
- Live-to-VOD mapping (MoQ groups → ManifestPhase state machine)

---

## Priority Summary

| Priority | Phase | Name | Items |
|----------|-------|------|-------|
| ~~P0~~ | ~~24~~ | ~~Spec Compliance Fixes~~ | ~~Done~~ |
| ~~P1~~ | ~~25~~ | ~~Manifest Correctness Fixes~~ | ~~Done~~ |
| **P1** | 26 | Error Handling Hardening | 3 items (unwrap→Result, panic→Result, OOM clamp) |
| **P2** | 27 | Hot Path Performance | 5 items (SencEntry inline, fused loops, write!(), box copy, minor allocs) |
| **P2** | 28 | DASH Manifest Polish | 2 items (SegmentTimeline @r, ContentProtection value) |
| **P2** | 18 | Binary Size Monitoring | Policy — reactive monitoring |
| **P2** | 29 | Feature Gaps | 2 items (multi-period DASH, SSAI research) |
| **P3** | 23 | MoQ Ingest | Research phase — blocked on spec maturity + WASI P3 |

## Unique Advantages (vs. AWS MediaPackage, USP, Broadpeak, Harmonic, Ateme, Wowza)

1. **Real-time CBCS ↔ CENC re-encryption** — no other packager converts between encryption schemes at runtime
2. **Dual-scheme simultaneous output** — CENC + CBCS from a single request
3. **CDN edge deployment** — ~628 KB WASM binary, sub-1ms cold start (every competitor runs at origin)
4. **Zero external state dependencies** — in-process encrypted cache only
5. **Combinatorial output matrix** — formats × schemes = all permutations from one request
6. **Clear lead, raw key mode, DRM systems override** — unique per-request DRM flexibility
7. **Pre-flight compatibility validation** — rejects invalid combinations before processing
8. **Encrypted in-process DRM key cache** — AES-128-CTR with minimum retention policies
