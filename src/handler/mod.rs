pub mod request;
pub mod webhook;

use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::OutputFormat;

/// An incoming HTTP request (abstracted from the WASI HTTP interface).
#[derive(Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// An outgoing HTTP response.
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Options,
}

impl HttpResponse {
    pub fn ok(body: Vec<u8>, content_type: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), content_type.into())],
            body,
        }
    }

    pub fn ok_with_cache(body: Vec<u8>, content_type: &str, cache_control: &str) -> Self {
        Self {
            status: 200,
            headers: vec![
                ("Content-Type".into(), content_type.into()),
                ("Cache-Control".into(), cache_control.into()),
            ],
            body,
        }
    }

    pub fn accepted(body: Vec<u8>) -> Self {
        Self {
            status: 202,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body,
        }
    }

    pub fn not_found(message: &str) -> Self {
        Self {
            status: 404,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: message.as_bytes().to_vec(),
        }
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: message.as_bytes().to_vec(),
        }
    }
}

/// Route an incoming request to the appropriate handler.
pub fn route(req: &HttpRequest) -> Result<HttpResponse> {
    // Parse the path to determine which handler to use.
    let path = req.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (req.method, segments.as_slice()) {
        // Health check
        (HttpMethod::Get, ["health"]) => {
            Ok(HttpResponse::ok(b"ok".to_vec(), "text/plain"))
        }

        // On-demand: GET /repackage/{content_id}/{format}/manifest
        (HttpMethod::Get, ["repackage", content_id, format, "manifest"]) => {
            let output_format = parse_format(format)?;
            request::handle_manifest_request(content_id, output_format)
        }

        // On-demand: GET /repackage/{content_id}/{format}/init.mp4
        (HttpMethod::Get, ["repackage", content_id, format, "init.mp4"]) => {
            let output_format = parse_format(format)?;
            request::handle_init_segment_request(content_id, output_format)
        }

        // On-demand: GET /repackage/{content_id}/{format}/segment_{n}.cmfv
        (HttpMethod::Get, ["repackage", content_id, format, segment_file]) => {
            let output_format = parse_format(format)?;
            if let Some(seg_num) = parse_segment_number(segment_file) {
                request::handle_media_segment_request(content_id, output_format, seg_num)
            } else {
                Ok(HttpResponse::not_found("unknown resource"))
            }
        }

        // Webhook: POST /webhook/repackage
        (HttpMethod::Post, ["webhook", "repackage"]) => {
            webhook::handle_repackage_webhook(req)
        }

        // Status: GET /status/{content_id}/{format}
        (HttpMethod::Get, ["status", content_id, format]) => {
            let output_format = parse_format(format)?;
            request::handle_status_request(content_id, output_format)
        }

        _ => Ok(HttpResponse::not_found("not found")),
    }
}

fn parse_format(s: &str) -> Result<OutputFormat> {
    match s {
        "hls" => Ok(OutputFormat::Hls),
        "dash" => Ok(OutputFormat::Dash),
        _ => Err(EdgePackagerError::InvalidInput(format!(
            "unknown format: {s} (expected 'hls' or 'dash')"
        ))),
    }
}

fn parse_segment_number(filename: &str) -> Option<u32> {
    // segment_0.cmfv, segment_1.cmfv, etc.
    let name = filename.strip_suffix(".cmfv")?;
    let num_str = name.strip_prefix("segment_")?;
    num_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_hls() {
        assert_eq!(parse_format("hls").unwrap(), OutputFormat::Hls);
    }

    #[test]
    fn parse_format_dash() {
        assert_eq!(parse_format("dash").unwrap(), OutputFormat::Dash);
    }

    #[test]
    fn parse_format_invalid() {
        let result = parse_format("mp4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown format"));
    }

    #[test]
    fn parse_segment_number_valid() {
        assert_eq!(parse_segment_number("segment_0.cmfv"), Some(0));
        assert_eq!(parse_segment_number("segment_1.cmfv"), Some(1));
        assert_eq!(parse_segment_number("segment_42.cmfv"), Some(42));
        assert_eq!(parse_segment_number("segment_999.cmfv"), Some(999));
    }

    #[test]
    fn parse_segment_number_invalid() {
        assert_eq!(parse_segment_number("segment_abc.cmfv"), None);
        assert_eq!(parse_segment_number("init.mp4"), None);
        assert_eq!(parse_segment_number("segment_0.mp4"), None);
        assert_eq!(parse_segment_number(""), None);
        assert_eq!(parse_segment_number("segment_.cmfv"), None);
    }

    #[test]
    fn http_response_ok() {
        let resp = HttpResponse::ok(b"hello".to_vec(), "text/plain");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert!(resp.headers.iter().any(|(k, v)| k == "Content-Type" && v == "text/plain"));
    }

    #[test]
    fn http_response_ok_with_cache() {
        let resp = HttpResponse::ok_with_cache(
            b"data".to_vec(),
            "video/mp4",
            "public, max-age=31536000, immutable",
        );
        assert_eq!(resp.status, 200);
        assert!(resp.headers.iter().any(|(k, v)| k == "Cache-Control" && v.contains("immutable")));
    }

    #[test]
    fn http_response_accepted() {
        let resp = HttpResponse::accepted(b"{}".to_vec());
        assert_eq!(resp.status, 202);
        assert!(resp.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
    }

    #[test]
    fn http_response_not_found() {
        let resp = HttpResponse::not_found("missing");
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, b"missing");
    }

    #[test]
    fn http_response_error() {
        let resp = HttpResponse::error(500, "internal error");
        assert_eq!(resp.status, 500);
        assert_eq!(resp.body, b"internal error");
    }

    #[test]
    fn route_health_check() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/health".to_string(),
            headers: vec![],
            body: None,
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
    }

    #[test]
    fn route_health_check_trailing_slash() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/health/".to_string(),
            headers: vec![],
            body: None,
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn route_manifest_request() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/repackage/content-1/hls/manifest".to_string(),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("manifest not found"));
    }

    #[test]
    fn route_init_segment_request() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/repackage/content-1/dash/init.mp4".to_string(),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("init segment not found"));
    }

    #[test]
    fn route_media_segment_request() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/repackage/content-1/hls/segment_5.cmfv".to_string(),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("segment 5 not found"));
    }

    #[test]
    fn route_invalid_segment_file() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/repackage/content-1/hls/unknown_file.xyz".to_string(),
            headers: vec![],
            body: None,
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn route_webhook_repackage() {
        let payload = serde_json::json!({
            "content_id": "test",
            "source_url": "https://example.com/source.m3u8",
            "format": "hls"
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = HttpRequest {
            method: HttpMethod::Post,
            path: "/webhook/repackage".to_string(),
            headers: vec![],
            body: Some(body),
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 202);
    }

    #[test]
    fn route_status_request() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/status/content-1/hls".to_string(),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no job found"));
    }

    #[test]
    fn route_unknown_path() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/unknown/path".to_string(),
            headers: vec![],
            body: None,
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn route_wrong_method() {
        let req = HttpRequest {
            method: HttpMethod::Post,
            path: "/health".to_string(),
            headers: vec![],
            body: None,
        };
        let resp = route(&req).unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn route_invalid_format_in_path() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: "/repackage/content-1/mp4/manifest".to_string(),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown format"));
    }

    #[test]
    fn http_method_equality() {
        assert_eq!(HttpMethod::Get, HttpMethod::Get);
        assert_eq!(HttpMethod::Post, HttpMethod::Post);
        assert_eq!(HttpMethod::Options, HttpMethod::Options);
        assert_ne!(HttpMethod::Get, HttpMethod::Post);
    }
}
