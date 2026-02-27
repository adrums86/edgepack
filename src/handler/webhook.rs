use crate::error::{EdgePackagerError, Result};
use crate::handler::{HttpRequest, HttpResponse};
use crate::manifest::types::OutputFormat;
use crate::repackager::RepackageRequest;
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
    /// Optional key IDs to request (hex strings).
    #[serde(default)]
    pub key_ids: Vec<String>,
}

/// Webhook response.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookResponse {
    pub status: String,
    pub content_id: String,
    pub format: String,
    pub manifest_url: String,
}

/// Handle a POST /webhook/repackage request.
///
/// Validates the payload, kicks off the repackaging pipeline, and returns 202 Accepted.
pub fn handle_repackage_webhook(req: &HttpRequest) -> Result<HttpResponse> {
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

    let _request = RepackageRequest {
        content_id: payload.content_id.clone(),
        source_url: payload.source_url,
        output_format,
        key_ids: payload.key_ids,
    };

    // TODO: Kick off the pipeline asynchronously
    //
    // In production, this would:
    // 1. Create a job entry in Redis with state=Pending
    // 2. Start the repackaging pipeline
    //    (In a WASI environment, this might need to be handled
    //     by the runtime's task scheduling, or by chaining
    //     HTTP requests to self)
    // 3. Return 202 immediately

    let manifest_path = format!(
        "/repackage/{}/{}/manifest",
        payload.content_id, payload.format
    );

    let response = WebhookResponse {
        status: "accepted".to_string(),
        content_id: payload.content_id,
        format: payload.format,
        manifest_url: manifest_path,
    };

    let body = serde_json::to_vec(&response)
        .map_err(|e| EdgePackagerError::Io(format!("serialize response: {e}")))?;

    Ok(HttpResponse::accepted(body))
}
