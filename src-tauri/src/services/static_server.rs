//! Loopback HTTP server for the workbench's static files (`out/`, `extensions/`).
//!
//! WebView2 does not keep custom-scheme responses (`vscode-app://`) in its HTTP cache,
//! so it parses and compiles the whole workbench bundle on every launch. Files served
//! from `http://127.0.0.1` are cached together with their compiled code.
//!
//! The origin must stay the same between launches for the cache to apply, so the port
//! is derived from the install path (with fallbacks). Files are revalidated with an
//! ETag on every load, so an update never runs stale code. Read-only, loopback only,
//! limited to the app's own installed files; CORS only for the app's origin.

use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// `http://127.0.0.1:<port>`.
static ORIGIN: OnceLock<String> = OnceLock::new();

/// Origins allowed to use the files (the workbench page).
const APP_ORIGINS: &[&str] = &["http://vscode-app.localhost", "vscode-app://localhost"];

/// Starts the server once; returns its origin.
pub fn start(app_root: &Path) -> Result<&'static str, String> {
    if let Some(o) = ORIGIN.get() {
        return Ok(o);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    app_root.hash(&mut h);
    let preferred = 20_000 + (h.finish() % 30_000) as u16;
    let server = (0..8)
        .find_map(|i| tiny_http::Server::http(("127.0.0.1", preferred + i * 7)).ok())
        .map(Ok)
        .unwrap_or_else(|| tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string()))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or("static server has no ip address")?;
    let _ = ORIGIN.set(format!("http://127.0.0.1:{port}"));

    let server = Arc::new(server);
    let root = Arc::new(app_root.to_path_buf());
    // Several workers: the workbench loads many files in parallel.
    for i in 0..4 {
        let (server, root) = (server.clone(), root.clone());
        std::thread::Builder::new()
            .name(format!("static-server-{i}"))
            .spawn(move || {
                for request in server.incoming_requests() {
                    let response = respond(&root, &request);
                    let _ = request.respond(response);
                }
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(ORIGIN.get().expect("set above"))
}

fn respond(root: &Path, request: &tiny_http::Request) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let not_found = || tiny_http::Response::from_data(b"not found".to_vec()).with_status_code(404);
    let path = request.url().split(['?', '#']).next().unwrap_or("");
    let Some(rel) = path.strip_prefix("/static/") else { return not_found() };
    let rel = PathBuf::from(super::app_protocol::percent_decode(rel));
    // Only plain relative paths inside out/ and extensions/.
    if rel.components().any(|c| !matches!(c, Component::Normal(_)))
        || !matches!(rel.components().next(), Some(Component::Normal(f)) if f == "out" || f == "extensions")
    {
        return not_found();
    }
    let full = root.join(&rel);
    let Ok(meta) = std::fs::metadata(&full) else { return not_found() };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{:x}-{:x}\"", meta.len(), mtime);

    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string())
        .filter(|o| APP_ORIGINS.contains(&o.as_str()));
    let with_common = |r: tiny_http::Response<std::io::Cursor<Vec<u8>>>| {
        let r = r
            .with_header(header("ETag", &etag))
            .with_header(header("Cache-Control", "no-cache"));
        match &origin {
            Some(o) => r.with_header(header("Access-Control-Allow-Origin", o)).with_header(header("Vary", "Origin")).with_header(header("Timing-Allow-Origin", o)),
            None => r,
        }
    };

    let if_none_match = request.headers().iter().find(|h| h.field.equiv("If-None-Match")).map(|h| h.value.as_str());
    if if_none_match == Some(etag.as_str()) {
        return with_common(tiny_http::Response::from_data(Vec::new()).with_status_code(304));
    }
    match std::fs::read(&full) {
        Ok(body) => with_common(
            tiny_http::Response::from_data(body)
                .with_header(header("Content-Type", super::app_protocol::mime_for(&full))),
        ),
        Err(_) => not_found(),
    }
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}
