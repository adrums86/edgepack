use crate::cache::CacheKeys;
use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::handler::{format_str, HandlerContext, HttpRequest, HttpResponse};
use crate::manifest::types::OutputFormat;
use crate::repackager::pipeline::RepackagePipeline;
use crate::repackager::{JobState, JobStatus, RepackageRequest};
use serde::{Deserialize, Serialize};

/// Webhook payload for triggering a repackaging job.
///
/// Accepts either `target_schemes` (array) or `target_scheme` (single string, backward compat).
/// If both are provided, `target_schemes` takes precedence. If neither, defaults to `["cenc"]`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// Unique content identifier.
    pub content_id: String,
    /// Source manifest URL (HLS or DASH).
    pub source_url: String,
    /// Output format: "hls" or "dash".
    pub format: String,
    /// Target encryption schemes (array). Takes precedence over `target_scheme`.
    #[serde(default)]
    pub target_schemes: Vec<String>,
    /// Target encryption scheme (single, backward compat). Used when `target_schemes` is empty.
    #[serde(default)]
    pub target_scheme: Option<String>,
    /// Target container format: "cmaf" or "fmp4" (default: "cmaf").
    #[serde(default = "default_container_format_str")]
    pub container_format: String,
    /// Optional key IDs to request (hex strings).
    #[serde(default)]
    pub key_ids: Vec<String>,
    /// Raw encryption keys (bypass SPEKE). Hex-encoded KID/key/IV.
    #[serde(default)]
    pub raw_keys: Vec<RawKeyInput>,
    /// Key rotation configuration.
    #[serde(default)]
    pub key_rotation: Option<KeyRotationInput>,
    /// Number of initial clear (unencrypted) segments.
    #[serde(default)]
    pub clear_lead_segments: Option<u32>,
    /// Explicit DRM systems to include (e.g. ["widevine", "clearkey"]).
    #[serde(default)]
    pub drm_systems: Vec<String>,
    /// Enable I-frame / trick play playlist generation.
    #[serde(default)]
    pub enable_iframe_playlist: Option<bool>,
    /// DVR sliding window duration in seconds. When set, live manifests use a sliding
    /// window instead of EVENT type. None = all segments rendered (EVENT playlist).
    #[serde(default)]
    pub dvr_window_duration: Option<f64>,
    /// Content steering configuration. When set, output manifests include steering directives.
    #[serde(default)]
    pub content_steering: Option<ContentSteeringInput>,
}

/// Raw key input from webhook (hex-encoded strings).
#[derive(Debug, Serialize, Deserialize)]
pub struct RawKeyInput {
    pub kid: String,
    pub key: String,
    #[serde(default)]
    pub iv: Option<String>,
}

/// Key rotation input from webhook.
#[derive(Debug, Serialize, Deserialize)]
pub struct KeyRotationInput {
    pub period_segments: u32,
}

/// Content steering input from webhook.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContentSteeringInput {
    /// Steering server URI (required).
    pub server_uri: String,
    /// Default pathway ID (HLS) / service location (DASH).
    #[serde(default)]
    pub default_pathway_id: Option<String>,
    /// Whether to query the steering server before playback starts (DASH only).
    #[serde(default)]
    pub query_before_start: Option<bool>,
}

impl WebhookPayload {
    /// Resolve the effective list of target scheme strings.
    ///
    /// Priority: `target_schemes` (if non-empty) > `target_scheme` (if present) > default `["cenc"]`.
    pub fn resolved_target_schemes(&self) -> Vec<String> {
        if !self.target_schemes.is_empty() {
            self.target_schemes.clone()
        } else if let Some(ref single) = self.target_scheme {
            vec![single.clone()]
        } else {
            vec!["cenc".to_string()]
        }
    }
}

fn default_container_format_str() -> String {
    "cmaf".to_string()
}

