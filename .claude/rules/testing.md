---
paths:
  - "tests/**/*.rs"
---

# Testing

- Integration tests use shared fixtures from `tests/common/mod.rs` — import with `mod common;`.
- Key fixtures: `build_cbcs_init_segment()`, `build_cbcs_media_segment()`, `build_clear_init_segment()`, `build_clear_media_segment()`, `make_drm_key_set()`.
- Test constants: `TEST_SOURCE_KEY`, `TEST_TARGET_KEY`, `TEST_KID`, `TEST_IV` (all `[u8; 16]`).
- Run a specific module's tests: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') module::name`.
- Run a specific integration test file: `cargo test --target $(rustc -vV | grep host | awk '{print $2}') --test file_name`.
- To run with all features: add `--features jit,cloudflare`.
