use crate::cache::{self, CacheBackend, CacheKeys};
use crate::error::Result;
use crate::handler::{format_str, HandlerContext, HttpResponse};
use crate::manifest;
use crate::manifest::types::{ManifestState, OutputFormat};

/// Handle a request for a manifest.
///
/// Looks up ManifestState from cache, renders it, and returns with
/// appropriate cache headers based on whether the manifest is live or complete.
/// When `scheme` is provided, uses scheme-qualified cache keys.
///
/// On cache miss, triggers JIT on-demand setup: fetch source manifest,
/// init segment, and DRM keys, then render.
pub fn handle_manifest_request(
    content_id: &str,
    format: OutputFormat,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::manifest_state_for_scheme(content_id, fmt, s)
    } else {
        CacheKeys::manifest_state(content_id, fmt)
    };

    let state_bytes = match cache.get(&key)? {
        Some(data) => data,
        None => {
            // JIT fallback: trigger on-demand setup on cache miss
            if let Some(resp) = jit_manifest_fallback(content_id, format, fmt, scheme, ctx)? {
                return Ok(resp);
            }

            return Ok(HttpResponse::not_found(&format!(
                "manifest not found for {content_id}/{fmt}"
            )));
        }
    };

    render_manifest_response(&state_bytes, format, ctx)
}

/// Handle a request for the init segment.
///
/// Init segments are immutable once created — always served with long cache TTL.
///
/// On cache miss, triggers JIT setup (which caches the init segment), then
/// reads it back from cache.
pub fn handle_init_segment_request(
    content_id: &str,
    format: OutputFormat,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let fmt = format_str(format);

    // Try format-agnostic key first (Phase 21+), then legacy format-qualified key
    let data = if let Some(s) = scheme {
        cache.get(&CacheKeys::init_segment_for_scheme_only(content_id, s))?
            .or(cache.get(&CacheKeys::init_segment_for_scheme(content_id, fmt, s))?)
    } else {
        cache.get(&CacheKeys::init_segment(content_id, fmt))?
    };

    match data {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => {
            // JIT fallback: trigger setup then read init from cache
            if let Some(resp) = jit_init_fallback(content_id, format, fmt, scheme, ctx)? {
                return Ok(resp);
            }

            Ok(HttpResponse::not_found(&format!(
                "init segment not found for {content_id}/{fmt}"
            )))
        }
    }
}

/// Handle a request for a media segment.
///
/// Segments are immutable once created — always served with long cache TTL.
///
/// On cache miss, triggers on-demand segment processing: fetches the source
/// segment, decrypts, re-encrypts, and caches the result.
pub fn handle_media_segment_request(
    content_id: &str,
    format: OutputFormat,
    segment_number: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let fmt = format_str(format);

    // Try format-agnostic key first (Phase 21+), then legacy format-qualified key
    let data = if let Some(s) = scheme {
        cache.get(&CacheKeys::media_segment_for_scheme_only(content_id, s, segment_number))?
            .or(cache.get(&CacheKeys::media_segment_for_scheme(content_id, fmt, s, segment_number))?)
    } else {
        cache.get(&CacheKeys::media_segment(content_id, fmt, segment_number))?
    };

    match data {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => {
            // JIT fallback: process segment on demand
            if let Some(resp) = jit_segment_fallback(content_id, format, fmt, segment_number, scheme, ctx)? {
                return Ok(resp);
            }

            Ok(HttpResponse::not_found(&format!(
                "segment {segment_number} not found for {content_id}/{fmt}"
            )))
        }
    }
}

