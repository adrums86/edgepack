pub mod encrypted;
pub mod memory;

use crate::error::Result;
use encrypted::EncryptedCacheBackend;
use memory::InMemoryCacheBackend;
use std::sync::OnceLock;

/// Abstract cache backend for application state storage.
///
/// Used for DRM keys, JIT packaging state, and manifest progress tracking.
/// Media segments/manifests themselves are cached at the CDN level via HTTP headers.
pub trait CacheBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()>;
    /// Atomic set-if-not-exists. Returns `true` if the key was set (did not exist),
    /// `false` if the key already existed. Used for distributed locking / request coalescing.
    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Type alias for the global cache backend (encrypted wrapper over in-memory HashMap).
pub type GlobalCacheBackend = EncryptedCacheBackend<InMemoryCacheBackend>;

/// Global encrypted in-memory cache singleton.
///
/// Sensitive cache entries (DRM keys, rewrite params) are encrypted with a per-process
/// AES-128-CTR key. Non-sensitive entries pass through unmodified.
///
/// Persists between requests in long-running runtimes (wasmtime serve, Cloudflare Workers).
/// In per-request runtimes (Spin), each request starts with an empty cache — SPEKE is
/// called on the first cache miss, which is acceptable since the CDN caches the HTTP response.
static CACHE: OnceLock<GlobalCacheBackend> = OnceLock::new();

/// Get the global encrypted in-memory cache instance.
///
/// Initializes on first call with a random per-process encryption key.
/// Subsequent calls return the same instance.
pub fn global_cache() -> GlobalCacheBackend {
    CACHE
        .get_or_init(|| {
            let key = encrypted::generate_process_key();
            EncryptedCacheBackend::new(InMemoryCacheBackend::new(), key)
        })
        .clone()
}

/// Cache key builders for consistent key naming.
pub struct CacheKeys;

impl CacheKeys {
    /// DRM content keys for a given content ID.
    pub fn drm_keys(content_id: &str) -> String {
        format!("ep:{content_id}:keys")
    }

    /// Repackaging job state (progress, status).
    pub fn job_state(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:state")
    }

    /// Progressive manifest state (segment list, live/complete).
    pub fn manifest_state(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:manifest_state")
    }

    /// SPEKE response cache to avoid duplicate license server calls.
    pub fn speke_response(content_id: &str) -> String {
        format!("ep:{content_id}:speke")
    }

    /// Rewritten init segment binary data.
    pub fn init_segment(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:init")
    }

    /// Rewritten media segment binary data.
    pub fn media_segment(content_id: &str, format: &str, number: u32) -> String {
        format!("ep:{content_id}:{format}:seg:{number}")
    }

    /// Serialized source manifest metadata.
    pub fn source_manifest(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:source")
    }

    /// Serialized segment rewrite parameters.
    pub fn rewrite_params(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:rewrite_params")
    }

    /// Target schemes list.
    pub fn target_schemes(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:target_schemes")
    }

    // --- Format-agnostic key builders (Phase 21: Generic HLS/DASH Pipeline) ---
    // Segments are identical for HLS and DASH — only scheme affects bytes.

    /// Rewritten init segment for a specific scheme (format-agnostic, shared across HLS/DASH).
    pub fn init_segment_for_scheme_only(content_id: &str, scheme: &str) -> String {
        format!("ep:{content_id}:{scheme}:init")
    }

    /// Rewritten media segment for a specific scheme (format-agnostic, shared across HLS/DASH).
    pub fn media_segment_for_scheme_only(content_id: &str, scheme: &str, number: u32) -> String {
        format!("ep:{content_id}:{scheme}:seg:{number}")
    }

    /// Target output formats list.
    pub fn target_formats(content_id: &str) -> String {
        format!("ep:{content_id}:target_formats")
    }

    // --- Scheme-qualified key builders (Phase 4: Dual-Scheme Output) ---

    /// Build a scheme-qualified format string: e.g. "hls_cenc", "dash_cbcs".
    fn scheme_fmt(format: &str, scheme: &str) -> String {
        format!("{format}_{scheme}")
    }