/// Webhook response returned after first manifest publishes.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookResponse {
    pub status: String,
    pub content_id: String,
    pub format: String,
    /// Manifest URLs keyed by scheme name (e.g. {"cenc": "/repackage/id/hls_cenc/manifest"}).
    pub manifest_urls: std::collections::HashMap<String, String>,
    pub segments_completed: u32,
    pub segments_total: Option<u32>,
}

/// Continue payload for internal self-invocation chaining.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContinuePayload {
    pub content_id: String,
    pub format: String,
}

fn hex_decode_16(hex: &str, field_name: &str) -> Result<[u8; 16]> {
    let hex = hex.trim();
    if hex.len() != 32 {
        return Err(EdgepackError::InvalidInput(format!(
            "{field_name} must be 32 hex characters (16 bytes), got {} chars", hex.len()
        )));
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
            EdgepackError::InvalidInput(format!("invalid hex in {field_name} at position {}", i * 2))
        })?;
    }
    Ok(bytes)
}

fn parse_raw_keys(inputs: &[RawKeyInput]) -> Result<Vec<crate::repackager::RawKeyEntry>> {
    inputs.iter().map(|rk| {
        let kid = hex_decode_16(&rk.kid, "kid")?;
        let key = hex_decode_16(&rk.key, "key")?;
        let iv = match &rk.iv {
            Some(iv_hex) => Some(hex_decode_16(iv_hex, "iv")?),
            None => None,
        };
        Ok(crate::repackager::RawKeyEntry { kid, key, iv })
    }).collect()
}

