//! Local sandbox for edge-packager.
//!
//! Provides a web UI and API server for testing the repackaging pipeline
//! locally without deploying to a CDN edge. Uses reqwest for HTTP transport
//! and an in-memory cache backend instead of Redis.
//!
//! Run with: `cargo run --bin sandbox --features sandbox --target $(rustc -vV | grep host | awk '{print $2}')`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use edge_packager::cache::memory::InMemoryCacheBackend;
use edge_packager::cache::{CacheBackend, CacheKeys};
use edge_packager::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, RedisBackendType, RedisConfig, SpekeAuth,
};
use edge_packager::manifest;
use edge_packager::manifest::types::ManifestState;
use edge_packager::manifest::types::OutputFormat;
use edge_packager::repackager::pipeline::RepackagePipeline;
use edge_packager::repackager::{JobStatus, RepackageRequest};

// ─── Application State ─────────────────────────────────────────────────

struct AppState {
    cache: InMemoryCacheBackend,
    jobs: Mutex<HashMap<String, JobHandle>>,
}

#[allow(dead_code)]
struct JobHandle {
    content_id: String,
    format: OutputFormat,
}

// ─── Request / Response Types ───────────────────────────────────────────

#[derive(Deserialize)]
struct RepackagePayload {
    source_url: String,
    speke_url: String,
    #[serde(default = "default_speke_auth_type")]
    speke_auth_type: String,
    #[serde(default)]
    speke_auth_value: String,
    #[serde(default)]
    speke_api_key_header: String,
    output_format: String,
    #[serde(default = "default_target_scheme")]
    target_scheme: String,
    #[serde(default = "default_container_format")]
    container_format: String,
}

fn default_speke_auth_type() -> String {
    "bearer".into()
}

fn default_target_scheme() -> String {
    "cenc".into()
}

fn default_container_format() -> String {
    "cmaf".into()
}

#[derive(Serialize)]
struct RepackageResponse {
    content_id: String,
    format: String,
    message: String,
    container_format: String,
}

#[derive(Serialize)]
struct StatusResponse {
    state: String,
    segments_completed: u32,
    segments_total: Option<u32>,
    output_dir: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

// ─── Handlers ───────────────────────────────────────────────────────────

async fn serve_ui() -> Html<&'static str> {
    Html(SANDBOX_HTML)
}