    /// Progressive manifest state for a specific scheme.
    pub fn manifest_state_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:manifest_state")
    }

    /// Rewritten init segment for a specific scheme.
    pub fn init_segment_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:init")
    }

    /// Rewritten media segment for a specific scheme.
    pub fn media_segment_for_scheme(content_id: &str, format: &str, scheme: &str, number: u32) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:seg:{number}")
    }

    /// Rewrite parameters for a specific scheme.
    pub fn rewrite_params_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:rewrite_params")
    }

    // --- JIT Packaging key builders ---

    /// Per-content source configuration (source URL, target schemes, container format).
    pub fn source_config(content_id: &str) -> String {
        format!("ep:{content_id}:source_config")
    }

    /// Distributed processing lock for a specific resource.
    /// Prevents duplicate JIT work when multiple requests arrive simultaneously.
    pub fn processing_lock(content_id: &str, format: &str, resource: &str) -> String {
        format!("ep:{content_id}:{format}:lock:{resource}")
    }

    /// Marker that JIT setup (manifest + init + keys) is complete for this content/format.
    pub fn jit_setup(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:jit_setup")
    }

    /// Part data (LL-HLS chunk).
    pub fn part(content_id: &str, format: &str, segment_number: u32, part_index: u32) -> String {
        format!("ep:{content_id}:{format}:part:{segment_number}:{part_index}")
    }

    // --- Per-Variant key builders (CDN Fan-Out) ---
    // Each variant is an independent cache key for parallel CDN processing.

    /// Build a variant-qualified key prefix: e.g. "v0", "v4".
    fn variant_prefix(variant_id: u32) -> String {
        format!("v{variant_id}")
    }

    /// Per-variant manifest state.
    /// Key: `ep:{id}:v{vid}:{fmt}_{scheme}:manifest_state` or `ep:{id}:v{vid}:{fmt}:manifest_state`
    pub fn variant_manifest_state(content_id: &str, variant_id: u32, format: &str, scheme: Option<&str>) -> String {
        let vp = Self::variant_prefix(variant_id);
        match scheme {
            Some(s) => format!("ep:{content_id}:{vp}:{format}_{s}:manifest_state"),
            None => format!("ep:{content_id}:{vp}:{format}:manifest_state"),
        }
    }

    /// Per-variant init segment.
    /// Key: `ep:{id}:v{vid}:{scheme}:init` or `ep:{id}:v{vid}:init`
    pub fn variant_init_segment(content_id: &str, variant_id: u32, scheme: Option<&str>) -> String {
        let vp = Self::variant_prefix(variant_id);
        match scheme {
            Some(s) => format!("ep:{content_id}:{vp}:{s}:init"),
            None => format!("ep:{content_id}:{vp}:init"),
        }
    }

    /// Per-variant media segment.
    /// Key: `ep:{id}:v{vid}:{scheme}:seg:{n}` or `ep:{id}:v{vid}:seg:{n}`
    pub fn variant_media_segment(content_id: &str, variant_id: u32, number: u32, scheme: Option<&str>) -> String {
        let vp = Self::variant_prefix(variant_id);
        match scheme {
            Some(s) => format!("ep:{content_id}:{vp}:{s}:seg:{number}"),
            None => format!("ep:{content_id}:{vp}:seg:{number}"),
        }
    }

    /// Per-variant source variants metadata (shared across formats).
    /// Key: `ep:{id}:variants`
    pub fn source_variants(content_id: &str) -> String {
        format!("ep:{content_id}:variants")
    }

    /// Master manifest key (all variant metadata, no segment processing).
    /// Key: `ep:{id}:master:{fmt}_{scheme}` or `ep:{id}:master:{fmt}`
    pub fn master_manifest(content_id: &str, format: &str, scheme: Option<&str>) -> String {
        match scheme {
            Some(s) => format!("ep:{content_id}:master:{format}_{s}"),
            None => format!("ep:{content_id}:master:{format}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_cache_returns_same_instance() {
        let c1 = global_cache();
        let c2 = global_cache();
        c1.set("test_global", b"hello", 0).unwrap();
        assert_eq!(c2.get("test_global").unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn cache_keys_drm_keys() {
        assert_eq!(CacheKeys::drm_keys("abc123"), "ep:abc123:keys");
    }

    #[test]
    fn cache_keys_drm_keys_special_chars() {
        assert_eq!(CacheKeys::drm_keys("my-content_v2"), "ep:my-content_v2:keys");
    }

    #[test]
    fn cache_keys_job_state() {
        assert_eq!(CacheKeys::job_state("abc", "hls"), "ep:abc:hls:state");
        assert_eq!(CacheKeys::job_state("abc", "dash"), "ep:abc:dash:state");
    }

    #[test]
    fn cache_keys_manifest_state() {
        assert_eq!(
            CacheKeys::manifest_state("abc", "hls"),
            "ep:abc:hls:manifest_state"
        );
    }

    #[test]
    fn cache_keys_speke_response() {
        assert_eq!(CacheKeys::speke_response("abc"), "ep:abc:speke");
    }

    #[test]
    fn cache_keys_init_segment() {
        assert_eq!(CacheKeys::init_segment("abc", "hls"), "ep:abc:hls:init");
        assert_eq!(CacheKeys::init_segment("abc", "dash"), "ep:abc:dash:init");
    }

    #[test]
    fn cache_keys_media_segment() {
        assert_eq!(CacheKeys::media_segment("abc", "hls", 0), "ep:abc:hls:seg:0");
        assert_eq!(CacheKeys::media_segment("abc", "dash", 42), "ep:abc:dash:seg:42");
    }

    #[test]
    fn cache_keys_source_manifest() {
        assert_eq!(CacheKeys::source_manifest("abc", "hls"), "ep:abc:hls:source");
    }

    #[test]
    fn cache_keys_rewrite_params() {
        assert_eq!(CacheKeys::rewrite_params("abc", "dash"), "ep:abc:dash:rewrite_params");
    }

    #[test]
    fn cache_keys_target_schemes() {
        assert_eq!(CacheKeys::target_schemes("abc", "hls"), "ep:abc:hls:target_schemes");
        assert_eq!(CacheKeys::target_schemes("abc", "dash"), "ep:abc:dash:target_schemes");
    }

    // --- Format-agnostic key tests (Phase 21) ---

    #[test]
    fn cache_keys_init_segment_for_scheme_only() {
        assert_eq!(
            CacheKeys::init_segment_for_scheme_only("abc", "cenc"),
            "ep:abc:cenc:init"
        );
        assert_eq!(
            CacheKeys::init_segment_for_scheme_only("abc", "cbcs"),
            "ep:abc:cbcs:init"
        );
    }

    #[test]
    fn cache_keys_media_segment_for_scheme_only() {
        assert_eq!(
            CacheKeys::media_segment_for_scheme_only("abc", "cenc", 0),
            "ep:abc:cenc:seg:0"
        );
        assert_eq!(
            CacheKeys::media_segment_for_scheme_only("abc", "cbcs", 42),
            "ep:abc:cbcs:seg:42"
        );
    }

    #[test]
    fn cache_keys_target_formats() {
        assert_eq!(CacheKeys::target_formats("abc"), "ep:abc:target_formats");
    }

    #[test]
    fn cache_keys_format_agnostic_differs_from_format_qualified() {
        // Format-agnostic keys should differ from format-qualified keys
        let agnostic = CacheKeys::init_segment_for_scheme_only("abc", "cenc");
        let qualified = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
        assert_ne!(agnostic, qualified);
        assert_eq!(agnostic, "ep:abc:cenc:init");
        assert_eq!(qualified, "ep:abc:hls_cenc:init");
    }

    // --- Scheme-qualified key tests ---

    #[test]
    fn cache_keys_manifest_state_for_scheme() {
        assert_eq!(
            CacheKeys::manifest_state_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:manifest_state"
        );
        assert_eq!(
            CacheKeys::manifest_state_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:manifest_state"
        );
    }

    #[test]
    fn cache_keys_init_segment_for_scheme() {
        assert_eq!(
            CacheKeys::init_segment_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:init"
        );
        assert_eq!(
            CacheKeys::init_segment_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:init"
        );
    }

    #[test]
    fn cache_keys_media_segment_for_scheme() {
        assert_eq!(
            CacheKeys::media_segment_for_scheme("abc", "hls", "cenc", 0),
            "ep:abc:hls_cenc:seg:0"
        );
        assert_eq!(
            CacheKeys::media_segment_for_scheme("abc", "dash", "cbcs", 42),
            "ep:abc:dash_cbcs:seg:42"
        );
    }

    #[test]
    fn cache_keys_rewrite_params_for_scheme() {
        assert_eq!(
            CacheKeys::rewrite_params_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:rewrite_params"
        );
        assert_eq!(
            CacheKeys::rewrite_params_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:rewrite_params"
        );
    }

    // --- JIT key tests ---

    #[test]
    fn cache_keys_source_config() {
        assert_eq!(CacheKeys::source_config("abc"), "ep:abc:source_config");
        assert_eq!(CacheKeys::source_config("my-video"), "ep:my-video:source_config");
    }

    #[test]
    fn cache_keys_processing_lock() {
        assert_eq!(
            CacheKeys::processing_lock("abc", "hls", "setup"),
            "ep:abc:hls:lock:setup"
        );
        assert_eq!(
            CacheKeys::processing_lock("abc", "dash", "seg:5"),
            "ep:abc:dash:lock:seg:5"
        );
    }

    #[test]
    fn cache_keys_jit_setup() {
        assert_eq!(CacheKeys::jit_setup("abc", "hls"), "ep:abc:hls:jit_setup");
        assert_eq!(CacheKeys::jit_setup("abc", "dash"), "ep:abc:dash:jit_setup");
    }

    #[test]
    fn cache_keys_part() {
        assert_eq!(
            CacheKeys::part("abc", "hls_cenc", 3, 2),
            "ep:abc:hls_cenc:part:3:2"
        );
        assert_eq!(
            CacheKeys::part("abc", "dash_cbcs", 0, 0),
            "ep:abc:dash_cbcs:part:0:0"
        );
    }

    #[test]
    fn cache_keys_scheme_fmt_different_from_unqualified() {
        // Scheme-qualified keys should differ from unqualified keys
        let unqualified = CacheKeys::init_segment("abc", "hls");
        let qualified = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
        assert_ne!(unqualified, qualified);
    }

    // --- Per-Variant key tests (CDN Fan-Out) ---

    #[test]
    fn cache_keys_variant_manifest_state() {
        assert_eq!(
            CacheKeys::variant_manifest_state("abc", 0, "hls", Some("cenc")),
            "ep:abc:v0:hls_cenc:manifest_state"
        );
        assert_eq!(
            CacheKeys::variant_manifest_state("abc", 4, "dash", Some("cbcs")),
            "ep:abc:v4:dash_cbcs:manifest_state"
        );
        assert_eq!(
            CacheKeys::variant_manifest_state("abc", 0, "hls", None),
            "ep:abc:v0:hls:manifest_state"
        );
    }

    #[test]
    fn cache_keys_variant_init_segment() {
        assert_eq!(
            CacheKeys::variant_init_segment("abc", 0, Some("cenc")),
            "ep:abc:v0:cenc:init"
        );
        assert_eq!(
            CacheKeys::variant_init_segment("abc", 8, Some("cbcs")),
            "ep:abc:v8:cbcs:init"
        );
        assert_eq!(
            CacheKeys::variant_init_segment("abc", 0, None),
            "ep:abc:v0:init"
        );
    }

    #[test]
    fn cache_keys_variant_media_segment() {
        assert_eq!(
            CacheKeys::variant_media_segment("abc", 0, 5, Some("cenc")),
            "ep:abc:v0:cenc:seg:5"
        );
        assert_eq!(
            CacheKeys::variant_media_segment("abc", 4, 42, None),
            "ep:abc:v4:seg:42"
        );
    }

    #[test]
    fn cache_keys_source_variants() {
        assert_eq!(CacheKeys::source_variants("abc"), "ep:abc:variants");
    }

    #[test]
    fn cache_keys_master_manifest() {
        assert_eq!(
            CacheKeys::master_manifest("abc", "hls", Some("cenc")),
            "ep:abc:master:hls_cenc"
        );
        assert_eq!(
            CacheKeys::master_manifest("abc", "dash", None),
            "ep:abc:master:dash"
        );
    }

    #[test]
    fn cache_keys_variant_keys_differ_from_global() {
        let global = CacheKeys::init_segment_for_scheme_only("abc", "cenc");
        let variant = CacheKeys::variant_init_segment("abc", 0, Some("cenc"));
        assert_ne!(global, variant);
    }
}