/// Handle a POST /webhook/repackage request.
///
/// Validates the payload, executes the pipeline through the first segment (producing
/// a live manifest), chains remaining processing via self-invocation, and returns 200.
pub fn handle_repackage_webhook(req: &HttpRequest, ctx: &HandlerContext) -> Result<HttpResponse> {
    let body = req
        .body
        .as_ref()
        .ok_or_else(|| EdgepackError::InvalidInput("missing request body".into()))?;

    let payload: WebhookPayload = serde_json::from_slice(body)
        .map_err(|e| EdgepackError::InvalidInput(format!("invalid JSON: {e}")))?;

    // Validate format
    let output_format = match payload.format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        other => {
            return Err(EdgepackError::InvalidInput(format!(
                "invalid format: {other} (expected 'hls' or 'dash')"
            )));
        }
    };

    // Validate source URL
    if payload.source_url.is_empty() {
        return Err(EdgepackError::InvalidInput(
            "source_url is required".into(),
        ));
    }

    // Parse target encryption schemes
    let scheme_strings = payload.resolved_target_schemes();
    if scheme_strings.is_empty() {
        return Err(EdgepackError::InvalidInput(
            "at least one target scheme is required".into(),
        ));
    }
    let mut target_schemes = Vec::with_capacity(scheme_strings.len());
    for s in &scheme_strings {
        let scheme = match s.as_str() {
            "cenc" => EncryptionScheme::Cenc,
            "cbcs" => EncryptionScheme::Cbcs,
            "none" => EncryptionScheme::None,
            other => {
                return Err(EdgepackError::InvalidInput(format!(
                    "invalid target_scheme: {other} (expected 'cenc', 'cbcs', or 'none')"
                )));
            }
        };
        if target_schemes.contains(&scheme) {
            return Err(EdgepackError::InvalidInput(format!(
                "duplicate target_scheme: {s}"
            )));
        }
        target_schemes.push(scheme);
    }

    // Parse container format
    let container_format = crate::media::container::ContainerFormat::from_str_value(
        &payload.container_format,
    )
    .ok_or_else(|| {
        EdgepackError::InvalidInput(format!(
            "invalid container_format: {} (expected 'cmaf', 'fmp4', or 'iso')",
            payload.container_format
        ))
    })?;

    // Validate DRM systems
    let valid_drm_systems = ["widevine", "playready", "fairplay", "clearkey"];
    for sys in &payload.drm_systems {
        if !valid_drm_systems.contains(&sys.as_str()) {
            return Err(EdgepackError::InvalidInput(format!(
                "invalid drm_system: {sys} (expected one of: {})",
                valid_drm_systems.join(", ")
            )));
        }
    }

    // Parse raw keys
    let raw_keys = parse_raw_keys(&payload.raw_keys)?;

    // Parse key rotation
    let key_rotation = payload.key_rotation.as_ref().map(|kr| {
        crate::repackager::KeyRotationConfig { period_segments: kr.period_segments }
    });

    // Validate DVR window duration
    if let Some(dvr_window) = payload.dvr_window_duration {
        if dvr_window <= 0.0 {
            return Err(EdgepackError::InvalidInput(
                "dvr_window_duration must be a positive number".into(),
            ));
        }
    }

    // Parse content steering
    let content_steering = match &payload.content_steering {
        Some(cs) => {
            if cs.server_uri.is_empty() {
                return Err(EdgepackError::InvalidInput(
                    "content_steering.server_uri must not be empty".into(),
                ));
            }
            Some(crate::manifest::types::ContentSteeringConfig {
                server_uri: cs.server_uri.clone(),
                default_pathway_id: cs.default_pathway_id.clone(),
                query_before_start: cs.query_before_start,
            })
        }
        None => None,
    };

    let request = RepackageRequest {
        content_id: payload.content_id.clone(),
        source_url: payload.source_url,
        output_format,
        target_schemes: target_schemes.clone(),
        container_format,
        key_ids: payload.key_ids,
        raw_keys,
        key_rotation,
        clear_lead_segments: payload.clear_lead_segments,
        drm_systems: payload.drm_systems,
        enable_iframe_playlist: payload.enable_iframe_playlist.unwrap_or(false),
        dvr_window_duration: payload.dvr_window_duration,
        content_steering,
    };

    // Hybrid mode (JIT feature): if JIT has already set up this content,
    // skip pipeline execution and return success immediately. This prevents
    // duplicate work when both JIT and webhook target the same content.
    #[cfg(feature = "jit")]
    {
        let fmt = format_str(output_format);
        if ctx.cache.exists(&CacheKeys::jit_setup(&payload.content_id, fmt))? {
            let mut manifest_urls = std::collections::HashMap::new();
            for scheme in &target_schemes {
                let scheme_str = scheme.scheme_type_str();
                let manifest_path = format!(
                    "/repackage/{}/{}_{}/manifest",
                    payload.content_id, payload.format, scheme_str
                );
                manifest_urls.insert(scheme_str.to_string(), manifest_path);
            }
            let response = WebhookResponse {
                status: "complete".to_string(),
                content_id: payload.content_id,
                format: payload.format,
                manifest_urls,
                segments_completed: 0,
                segments_total: None,
            };
            let resp_body = serde_json::to_vec(&response)
                .map_err(|e| EdgepackError::Io(format!("serialize response: {e}")))?;
            return Ok(HttpResponse::ok(resp_body, "application/json"));
        }
    }

    // Create a pipeline with a fresh cache backend
    let cache = crate::cache::create_backend(&ctx.config)?;
    let pipeline = RepackagePipeline::new(ctx.config.clone(), cache);

    // Execute through first segment — produces a live manifest
    let job_status = match pipeline.execute_first(&request) {
        Ok(status) => status,
        Err(e) => {
            // Log error and return 500
            return Ok(HttpResponse::error(
                500,
                &format!("pipeline execution failed: {e}"),
            ));
        }
    };

    // If there are remaining segments, fire self-invocation to continue processing.
    // This is a fire-and-forget: we don't wait for the response.
    if job_status.state != JobState::Complete {
        let continue_body = serde_json::to_vec(&ContinuePayload {
            content_id: payload.content_id.clone(),
            format: payload.format.clone(),
        })
        .unwrap_or_default();

        // Best-effort self-invocation — failures here are non-fatal since the
        // content is already partially available via the live manifest.
        let _ = crate::http_client::post(
            "/webhook/repackage/continue",
            &[("Content-Type".to_string(), "application/json".to_string())],
            continue_body,
        );
    }

    let mut manifest_urls = std::collections::HashMap::new();
    for scheme in &target_schemes {
        let scheme_str = scheme.scheme_type_str();
        let manifest_path = format!(
            "/repackage/{}/{}_{}/manifest",
            payload.content_id, payload.format, scheme_str
        );
        manifest_urls.insert(scheme_str.to_string(), manifest_path);
    }

    let response = WebhookResponse {
        status: "processing".to_string(),
        content_id: payload.content_id,
        format: payload.format,
        manifest_urls,
        segments_completed: job_status.segments_completed,
        segments_total: job_status.segments_total,
    };

    let resp_body = serde_json::to_vec(&response)
        .map_err(|e| EdgepackError::Io(format!("serialize response: {e}")))?;

    Ok(HttpResponse::ok(resp_body, "application/json"))
}

