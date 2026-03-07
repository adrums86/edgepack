---
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
---

# Rust Conventions

- No async/await — WASI Preview 2 has no standard async runtime. All I/O is synchronous.
- Use `crate::error::Result<T>` for all fallible functions. Propagate errors with `?`.
- Zero-copy parsing where possible — work with byte slices and offsets over per-box allocation.
- All types stored in the cache must derive `Serialize, Deserialize`.
- Explicit state machines via enums over implicit boolean flags.
- Unit tests go in `#[cfg(test)] mod tests` blocks at the bottom of each source file.
- Integration tests go in `tests/` and use shared fixtures from `tests/common/mod.rs`.
- Always run tests with the native host target: `cargo test --target $(rustc -vV | grep host | awk '{print $2}')`.
- The default build target is `wasm32-wasip2` (set in `.cargo/config.toml`).
