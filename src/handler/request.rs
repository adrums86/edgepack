use crate::cache::CacheKeys;
use crate::error::Result;
use crate::handler::{format_str, HandlerContext, HttpResponse};
use crate::manifest;
use crate::manifest::types::{ManifestPhase, ManifestState, OutputFormat};
use crate::repackager::JobStatus;

/// Handle a request for a manifest.
///
/// Looks up ManifestState from Redis, renders it, and returns with
/// appropriate cache headers based on whether the manifest is live or complete.
/// When `scheme` is provided, uses scheme-qualified cache keys.
///
/// When JIT is enabled (feature = "jit"), a cache miss triggers on-demand
/// setup: fetch source manifest, init segment, and DRM keys, then render.
pub fn handle_manifest_request(
    content_id: &str,
    format: OutputFormat,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::manifest_state_for_scheme(content_id, fmt, s)
    } else {
        CacheKeys::manifest_state(content_id, fmt)
    };

    let state_bytes = match ctx.cache.get(&key)? {
        Some(data) => data,
        None => {
            // JIT fallback: trigger on-demand setup on cache miss
            #[cfg(feature = "jit")]
            {
                if let Some(resp) = jit_manifest_fallback(content_id, format, fmt, scheme, ctx)? {
                    return Ok(resp);
                }
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
/// When JIT is enabled, a cache miss triggers JIT setup (which caches the init
/// segment), then reads it back from cache.
pub fn handle_init_segment_request(
    content_id: &str,
    format: OutputFormat,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::init_segment_for_scheme(content_id, fmt, s)
    } else {
        CacheKeys::init_segment(content_id, fmt)
    };

    match ctx.cache.get(&key)? {
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
            #[cfg(feature = "jit")]
            {
                if let Some(resp) = jit_init_fallback(content_id, format, fmt, scheme, ctx)? {
                    return Ok(resp);
                }
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
/// When JIT is enabled, a cache miss triggers on-demand segment processing:
/// fetches the source segment, decrypts, re-encrypts, and caches the result.
pub fn handle_media_segment_request(
    content_id: &str,
    format: OutputFormat,
    segment_number: u32,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::media_segment_for_scheme(content_id, fmt, s, segment_number)
    } else {
        CacheKeys::media_segment(content_id, fmt, segment_number)
    };

    match ctx.cache.get(&key)? {
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
            #[cfg(feature = "jit")]
            {
                if let Some(resp) = jit_segment_fallback(content_id, format, fmt, segment_number, scheme, ctx)? {
                    return Ok(resp);
                }
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

    let fmt = format_str(format);
    let key = if let Some(s) = scheme {
        CacheKeys::manifest_state_for_scheme(content_id, fmt, s)
    } else {
        CacheKeys::manifest_state(content_id, fmt)
    };

    let state_bytes = match ctx.cache.get(&key)? {
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
            let cache_control = match state.phase {
                ManifestPhase::Complete => format!(
                    "public, max-age={}, immutable",
                    ctx.config.cache.vod_max_age
                ),
                ManifestPhase::Live => format!(
                    "public, max-age={m}, s-maxage={m}",
                    m = ctx.config.cache.live_manifest_max_age
                ),
                ManifestPhase::AwaitingFirstSegment => "no-cache".to_string(),
            };
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

/// Handle a request for job status.
pub fn handle_status_request(
    content_id: &str,
    format: OutputFormat,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = CacheKeys::job_state(content_id, fmt);

    match ctx.cache.get(&key)? {
        Some(data) => {
            // Validate it's valid JSON by attempting to deserialize
            let _status: JobStatus = serde_json::from_slice(&data).map_err(|e| {
                crate::error::EdgepackError::Cache(format!("deserialize job status: {e}"))
            })?;

            Ok(HttpResponse::ok_with_cache(
                data,
                "application/json",
                "no-cache",
            ))
        }
        None => Ok(HttpResponse::not_found(&format!(
            "no job found for {content_id}/{fmt}"
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

    let cache_control = match state.phase {
        ManifestPhase::Complete => format!(
            "public, max-age={}, immutable",
            ctx.config.cache.vod_max_age
        ),
        ManifestPhase::Live => format!(
            "public, max-age={m}, s-maxage={m}",
            m = ctx.config.cache.live_manifest_max_age
        ),
        ManifestPhase::AwaitingFirstSegment => "no-cache".to_string(),
    };

    Ok(HttpResponse::ok_with_cache(
        manifest_body.into_bytes(),
        format.content_type(),
        &cache_control,
    ))
}

// ---------------------------------------------------------------------------
// JIT Fallback Handlers (feature = "jit")
// ---------------------------------------------------------------------------

/// Resolve the effective scheme string for JIT operations.
///
/// Uses the URL scheme if present, otherwise falls back to the JIT default.
#[cfg(feature = "jit")]
fn resolve_jit_scheme(scheme: Option<&str>, ctx: &HandlerContext) -> String {
    scheme
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.config.jit.default_target_scheme.scheme_type_str().to_string())
}

/// Build a base URL for manifest references (e.g. init/segment URIs).
///
/// Given content_id, format, and scheme, produces `/repackage/{id}/{fmt}_{scheme}/`.
#[cfg(feature = "jit")]
fn jit_base_url(content_id: &str, fmt: &str, scheme_str: &str) -> String {
    format!("/repackage/{content_id}/{fmt}_{scheme_str}/")
}

/// Ensure JIT setup has been performed for the given content/format/scheme.
///
/// If setup is already done (marker key exists), returns Ok(true).
/// If we acquire the lock and perform setup, returns Ok(true).
/// If another request holds the lock (contention), returns Ok(false) — caller
/// should return 202 Accepted with Retry-After.
#[cfg(feature = "jit")]
fn ensure_jit_setup(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme_str: &str,
    ctx: &HandlerContext,
) -> Result<bool> {
    use crate::drm::scheme::EncryptionScheme;
    use crate::repackager::pipeline::{resolve_source_config, RepackagePipeline};

    // Check if setup is already done
    let setup_key = CacheKeys::jit_setup(content_id, fmt);
    if ctx.cache.exists(&setup_key)? {
        return Ok(true);
    }

    // Try to acquire the processing lock
    let lock_key = CacheKeys::processing_lock(content_id, fmt, "setup");
    let lock_ttl = ctx.config.jit.lock_ttl_seconds;
    let acquired = ctx.cache.set_nx(&lock_key, b"1", lock_ttl)?;

    if !acquired {
        // Another request is processing — check cache one more time
        if ctx.cache.exists(&setup_key)? {
            return Ok(true);
        }
        return Ok(false); // Lock contention → 202
    }

    // We hold the lock — perform JIT setup
    let target_scheme = EncryptionScheme::from_str_value(scheme_str)
        .unwrap_or(ctx.config.jit.default_target_scheme);

    let source_config = resolve_source_config(
        ctx.cache.as_ref(),
        content_id,
        &ctx.config,
        Some(scheme_str),
    )?;

    let cache = crate::cache::create_backend(&ctx.config)?;
    let pipeline = RepackagePipeline::new(ctx.config.clone(), cache);

    let base_url = jit_base_url(content_id, fmt, scheme_str);

    let result = pipeline.jit_setup(
        content_id,
        &source_config,
        format,
        target_scheme,
        &base_url,
    );

    // Release lock regardless of success
    let _ = ctx.cache.delete(&lock_key);

    result?;
    Ok(true)
}

/// JIT manifest fallback: on cache miss, trigger setup and render manifest.
///
/// Returns `Some(HttpResponse)` if JIT handled the request, `None` if JIT is
/// disabled or source config is unavailable.
#[cfg(feature = "jit")]
fn jit_manifest_fallback(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<Option<HttpResponse>> {
    if !ctx.config.jit.enabled {
        return Ok(None);
    }

    let scheme_str = resolve_jit_scheme(scheme, ctx);

    match ensure_jit_setup(content_id, format, fmt, &scheme_str, ctx) {
        Ok(true) => {
            // Setup complete — read the manifest from cache and render
            let key = CacheKeys::manifest_state_for_scheme(content_id, fmt, &scheme_str);
            match ctx.cache.get(&key)? {
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
#[cfg(feature = "jit")]
fn jit_init_fallback(
    content_id: &str,
    format: OutputFormat,
    fmt: &str,
    scheme: Option<&str>,
    ctx: &HandlerContext,
) -> Result<Option<HttpResponse>> {
    if !ctx.config.jit.enabled {
        return Ok(None);
    }

    let scheme_str = resolve_jit_scheme(scheme, ctx);

    match ensure_jit_setup(content_id, format, fmt, &scheme_str, ctx) {
        Ok(true) => {
            // Setup complete — init segment should now be in cache
            let key = CacheKeys::init_segment_for_scheme(content_id, fmt, &scheme_str);
            match ctx.cache.get(&key)? {
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
#[cfg(feature = "jit")]
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

    if !ctx.config.jit.enabled {
        return Ok(None);
    }

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
    let acquired = ctx.cache.set_nx(&lock_key, b"1", lock_ttl)?;

    if !acquired {
        // Another request is processing this segment — check cache one more time
        let seg_key = CacheKeys::media_segment_for_scheme(content_id, fmt, &scheme_str, segment_number);
        if let Some(data) = ctx.cache.get(&seg_key)? {
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

    let cache = crate::cache::create_backend(&ctx.config)?;
    let pipeline = RepackagePipeline::new(ctx.config.clone(), cache);

    let result = pipeline.jit_segment(content_id, format, target_scheme, segment_number);

    // Release lock
    let _ = ctx.cache.delete(&lock_key);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_helpers::test_context;

    #[test]
    fn handle_manifest_request_hls_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("content-1", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("manifest not found"));
    }

    #[test]
    fn handle_manifest_request_dash_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("content-2", OutputFormat::Dash, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("dash"));
    }

    #[test]
    fn handle_manifest_request_with_scheme_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("content-1", OutputFormat::Hls, Some("cenc"), &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn handle_init_segment_request_not_found() {
        let ctx = test_context();
        let resp = handle_init_segment_request("content-1", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("init segment not found"));
    }

    #[test]
    fn handle_media_segment_request_not_found() {
        let ctx = test_context();
        let resp =
            handle_media_segment_request("content-1", OutputFormat::Hls, 5, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 5 not found"));
    }

    #[test]
    fn handle_media_segment_request_different_numbers() {
        let ctx = test_context();
        let resp = handle_media_segment_request("c", OutputFormat::Dash, 0, None, &ctx).unwrap();
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 0"));

        let resp = handle_media_segment_request("c", OutputFormat::Dash, 42, None, &ctx).unwrap();
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 42"));
    }

    #[test]
    fn handle_status_request_not_found() {
        let ctx = test_context();
        let resp = handle_status_request("content-1", OutputFormat::Hls, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("no job found"));
    }

    #[test]
    fn handle_status_request_dash_not_found() {
        let ctx = test_context();
        let resp = handle_status_request("content-99", OutputFormat::Dash, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("content-99"));
    }

    #[test]
    fn handle_iframe_manifest_hls_not_found() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("content-1", OutputFormat::Hls, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("manifest not found"));
    }

    #[test]
    fn handle_iframe_manifest_dash_returns_404() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("content-1", OutputFormat::Dash, None, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("embedded in the regular MPD"));
    }

    #[test]
    fn handle_iframe_manifest_with_scheme_not_found() {
        let ctx = test_context();
        let resp = handle_iframe_manifest_request("content-1", OutputFormat::Hls, Some("cenc"), &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }
}
