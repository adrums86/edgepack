use crate::cache::{CacheBackend, CacheKeys};
use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::handler::{HandlerContext, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};

fn default_container_format_str() -> String {
    "cmaf".to_string()
}

/// Payload for POST /config/source — per-content source configuration for JIT.
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
/// Stores per-content source configuration in cache for JIT packaging. This allows
/// GET requests to know where to fetch the source manifest from.
pub fn handle_source_config(req: &HttpRequest, _ctx: &HandlerContext) -> Result<HttpResponse> {
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
    let cache = crate::cache::global_cache();
    cache.set(&cache_key, &serialized, 172_800)?; // 48h TTL

    let resp_body = serde_json::to_vec(&serde_json::json!({
        "status": "ok",
        "content_id": payload.content_id,
    }))
    .unwrap_or_default();

    Ok(HttpResponse::ok(resp_body, "application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_helpers::test_context;
    use crate::handler::HttpMethod;

    fn make_source_config_request(body: Option<Vec<u8>>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path: "/config/source".to_string(),
            headers: vec![],
            body,
        }
    }

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

    #[test]
    fn handle_source_config_missing_body() {
        let ctx = test_context();
        let req = make_source_config_request(None);
        let result = handle_source_config(&req, &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing request body"));
    }

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
}