async fn handle_repackage(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RepackagePayload>,
) -> Response {
    let output_format = match payload.output_format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "output_format must be 'hls' or 'dash'".into(),
                }),
            )
                .into_response();
        }
    };

    if payload.target_scheme == "none" {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(ErrorResponse {
                error: "Clear (unencrypted) output is planned for Phase 3. Currently only 'cenc' and 'cbcs' target schemes are supported.".into(),
            }),
        )
            .into_response();
    }

    let target_scheme = match payload.target_scheme.as_str() {
        "cenc" => edge_packager::drm::scheme::EncryptionScheme::Cenc,
        "cbcs" => edge_packager::drm::scheme::EncryptionScheme::Cbcs,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "target_scheme must be 'cenc', 'cbcs', or 'none'".into(),
                }),
            )
                .into_response();
        }
    };

    let container_format = match payload.container_format.as_str() {
        "cmaf" => edge_packager::media::container::ContainerFormat::Cmaf,
        "fmp4" => edge_packager::media::container::ContainerFormat::Fmp4,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "container_format must be 'cmaf' or 'fmp4'".into(),
                }),
            )
                .into_response();
        }
    };

    let speke_url = match url::Url::parse(&payload.speke_url) {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("invalid speke_url: {e}"),
                }),
            )
                .into_response();
        }
    };

    let speke_auth = match payload.speke_auth_type.as_str() {
        "bearer" => SpekeAuth::Bearer(payload.speke_auth_value.clone()),
        "api_key" => SpekeAuth::ApiKey {
            header: if payload.speke_api_key_header.is_empty() {
                "x-api-key".into()
            } else {
                payload.speke_api_key_header.clone()
            },
            value: payload.speke_auth_value.clone(),
        },
        "basic" => {
            let parts: Vec<&str> = payload.speke_auth_value.splitn(2, ':').collect();
            if parts.len() != 2 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "basic auth value must be 'username:password'".into(),
                    }),
                )
                    .into_response();
            }
            SpekeAuth::Basic {
                username: parts[0].into(),
                password: parts[1].into(),
            }
        }
        _ => SpekeAuth::Bearer(payload.speke_auth_value.clone()),
    };

    // Generate a content_id from the source URL
    let content_id = generate_content_id(&payload.source_url);
    let fmt_str = match output_format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };

    // Check if source is a local file path
    let source_url = if is_local_path(&payload.source_url) {
        match start_local_file_server(&payload.source_url).await {
            Ok(url) => url,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("failed to serve local file: {e}"),
                    }),
                )
                    .into_response();
            }
        }
    } else {
        payload.source_url.clone()
    };

    // Build config — redis config is unused (in-memory cache), but required by AppConfig
    let config = AppConfig {
        redis: RedisConfig {
            url: "unused://localhost".into(),
            token: "unused".into(),
            backend: RedisBackendType::Http,
        },
        drm: DrmConfig {
            speke_url,
            speke_auth,
            system_ids: DrmSystemIds::default(),
        },
        cache: CacheConfig::default(),
    };

    let container_format_str = payload.container_format.clone();
    let request = RepackageRequest {
        content_id: content_id.clone(),
        source_url,
        output_format,
        target_scheme,
        container_format,
        key_ids: vec![],
    };

    // Track the job
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.insert(
            format!("{content_id}/{fmt_str}"),
            JobHandle {
                content_id: content_id.clone(),
                format: output_format,
            },
        );
    }

    // Run pipeline in a blocking thread
    let cache = state.cache.clone();
    let cid = content_id.clone();
    let fmt = output_format;
    tokio::task::spawn_blocking(move || {
        // Wrap cache with encryption layer so sensitive data (DRM keys, SPEKE
        // responses, rewrite params) is never stored in plaintext.
        let enc_key = edge_packager::cache::encrypted::derive_key("edge-packager-sandbox");
        let encrypted_cache = edge_packager::cache::encrypted::EncryptedCacheBackend::new(
            Box::new(cache.clone()),
            &enc_key,
        );
        let pipeline = RepackagePipeline::new(config, Box::new(encrypted_cache));
        match pipeline.execute(&request) {
            Ok(status) => {
                eprintln!(
                    "  Pipeline complete: {}/{} — {} segments",
                    status.content_id,
                    fmt_str,
                    status.segments_completed
                );
                // Write output to disk
                if let Err(e) = write_output_to_disk(&cid, fmt, &cache) {
                    eprintln!("  Warning: failed to write output to disk: {e}");
                }
            }
            Err(e) => {
                eprintln!("  Pipeline failed: {e}");
            }
        }
    });

    (
        StatusCode::OK,
        Json(RepackageResponse {
            content_id,
            format: fmt_str.into(),
            message: "repackaging started".into(),
            container_format: container_format_str,
        }),
    )
        .into_response()
}

async fn handle_status(
    State(state): State<Arc<AppState>>,
    Path((content_id, format)): Path<(String, String)>,
) -> Response {
    let fmt = match format.as_str() {
        "hls" | "dash" => format.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "format must be 'hls' or 'dash'".into(),
                }),
            )
                .into_response();
        }
    };

    let key = CacheKeys::job_state(&content_id, fmt);
    match state.cache.get(&key) {
        Ok(Some(data)) => match serde_json::from_slice::<JobStatus>(&data) {
            Ok(status) => {
                let state_str = format!("{:?}", status.state);
                let output_dir = if state_str == "Complete" {
                    Some(format!("sandbox/output/{content_id}/{fmt}/"))
                } else {
                    None
                };
                (
                    StatusCode::OK,
                    Json(StatusResponse {
                        state: state_str,
                        segments_completed: status.segments_completed,
                        segments_total: status.segments_total,
                        output_dir,
                    }),
                )
                    .into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "corrupt job state".into(),
                }),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no job found".into(),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("cache error: {e}"),
            }),
        )
            .into_response(),
    }
}

