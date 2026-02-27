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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::HttpMethod;

    fn make_webhook_request(body: Option<Vec<u8>>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path: "/webhook/repackage".to_string(),
            headers: vec![],
            body,
        }
    }

    #[test]
    fn valid_hls_webhook() {
        let payload = serde_json::json!({
            "content_id": "movie-123",
            "source_url": "https://cdn.example.com/manifest.m3u8",
            "format": "hls"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req).unwrap();
        assert_eq!(resp.status, 202);

        let resp_body: WebhookResponse = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(resp_body.status, "accepted");
        assert_eq!(resp_body.content_id, "movie-123");
        assert_eq!(resp_body.format, "hls");
        assert_eq!(resp_body.manifest_url, "/repackage/movie-123/hls/manifest");
    }

    #[test]
    fn valid_dash_webhook() {
        let payload = serde_json::json!({
            "content_id": "show-456",
            "source_url": "https://cdn.example.com/manifest.mpd",
            "format": "dash"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req).unwrap();
        assert_eq!(resp.status, 202);

        let resp_body: WebhookResponse = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(resp_body.format, "dash");
        assert_eq!(resp_body.manifest_url, "/repackage/show-456/dash/manifest");
    }

    #[test]
    fn webhook_with_key_ids() {
        let payload = serde_json::json!({
            "content_id": "content-1",
            "source_url": "https://example.com/manifest.m3u8",
            "format": "hls",
            "key_ids": ["aabbccdd", "11223344"]
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let resp = handle_repackage_webhook(&req).unwrap();
        assert_eq!(resp.status, 202);
    }

    #[test]
    fn webhook_missing_body() {
        let req = make_webhook_request(None);
        let result = handle_repackage_webhook(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing request body"));
    }

    #[test]
    fn webhook_invalid_json() {
        let req = make_webhook_request(Some(b"not json".to_vec()));
        let result = handle_repackage_webhook(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid JSON"));
    }

    #[test]
    fn webhook_invalid_format() {
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source",
            "format": "mp4"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid format: mp4"));
    }

    #[test]
    fn webhook_empty_source_url() {
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "",
            "format": "hls"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_url is required"));
    }

    #[test]
    fn webhook_missing_required_field() {
        let payload = serde_json::json!({
            "content_id": "test"
        });
        let req = make_webhook_request(Some(serde_json::to_vec(&payload).unwrap()));
        let result = handle_repackage_webhook(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid JSON"));
    }

    #[test]
    fn webhook_payload_serde_roundtrip() {
        let payload = WebhookPayload {
            content_id: "c1".into(),
            source_url: "https://example.com".into(),
            format: "hls".into(),
            key_ids: vec!["aabb".into()],
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: WebhookPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "c1");
        assert_eq!(parsed.key_ids.len(), 1);
    }

    #[test]
    fn webhook_payload_default_key_ids() {
        let json = r#"{"content_id":"test","source_url":"https://example.com","format":"hls"}"#;
        let parsed: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(parsed.key_ids.is_empty());
    }

    #[test]
    fn webhook_response_serde() {
        let resp = WebhookResponse {
            status: "accepted".into(),
            content_id: "c1".into(),
            format: "hls".into(),
            manifest_url: "/repackage/c1/hls/manifest".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WebhookResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "accepted");
        assert_eq!(parsed.manifest_url, "/repackage/c1/hls/manifest");
    }
}
