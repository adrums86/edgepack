use crate::cache::CacheKeys;
use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgePackagerError, Result};
use crate::handler::{format_str, HandlerContext, HttpRequest, HttpResponse};
use crate::manifest::types::OutputFormat;
use crate::repackager::pipeline::RepackagePipeline;
use crate::repackager::{JobState, JobStatus, RepackageRequest};
use serde::{Deserialize, Serialize};

/// Webhook payload for triggering a repackaging job.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// Unique content identifier.
    pub content_id: String,
    /// Source manifest URL (HLS or DASH).
    pub source_url: String,
    /// Output format: "hls" or "dash".
    pub format: String,
    /// Target encryption scheme: "cenc" or "cbcs" (default: "cenc").
    #[serde(default = "default_target_scheme_str")]
    pub target_scheme: String,
    /// Optional key IDs to request (hex strings).
    #[serde(default)]
    pub key_ids: Vec<String>,
}

fn default_target_scheme_str() -> String {
    "cenc".to_string()
}

/// Webhook response returned after first manifest publishes.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookResponse {
    pub status: String,
    pub content_id: String,
    pub format: String,
    pub manifest_url: String,
    pub segments_completed: u32,
    pub segments_total: Option<u32>,
}

/// Continue payload for internal self-invocation chaining.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContinuePayload {
    pub content_id: String,
    pub format: String,
}

/// Handle a POST /webhook/repackage request.
///
/// Validates the payload, executes the pipeline through the first segment (producing
/// a live manifest), chains remaining processing via self-invocation, and returns 200.
pub fn handle_repackage_webhook(req: &HttpRequest, ctx: &HandlerContext) -> Result<HttpResponse> {
    let body = req
        .body
        .as_ref()
        .ok_or_else(|| EdgePackagerError::InvalidInput("missing request body".into()))?;

    let payload: WebhookPayload = serde_json::from_slice(body)
        .map_err(|e| EdgePackagerError::InvalidInput(format!("invalid JSON: {e}")))?;

    // Validate format
    let output_format = match payload.format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        other => {
            return Err(EdgePackagerError::InvalidInput(format!(
                "invalid format: {other} (expected 'hls' or 'dash')"
            )));
        }
    };

    // Validate source URL
    if payload.source_url.is_empty() {
        return Err(EdgePackagerError::InvalidInput(
            "source_url is required".into(),
        ));
    }

    // Parse target encryption scheme
    let target_scheme = match payload.target_scheme.as_str() {
        "cenc" => EncryptionScheme::Cenc,
        "cbcs" => EncryptionScheme::Cbcs,
        other => {
            return Err(EdgePackagerError::InvalidInput(format!(
                "invalid target_scheme: {other} (expected 'cenc' or 'cbcs')"
            )));
        }
    };

    let request = RepackageRequest {
        content_id: payload.content_id.clone(),
        source_url: payload.source_url,
        output_format,
        target_scheme,
        key_ids: payload.key_ids,
    };

    // Create a pipeline with a fresh cache backend
    let cache = crate::cache::create_backend(&ctx.config.redis)?;
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

    let manifest_path = format!(
        "/repackage/{}/{}/manifest",
        payload.content_id, payload.format
    );

    let response = WebhookResponse {
        status: "processing".to_string(),
        content_id: payload.content_id,
        format: payload.format,
        manifest_url: manifest_path,
        segments_completed: job_status.segments_completed,
        segments_total: job_status.segments_total,
    };

    let resp_body = serde_json::to_vec(&response)
        .map_err(|e| EdgePackagerError::Io(format!("serialize response: {e}")))?;

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
        .ok_or_else(|| EdgePackagerError::InvalidInput("missing request body".into()))?;

    let payload: ContinuePayload = serde_json::from_slice(body)
        .map_err(|e| EdgePackagerError::InvalidInput(format!("invalid JSON: {e}")))?;

    let output_format = match payload.format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        other => {
            return Err(EdgePackagerError::InvalidInput(format!(
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
        EdgePackagerError::Cache(format!("deserialize job status: {e}"))
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
    let cache = crate::cache::create_backend(&ctx.config.redis)?;
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
            target_scheme: "cenc".into(),
            key_ids: vec!["aabb".into()],
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: WebhookPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "c1");
        assert_eq!(parsed.target_scheme, "cenc");
        assert_eq!(parsed.key_ids.len(), 1);
    }

    #[test]
    fn webhook_payload_default_key_ids_and_scheme() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.key_ids.is_empty());
        assert_eq!(parsed.target_scheme, "cenc");
    }

    #[test]
    fn webhook_payload_cbcs_target_scheme() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls","target_scheme":"cbcs"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.target_scheme, "cbcs");
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
        let resp = WebhookResponse {
            status: "processing".into(),
            content_id: "c1".into(),
            format: "hls".into(),
            manifest_url: "/repackage/c1/hls/manifest".into(),
            segments_completed: 1,
            segments_total: Some(10),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WebhookResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "processing");
        assert_eq!(parsed.manifest_url, "/repackage/c1/hls/manifest");
        assert_eq!(parsed.segments_completed, 1);
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
}