/// Payload for POST /config/source — per-content source configuration for JIT.
#[cfg(feature = "jit")]
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceConfigPayload {
    /// Unique content identifier.
    pub content_id: String,
    /// Source manifest URL.
    pub source_url: String,
    /// Target encryption schemes (optional, defaults to ["cenc"]).
    #[serde(default)]
    pub target_schemes: Vec<String>,
    /// Container format (optional, defaults to "cmaf").
    #[serde(default = "default_container_format_str")]
    pub container_format: String,
}

/// Handle a POST /config/source request.
///
/// Stores per-content source configuration in Redis for JIT packaging. This allows
/// GET requests to know where to fetch the source manifest from.
#[cfg(feature = "jit")]
pub fn handle_source_config(req: &HttpRequest, ctx: &HandlerContext) -> Result<HttpResponse> {
    let body = req
        .body
        .as_ref()
        .ok_or_else(|| EdgepackError::InvalidInput("missing request body".into()))?;

    let payload: SourceConfigPayload = serde_json::from_slice(body)
        .map_err(|e| EdgepackError::InvalidInput(format!("invalid JSON: {e}")))?;

    // Validate content_id
    if payload.content_id.is_empty() {
        return Err(EdgepackError::InvalidInput(
            "content_id is required".into(),
        ));
    }

    // Validate source_url
    if payload.source_url.is_empty() {
        return Err(EdgepackError::InvalidInput(
            "source_url is required".into(),
        ));
    }

    // Parse target schemes
    let target_schemes = if payload.target_schemes.is_empty() {
        vec![EncryptionScheme::Cenc]
    } else {
        let mut schemes = Vec::with_capacity(payload.target_schemes.len());
        for s in &payload.target_schemes {
            let scheme = match s.as_str() {
                "cenc" => EncryptionScheme::Cenc,
                "cbcs" => EncryptionScheme::Cbcs,
                "none" => EncryptionScheme::None,
                other => {
                    return Err(EdgepackError::InvalidInput(format!(
                        "invalid target_scheme: {other}"
                    )));
                }
            };
            schemes.push(scheme);
        }
        schemes
    };

    // Parse container format
    let container_format = crate::media::container::ContainerFormat::from_str_value(
        &payload.container_format,
    )
    .ok_or_else(|| {
        EdgepackError::InvalidInput(format!(
            "invalid container_format: {}",
            payload.container_format
        ))
    })?;

    // Build and store SourceConfig
    let source_config = crate::repackager::SourceConfig {
        source_url: payload.source_url,
        target_schemes,
        container_format,
    };

    let serialized = serde_json::to_vec(&source_config)
        .map_err(|e| EdgepackError::Io(format!("serialize source config: {e}")))?;

    let cache_key = CacheKeys::source_config(&payload.content_id);
    ctx.cache.set(&cache_key, &serialized, ctx.config.cache.job_state_ttl)?;

    let resp_body = serde_json::to_vec(&serde_json::json!({
        "status": "ok",
        "content_id": payload.content_id,
    }))
    .unwrap_or_default();

    Ok(HttpResponse::ok(resp_body, "application/json"))
}

