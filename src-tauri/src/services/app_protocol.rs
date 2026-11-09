//! `vscode-app://` protocol: serves the compiled workbench straight from disk,
//! so the app no longer needs a Node web server (`@vscode/test-web`).
//!
//! * `/` renders `out/vs/code/browser/workbench/workbench-dev.html`, filling in
//!   the placeholders the VS Code web server would normally provide.
//! * Every other path is served as a file relative to the app root
//!   under `/static/` (`/static/out/...`, `/static/extensions/...`), never
//!   outside of it.
//!
//! On Windows/Android WebView2 exposes the scheme as
//! `http://vscode-app.localhost/`; elsewhere it is `vscode-app://localhost/`.

use std::path::{Component, Path, PathBuf};

use tauri::http::{header, Request, Response, StatusCode};

pub const SCHEME: &str = "vscode-app";

/// Path prefix under which the app root is served (like the VS Code server's static route).
const STATIC_PREFIX: &str = "/static";

/// URL the main window should load.
pub fn start_url() -> String {
    if cfg!(any(windows, target_os = "android")) {
        format!("http://{SCHEME}.localhost/")
    } else {
        format!("{SCHEME}://localhost/")
    }
}

/// Base URL as seen by the page (no trailing slash).
fn base_url() -> String {
    start_url().trim_end_matches('/').to_string()
}

static APP_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Resolves the app root once at startup:
/// 1. `VSCODE_APP_ROOT` (explicit override),
/// 2. release builds: `<resource dir>/app` (assembled by scripts/code-rs/stage-app.mjs),
/// 3. dev builds: the repository checkout (`<repo>/src-tauri/..`).
pub fn init_app_root<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager as _;
    let root = if let Ok(root) = std::env::var("VSCODE_APP_ROOT") {
        PathBuf::from(root)
    } else {
        let bundled = app.path().resource_dir().ok().map(|d| d.join("app"));
        match bundled {
            Some(dir) if !cfg!(debug_assertions) && dir.join("out").is_dir() => dunce_canonicalize(&dir).unwrap_or(dir),
            _ => dev_root(),
        }
    };
    let _ = APP_ROOT.set(root);
}

/// Root that contains `out/`, `extensions/` and `node_modules/`.
pub fn app_root() -> PathBuf {
    APP_ROOT.get().cloned().unwrap_or_else(dev_root)
}

fn dev_root() -> PathBuf {
    if let Ok(root) = std::env::var("VSCODE_APP_ROOT") {
        return PathBuf::from(root);
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    dunce_canonicalize(&dev).unwrap_or(dev)
}

fn dunce_canonicalize(p: &Path) -> Option<PathBuf> {
    let c = std::fs::canonicalize(p).ok()?;
    // Strip the Windows verbatim prefix (\\?\C:\...) so it can be used in URIs.
    let s = c.to_string_lossy();
    Some(PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()))
}

pub fn handle(request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = percent_decode(request.uri().path());
    let root = app_root();
    let result = if path == "/" || path == "/index.html" {
        render_workbench(&root).map(|body| (body.into_bytes(), "text/html; charset=utf-8"))
    } else {
        match path.strip_prefix(STATIC_PREFIX) {
            Some(rest) => serve_file(&root, rest),
            None => Err((StatusCode::NOT_FOUND, format!("not found: {path}"))),
        }
    };
    match result {
        Ok((body, mime)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap(),
        Err((status, msg)) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(msg.into_bytes())
            .unwrap(),
    }
}

fn serve_file(root: &Path, url_path: &str) -> Result<(Vec<u8>, &'static str), (StatusCode, String)> {
    let rel = Path::new(url_path.trim_start_matches('/'));
    // Reject `..`, absolute paths and drive prefixes.
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err((StatusCode::FORBIDDEN, "forbidden".into()));
    }
    let full = root.join(rel);
    match std::fs::read(&full) {
        Ok(bytes) => Ok((bytes, mime_for(&full))),
        Err(_) => Err((StatusCode::NOT_FOUND, format!("not found: {url_path}"))),
    }
}

