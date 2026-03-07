//! Per-feature binary size guards for the release WASM artifact.
//!
//! Each test builds with a specific feature combination, checks the resulting
//! `.wasm` file stays below a size limit, and optionally reports the WASM
//! function count (a cold-start proxy) via `wasm-tools` if installed.
//!
//! Build variants and thresholds:
//!   - Base (no features, JIT always compiled): 750,000 bytes (732 KB)
//!   - TS-only (`--features ts`):               800,000 bytes (781 KB)

/// Build the WASM binary with the given features, assert it's under `max_bytes`,
/// and report size + function count.
fn build_and_measure(features: &[&str], max_bytes: u64, label: &str) {
    // Build the release WASM binary.
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["build", "--release", "--target", "wasm32-wasip2"]);
    if !features.is_empty() {
        cmd.arg("--features");
        cmd.arg(features.join(","));
    }
    let status = cmd.status().expect("failed to invoke cargo build");
    assert!(status.success(), "cargo build --release failed for {label}");

    // Locate the artifact.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let wasm_path = std::path::Path::new(manifest_dir)
        .join("target/wasm32-wasip2/release/edgepack.wasm");

    assert!(
        wasm_path.exists(),
        "[{label}] WASM binary not found at {}",
        wasm_path.display()
    );

    let size = std::fs::metadata(&wasm_path)
        .unwrap_or_else(|e| panic!("[{label}] cannot stat {}: {e}", wasm_path.display()))
        .len();

    // Report size.
    let features_str = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    };
    eprintln!(
        "  [{label}] features: {features_str} | size: {size} bytes ({:.0} KB) | {:.1}% of {:.0} KB limit",
        size as f64 / 1024.0,
        (size as f64 / max_bytes as f64) * 100.0,
        max_bytes as f64 / 1024.0,
    );

    // Report function count via wasm-tools (informational, not enforced).
    report_function_count(&wasm_path, label);

    // Assert size limit.
    assert!(
        size <= max_bytes,
        "[{label}] WASM binary is {size} bytes ({:.0} KB) — exceeds {} byte ({:.0} KB) limit. \
         Check for unnecessary dependencies or feature flags.",
        size as f64 / 1024.0,
        max_bytes,
        max_bytes as f64 / 1024.0,
    );
}

/// Try to report the WASM function count using `wasm-tools`.
/// Silently skips if wasm-tools is not installed.
fn report_function_count(wasm_path: &std::path::Path, label: &str) {
    let output = std::process::Command::new("wasm-tools")
        .args(["objdump", wasm_path.to_str().unwrap()])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // wasm-tools objdump format for function sections:
            //   "    functions                            |  ... | 1849 count"
            // We look for lines starting with "functions" and extract the count.
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("functions") {
                    // Extract the count from "... | NNNN count"
                    if let Some(count_part) = trimmed.rsplit('|').next() {
                        let count_str = count_part.trim().trim_end_matches(" count").trim();
                        // Only report the first "functions" line (module 0 = main module)
                        eprintln!("  [{label}] wasm functions: {count_str}");
                        return;
                    }
                }
            }
            eprintln!("  [{label}] wasm-tools: function count not parsed from objdump output");
        }
        Ok(_) => {
            eprintln!("  [{label}] wasm-tools objdump failed (non-zero exit)");
        }
        Err(_) => {
            eprintln!("  [{label}] wasm-tools not installed — skipping function count");
        }
    }
}

// ---------------------------------------------------------------------------
// Per-feature binary size tests
// ---------------------------------------------------------------------------

#[test]
fn wasm_base_binary_size() {
    build_and_measure(&[], 750_000, "base");
}

#[test]
#[cfg(feature = "ts")]
fn wasm_ts_binary_size() {
    build_and_measure(&["ts"], 800_000, "ts");
}