/// Handle a request for an I-frame / trick play manifest.
///
/// - **HLS**: Renders an `#EXT-X-I-FRAMES-ONLY` playlist from ManifestState.
/// - **DASH**: Returns 404 (trick play is embedded in the regular MPD).
pub fn handle_iframe_manifest_request(
    content_id: &str,
    format: OutputFormat,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    // DASH embeds trick play in the regular MPD — no separate endpoint
    if format == OutputFormat::Dash {
        return Ok(HttpResponse::not_found(
            "DASH trick play is embedded in the regular MPD, not a separate endpoint",
        ));
    }

    let cache = cache::global_cache();
    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::manifest_state_for_scheme(content_id, fmt, s)
    } else {
        CacheKeys::manifest_state(content_id, fmt)
    };

    let state_bytes = match cache.get(&key)? {
        Some(data) => data,
        None => {
            return Ok(HttpResponse::not_found(&format!(
                "manifest not found for {content_id}/{fmt}"
            )));
        }
    };

    let state: ManifestState = serde_json::from_slice(&state_bytes).map_err(|e| {
        crate::error::EdgepackError::Cache(format!("deserialize manifest state: {e}"))
    })?;

    match manifest::render_iframe_manifest(&state)? {
        Some(playlist) => {
            let cache_control = state.manifest_cache_header(&ctx.config.cache);
            Ok(HttpResponse::ok_with_cache(
                playlist.into_bytes(),
                format.content_type(),
                &cache_control,
            ))
        }
        None => Ok(HttpResponse::not_found(&format!(
            "I-frame playlist not available for {content_id}/{fmt}"
        ))),
    }
}

/// Handle a request for the AES-128 content key (TS output only).
///
/// Returns the raw 16-byte content key for HLS AES-128 decryption.
/// The key is loaded from the cached DRM key set.
pub fn handle_key_request(
    content_id: &str,
    _format: OutputFormat,
    scheme: Option<&str>,
    _ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();

    // Load the DRM key set from cache
    let key_data = cache.get(&CacheKeys::drm_keys(content_id))?;
    match key_data {
        Some(data) => {
            // Parse the cached key set to extract the raw content key
            let cached: serde_json::Value = serde_json::from_slice(&data).map_err(|e| {
                crate::error::EdgepackError::Cache(format!("deserialize key set: {e}"))
            })?;

            // Extract the first key's raw bytes
            if let Some(keys) = cached.get("keys").and_then(|k| k.as_array()) {
                if let Some(first_key) = keys.first() {
                    if let Some(key_b64) = first_key.get("key").and_then(|k| k.as_str()) {
                        use base64::Engine;
                        if let Ok(key_bytes) = base64::engine::general_purpose::STANDARD.decode(key_b64) {
                            let _ = scheme; // scheme is used for key selection in future
                            return Ok(HttpResponse::ok_with_cache(
                                key_bytes,
                                "application/octet-stream",
                                "no-cache",
                            ));
                        }
                    }
                }
            }

            Ok(HttpResponse::not_found(&format!(
                "key not found in key set for {content_id}"
            )))
        }
        None => Ok(HttpResponse::not_found(&format!(
            "no keys found for {content_id}"
        ))),
    }
}

/// Render a manifest response from cached ManifestState bytes.
fn render_manifest_response(
    state_bytes: &[u8],
    format: OutputFormat,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let state: ManifestState = serde_json::from_slice(state_bytes).map_err(|e| {
        crate::error::EdgepackError::Cache(format!("deserialize manifest state: {e}"))
    })?;

    let manifest_body = manifest::render_manifest(&state)?;

    let cache_control = state.manifest_cache_header(&ctx.config.cache);

    Ok(HttpResponse::ok_with_cache(
        manifest_body.into_bytes(),
        format.content_type(),
        &cache_control,
    ))
}

// ---------------------------------------------------------------------------
// JIT Fallback Handlers
// ---------------------------------------------------------------------------

/// Resolve the effective scheme string for JIT operations.
///
/// Uses the URL scheme if present, otherwise falls back to the JIT default.
fn resolve_jit_scheme(scheme: Option<&str>, ctx: &HandlerContext) -> String {
    scheme
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.config.jit.default_target_scheme.scheme_type_str().to_string())
}

/// Build a base URL for manifest references (e.g. init/segment URIs).
///
/// Given content_id, format, and scheme, produces `/repackage/{id}/{fmt}_{scheme}/`.
fn jit_base_url(content_id: &str, fmt: &str, scheme_str: &str) -> String {
    format!("/repackage/{content_id}/{fmt}_{scheme_str}/")
}

