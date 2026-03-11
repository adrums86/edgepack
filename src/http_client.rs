//! Shared outgoing HTTP client abstraction.
//!
//! On `wasm32` targets, uses `wasi:http/outgoing-handler` to make real HTTP requests.
//! On native targets, returns an error (HTTP transport is only available in the WASI runtime).

use crate::error::{EdgepackError, Result};

/// HTTP request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpClientMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// An outgoing HTTP request to send.
#[derive(Debug, Clone)]
pub struct OutgoingHttpRequest {
    pub method: HttpClientMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// An HTTP response received from the outgoing handler.
#[derive(Debug, Clone)]
pub struct HttpClientResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Convenience: HTTP GET with headers.
pub fn get(url: &str, headers: &[(String, String)]) -> Result<HttpClientResponse> {
    send(OutgoingHttpRequest {
        method: HttpClientMethod::Get,
        url: url.to_string(),
        headers: headers.to_vec(),
        body: None,
    })
}

/// Convenience: HTTP POST with headers and body.
pub fn post(
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<HttpClientResponse> {
    send(OutgoingHttpRequest {
        method: HttpClientMethod::Post,
        url: url.to_string(),
        headers: headers.to_vec(),
        body: Some(body),
    })
}

/// Convenience: HTTP PUT with headers and body.
pub fn put(
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<HttpClientResponse> {
    send(OutgoingHttpRequest {
        method: HttpClientMethod::Put,
        url: url.to_string(),
        headers: headers.to_vec(),
        body: Some(body),
    })
}

/// Convenience: HTTP DELETE with headers (no body).
///
/// Named `delete_request` to avoid conflict with Rust's `drop` semantics.
pub fn delete_request(url: &str, headers: &[(String, String)]) -> Result<HttpClientResponse> {
    send(OutgoingHttpRequest {
        method: HttpClientMethod::Delete,
        url: url.to_string(),
        headers: headers.to_vec(),
        body: None,
    })
}

/// Send an HTTP request. Dispatches to WASI, reqwest (sandbox), or native stub.
fn send(req: OutgoingHttpRequest) -> Result<HttpClientResponse> {
    #[cfg(target_arch = "wasm32")]
    {
        send_wasi(req)
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "sandbox"))]
    {
        send_reqwest(req)
    }

    #[cfg(all(not(target_arch = "wasm32"), not(feature = "sandbox")))]
    {
        send_native_stub(req)
    }
}

// ---------------------------------------------------------------------------
// WASI implementation (wasm32 targets only)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
fn send_wasi(req: OutgoingHttpRequest) -> Result<HttpClientResponse> {
    use wasi::http::outgoing_handler;
    use wasi::http::types::{Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, Scheme};

    // 1. Parse URL into components
    let parsed = crate::url::Url::parse(&req.url).map_err(|e| EdgepackError::Http {
        status: 0,
        message: format!("invalid URL: {e}"),
    })?;

    let scheme = match parsed.scheme() {
        "https" => Some(Scheme::Https),
        "http" => Some(Scheme::Http),
        _ => None,
    };
    let authority = match parsed.port() {
        Some(port) => format!("{}:{}", parsed.host_str().unwrap_or(""), port),
        None => parsed.host_str().unwrap_or("").to_string(),
    };
    let path_and_query = match parsed.query() {
        Some(q) => format!("{}?{}", parsed.path(), q),
        None => parsed.path().to_string(),
    };

    // 2. Build headers
    let header_entries: Vec<(String, Vec<u8>)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
        .collect();
    let fields = Fields::from_list(&header_entries).map_err(|e| EdgepackError::Http {
        status: 0,
        message: format!("invalid headers: {e:?}"),
    })?;

    // 3. Create OutgoingRequest
    let method = match req.method {
        HttpClientMethod::Get => Method::Get,
        HttpClientMethod::Post => Method::Post,
        HttpClientMethod::Put => Method::Put,
        HttpClientMethod::Delete => Method::Delete,
    };
    let outgoing = OutgoingRequest::new(fields);
    outgoing
        .set_method(&method)
        .map_err(|_| wasi_err("set_method"))?;
    outgoing
        .set_scheme(scheme.as_ref())
        .map_err(|_| wasi_err("set_scheme"))?;
    outgoing
        .set_authority(Some(&authority))
        .map_err(|_| wasi_err("set_authority"))?;
    outgoing
        .set_path_with_query(Some(&path_and_query))
        .map_err(|_| wasi_err("set_path_with_query"))?;

    // 4. Write body if present
    if let Some(body_bytes) = req.body {
        let body = outgoing.body().map_err(|_| wasi_err("get request body"))?;
        let stream = body
            .write()
            .map_err(|_| wasi_err("get request write stream"))?;
        stream
            .blocking_write_and_flush(&body_bytes)
            .map_err(|e| EdgepackError::Http {
                status: 0,
                message: format!("write request body: {e:?}"),
            })?;
        drop(stream);
        OutgoingBody::finish(body, None).map_err(|_| wasi_err("finish request body"))?;
    }

    // 5. Send request
    let future_resp = outgoing_handler::handle(outgoing, None).map_err(|e| {
        EdgepackError::Http {
            status: 0,
            message: format!("send request: {e:?}"),
        }
    })?;

    // 6. Block on response (synchronous — WASI Preview 2 guest model)
    let incoming = loop {
        if let Some(result) = future_resp.get() {
            break result
                .map_err(|_| wasi_err("get future response"))?
                .map_err(|e| EdgepackError::Http {
                    status: 0,
                    message: format!("response error: {e:?}"),
                })?;
        }
        future_resp.subscribe().block();
    };

    // 7. Read status and headers
    let status = incoming.status();
    let resp_headers: Vec<(String, String)> = incoming
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).to_string()))
        .collect();

    // 8. Read response body
    let body = incoming
        .consume()
        .map_err(|_| wasi_err("consume response body"))?;
    let stream = body
        .stream()
        .map_err(|_| wasi_err("get response read stream"))?;
    let mut response_bytes = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) => response_bytes.extend_from_slice(&chunk),
            Err(_) => break, // stream ended (closed or error)
        }
    }
    drop(stream);
    IncomingBody::finish(body);

    Ok(HttpClientResponse {
        status,
        headers: resp_headers,
        body: response_bytes,
    })
}

