---
paths:
  - "tests/**/*.rs"
---

# Testing

- Integration tests use shared fixtures from `tests/common/mod.rs` — import with `mod common;`.
- Key fixtures: `build_cbcs_init_segment()`, `build_cbcs_media_segment()`, `build_clear_init_segment()`, `build_clear_media_segment()`, `make_drm_key_set()`, `make_hls_manifest_state()`, `make_dash_manifest_state()`, `make_hls_iframe_manifest_state()`, `make_hls_dvr_manifest_state()`.
- Test constants: `TEST_SOURCE_KEY`, `TEST_TARGET_KEY`, `TEST_KID`, `TEST_IV` (all `[u8; 16]`).
- Run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') module::name`.
- Run a specific integration test file: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test file_name`.
- To run with all features (excl. TS): add `--features jit,cloudflare`.
- To run with all features (incl. TS): add `--features jit,cloudflare,ts`.
- TS-specific tests are gated behind `#[cfg(feature = "ts")]` — they only run with `--features ts`.
- Output integrity tests (`tests/output_integrity.rs`) validate structural correctness across all input/output lanes: segment ISOBMFF structure, encrypt-decrypt roundtrip, I-frame BYTERANGE, init rewrite roundtrip, multi-KID PSSH, manifest roundtrips.
- Criterion benchmarks (`benches/jit_latency.rs`) measure JIT-critical latencies: `cargo bench --target $(rustc -vV | grep host | awk '{print $2}') --bench jit_latency`.
- When calling `parse_trun`/`parse_senc`/`parse_pssh`, pass the box **payload** (after header), not the full box including header.
