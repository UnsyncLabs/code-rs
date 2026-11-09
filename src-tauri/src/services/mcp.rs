//! MCP (Model Context Protocol) server: lets external AI clients (Claude Code,
//! Claude Desktop, Kiro, ...) observe and drive code-rs.
//!
//! Design: docs/rust-rewrite/MCP_DESIGN.md. Phase 1 = transport, auth, audit
//! log and read / UI tools.
//!
//! * Off by default: the workbench calls `mcp_set_enabled` from the
//!   `codeRs.mcp.enabled` setting.
//! * Streamable HTTP (JSON responses) on `127.0.0.1:<random port>/mcp`, bearer
//!   token required, browser `Origin`s and foreign `Host`s rejected.
//! * `<app data>/mcp/endpoint.json` = `{ port, token, pid }` for discovery; the
//!   `code-rs --mcp` stdio bridge (see `stdio_bridge`) reads it.
//! * Tools run in the workbench: Rust emits `mcp://request` with an id and waits
//!   for `mcp_respond`. Every call is appended to `<app data>/mcp/audit.jsonl`.
//! * No telemetry: nothing leaves the machine.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Long enough for the user to read a confirmation dialog or for a command / AI review to finish.
const TOOL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

struct McpServer {
    port: u16,
    token: String,
    enabled: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    dir: PathBuf,
    /// Open SSE streams (GET /mcp): server-to-client notifications.
    streams: Mutex<Vec<mpsc::Sender<String>>>,
    /// Messages the user sent to the AI ("Ask the AI..."), until a client reads them.
    inbox: Mutex<Vec<Value>>,
}

const MAX_INBOX: usize = 50;

static SERVER: OnceLock<McpServer> = OnceLock::new();

/// Tool catalog (phase 1). Levels: 0 = read, 1 = UI (see design §4).
/// Reusable flows for any MCP client: (name, description, argument, template).
fn prompts() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("review-extension", "Review an extension (or update) for security before installing it", "id",
         "Use the code-rs MCP tools to decide whether the extension {} is safe to install. Call extensions_audit (and assistant_review_extension if a reviewer is configured), explain the risk in plain words, then record your verdict with extensions_review_submit. Do not install it unless I ask."),
        ("fix-diagnostics", "Fix the errors code-rs reports in a file, proposing the changes for approval", "path",
         "Call diagnostics_get for {}, read the file with file_read, and fix the errors. Apply fixes only with edit_propose (I will see the diff), one coherent change at a time."),
        ("safe-command", "Check a shell command with the code-rs guardian before running it", "command",
         "Call command_assess for this command: {}
Explain what it would change or delete and whether it can be undone. If the risk is medium or high, suggest a safer alternative before running anything."),
    ]
}