#[cfg(target_arch = "wasm32")]
fn wasi_err(context: &str) -> EdgepackError {
    EdgepackError::Http {
        status: 0,
        message: format!("WASI HTTP {context} failed"),
    }
}

// ---------------------------------------------------------------------------
// Reqwest implementation (sandbox feature on non-wasm32 targets)
// ---------------------------------------------------------------------------

#[cfg(all(not(target_arch = "wasm32"), feature = "sandbox"))]
fn shared_reqwest_client() -> &'static reqwest::blocking::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(32)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sandbox"))]
fn send_reqwest(req: OutgoingHttpRequest) -> Result<HttpClientResponse> {
    let client = shared_reqwest_client();

    let mut builder = match req.method {
        HttpClientMethod::Get => client.get(&req.url),
        HttpClientMethod::Post => client.post(&req.url),
        HttpClientMethod::Put => client.put(&req.url),
        HttpClientMethod::Delete => client.delete(&req.url),
    };

    for (key, value) in &req.headers {
        builder = builder.header(key, value);
    }

    if let Some(body) = req.body {
        builder = builder.body(body);
    }

    let response = builder.send().map_err(|e| EdgepackError::Http {
        status: 0,
        message: format!("HTTP request failed: {e}"),
    })?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response
        .bytes()
        .map_err(|e| EdgepackError::Http {
            status,
            message: format!("failed to read response body: {e}"),
        })?
        .to_vec();

    Ok(HttpClientResponse {
        status,
        headers,
        body,
    })
}

// ---------------------------------------------------------------------------
// Native stub (non-wasm32 targets without sandbox — used during testing)
// ---------------------------------------------------------------------------

#[cfg(all(not(target_arch = "wasm32"), not(feature = "sandbox")))]
fn send_native_stub(_req: OutgoingHttpRequest) -> Result<HttpClientResponse> {
    Err(EdgepackError::Http {
        status: 0,
        message: "HTTP client only available in WASI environment (wasm32-wasip2)".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "sandbox"))]
    #[test]
    fn get_returns_error_on_native() {
        let result = get("https://example.com", &[]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("only available in WASI"));
    }

    #[cfg(not(feature = "sandbox"))]
    #[test]
    fn post_returns_error_on_native() {
        let result = post(
            "https://example.com",
            &[("Content-Type".into(), "text/plain".into())],
            b"body".to_vec(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("only available in WASI"));
    }

    #[cfg(not(feature = "sandbox"))]
    #[test]
    fn put_returns_error_on_native() {
        let result = put(
            "https://example.com/key",
            &[("Authorization".into(), "Bearer tok".into())],
            b"value".to_vec(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("only available in WASI"));
    }

    #[cfg(not(feature = "sandbox"))]
    #[test]
    fn delete_request_returns_error_on_native() {
        let result = delete_request(
            "https://example.com/key",
            &[("Authorization".into(), "Bearer tok".into())],
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("only available in WASI"));
    }

    #[test]
    fn outgoing_request_construction() {
        let req = OutgoingHttpRequest {
            method: HttpClientMethod::Get,
            url: "https://example.com/api/data?foo=bar".into(),
            headers: vec![("Authorization".into(), "Bearer tok".into())],
            body: None,
        };
        assert_eq!(req.method, HttpClientMethod::Get);
        assert_eq!(req.headers.len(), 1);
        assert!(req.body.is_none());
    }

    #[test]
    fn outgoing_request_with_body() {
        let req = OutgoingHttpRequest {
            method: HttpClientMethod::Post,
            url: "https://example.com/api".into(),
            headers: vec![],
            body: Some(b"<xml/>".to_vec()),
        };
        assert_eq!(req.method, HttpClientMethod::Post);
        assert_eq!(req.body.as_ref().unwrap(), b"<xml/>");
    }

    #[test]
    fn outgoing_request_with_put_method() {
        let req = OutgoingHttpRequest {
            method: HttpClientMethod::Put,
            url: "https://example.com/kv/key1".into(),
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: Some(b"binary data".to_vec()),
        };
        assert_eq!(req.method, HttpClientMethod::Put);
        assert_eq!(req.body.as_ref().unwrap(), b"binary data");
    }

    #[test]
    fn outgoing_request_with_delete_method() {
        let req = OutgoingHttpRequest {
            method: HttpClientMethod::Delete,
            url: "https://example.com/kv/key1".into(),
            headers: vec![("Authorization".into(), "Bearer tok".into())],
            body: None,
        };
        assert_eq!(req.method, HttpClientMethod::Delete);
        assert!(req.body.is_none());
    }

    #[test]
    fn http_client_response_structure() {
        let resp = HttpClientResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: b"{\"ok\": true}".to_vec(),
        };
        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers.len(), 1);
        assert!(!resp.body.is_empty());
    }
}
