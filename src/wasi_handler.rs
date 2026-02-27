//! WASI HTTP incoming handler bridge.
//!
//! This module is only compiled for `wasm32` targets. It bridges the WASI
//! `wasi:http/incoming-handler` interface to the library's `handler::route()` function.
//!
//! The WASI Preview 2 component model exports this as the HTTP proxy entrypoint.
//! When the CDN runtime receives an incoming request, it invokes the `handle()`
//! method, which:
//!
//! 1. Converts `IncomingRequest` → library `HttpRequest`
//! 2. Loads `AppConfig` from environment variables
//! 3. Creates a `CacheBackend` (Redis HTTP)
//! 4. Constructs a `HandlerContext`
//! 5. Calls `handler::route(&req, &ctx)`
//! 6. Converts library `HttpResponse` → WASI `OutgoingResponse`
//! 7. Maps library errors to appropriate HTTP error responses

use crate::cache;
use crate::config::AppConfig;
use crate::error::EdgePackagerError;
use crate::handler::{self, HandlerContext, HttpMethod, HttpRequest, HttpResponse};

use wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Component;

impl wasi::exports::http::incoming_handler::Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let response = match handle_inner(request) {
            Ok(resp) => resp,
            Err(e) => error_to_http_response(&e),
        };

        send_response(response, response_out);
    }
}

wasi::http::proxy::export!(Component);

/// Inner handler that does the actual work, returning a `Result` for clean error handling.
fn handle_inner(request: IncomingRequest) -> Result<HttpResponse, EdgePackagerError> {
    // 1. Parse incoming request into our HttpRequest type
    let http_req = parse_incoming_request(request)?;

    // 2. Load configuration from environment variables
    let config = AppConfig::from_env()?;

    // 3. Create cache backend from config
    let cache_backend = cache::create_backend(&config.redis)?;

    // 4. Build handler context
    let ctx = HandlerContext {
        cache: cache_backend,
        config,
    };

    // 5. Route the request
    handler::route(&http_req, &ctx)
}

/// Convert a WASI `IncomingRequest` to our library's `HttpRequest`.
fn parse_incoming_request(request: IncomingRequest) -> Result<HttpRequest, EdgePackagerError> {
    // Method
    let method = match request.method() {
        wasi::http::types::Method::Get => HttpMethod::Get,
        wasi::http::types::Method::Post => HttpMethod::Post,
        wasi::http::types::Method::Options => HttpMethod::Options,
        _ => HttpMethod::Get, // Default unsupported methods to GET (will 404)
    };

    // Path
    let path = request
        .path_with_query()
        .unwrap_or_else(|| "/".to_string());

    // Headers
    let headers: Vec<(String, String)> = request
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).to_string()))
        .collect();

    // Body
    let body = read_incoming_body(&request)?;

    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Read the full body from an `IncomingRequest`.
fn read_incoming_body(request: &IncomingRequest) -> Result<Option<Vec<u8>>, EdgePackagerError> {
    let body = match request.consume() {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    let stream = match body.stream() {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Err(_) => break, // stream ended
        }
    }
    drop(stream);
    IncomingBody::finish(body);

    if bytes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(bytes))
    }
}

/// Convert a library error into an HTTP error response.
fn error_to_http_response(err: &EdgePackagerError) -> HttpResponse {
    let (status, message) = match err {
        EdgePackagerError::NotFound(msg) => (404, msg.clone()),
        EdgePackagerError::InvalidInput(msg) => (400, msg.clone()),
        EdgePackagerError::Config(msg) => (
            500,
            format!("configuration error: {msg}"),
        ),
        other => (500, format!("internal error: {other}")),
    };

    HttpResponse::error(status, &message)
}

/// Send an `HttpResponse` via the WASI `ResponseOutparam`.
fn send_response(response: HttpResponse, response_out: ResponseOutparam) {
    // Build response headers
    let header_entries: Vec<(String, Vec<u8>)> = response
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
        .collect();

    let fields = Fields::from_list(&header_entries).unwrap_or_else(|_| Fields::new());

    let outgoing = OutgoingResponse::new(fields);
    outgoing.set_status_code(response.status).ok();

    // Write body
    let body = outgoing.body().expect("get response body");
    ResponseOutparam::set(response_out, Ok(outgoing));

    if !response.body.is_empty() {
        let stream = body.write().expect("get response write stream");
        stream
            .blocking_write_and_flush(&response.body)
            .expect("write response body");
        drop(stream);
    }

    OutgoingBody::finish(body, None).expect("finish response body");
}