/// Ensure JIT setup has been performed for the given content/format/scheme.
///
/// If setup is already done (marker key exists), returns Ok(true).
/// If we acquire the lock and perform setup, returns Ok(true).
/// If another request holds the lock (contention), returns Ok(false) — caller
/// should return 202 Accepted with Retry-After.
fn ensure_jit_setup(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme_str: &str,
    ctx: &HandlerContext,
) -> Result<bool> {
    use crate::drm::scheme::EncryptionScheme;
    use crate::repackager::pipeline::{resolve_source_config, RepackagePipeline};

    let cache = cache::global_cache();

    // Check if setup is already done
    let setup_key = CacheKeys::jit_setup(content_id, fmt);
    if cache.exists(&setup_key)? {
        return Ok(true);
    }

    // Try to acquire the processing lock
    let lock_key = CacheKeys::processing_lock(content_id, fmt, "setup");
    let lock_ttl = ctx.config.jit.lock_ttl_seconds;
    let acquired = cache.set_nx(&lock_key, b"1", lock_ttl)?;

    if !acquired {
        // Another request is processing — check cache one more time
        if cache.exists(&setup_key)? {
            return Ok(true);
        }
        return Ok(false); // Lock contention → 202
    }

    // We hold the lock — perform JIT setup.
    // Use a closure to ensure the lock is always released, even on early errors.
    let setup_result = (|| -> Result<()> {
        let target_scheme = EncryptionScheme::from_str_value(scheme_str)
            .unwrap_or(ctx.config.jit.default_target_scheme);

        // Belt-and-suspenders policy check: enforce scheme policy for schemes
        // resolved from JIT defaults (not already checked at route level).
        ctx.config.policy.check_scheme(&target_scheme)?;

        let source_config = resolve_source_config(
            content_id,
            &ctx.config,
            Some(scheme_str),
        )?;

        // Enforce container format policy on the resolved source config.
        ctx.config.policy.check_container(&source_config.container_format)?;

        let pipeline = RepackagePipeline::new(ctx.config.clone());

        let base_url = jit_base_url(content_id, fmt, scheme_str);

        pipeline.jit_setup(
            content_id,
            &source_config,
            format,
            target_scheme,
            &base_url,
        )?;

        Ok(())
    })();

    // Release lock regardless of success or failure
    let _ = cache.delete(&lock_key);

    setup_result?;
    Ok(true)
}

