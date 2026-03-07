---
paths:
  - "Cargo.toml"
  - "src/**/*.rs"
---

# WASM Binary Size

Binary size is the primary cold start proxy. Keep the WASM module small.

- Never add dependencies without considering WASM size impact.
- Use lightweight built-in modules (e.g., `src/url.rs`) instead of heavy crates when feasible.
- Sandbox-only dependencies must be gated behind `cfg(not(target_arch = "wasm32"))`.
- Size limits are enforced by tests in `tests/wasm_binary_size.rs` (base: 750 KB).
- Release profile uses `opt-level = "z"`, LTO, strip, codegen-units=1, panic=abort.
