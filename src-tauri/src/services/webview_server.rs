//! Loopback HTTP server for workbench webviews (markdown preview, extension
//! details, extension webviews).
//!
//! VS Code isolates every webview in its own origin: the iframe must be served
//! from `<hash>.<host>`, and `pre/index.html` refuses to start otherwise. The
//! `vscode-app://` protocol cannot serve per-webview subdomains, but Chromium
//! resolves any `*.localhost` name to the loopback address, so a tiny server on
//! `127.0.0.1` gives each webview a distinct origin
//! (`http://<hash>.localhost:<port>/`).
//!
//! It only serves the files of `out/vs/workbench/contrib/webview/browser/pre/`
//! (index.html, service-worker.js, fake.html). Webview content itself is
//! streamed by the workbench through the service worker, not by this server.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PORT: OnceLock<u16> = OnceLock::new();

/// Port the server listens on, once started.
pub fn port() -> Option<u16> {
    PORT.get().copied()
}

/// Starts the server on a random loopback port (idempotent).
pub fn start(app_root: &Path) -> Result<u16, String> {
    if let Some(port) = port() {
        return Ok(port);
    }
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or("webview server has no ip address")?;
    let _ = PORT.set(port);

    let pre_dir = app_root.join("out/vs/workbench/contrib/webview/browser/pre");
    std::thread::Builder::new()
        .name("webview-server".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                let response = respond(&pre_dir, request.url());
                let _ = request.respond(response);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(port)
}

fn respond(pre_dir: &Path, url: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let path = url.split(['?', '#']).next().unwrap_or("/");
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };

    // Only flat file names from pre/ are served.
    let allowed = matches!(name, "index.html" | "service-worker.js" | "fake.html");
    let file: Option<PathBuf> = allowed.then(|| pre_dir.join(name));
    match file.and_then(|f| std::fs::read(f).ok()) {
        Some(body) => {
            let mime = if name.ends_with(".js") {
                "text/javascript; charset=utf-8"
            } else {
                "text/html; charset=utf-8"
            };
            let mut response = tiny_http::Response::from_data(body)
                .with_header(header("Content-Type", mime))
                .with_header(header("Cache-Control", "no-cache"));
            if name == "service-worker.js" {
                // The service worker controls the whole webview origin.
                response = response.with_header(header("Service-Worker-Allowed", "/"));
            }
            response
        }
        None => tiny_http::Response::from_data(b"not found".to_vec()).with_status_code(404),
    }
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}