async fn handle_output(
    State(state): State<Arc<AppState>>,
    Path((content_id, format, file)): Path<(String, String, String)>,
) -> Response {
    let fmt = match format.as_str() {
        "hls" | "dash" => format.as_str(),
        _ => {
            return (StatusCode::BAD_REQUEST, "invalid format").into_response();
        }
    };

    let output_format = match fmt {
        "hls" => OutputFormat::Hls,
        _ => OutputFormat::Dash,
    };

    // Manifest
    if file == "manifest" || file == format!("manifest.{}", output_format.manifest_extension()) {
        let key = CacheKeys::manifest_state(&content_id, fmt);
        match state.cache.get(&key) {
            Ok(Some(data)) => match serde_json::from_slice::<ManifestState>(&data) {
                Ok(ms) => match manifest::render_manifest(&ms) {
                    Ok(rendered) => {
                        let ct = output_format.content_type();
                        ([(axum::http::header::CONTENT_TYPE, ct)], rendered).into_response()
                    }
                    Err(e) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("render error: {e}"))
                            .into_response()
                    }
                },
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("deserialize error: {e}"),
                )
                    .into_response(),
            },
            Ok(None) => (StatusCode::NOT_FOUND, "manifest not found").into_response(),
            Err(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("cache error: {e}")).into_response()
            }
        }
    }
    // Init segment
    else if file == "init.mp4" {
        let key = CacheKeys::init_segment(&content_id, fmt);
        serve_binary(&state.cache, &key, "video/mp4").await
    }
    // Media segment
    else if let Some(num) = parse_segment_number(&file) {
        let key = CacheKeys::media_segment(&content_id, fmt, num);
        serve_binary(&state.cache, &key, "video/mp4").await
    } else {
        (StatusCode::NOT_FOUND, "unknown file").into_response()
    }
}

async fn serve_binary(cache: &InMemoryCacheBackend, key: &str, content_type: &str) -> Response {
    match cache.get(key) {
        Ok(Some(data)) => ([(axum::http::header::CONTENT_TYPE, content_type)], data).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("cache error: {e}")).into_response(),
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn generate_content_id(source_url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    source_url.hash(&mut hasher);
    let hash = hasher.finish();
    format!("sb-{hash:016x}")
}

fn is_local_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || (!s.contains("://") && !s.starts_with("http"))
}

fn parse_segment_number(filename: &str) -> Option<u32> {
    let name = filename
        .strip_suffix(".cmfv")
        .or_else(|| filename.strip_suffix(".m4s"))?;
    let num_str = name.strip_prefix("segment_")?;
    num_str.parse().ok()
}

async fn start_local_file_server(path: &str) -> Result<String, String> {
    let abs_path = std::fs::canonicalize(path).map_err(|e| format!("path not found: {e}"))?;
    let parent = abs_path
        .parent()
        .ok_or("cannot determine parent directory")?
        .to_path_buf();
    let filename = abs_path
        .file_name()
        .ok_or("cannot determine filename")?
        .to_str()
        .ok_or("filename is not valid UTF-8")?
        .to_string();

    let serve_dir = tower_http::services::ServeDir::new(&parent);
    let app = Router::new().fallback_service(serve_dir);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind failed: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("addr error: {e}"))?
        .port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let url = format!("http://127.0.0.1:{port}/{filename}");
    eprintln!("  Serving local files from {} on port {port}", parent.display());
    Ok(url)
}

