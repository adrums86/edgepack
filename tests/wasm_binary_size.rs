//! Binary size guard: ensures the release WASM artifact stays below 600 KB.
//!
//! This test shells out to `cargo build --release --target wasm32-wasip2`
//! and then checks the size of the resulting `.wasm` file.  It prevents
//! accidental dependency bloat from slipping in unnoticed.
//!
//! Current baseline: ~495 KB (after removing the `url` crate, applying
//! opt-level="z", codegen-units=1, and panic="abort").

#[test]
fn wasm_release_binary_is_under_600kb() {
    const MAX_SIZE_BYTES: u64 = 600_000; // 600 KB

    // Build the release WASM binary.
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .status()
        .expect("failed to invoke cargo build");
    assert!(status.success(), "cargo build --release failed");

    // Locate the artifact.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let wasm_path = std::path::Path::new(manifest_dir)
        .join("target/wasm32-wasip2/release/edgepack.wasm");

    assert!(
        wasm_path.exists(),
        "WASM binary not found at {}",
        wasm_path.display()
    );

    let metadata = std::fs::metadata(&wasm_path)
        .unwrap_or_else(|e| panic!("cannot stat {}: {e}", wasm_path.display()));

    let size = metadata.len();

    assert!(
        size <= MAX_SIZE_BYTES,
        "WASM binary is {size} bytes ({:.0} KB) — exceeds {MAX_SIZE_BYTES} byte (600 KB) limit. \
         Check for unnecessary dependencies or feature flags.",
        size as f64 / 1024.0,
    );

    eprintln!(
        "  WASM binary size: {} bytes ({:.0} KB) — {:.1}% of 600 KB limit",
        size,
        size as f64 / 1024.0,
        (size as f64 / MAX_SIZE_BYTES as f64) * 100.0,
    );
}