fn mime_for(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn render_workbench(root: &Path) -> Result<String, (StatusCode, String)> {
    let tpl_path = root.join("out/vs/code/browser/workbench/workbench-dev.html");
    let tpl = std::fs::read_to_string(&tpl_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", tpl_path.display())))?;

    let base = base_url();
    let config = serde_json::json!({
        "productConfiguration": {
            "enableTelemetry": false,
            // Open VSX: the open extension registry usable by non-Microsoft builds.
            "extensionsGallery": {
                "serviceUrl": "https://open-vsx.org/vscode/gallery",
                "itemUrl": "https://open-vsx.org/vscode/item",
                "resourceUrlTemplate": "https://open-vsx.org/vscode/unpkg/{publisher}/{name}/{version}/{path}",
                "extensionUrlTemplate": "https://open-vsx.org/vscode/gallery/{publisher}/{name}/latest",
                "controlUrl": "",
                "nlsBaseUrl": "",
            },
            // Each webview gets its own origin: http://<hash>.localhost:<port>/
            // (see webview_server.rs).
            "webviewContentExternalBaseUrlTemplate": match super::webview_server::start(root) {
                Ok(port) => format!("http://{{{{uuid}}}}.localhost:{port}/"),
                Err(_) => format!("{base}{STATIC_PREFIX}/out/vs/workbench/contrib/webview/browser/pre/"),
            },
        },
    });

    let values: [(&str, String); 6] = [
        ("WORKBENCH_WEB_CONFIGURATION", html_attr(&config.to_string())),
        ("WORKBENCH_WEB_BASE_URL", STATIC_PREFIX.to_string()),
        ("WORKBENCH_AUTH_SESSION", String::new()),
        ("WORKBENCH_SCRIPT_NONCE", String::new()),
        ("WORKBENCH_BUILTIN_EXTENSIONS", html_attr(&builtin_extensions(root).to_string())),
        ("WORKBENCH_DEV_CSS_MODULES", html_attr(&serde_json::to_string(&css_modules(root)).unwrap())),
    ];
    let mut out = tpl;
    for (k, v) in values {
        out = out.replace(&format!("{{{{{k}}}}}"), &v);
    }
    // Paint the desktop-style layout skeleton before the (large) workbench
    // module graph loads, so window reloads keep the layout on screen.
    let workbench_script = format!(
        r#"<script type="module" src="{STATIC_PREFIX}/out/vs/code/browser/workbench/workbench.js"></script>"#
    );
    let splash_script = format!(
        r#"<script type="module" src="{STATIC_PREFIX}/out/vs/code/browser/workbench/tauriSplash.js"></script>"#
    );
    out = out.replacen(&workbench_script, &format!("{splash_script}\n\t{workbench_script}"), 1);
    Ok(out)
}

/// All `.css` files under `out/`, relative to `out/` with forward slashes.
fn css_modules(root: &Path) -> Vec<String> {
    let out_dir = root.join("out");
    let mut list = Vec::new();
    for entry in ignore::WalkBuilder::new(&out_dir).standard_filters(false).build().flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "css") {
            if let Ok(rel) = p.strip_prefix(&out_dir) {
                list.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    list.sort();
    list
}

/// `[{ extensionPath, packageJSON, packageNLS? }]` for every folder in `extensions/`.
fn builtin_extensions(root: &Path) -> serde_json::Value {
    let mut list = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("extensions")) else {
        return serde_json::Value::Array(list);
    };
    for entry in entries.flatten() {
        // The proprietary Copilot extension is not part of code-rs (see stage-app.mjs).
        if entry.file_name() == "copilot" || entry.file_name() == "node_modules" {
            continue;
        }
        let dir = entry.path();
        let Ok(pkg) = std::fs::read_to_string(dir.join("package.json")) else { continue };
        let Ok(package_json) = serde_json::from_str::<serde_json::Value>(&pkg) else { continue };
        let mut item = serde_json::json!({
            "extensionPath": entry.file_name().to_string_lossy(),
            "packageJSON": package_json,
        });
        if let Ok(nls) = std::fs::read_to_string(dir.join("package.nls.json")) {
            if let Ok(nls) = serde_json::from_str::<serde_json::Value>(&nls) {
                item["packageNLS"] = nls;
            }
        }
        list.push(item);
    }
    serde_json::Value::Array(list)
}

fn html_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