fn write_output_to_disk(
    content_id: &str,
    format: OutputFormat,
    cache: &InMemoryCacheBackend,
) -> Result<(), String> {
    let fmt_str = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };

    let out_dir = PathBuf::from(format!("sandbox/output/{content_id}/{fmt_str}"));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("create output dir: {e}"))?;

    // Write manifest
    let manifest_key = CacheKeys::manifest_state(content_id, fmt_str);
    if let Ok(Some(data)) = cache.get(&manifest_key) {
        if let Ok(state) = serde_json::from_slice::<ManifestState>(&data) {
            if let Ok(rendered) = manifest::render_manifest(&state) {
                let ext = format.manifest_extension();
                let manifest_path = out_dir.join(format!("manifest.{ext}"));
                std::fs::write(&manifest_path, rendered)
                    .map_err(|e| format!("write manifest: {e}"))?;
                eprintln!("  Wrote {}", manifest_path.display());
            }
        }
    }

    // Write init segment
    let init_key = CacheKeys::init_segment(content_id, fmt_str);
    if let Ok(Some(data)) = cache.get(&init_key) {
        let init_path = out_dir.join("init.mp4");
        std::fs::write(&init_path, &data).map_err(|e| format!("write init segment: {e}"))?;
        eprintln!("  Wrote {} ({} bytes)", init_path.display(), data.len());
    }

    // Write media segments
    // Determine extension from manifest state if available
    let seg_ext = if let Ok(Some(state_data)) = cache.get(&manifest_key) {
        if let Ok(state) = serde_json::from_slice::<ManifestState>(&state_data) {
            state.container_format.video_segment_extension().to_string()
        } else {
            ".cmfv".to_string()
        }
    } else {
        ".cmfv".to_string()
    };

    let mut seg_num = 0u32;
    loop {
        let seg_key = CacheKeys::media_segment(content_id, fmt_str, seg_num);
        match cache.get(&seg_key) {
            Ok(Some(data)) => {
                let seg_path = out_dir.join(format!("segment_{seg_num}{seg_ext}"));
                std::fs::write(&seg_path, &data)
                    .map_err(|e| format!("write segment {seg_num}: {e}"))?;
                seg_num += 1;
            }
            _ => break,
        }
    }
    if seg_num > 0 {
        eprintln!("  Wrote {seg_num} media segments to {}", out_dir.display());
    }

    Ok(())
}

// ─── Main ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(AppState {
        cache: InMemoryCacheBackend::new(),
        jobs: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/api/repackage", post(handle_repackage))
        .route("/api/status/{content_id}/{format}", get(handle_status))
        .route(
            "/api/output/{content_id}/{format}/{file}",
            get(handle_output),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3333")
        .await
        .expect("failed to bind to port 3333");

    eprintln!();
    eprintln!("  edge-packager sandbox running on http://localhost:3333");
    eprintln!();

    axum::serve(listener, app)
        .await
        .expect("server error");
}

// ─── Embedded HTML UI ───────────────────────────────────────────────────