fn tools() -> Value {
    let path = json!({ "type": "string", "description": "Absolute path, or relative to the first workspace folder" });
    json!([
        { "name": "window_list", "description": "List code-rs windows with their workspace folders and active editor.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": { "readOnlyHint": true } },
        { "name": "workspace_open_folder", "description": "Open a folder in the window (replaces the current folder; the window reloads).",
          "inputSchema": { "type": "object", "properties": { "path": path }, "required": ["path"] } },
        { "name": "workspace_add_folder", "description": "Add a folder to the window's workspace (no reload).",
          "inputSchema": { "type": "object", "properties": { "path": path }, "required": ["path"] } },
        { "name": "workspace_close_folder", "description": "Close the window's folder or workspace.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "file_read", "description": "Read a text file inside the workspace. Optional 1-based line range.",
          "inputSchema": { "type": "object", "properties": { "path": path,
              "startLine": { "type": "integer", "minimum": 1 }, "endLine": { "type": "integer", "minimum": 1 } }, "required": ["path"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "file_list", "description": "List a directory inside the workspace (one level).",
          "inputSchema": { "type": "object", "properties": { "path": path } }, "annotations": { "readOnlyHint": true } },
        { "name": "editor_state", "description": "Active editor (file, selection, cursor), open editors and unsaved files.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": { "readOnlyHint": true } },
        { "name": "editor_open", "description": "Open a file in the editor and optionally reveal a line/column.",
          "inputSchema": { "type": "object", "properties": { "path": path,
              "line": { "type": "integer", "minimum": 1 }, "column": { "type": "integer", "minimum": 1 } }, "required": ["path"] } },
        { "name": "diagnostics_get", "description": "Errors and warnings reported by language extensions, for one file or the whole workspace.",
          "inputSchema": { "type": "object", "properties": { "path": path } }, "annotations": { "readOnlyHint": true } },
        { "name": "extensions_audit", "description": "Security report for an extension from Open VSX (optionally a specific version) without installing it: origin, SHA-256, publisher, native binaries, scripts, child_process/eval usage, network domains, obfuscation, and changes since the installed version. Static analysis: signals, not proof.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "publisher.name, e.g. ms-python.python" },
              "version": { "type": "string", "description": "Defaults to the latest version" } }, "required": ["id"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "assistant_providers", "description": "AI providers configured in code-rs (no API keys are ever returned) and whether each is ready.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": { "readOnlyHint": true } },
        { "name": "assistant_review_extension", "description": "Audit an Open VSX extension (like extensions_audit) and get the configured code-rs AI reviewer's opinion on it. Nothing is installed.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "version": { "type": "string" },
              "providerId": { "type": "string", "description": "Defaults to the extension reviewer chosen in settings" } }, "required": ["id"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "assistant_review_code", "description": "Ask the code reviewer configured in code-rs to review workspace files. If it is a cloud provider the user is asked once per workspace before code is sent.",
          "inputSchema": { "type": "object", "properties": { "paths": { "type": "array", "items": path, "maxItems": 20 },
              "focus": { "type": "string", "description": "Optional: what to focus on" } }, "required": ["paths"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "edit_propose", "description": "Propose changes to workspace files. code-rs shows the diff and the user accepts or rejects; accepted edits can be undone. Each edit either replaces the whole file (content) or replaces one exact occurrence of oldText with newText. New files are allowed with content.",
          "inputSchema": { "type": "object", "properties": {
              "edits": { "type": "array", "minItems": 1, "maxItems": 50, "items": { "type": "object", "properties": {
                  "path": path, "content": { "type": "string" }, "oldText": { "type": "string" }, "newText": { "type": "string" } }, "required": ["path"] } },
              "summary": { "type": "string", "description": "One line shown to the user: what and why" } }, "required": ["edits"] } },
        { "name": "extensions_install", "description": "Install or update an extension from Open VSX. The user confirms and sees the security report (plus reviews submitted with extensions_review_submit).",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "version": { "type": "string" } }, "required": ["id"] } },
        { "name": "extensions_uninstall", "description": "Uninstall a user-installed extension. The user confirms.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }, "annotations": { "destructiveHint": true } },
        { "name": "extensions_list", "description": "Installed extensions (id, version, built-in or user, enabled).",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": { "readOnlyHint": true } },
        { "name": "extensions_review_submit", "description": "Attach your security review of an extension (e.g. after extensions_audit). It is shown to the user in the install dialog of that extension during this session.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "version": { "type": "string" },
              "verdict": { "type": "string", "enum": ["looks-fine", "caution", "do-not-install"] }, "notes": { "type": "string" } }, "required": ["id", "verdict", "notes"] } },
        { "name": "command_run", "description": "Run an editor command. Allowed without extra risk: format document, organize imports, fix all, save, save all, markdown preview. Any other command id needs the user's confirmation. Optionally opens a file first.",
          "inputSchema": { "type": "object", "properties": { "command": { "type": "string" }, "path": path,
              "args": { "type": "array", "description": "Arguments for the command (JSON values)" } }, "required": ["command"] } },
        { "name": "command_assess", "description": "Ask the code-rs command guardian how risky a shell command is BEFORE running it (also for commands you run in your own terminal): risk low/medium/high, categories (recursive-delete, history-rewrite, download-and-execute, exfiltration, ...) and reasons. Signals, not a guarantee.",
          "inputSchema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }, "annotations": { "readOnlyHint": true } },
        { "name": "terminal_run", "description": "Run a shell command (PowerShell on Windows, $SHELL elsewhere) in a workspace folder and return its output. Assessed by the command guardian; the user's approval mode decides whether they are asked (default: always ask). Non-interactive: stdin is closed.",
          "inputSchema": { "type": "object", "properties": { "command": { "type": "string" }, "cwd": path,
              "timeoutMs": { "type": "integer", "minimum": 1000, "maximum": 600000 },
              "reason": { "type": "string", "description": "Why you need to run it (shown to the user)" } }, "required": ["command"] } },
        { "name": "ui_ask", "description": "Ask the user a question in code-rs and wait for the answer: yes/no (no options), a choice (options) or free text (text: true).",
          "inputSchema": { "type": "object", "properties": { "question": { "type": "string" },
              "options": { "type": "array", "items": { "type": "string" }, "maxItems": 10 }, "text": { "type": "boolean" } }, "required": ["question"] } },
        { "name": "user_messages", "description": "Messages the user sent to you from code-rs (command 'Ask the AI...'), with the file and selection they were looking at. Returns and clears the pending messages. Clients that show MCP notifications also get them live.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "ui_notify", "description": "Show a notification to the user in code-rs.",
          "inputSchema": { "type": "object", "properties": { "message": { "type": "string" },
              "severity": { "type": "string", "enum": ["info", "warning", "error"] } }, "required": ["message"] } },
        { "name": "ui_status", "description": "Show a short message in the status bar (e.g. 'Reviewing extension…').",
          "inputSchema": { "type": "object", "properties": { "message": { "type": "string" },
              "timeoutMs": { "type": "integer", "minimum": 500, "maximum": 60000 } }, "required": ["message"] } }
    ])
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn mcp_dir_for_identifier() -> Option<PathBuf> {
    // Same location as Tauri's app_data_dir() for the bundle identifier, so the
    // `--mcp` bridge can find it without a running Tauri app.
    dirs::data_dir().map(|d| d.join(IDENTIFIER).join("mcp"))
}

/// Bundle identifier (tauri.conf.json `identifier`).
const IDENTIFIER: &str = "dev.coders.workbench";

/// Enables or disables the server (driven by the `codeRs.mcp.enabled` setting).
#[tauri::command]
pub fn mcp_set_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<Option<u16>, String> {
    if enabled {
        let server = start(&app)?;
        server.enabled.store(true, Ordering::SeqCst);
        write_endpoint(server)?;
        Ok(Some(server.port))
    } else {
        if let Some(server) = SERVER.get() {
            server.enabled.store(false, Ordering::SeqCst);
            let _ = std::fs::remove_file(server.dir.join("endpoint.json"));
        }
        Ok(None)
    }
}

/// SSE stream for server-to-client notifications (events and user messages). Written by
/// hand so every event is flushed immediately (tiny_http's chunked writer buffers 8 KB).
fn open_stream(server: &McpServer, request: tiny_http::Request) {
    let (tx, rx) = mpsc::channel::<String>();
    {
        let mut streams = server.streams.lock().unwrap();
        if streams.len() >= 16 {
            return respond(request, 429, None);
        }
        streams.push(tx);
    }
    let mut writer = request.into_writer();
    let head = "HTTP/1.1 200 OK

Content-Type: text/event-stream

Cache-Control: no-cache

Connection: keep-alive

Transfer-Encoding: chunked



";
    if writer.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut send = |payload: &str| -> std::io::Result<()> {
        write!(writer, "{:x}

{payload}

", payload.len())?;
        writer.flush()
    };
    if send(": connected

").is_err() {
        return;
    }
    loop {
        let payload = match rx.recv_timeout(Duration::from_secs(25)) {
            Ok(data) => format!("event: message
data: {data}

"),
            // Keep-alive; a closed connection fails here and ends the thread.
            Err(mpsc::RecvTimeoutError::Timeout) => ": keep-alive

".to_string(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if send(&payload).is_err() {
            return;
        }
    }
}

fn broadcast(server: &McpServer, notification: &Value) {
    let data = notification.to_string();
    server.streams.lock().unwrap().retain(|s| s.send(data.clone()).is_ok());
}

/// Workbench events (file saved, extension installed, ...) sent to connected MCP clients as
/// `notifications/message`. Logged locally only; nothing leaves the machine.
#[tauri::command]
pub fn mcp_emit_event(kind: String, data: Value) {
    let Some(server) = SERVER.get() else { return };
    if !server.enabled.load(Ordering::SeqCst) {
        return;
    }
    if kind == "user-message" {
        let mut inbox = server.inbox.lock().unwrap();
        inbox.push(data.clone());
        let excess = inbox.len().saturating_sub(MAX_INBOX);
        inbox.drain(..excess);
    }
    broadcast(server, &json!({ "jsonrpc": "2.0", "method": "notifications/message",
        "params": { "level": "info", "logger": "code-rs", "data": { "event": kind, "data": data } } }));
}

/// The MCP tool catalog, also used by the built-in chat agent (same tools, same approvals).
#[tauri::command]
pub fn mcp_tools() -> Value {
    tools()
}

/// Result of a tool call executed by the workbench.
#[tauri::command]
pub fn mcp_respond(id: u64, result: Option<Value>, error: Option<String>) {
    if let Some(server) = SERVER.get() {
        if let Some(tx) = server.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(match error {
                Some(e) => Err(e),
                None => Ok(result.unwrap_or(Value::Null)),
            });
        }
    }
}

fn start<R: Runtime>(app: &AppHandle<R>) -> Result<&'static McpServer, String> {
    if let Some(server) = SERVER.get() {
        return Ok(server);
    }
    let dir = mcp_dir_for_identifier()
        .or_else(|| app.path().app_data_dir().ok().map(|d| d.join("mcp")))
        .ok_or("no app data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let http = tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = http.server_addr().to_ip().map(|a| a.port()).ok_or("no port")?;
    let server = McpServer {
        port,
        token: random_token()?,
        enabled: AtomicBool::new(false),
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        dir,
        streams: Mutex::new(Vec::new()),
        inbox: Mutex::new(Vec::new()),
    };
    let _ = SERVER.set(server);
    let server = SERVER.get().expect("set above");

    let app = app.clone();
    std::thread::Builder::new()
        .name("mcp-server".into())
        .spawn(move || {
            for request in http.incoming_requests() {
                let app = app.clone();
                // One thread per request: tool calls block until the workbench answers.
                std::thread::spawn(move || handle_http(&app, server, request));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(server)
}

fn write_endpoint(server: &McpServer) -> Result<(), String> {
    let path = server.dir.join("endpoint.json");
    let body = json!({ "port": server.port, "token": server.token, "pid": std::process::id(), "url": format!("http://127.0.0.1:{}/mcp", server.port) });
    std::fs::write(&path, body.to_string()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn header<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn respond(request: tiny_http::Request, status: u16, body: Option<Value>) {
    let response = match body {
        Some(b) => tiny_http::Response::from_string(b.to_string())
            .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()),
        None => tiny_http::Response::from_string(String::new()),
    }
    .with_status_code(status);
    let _ = request.respond(response);
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn handle_http<R: Runtime>(app: &AppHandle<R>, server: &McpServer, mut request: tiny_http::Request) {
    if !server.enabled.load(Ordering::SeqCst) {
        return respond(request, 503, Some(json!({ "error": "MCP is disabled in code-rs settings" })));
    }
    // Browsers always send Origin on cross-site requests: reject them (DNS rebinding / CSRF).
    if header(&request, "Origin").is_some() {
        return respond(request, 403, None);
    }
    let host_ok = header(&request, "Host").is_some_and(|h| h == format!("127.0.0.1:{}", server.port) || h == format!("localhost:{}", server.port));
    if !host_ok {
        return respond(request, 403, None);
    }
    let authorized = header(&request, "Authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), server.token.as_bytes()));
    if !authorized {
        return respond(request, 401, None);
    }
    if request.url().split('?').next() != Some("/mcp") {
        return respond(request, 404, None);
    }
    if request.method() == &tiny_http::Method::Get {
        return open_stream(server, request);
    }
    if request.method() != &tiny_http::Method::Post {
        return respond(request, 405, None);
    }

    let mut body = String::new();
    if request.as_reader().take(MAX_BODY_BYTES).read_to_string(&mut body).is_err() {
        return respond(request, 400, None);
    }
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return respond(request, 400, Some(rpc_error(Value::Null, -32700, "parse error")));
    };
    let client = header(&request, "User-Agent").unwrap_or("unknown").to_string();
    match handle_rpc(app, server, &message, &client) {
        Some(reply) => respond(request, 200, Some(reply)),
        None => respond(request, 202, None), // notification
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn handle_rpc<R: Runtime>(app: &AppHandle<R>, server: &McpServer, message: &Value, client: &str) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?; // notifications (e.g. notifications/initialized) need no reply

    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false }, "logging": {} },
            "serverInfo": { "name": "code-rs", "version": app.package_info().version.to_string() },
            "instructions": "Tools to observe and drive the code-rs editor. Paths are absolute or relative to the first workspace folder; files outside the workspace are not accessible."
        }),
        "ping" | "logging/setLevel" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "prompts/list" => json!({ "prompts": prompts().iter().map(|(name, description, arg, _)| json!({
            "name": name, "description": description,
            "arguments": [{ "name": arg, "required": true }]
        })).collect::<Vec<_>>() }),
        "prompts/get" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let Some((_, description, arg, template)) = prompts().into_iter().find(|p| p.0 == name) else {
                return Some(rpc_error(id, -32602, &format!("unknown prompt: {name}")));
            };
            let value = params.get("arguments").and_then(|a| a.get(arg)).and_then(Value::as_str).unwrap_or("");
            json!({ "description": description, "messages": [{ "role": "user",
                "content": { "type": "text", "text": template.replace("{}", value) } }] })
        }
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let known = tools().as_array().is_some_and(|t| t.iter().any(|t| t["name"] == name));
            if !known {
                return Some(rpc_error(id, -32602, &format!("unknown tool: {name}")));
            }
            let outcome = if name == "user_messages" {
                let messages: Vec<Value> = std::mem::take(&mut *server.inbox.lock().unwrap());
                Ok(json!({ "messages": messages }))
            } else if name == "command_assess" {
                // Level 0 and pure: answered here, no window needed.
                let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                serde_json::to_value(super::guardian::assess(command)).map_err(|e| e.to_string())
            } else {
                call_tool(app, server, &name, &args, client)
            };
            audit(server, client, &name, &args, &outcome);
            match outcome {
                Ok(value) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                    "structuredContent": value,
                    "isError": false
                }),
                Err(error) => json!({ "content": [{ "type": "text", "text": error }], "isError": true }),
            }
        }
        _ => return Some(rpc_error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Forwards a tool call to the workbench and waits for `mcp_respond`.
fn call_tool<R: Runtime>(app: &AppHandle<R>, server: &McpServer, name: &str, args: &Value, client: &str) -> Result<Value, String> {
    let id = server.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    server.pending.lock().unwrap().insert(id, tx);
    if let Err(e) = app.emit("mcp://request", json!({ "id": id, "tool": name, "args": args, "client": client })) {
        server.pending.lock().unwrap().remove(&id);
        return Err(e.to_string());
    }
    let result = rx.recv_timeout(TOOL_TIMEOUT).unwrap_or_else(|_| Err("no code-rs window answered (is a window open?)".into()));
    server.pending.lock().unwrap().remove(&id);
    result
}

/// Appends one line per tool call to audit.jsonl (local only).
fn audit(server: &McpServer, client: &str, tool: &str, args: &Value, outcome: &Result<Value, String>) {
    let summary: String = args.to_string().chars().take(300).collect();
    let line = json!({
        "time": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "client": client,
        "tool": tool,
        "args": summary,
        "ok": outcome.is_ok(),
        "error": outcome.as_ref().err(),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(server.dir.join("audit.jsonl")) {
        let _ = writeln!(file, "{line}");
    }
}

/// `code-rs --mcp`: stdio <-> HTTP bridge to the running instance, for MCP
/// clients configured with a command (Claude Code, Kiro, ...).
fn stream_notifications(ep: &Value) {
    let url = ep["url"].as_str().unwrap_or_default().to_string();
    let token = ep["token"].as_str().unwrap_or_default().to_string();
    loop {
        if let Ok(response) = ureq::get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "text/event-stream")
            .header("User-Agent", "stdio-events")
            .call()
        {
            let reader = std::io::BufReader::new(response.into_body().into_reader());
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Some(data) = line.strip_prefix("data: ") {
                    let mut out = std::io::stdout().lock();
                    if writeln!(out, "{data}").and_then(|_| out.flush()).is_err() {
                        return;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

pub fn stdio_bridge() -> i32 {
    let endpoint = mcp_dir_for_identifier()
        .map(|d| d.join("endpoint.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    // Forward server notifications (SSE) to stdout, for clients that only speak stdio.
    if let Some(ep) = endpoint.clone() {
        std::thread::spawn(move || stream_notifications(&ep));
    }
    // Name the real client (from `initialize`) in confirmation dialogs and the audit log.
    let mut client = "stdio".to_string();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(&line);
        if parsed.is_err() {
            // JSON-RPC: invalid JSON gets a parse error with a null id.
            let reply = rpc_error(Value::Null, -32700, "parse error").to_string();
            if { let mut out = stdout.lock(); writeln!(out, "{reply}").and_then(|_| out.flush()) }.is_err() {
                break;
            }
            continue;
        }
        let message = parsed.ok();
        if let Some(name) = message.as_ref().filter(|m| m["method"] == "initialize").and_then(|m| m["params"]["clientInfo"]["name"].as_str()) {
            let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_. ".contains(*c)).take(40).collect();
            if !clean.trim().is_empty() {
                client = format!("{} (stdio)", clean.trim());
            }
        }
        let id = message.and_then(|m| m.get("id").cloned());
        let reply = match &endpoint {
            None => id.map(|id| rpc_error(id, -32000, "code-rs is not running or MCP is disabled (Settings > code-rs MCP)").to_string()),
            Some(ep) => {
                let url = ep["url"].as_str().unwrap_or_default();
                let token = ep["token"].as_str().unwrap_or_default();
                match ureq::post(url)
                    .config()
                    // 4xx/5xx bodies are JSON-RPC errors: pass them through.
                    .http_status_as_error(false)
                    .build()
                    .header("Authorization", &format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .header("User-Agent", client.as_str())
                    .send(line.as_str())
                {
                    Ok(response) => response.into_body().read_to_string().ok().filter(|b| !b.trim().is_empty()),
                    Err(e) => id.map(|id| rpc_error(id, -32000, &format!("code-rs MCP unreachable: {e}")).to_string()),
                }
            }
        };
        if let Some(reply) = reply {
            if { let mut out = stdout.lock(); writeln!(out, "{reply}").and_then(|_| out.flush()) }.is_err() {
                break;
            }
        }
    }
    0
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub enabled: bool,
    pub url: Option<String>,
    /// How MCP clients start the stdio bridge.
    pub bridge_command: String,
}

/// State shown in the MCP panel.
#[tauri::command]
pub fn mcp_status() -> McpStatus {
    let server = SERVER.get();
    let enabled = server.is_some_and(|s| s.enabled.load(Ordering::SeqCst));
    McpStatus {
        enabled,
        url: server.filter(|_| enabled).map(|s| format!("http://127.0.0.1:{}/mcp", s.port)),
        bridge_command: std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "code-rs".into()),
    }
}

/// Most recent audit log entries (newest first).
#[tauri::command]
pub fn mcp_recent_activity(limit: Option<usize>) -> Vec<Value> {
    let Some(dir) = SERVER.get().map(|s| s.dir.clone()).or_else(mcp_dir_for_identifier) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("audit.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .rev()
        .take(limit.unwrap_or(50))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Called when the app exits: removes the discovery file so MCP clients do not
/// try to reach a dead port.
pub fn on_exit() {
    if let Some(server) = SERVER.get() {
        let _ = std::fs::remove_file(server.dir.join("endpoint.json"));
    }
}
