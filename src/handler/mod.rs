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