const SANDBOX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>edge-packager sandbox</title>
<style>
  :root {
    --bg: #0f1117;
    --surface: #1a1d27;
    --border: #2a2d3a;
    --text: #e4e4e7;
    --text-muted: #8b8d98;
    --accent: #6366f1;
    --accent-hover: #818cf8;
    --success: #22c55e;
    --error: #ef4444;
    --warning: #f59e0b;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, monospace;
    background: var(--bg);
    color: var(--text);
    min-height: 100vh;
    padding: 2rem;
  }
  .container { max-width: 640px; margin: 0 auto; }
  h1 {
    font-size: 1.5rem;
    font-weight: 600;
    margin-bottom: 0.25rem;
  }
  .subtitle {
    color: var(--text-muted);
    font-size: 0.875rem;
    margin-bottom: 2rem;
  }
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 0.75rem;
    padding: 1.5rem;
    margin-bottom: 1.5rem;
  }
  label {
    display: block;
    font-size: 0.8rem;
    color: var(--text-muted);
    margin-bottom: 0.375rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  input[type="text"], select {
    width: 100%;
    padding: 0.625rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    color: var(--text);
    font-size: 0.875rem;
    font-family: inherit;
    margin-bottom: 1rem;
    outline: none;
    transition: border-color 0.2s;
  }
  input[type="text"]:focus, select:focus {
    border-color: var(--accent);
  }
  input[type="text"]::placeholder {
    color: var(--text-muted);
    opacity: 0.6;
  }
  .radio-group {
    display: flex;
    gap: 1rem;
    margin-bottom: 1rem;
  }
  .radio-group label {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    text-transform: none;
    letter-spacing: 0;
    font-size: 0.875rem;
    color: var(--text);
  }
  input[type="radio"] {
    accent-color: var(--accent);
  }
  .format-hint {
    display: inline-block;
    font-size: 0.75rem;
    padding: 0.125rem 0.5rem;
    border-radius: 0.25rem;
    background: var(--accent);
    color: white;
    margin-left: 0.5rem;
    opacity: 0;
    transition: opacity 0.2s;
  }
  .format-hint.visible { opacity: 1; }
  button {
    width: 100%;
    padding: 0.75rem;
    background: var(--accent);
    color: white;
    border: none;
    border-radius: 0.5rem;
    font-size: 0.9rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.2s;
    font-family: inherit;
  }
  button:hover { background: var(--accent-hover); }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  #status-panel {
    display: none;
  }
  .progress-bar {
    width: 100%;
    height: 6px;
    background: var(--bg);
    border-radius: 3px;
    overflow: hidden;
    margin: 0.75rem 0;
  }
  .progress-fill {
    height: 100%;
    background: var(--accent);
    border-radius: 3px;
    transition: width 0.3s ease;
    width: 0%;
  }
  .progress-fill.complete { background: var(--success); }
  .progress-fill.error { background: var(--error); }
  .status-text {
    font-size: 0.8rem;
    color: var(--text-muted);
  }
  .status-text .state {
    color: var(--warning);
    font-weight: 600;
  }
  .status-text .state.complete { color: var(--success); }
  .status-text .state.failed { color: var(--error); }
  .output-info {
    margin-top: 1rem;
    padding: 0.75rem;
    background: var(--bg);
    border-radius: 0.5rem;
    font-size: 0.8rem;
  }
  .output-info code {
    color: var(--accent);
    font-family: inherit;
  }
  .output-links {
    margin-top: 0.75rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .output-links a {
    color: var(--accent);
    text-decoration: none;
    font-size: 0.8rem;
    padding: 0.25rem 0.625rem;
    border: 1px solid var(--border);
    border-radius: 0.375rem;
    transition: border-color 0.2s;
  }
  .output-links a:hover {
    border-color: var(--accent);
  }
  .hidden { display: none; }
</style>
</head>
<body>
<div class="container">
  <h1>edge-packager sandbox</h1>
  <p class="subtitle">Local repackaging tool &mdash; configurable encryption &amp; container format</p>

  <div class="card">
    <label for="source-url">Source Manifest URL or Local Path
      <span id="format-hint" class="format-hint">HLS</span>
    </label>
    <input type="text" id="source-url" placeholder="https://cdn.example.com/master.m3u8 or ./path/to/manifest.mpd">

    <div id="speke-section">
      <label for="speke-url">SPEKE License Server URL</label>
      <input type="text" id="speke-url" placeholder="https://drm-provider.example.com/speke/v2">

      <label for="speke-auth-type">SPEKE Authentication</label>
      <select id="speke-auth-type">
        <option value="bearer">Bearer Token</option>
        <option value="api_key">API Key</option>
        <option value="basic">Basic Auth</option>
      </select>

      <label id="auth-value-label" for="speke-auth-value">Bearer Token</label>
      <input type="text" id="speke-auth-value" placeholder="your-token-here">

      <div id="api-key-header-row" class="hidden">
        <label for="api-key-header">API Key Header Name</label>
        <input type="text" id="api-key-header" placeholder="x-api-key" value="x-api-key">
      </div>
    </div>
    <p id="clear-hint" class="hidden" style="font-size:0.8rem;color:var(--text-muted);margin:0.25rem 0;">Source encryption is auto-detected. SPEKE fields hidden because target is clear (no encryption).</p>

    <label>Output Format</label>
    <div class="radio-group">
      <label><input type="radio" name="output-format" value="hls" checked> HLS (.m3u8)</label>
      <label><input type="radio" name="output-format" value="dash"> DASH (.mpd)</label>
    </div>

    <label>Target Encryption Scheme</label>
    <div class="radio-group">
      <label><input type="radio" name="target-scheme" value="cenc" checked> CENC (AES-CTR)</label>
      <label><input type="radio" name="target-scheme" value="cbcs"> CBCS (AES-CBC)</label>
      <label><input type="radio" name="target-scheme" value="none"> None (Clear)</label>
    </div>

    <label>Container Format</label>
    <div class="radio-group">
      <label><input type="radio" name="container-format" value="cmaf" checked> CMAF (.cmfv)</label>
      <label><input type="radio" name="container-format" value="fmp4"> fMP4 (.m4s)</label>
    </div>

    <button id="submit-btn" onclick="startRepackage()">Repackage</button>
  </div>

  <div id="status-panel" class="card">
    <div class="status-text">
      <span id="status-state" class="state">Pending</span>
      <span id="status-segments"></span>
    </div>
    <div class="progress-bar">
      <div id="progress-fill" class="progress-fill"></div>
    </div>
    <div id="output-section" class="hidden">
      <div class="output-info">
        Output written to <code id="output-dir"></code>
      </div>
      <div class="output-links" id="output-links"></div>
    </div>
  </div>
</div>

<script>
const sourceInput = document.getElementById('source-url');
const formatHint = document.getElementById('format-hint');
const authType = document.getElementById('speke-auth-type');
const authLabel = document.getElementById('auth-value-label');
const authValue = document.getElementById('speke-auth-value');
const apiKeyRow = document.getElementById('api-key-header-row');

sourceInput.addEventListener('input', () => {
  const v = sourceInput.value.toLowerCase();
  if (v.includes('.m3u8')) {
    formatHint.textContent = 'HLS';
    formatHint.classList.add('visible');
  } else if (v.includes('.mpd')) {
    formatHint.textContent = 'DASH';
    formatHint.classList.add('visible');
  } else {
    formatHint.classList.remove('visible');
  }
});

authType.addEventListener('change', () => {
  const t = authType.value;
  if (t === 'bearer') {
    authLabel.textContent = 'Bearer Token';
    authValue.placeholder = 'your-token-here';
    apiKeyRow.classList.add('hidden');
  } else if (t === 'api_key') {
    authLabel.textContent = 'API Key Value';
    authValue.placeholder = 'your-api-key';
    apiKeyRow.classList.remove('hidden');
  } else {
    authLabel.textContent = 'Credentials (username:password)';
    authValue.placeholder = 'user:pass';
    apiKeyRow.classList.add('hidden');
  }
});

// Toggle SPEKE section visibility based on target encryption scheme
const spekeSection = document.getElementById('speke-section');
const clearHint = document.getElementById('clear-hint');
document.querySelectorAll('input[name="target-scheme"]').forEach(radio => {
  radio.addEventListener('change', () => {
    const scheme = document.querySelector('input[name="target-scheme"]:checked').value;
    if (scheme === 'none') {
      spekeSection.classList.add('hidden');
      clearHint.classList.remove('hidden');
    } else {
      spekeSection.classList.remove('hidden');
      clearHint.classList.add('hidden');
    }
  });
});

let pollTimer = null;

async function startRepackage() {
  const btn = document.getElementById('submit-btn');
  btn.disabled = true;
  btn.textContent = 'Starting...';

  const outputFormat = document.querySelector('input[name="output-format"]:checked').value;
  const targetScheme = document.querySelector('input[name="target-scheme"]:checked').value;
  const containerFormat = document.querySelector('input[name="container-format"]:checked').value;

  const body = {
    source_url: sourceInput.value,
    speke_url: document.getElementById('speke-url').value,
    speke_auth_type: authType.value,
    speke_auth_value: authValue.value,
    speke_api_key_header: document.getElementById('api-key-header').value,
    output_format: outputFormat,
    target_scheme: targetScheme,
    container_format: containerFormat,
  };

  try {
    const resp = await fetch('/api/repackage', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await resp.json();

    if (!resp.ok) {
      alert('Error: ' + (data.error || 'unknown error'));
      btn.disabled = false;
      btn.textContent = 'Repackage';
      return;
    }

    // Show status panel and start polling
    const panel = document.getElementById('status-panel');
    panel.style.display = 'block';
    document.getElementById('status-state').textContent = 'Pending';
    document.getElementById('status-state').className = 'state';
    document.getElementById('status-segments').textContent = '';
    document.getElementById('progress-fill').style.width = '0%';
    document.getElementById('progress-fill').className = 'progress-fill';
    document.getElementById('output-section').classList.add('hidden');

    if (pollTimer) clearInterval(pollTimer);
    const storedContainerFormat = data.container_format || 'cmaf';
    pollTimer = setInterval(() => pollStatus(data.content_id, data.format, storedContainerFormat), 1000);
  } catch (e) {
    alert('Request failed: ' + e.message);
    btn.disabled = false;
    btn.textContent = 'Repackage';
  }
}

async function pollStatus(contentId, format, containerFormat) {
  try {
    const resp = await fetch(`/api/status/${contentId}/${format}`);
    if (!resp.ok) return;
    const data = await resp.json();

    const stateEl = document.getElementById('status-state');
    const segEl = document.getElementById('status-segments');
    const fill = document.getElementById('progress-fill');

    stateEl.textContent = data.state;
    stateEl.className = 'state';

    if (data.segments_total) {
      const pct = Math.round((data.segments_completed / data.segments_total) * 100);
      segEl.textContent = ` \u2014 ${data.segments_completed}/${data.segments_total} segments (${pct}%)`;
      fill.style.width = pct + '%';
    } else if (data.segments_completed > 0) {
      segEl.textContent = ` \u2014 ${data.segments_completed} segments`;
      fill.style.width = '10%';
    }

    if (data.state === 'Complete') {
      stateEl.classList.add('complete');
      fill.classList.add('complete');
      clearInterval(pollTimer);
      pollTimer = null;

      const btn = document.getElementById('submit-btn');
      btn.disabled = false;
      btn.textContent = 'Repackage';

      // Show output info
      if (data.output_dir) {
        document.getElementById('output-dir').textContent = data.output_dir;
        const linksEl = document.getElementById('output-links');
        linksEl.innerHTML = '';

        const base = `/api/output/${contentId}/${format}`;
        const ext = format === 'hls' ? 'm3u8' : 'mpd';
        const segExt = containerFormat === 'fmp4' ? '.m4s' : '.cmfv';
        linksEl.innerHTML += `<a href="${base}/manifest.${ext}" target="_blank">manifest.${ext}</a>`;
        linksEl.innerHTML += `<a href="${base}/init.mp4" target="_blank">init.mp4</a>`;
        for (let i = 0; i < data.segments_completed; i++) {
          linksEl.innerHTML += `<a href="${base}/segment_${i}${segExt}" target="_blank">segment_${i}${segExt}</a>`;
        }

        document.getElementById('output-section').classList.remove('hidden');
      }
    } else if (data.state === 'Failed') {
      stateEl.classList.add('failed');
      fill.classList.add('error');
      fill.style.width = '100%';
      clearInterval(pollTimer);
      pollTimer = null;

      const btn = document.getElementById('submit-btn');
      btn.disabled = false;
      btn.textContent = 'Repackage';
    }
  } catch (e) {
    // Ignore polling errors
  }
}
</script>
</body>
</html>
"#;