/// Handle a POST /webhook/repackage/continue request.
///
/// This is an internal endpoint used for self-invocation chaining. It processes
/// the next segment(s) and chains further if more remain.
pub fn handle_continue(req: &HttpRequest, ctx: &HandlerContext) -> Result<HttpResponse> {
    let body = req
        .body
        .as_ref()
        .ok_or_else(|| EdgepackError::InvalidInput("missing request body".into()))?;

    let payload: ContinuePayload = serde_json::from_slice(body)
        .map_err(|e| EdgepackError::InvalidInput(format!("invalid JSON: {e}")))?;

    let output_format = match payload.format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        other => {
            return Err(EdgepackError::InvalidInput(format!(
                "invalid format: {other}"
            )));
        }
    };

    let fmt = format_str(output_format);

    // Check if job exists
    let job_key = CacheKeys::job_state(&payload.content_id, fmt);
    let job_data = match ctx.cache.get(&job_key)? {
        Some(data) => data,
        None => {
            return Ok(HttpResponse::not_found(&format!(
                "no job found for {}/{}",
                payload.content_id, fmt
            )));
        }
    };

    let job_status: JobStatus = serde_json::from_slice(&job_data).map_err(|e| {
        EdgepackError::Cache(format!("deserialize job status: {e}"))
    })?;

    // If already complete, nothing to do
    if job_status.state == JobState::Complete {
        let resp_body = serde_json::to_vec(&serde_json::json!({
            "status": "complete",
            "content_id": payload.content_id,
            "format": fmt,
            "segments_completed": job_status.segments_completed,
            "segments_total": job_status.segments_total,
        }))
        .unwrap_or_default();

        return Ok(HttpResponse::ok(resp_body, "application/json"));
    }

    // Create pipeline and execute remaining
    let cache = crate::cache::create_backend(&ctx.config)?;
    let pipeline = RepackagePipeline::new(ctx.config.clone(), cache);

    let updated_status = match pipeline.execute_remaining(&payload.content_id, output_format) {
        Ok(status) => status,
        Err(e) => {
            return Ok(HttpResponse::error(
                500,
                &format!("continue execution failed: {e}"),
            ));
        }
    };

    // If still not complete, chain another self-invocation
    if updated_status.state != JobState::Complete {
        let continue_body = serde_json::to_vec(&ContinuePayload {
            content_id: payload.content_id.clone(),
            format: payload.format.clone(),
        })
        .unwrap_or_default();

        let _ = crate::http_client::post(
            "/webhook/repackage/continue",
            &[("Content-Type".to_string(), "application/json".to_string())],
            continue_body,
        );
    }

    let resp_body = serde_json::to_vec(&serde_json::json!({
        "status": if updated_status.state == JobState::Complete { "complete" } else { "processing" },
        "content_id": payload.content_id,
        "format": fmt,
        "segments_completed": updated_status.segments_completed,
        "segments_total": updated_status.segments_total,
    }))
    .unwrap_or_default();

    Ok(HttpResponse::ok(resp_body, "application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_helpers::test_context;
    use crate::handler::HttpMethod;

    fn make_webhook_request(body: Option<Vec<u8>>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path: "/webhook/repackage".to_string(),
            headers: vec![],
            body,
        }
    }

    fn make_continue_request(body: Option<Vec<u8>>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path: "/webhook/repackage/continue".to_string(),
            headers: vec![],
            body,
        }
    }

    #[test]
    fn valid_hls_webhook() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "movie-123",
            "source_url": "https://cdn.example.com/manifest.m3u8",
            "format": "hls"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req, &ctx).unwrap();
        // On native targets, pipeline fails (HTTP client not available) → 500
        // On WASI targets, would succeed → 200
        assert!(resp.status == 200 || resp.status == 500);
    }

    #[test]
    fn valid_dash_webhook() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "show-456",
            "source_url": "https://cdn.example.com/manifest.mpd",
            "format": "dash"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req, &ctx).unwrap();
        assert!(resp.status == 200 || resp.status == 500);
    }

    #[test]
    fn webhook_with_key_ids() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "content-1",
            "source_url": "https://example.com/manifest.m3u8",
            "format": "hls",
            "key_ids": ["aabbccdd", "11223344"]
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req, &ctx).unwrap();
        assert!(resp.status == 200 || resp.status == 500);
    }

    #[test]
    fn webhook_missing_body() {
        let ctx = test_context();
        let req = make_webhook_request(None);
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing request body"));
    }

    #[test]
    fn webhook_invalid_json() {
        let ctx = test_context();
        let req = make_webhook_request(Some(b"not json".to_vec()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid JSON"));
    }

    #[test]
    fn webhook_invalid_format() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source",
            "format": "mp4"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid format: mp4"));
    }

    #[test]
    fn webhook_empty_source_url() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "",
            "format": "hls"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_url is required"));
    }

    #[test]
    fn webhook_missing_required_field() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid JSON"));
    }

    #[test]
    fn webhook_payload_serde_roundtrip() {
        let payload = WebhookPayload {
            content_id: "c1".into(),
            source_url: "https://example.com".into(),
            format: "hls".into(),
            target_schemes: vec!["cenc".into()],
            target_scheme: None,
            container_format: "cmaf".into(),
            key_ids: vec!["aabb".into()],
            raw_keys: vec![],
            key_rotation: None,
            clear_lead_segments: None,
            drm_systems: vec![],
            enable_iframe_playlist: None,
            dvr_window_duration: None,
            content_steering: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: WebhookPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "c1");
        assert_eq!(parsed.resolved_target_schemes(), vec!["cenc"]);
        assert_eq!(parsed.container_format, "cmaf");
        assert_eq!(parsed.key_ids.len(), 1);
        assert!(parsed.enable_iframe_playlist.is_none());
        assert!(parsed.dvr_window_duration.is_none());
        assert!(parsed.content_steering.is_none());
    }

    #[test]
    fn webhook_payload_default_key_ids_and_scheme() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.key_ids.is_empty());
        assert_eq!(parsed.resolved_target_schemes(), vec!["cenc"]);
        assert_eq!(parsed.container_format, "cmaf");
    }

    #[test]
    fn webhook_payload_backward_compat_single_scheme() {
        // Old API: target_scheme (singular) still works
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","target_scheme":"cbcs"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resolved_target_schemes(), vec!["cbcs"]);
    }

    #[test]
    fn webhook_payload_multi_scheme() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","target_schemes":["cenc","cbcs"]}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resolved_target_schemes(), vec!["cenc", "cbcs"]);
    }

    #[test]
    fn webhook_payload_target_schemes_takes_precedence() {
        // If both target_scheme and target_schemes are set, target_schemes wins
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","target_scheme":"none","target_schemes":["cenc","cbcs"]}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resolved_target_schemes(), vec!["cenc", "cbcs"]);
    }

    #[test]
    fn webhook_payload_fmp4_container_format() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","container_format":"fmp4","target_scheme":"cenc"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.container_format, "fmp4");
    }

    #[test]
    fn webhook_payload_iso_container_format() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","container_format":"iso","target_scheme":"cenc"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.container_format, "iso");
    }

    #[test]
    fn webhook_iso_container_format_accepted() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "container_format": "iso"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req, &ctx);
        // On native targets, pipeline fails (no HTTP client), so webhook returns 500 or Ok.
        // The key assertion is that it does NOT fail with "invalid container_format".
        match resp {
            Ok(r) => assert!(r.status == 200 || r.status == 500),
            Err(e) => assert!(
                !e.to_string().contains("invalid container_format"),
                "iso should be accepted as valid container format, got: {e}"
            ),
        }
    }

    #[test]
    fn webhook_invalid_container_format() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "container_format": "webm"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid container_format"));
    }

    #[test]
    fn webhook_payload_none_target_scheme() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","target_scheme":"none"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resolved_target_schemes(), vec!["none"]);
    }

    #[test]
    fn webhook_none_target_scheme_accepted() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "target_scheme": "none"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req, &ctx);
        // On native targets, pipeline fails (no HTTP client), so webhook returns 500 or Ok.
        // The key assertion is that it does NOT fail with "invalid target_scheme".
        match resp {
            Ok(r) => assert!(r.status == 200 || r.status == 500),
            Err(e) => assert!(
                !e.to_string().contains("invalid target_scheme"),
                "none should be accepted as valid target scheme, got: {e}"
            ),
        }
    }

    #[test]
    fn webhook_invalid_target_scheme() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "target_scheme": "aes256"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid target_scheme"));
    }

    #[test]
    fn webhook_response_serde() {
        let mut manifest_urls = std::collections::HashMap::new();
        manifest_urls.insert("cenc".into(), "/repackage/c1/hls_cenc/manifest".into());
        let resp = WebhookResponse {
            status: "processing".into(),
            content_id: "c1".into(),
            format: "hls".into(),
            manifest_urls,
            segments_completed: 1,
            segments_total: Some(10),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WebhookResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "processing");
        assert_eq!(parsed.manifest_urls.get("cenc").unwrap(), "/repackage/c1/hls_cenc/manifest");
        assert_eq!(parsed.segments_completed, 1);
    }

    #[test]
    fn webhook_duplicate_target_scheme_rejected() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "target_schemes": ["cenc", "cenc"]
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate target_scheme"));
    }

    #[test]
    fn webhook_payload_enable_iframe_playlist() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","enable_iframe_playlist":true}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.enable_iframe_playlist, Some(true));
    }

    #[test]
    fn webhook_payload_enable_iframe_playlist_default() {
        // Old JSON without enable_iframe_playlist should parse with None default
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.enable_iframe_playlist.is_none());
    }

    // --- Source Config tests (JIT feature) ---

    #[cfg(feature = "jit")]
    fn make_source_config_request(body: Option<Vec<u8>>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path: "/config/source".to_string(),
            headers: vec![],
            body,
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_valid() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "movie-1",
            "source_url": "https://origin.example.com/movie-1/manifest.m3u8"
        });
        let req = make_source_config_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_source_config(&req, &ctx).unwrap();
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["content_id"], "movie-1");
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_with_schemes() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "movie-2",
            "source_url": "https://origin.example.com/manifest.mpd",
            "target_schemes": ["cenc", "cbcs"],
            "container_format": "fmp4"
        });
        let req = make_source_config_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_source_config(&req, &ctx).unwrap();
        assert_eq!(resp.status, 200);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_missing_body() {
        let ctx = test_context();
        let req = make_source_config_request(None);
        let result = handle_source_config(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing request body"));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_missing_content_id() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "",
            "source_url": "https://example.com"
        });
        let req = make_source_config_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_source_config(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content_id is required"));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_missing_source_url() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": ""
        });
        let req = make_source_config_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_source_config(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_url is required"));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn handle_source_config_invalid_scheme() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com",
            "target_schemes": ["aes256"]
        });
        let req = make_source_config_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_source_config(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid target_scheme"));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn source_config_payload_serde_roundtrip() {
        let payload = SourceConfigPayload {
            content_id: "c1".into(),
            source_url: "https://example.com/manifest.m3u8".into(),
            target_schemes: vec!["cenc".into(), "cbcs".into()],
            container_format: "fmp4".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: SourceConfigPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "c1");
        assert_eq!(parsed.target_schemes, vec!["cenc", "cbcs"]);
    }

    #[test]
    fn continue_payload_serde_roundtrip() {
        let payload = ContinuePayload {
            content_id: "c1".into(),
            format: "dash".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ContinuePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "c1");
        assert_eq!(parsed.format, "dash");
    }

    #[test]
    fn continue_missing_body() {
        let ctx = test_context();
        let req = make_continue_request(None);
        let result = handle_continue(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing request body"));
    }

    #[test]
    fn continue_no_job_state() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "nonexistent",
            "format": "hls"
        });
        let req = make_continue_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_continue(&req, &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn continue_invalid_format() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "format": "mp4"
        });
        let req = make_continue_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_continue(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid format"));
    }

    #[test]
    fn webhook_with_raw_keys() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","raw_keys":[{"kid":"00112233445566778899aabbccddeeff","key":"aabbccddeeff00112233445566778899"}]}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.raw_keys.len(), 1);
        assert_eq!(parsed.raw_keys[0].kid, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn webhook_with_key_rotation() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","key_rotation":{"period_segments":10}}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.key_rotation.is_some());
        assert_eq!(parsed.key_rotation.unwrap().period_segments, 10);
    }

    #[test]
    fn webhook_with_clear_lead() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","clear_lead_segments":3}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.clear_lead_segments, Some(3));
    }

    #[test]
    fn webhook_with_drm_systems() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","drm_systems":["widevine","clearkey"]}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.drm_systems, vec!["widevine", "clearkey"]);
    }

    #[test]
    fn webhook_invalid_drm_system() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "drm_systems": ["widevine", "unknown_drm"]
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid drm_system"));
    }

    #[test]
    fn webhook_payload_backward_compat_no_new_fields() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.raw_keys.is_empty());
        assert!(parsed.key_rotation.is_none());
        assert!(parsed.clear_lead_segments.is_none());
        assert!(parsed.drm_systems.is_empty());
    }

    #[test]
    fn raw_key_input_serde_roundtrip() {
        let rk = RawKeyInput {
            kid: "00112233445566778899aabbccddeeff".into(),
            key: "aabbccddeeff00112233445566778899".into(),
            iv: Some("11111111111111111111111111111111".into()),
        };
        let json = serde_json::to_string(&rk).unwrap();
        let parsed: RawKeyInput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kid, rk.kid);
        assert_eq!(parsed.iv.unwrap(), "11111111111111111111111111111111");
    }

    #[test]
    fn key_rotation_input_serde_roundtrip() {
        let kr = KeyRotationInput { period_segments: 5 };
        let json = serde_json::to_string(&kr).unwrap();
        let parsed: KeyRotationInput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.period_segments, 5);
    }

    // --- DVR Window webhook tests ---

    #[test]
    fn webhook_payload_dvr_window_duration() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","dvr_window_duration":3600.0}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.dvr_window_duration, Some(3600.0));
    }

    #[test]
    fn webhook_payload_dvr_window_duration_default() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.dvr_window_duration.is_none());
    }

    #[test]
    fn webhook_invalid_dvr_window_zero() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "dvr_window_duration": 0.0
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dvr_window_duration must be a positive number"));
    }

    #[test]
    fn webhook_invalid_dvr_window_negative() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "dvr_window_duration": -10.0
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dvr_window_duration must be a positive number"));
    }

    // --- Content steering tests ---

    #[test]
    fn webhook_payload_content_steering() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","content_steering":{"server_uri":"https://steer.example.com/v1","default_pathway_id":"cdn-a","query_before_start":true}}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        let cs = parsed.content_steering.unwrap();
        assert_eq!(cs.server_uri, "https://steer.example.com/v1");
        assert_eq!(cs.default_pathway_id.as_deref(), Some("cdn-a"));
        assert_eq!(cs.query_before_start, Some(true));
    }

    #[test]
    fn webhook_payload_content_steering_minimal() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","content_steering":{"server_uri":"https://steer.example.com/v1"}}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        let cs = parsed.content_steering.unwrap();
        assert_eq!(cs.server_uri, "https://steer.example.com/v1");
        assert!(cs.default_pathway_id.is_none());
        assert!(cs.query_before_start.is_none());
    }

    #[test]
    fn webhook_payload_content_steering_default() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.content_steering.is_none());
    }

    #[test]
    fn webhook_invalid_content_steering_empty_uri() {
        let ctx = test_context();
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls",
            "content_steering": {"server_uri": ""}
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("server_uri must not be empty"));
    }
}