/// JIT manifest fallback: on cache miss, trigger setup and render manifest.
///
/// Returns `Some(HttpResponse)` if JIT handled the request, `None` if
/// source config is unavailable.
fn jit_manifest_fallback(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<Option<HttpResponse>> {
    let cache = cache::global_cache();
    let scheme_str = resolve_jit_scheme(scheme, ctx);

    match ensure_jit_setup(content_id, format, fmt, &scheme_str, ctx) {
        Ok(true) => {
            // Setup complete — read the manifest from cache and render
            let key = CacheKeys::manifest_state_for_scheme(content_id, fmt, &scheme_str);
            match cache.get(&key)? {
                Some(data) => Ok(Some(render_manifest_response(&data, format, ctx)?)),
                None => Ok(None), // Setup succeeded but no manifest — shouldn't happen
            }
        }
        Ok(false) => {
            // Lock contention — return 202 Accepted with Retry-After
            let body = serde_json::json!({
                "status": "processing",
                "content_id": content_id,
                "retry_after": 1
            });
            Ok(Some(HttpResponse::accepted_retry_after(
                serde_json::to_vec(&body).unwrap_or_default(),
                1,
            )))
        }
        Err(e) => {
            // Source config not found or other error — let it fall through to 404
            log::warn!("JIT manifest fallback failed for {content_id}: {e}");
            Ok(None)
        }
    }
}

/// JIT init segment fallback: ensure setup is done, then read init from cache.
fn jit_init_fallback(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<Option<HttpResponse>> {
    let cache = cache::global_cache();
    let scheme_str = resolve_jit_scheme(scheme, ctx);

    match ensure_jit_setup(content_id, format, fmt, &scheme_str, ctx) {
        Ok(true) => {
            // Setup complete — init segment should now be in cache
            let key = CacheKeys::init_segment_for_scheme(content_id, fmt, &scheme_str);
            match cache.get(&key)? {
                Some(data) => Ok(Some(HttpResponse::ok_with_cache(
                    data,
                    "video/mp4",
                    &format!(
                        "public, max-age={}, immutable",
                        ctx.config.cache.vod_max_age
                    ),
                ))),
                None => Ok(None),
            }
        }
        Ok(false) => {
            let body = serde_json::json!({
                "status": "processing",
                "content_id": content_id,
                "retry_after": 1
            });
            Ok(Some(HttpResponse::accepted_retry_after(
                serde_json::to_vec(&body).unwrap_or_default(),
                1,
            )))
        }
        Err(e) => {
            log::warn!("JIT init fallback failed for {content_id}: {e}");
            Ok(None)
        }
    }
}

/// JIT media segment fallback: ensure setup is done, then process segment on demand.
fn jit_segment_fallback(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    segment_number: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<Option<HttpResponse>> {
    use crate::drm::scheme::EncryptionScheme;
    use crate::repackager::pipeline::RepackagePipeline;

    let cache = cache::global_cache();
    let scheme_str = resolve_jit_scheme(scheme, ctx);

    // Ensure setup has been done first
    match ensure_jit_setup(content_id, format, fmt, &scheme_str, ctx) {
        Ok(true) => { /* Setup done, continue to segment processing */ }
        Ok(false) => {
            let body = serde_json::json!({
                "status": "processing",
                "content_id": content_id,
                "retry_after": 1
            });
            return Ok(Some(HttpResponse::accepted_retry_after(
                serde_json::to_vec(&body).unwrap_or_default(),
                1,
            )));
        }
        Err(e) => {
            log::warn!("JIT segment setup failed for {content_id}: {e}");
            return Ok(None);
        }
    }

    // Try to acquire segment-level processing lock
    let lock_key = CacheKeys::processing_lock(content_id, fmt, &format!("seg:{segment_number}"));
    let lock_ttl = ctx.config.jit.lock_ttl_seconds;
    let acquired = cache.set_nx(&lock_key, b"1", lock_ttl)?;

    if !acquired {
        // Another request is processing this segment — check cache one more time
        let seg_key = CacheKeys::media_segment_for_scheme(content_id, fmt, &scheme_str, segment_number);
        if let Some(data) = cache.get(&seg_key)? {
            return Ok(Some(HttpResponse::ok_with_cache(
                data,
                "video/mp4",
                &format!(
                    "public, max-age={}, immutable",
                    ctx.config.cache.vod_max_age
                ),
            )));
        }

        // Still processing — 202
        let body = serde_json::json!({
            "status": "processing",
            "content_id": content_id,
            "segment": segment_number,
            "retry_after": 1
        });
        return Ok(Some(HttpResponse::accepted_retry_after(
            serde_json::to_vec(&body).unwrap_or_default(),
            1,
        )));
    }

    // We hold the segment lock — process it
    let target_scheme = EncryptionScheme::from_str_value(&scheme_str)
        .unwrap_or(ctx.config.jit.default_target_scheme);

    let pipeline = RepackagePipeline::new(ctx.config.clone());

    let result = pipeline.jit_segment(content_id, format, target_scheme, segment_number);

    // Release lock
    let _ = cache.delete(&lock_key);

    match result {
        Ok(data) => Ok(Some(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        ))),
        Err(e) => {
            log::warn!("JIT segment {segment_number} failed for {content_id}: {e}");
            Ok(None) // Fall through to 404
        }
    }
}

// ─── Per-Variant Handlers (CDN Fan-Out) ────────────────────────────

/// Handle a request for a per-variant manifest.
///
/// In the CDN fan-out model, each variant is an independent cache key.
/// Uses variant-qualified cache keys: `ep:{id}:v{vid}:{fmt}_{scheme}:manifest`.
pub fn handle_variant_manifest_request(
    content_id: &str,
    format: OutputFormat,
    variant_id: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let fmt = format_str(format);
    let key = CacheKeys::variant_manifest_state(content_id, variant_id, fmt, scheme);

    let state_bytes = match cache.get(&key)? {
        Some(data) => data,
        None => {
            // JIT fallback for per-variant manifest
            if let Some(resp) = jit_manifest_fallback(content_id, format, fmt, scheme, ctx)? {
                return Ok(resp);
            }
            return Ok(HttpResponse::not_found(&format!(
                "variant {variant_id} manifest not found for {content_id}/{fmt}"
            )));
        }
    };

    render_manifest_response(&state_bytes, format, ctx)
}

/// Handle a request for a per-variant init segment.
pub fn handle_variant_init_request(
    content_id: &str,
    _format: OutputFormat,
    variant_id: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let key = CacheKeys::variant_init_segment(content_id, variant_id, scheme);

    match cache.get(&key)? {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => Ok(HttpResponse::not_found(&format!(
            "variant {variant_id} init not found for {content_id}"
        ))),
    }
}

/// Handle a request for a per-variant I-frame playlist.
pub fn handle_variant_iframe_request(
    content_id: &str,
    format: OutputFormat,
    variant_id: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    // DASH trick play is embedded in the regular MPD — no separate per-variant iframe endpoint
    if format == OutputFormat::Dash {
        return Ok(HttpResponse::not_found(
            "DASH trick play is embedded in the regular MPD — use the per-variant manifest instead"
        ));
    }

    let cache = cache::global_cache();
    let fmt = format_str(format);
    let key = CacheKeys::variant_manifest_state(content_id, variant_id, fmt, scheme);

    match cache.get(&key)? {
        Some(state_bytes) => {
            let state: ManifestState = serde_json::from_slice(&state_bytes)
                .map_err(|e| crate::error::EdgepackError::Cache(format!("deserialize manifest: {e}")))?;
            let rendered = manifest::render_iframe_manifest(&state)?;
            match rendered {
                Some(body) => Ok(HttpResponse::ok_with_cache(
                    body.into_bytes(),
                    format.content_type(),
                    &format!(
                        "public, max-age={}, immutable",
                        ctx.config.cache.vod_max_age
                    ),
                )),
                None => Ok(HttpResponse::not_found(
                    "I-frame playlist not available for this variant"
                )),
            }
        }
        None => Ok(HttpResponse::not_found(&format!(
            "variant {variant_id} iframe manifest not found for {content_id}"
        ))),
    }
}

/// Handle a request for a per-variant media segment.
pub fn handle_variant_segment_request(
    content_id: &str,
    _format: OutputFormat,
    variant_id: u32,
    segment_number: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let cache = cache::global_cache();
    let key = CacheKeys::variant_media_segment(content_id, variant_id, segment_number, scheme);

    match cache.get(&key)? {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => Ok(HttpResponse::not_found(&format!(
            "variant {variant_id} segment {segment_number} not found for {content_id}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_helpers::test_context;

    #[test]
    fn handle_manifest_request_hls_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("req-hls-mfst-1", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("manifest not found"));
    }

    #[test]
    fn handle_manifest_request_dash_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("req-dash-mfst-2", OutputFormat::Dash, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("dash"));
    }

    #[test]
    fn handle_manifest_request_with_scheme_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("req-scheme-mfst-3", OutputFormat::Hls, Some("cenc"), &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn handle_init_segment_request_not_found() {
        let ctx = test_context();
        let resp = handle_init_segment_request("req-init-4", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("init segment not found"));
    }

    #[test]
    fn handle_media_segment_request_not_found() {
        let ctx = test_context();
        let resp =
            handle_media_segment_request("req-seg-5", OutputFormat::Hls, 5, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 5 not found"));
    }

    #[test]
    fn handle_media_segment_request_different_numbers() {
        let ctx = test_context();
        let resp = handle_media_segment_request("req-segnum-6", OutputFormat::Dash, 0, None, &ctx).unwrap();
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 0"));

        let resp = handle_media_segment_request("req-segnum-6b", OutputFormat::Dash, 42, None, &ctx).unwrap();
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 42"));
    }

    #[test]
    fn handle_iframe_manifest_hls_not_found() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("req-iframe-7", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("manifest not found"));
    }

    #[test]
    fn handle_iframe_manifest_dash_returns_404() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("req-iframe-8", OutputFormat::Dash, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("embedded in the regular MPD"));
    }

    #[test]
    fn handle_iframe_manifest_with_scheme_not_found() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("req-iframe-9", OutputFormat::Hls, Some("cenc"), &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }
}
