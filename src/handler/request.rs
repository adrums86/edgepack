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
pub fn handle_manifest_request(
    content_id: &str,
    format: OutputFormat,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = CacheKeys::manifest_state(content_id, fmt);

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

/// Handle a request for the init segment.
///
/// Init segments are immutable once created — always served with long cache TTL.
pub fn handle_init_segment_request(
    content_id: &str,
    format: OutputFormat,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = CacheKeys::init_segment(content_id, fmt);

    match ctx.cache.get(&key)? {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => Ok(HttpResponse::not_found(&format!(
            "init segment not found for {content_id}/{fmt}"
        ))),
    }
}

/// Handle a request for a media segment.
///
/// Segments are immutable once created — always served with long cache TTL.
pub fn handle_media_segment_request(
    content_id: &str,
    format: OutputFormat,
    segment_number: u32,
    ctx: &HandlerContext,
) -> Result<HttpResponse> {
    let fmt = format_str(format);
    let key = CacheKeys::media_segment(content_id, fmt, segment_number);

    match ctx.cache.get(&key)? {
        Some(data) => Ok(HttpResponse::ok_with_cache(
            data,
            "video/mp4",
            &format!(
                "public, max-age={}, immutable",
                ctx.config.cache.vod_max_age
            ),
        )),
        None => Ok(HttpResponse::not_found(&format!(
            "segment {segment_number} not found for {content_id}/{fmt}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_helpers::test_context;

    #[test]
    fn handle_manifest_request_hls_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("content-1", OutputFormat::Hls, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("manifest not found"));
    }

    #[test]
    fn handle_manifest_request_dash_not_found() {
        let ctx = test_context();
        let resp = handle_manifest_request("content-2", OutputFormat::Dash, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("dash"));
    }

    #[test]
    fn handle_init_segment_request_not_found() {
        let ctx = test_context();
        let resp = handle_init_segment_request("content-1", OutputFormat::Hls, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("init segment not found"));
    }

    #[test]
    fn handle_media_segment_request_not_found() {
        let ctx = test_context();
        let resp =
            handle_media_segment_request("content-1", OutputFormat::Hls, 5, &ctx).unwrap();
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 5 not found"));
    }

    #[test]
    fn handle_media_segment_request_different_numbers() {
        let ctx = test_context();
        let resp = handle_media_segment_request("c", OutputFormat::Dash, 0, &ctx).unwrap();
        assert!(String::from_utf8_lossy(&resp.body).contains("segment 0"));

        let resp = handle_media_segment_request("c", OutputFormat::Dash, 42, &ctx).unwrap();
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
}
