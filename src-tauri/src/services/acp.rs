//! Agent Client Protocol (ACP) client.
//!
//! code-rs is the **client** (editor) side of the Agent Client Protocol
//! (<https://agentclientprotocol.com>, schema v1). It spawns an ACP agent as a
//! subprocess (Antigravity: `agy acp`; the list is generic so other agents can
//! be added) and talks newline-delimited JSON-RPC 2.0 over the child's
//! stdin/stdout.
//!
//! Protocol (v1) methods used, exactly as the spec names them:
//!   * client -> agent requests:  `initialize`, `session/new`, `session/prompt`
//!   * client -> agent notification: `session/cancel`
//!   * agent -> client notification: `session/update`
//!   * agent -> client requests:  `session/request_permission`,
//!     `fs/read_text_file`, `fs/write_text_file`
//!
//! Architecture (one child process per chat session):
//!
//! ```text
//!   workbench (tauriExternalAgents.ts)
//!      |  acp_start / acp_prompt / acp_cancel / acp_stop        ^  acp://update
//!      |  acp_permission_result / acp_fs_result                 |  acp://permission
//!      v                                                        |  acp://fs
//!   ┌──────────────────────── Rust (this module) ─────────────────────────┐
//!   │  per-session writer task  <--- control channel --- Tauri commands    │
//!   │  per-session reader task  ---> routes responses to pending requests  │
//!   │                           ---> turns agent->client requests into     │
//!   │                                acp:// events (keeping the JSON-RPC id)│
//!   └──────────────────────────────────────────────────────────────────────┘
//!      |  stdin (JSON-RPC, \n-delimited)      ^  stdout (JSON-RPC, \n-delimited)
//!      v                                      |
//!   agent child process (e.g. `agy acp`)
//! ```
//!
//! The reader task owns response routing: replies to our requests resolve the
//! oneshot stored in `pending`; agent->client requests are emitted as Tauri
//! events carrying an opaque `requestId` so the workbench can answer them with
//! `acp_permission_result` / `acp_fs_result`, which the module turns back into
//! JSON-RPC results on the child's stdin.
//!
//! No telemetry; nothing leaves the machine. The agent command is resolved on
//! PATH like the extension host resolves `node`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// ACP protocol version we speak (schema v1: a single MAJOR integer).
const PROTOCOL_VERSION: i64 = 1;

// ---------------------------------------------------------------------------
// Agent registry
// ---------------------------------------------------------------------------

/// A known ACP agent and how to launch it.
struct AgentDef {
    id: &'static str,
    label: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

/// The agents code-rs knows how to launch. Generic by design: add a row here to
/// support another ACP agent.
const AGENTS: &[AgentDef] = &[
    AgentDef {
        id: "antigravity",
        label: "Antigravity",
        command: "agy",
        // code-rs has no telemetry; ask the agent not to send its own either.
        args: &["acp", "--disable-telemetry"],
    },
    AgentDef {
        id: "kiro",
        label: "Kiro",
        // Kiro CLI (AWS); `kiro-cli acp` is its ACP server. Uses the user's Kiro login.
        command: "kiro-cli",
        args: &["acp"],
    },
];

/// One entry of the `acp_agents` reply: a known agent plus whether its command
/// was found on `PATH`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentInfo {
    pub id: String,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub available: bool,
}

/// List known agents and whether each one's command resolves on `PATH`.
#[tauri::command]
pub fn acp_agents() -> Vec<AcpAgentInfo> {
    AGENTS
        .iter()
        .map(|a| AcpAgentInfo {
            id: a.id.to_string(),
            label: a.label.to_string(),
            command: a.command.to_string(),
            args: a.args.iter().map(|s| s.to_string()).collect(),
            available: resolve_command(a.command).is_some(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// A control message delivered to a session's writer task.
enum Control {
    /// Write one JSON-RPC message (already serialized, without the trailing
    /// newline) to the child's stdin.
    Send(String),
    /// Tear the session down: kill the child and end both tasks.
    Shutdown,
}

/// Per-session handle kept in shared state. The `Child` itself is owned by the
/// supervisor task so it can be awaited for exit without holding a lock.
struct Session {
    control: tokio::sync::mpsc::UnboundedSender<Control>,
    /// Pending client->agent requests keyed by JSON-RPC id, awaiting a reply.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    /// Pending agent->client requests keyed by our opaque `requestId`; the
    /// value is the JSON-RPC id to answer on the child's stdin.
    inbound: Mutex<HashMap<u64, InboundRequest>>,
    next_rpc_id: AtomicU64,
    /// Working folder of the session: reads inside it are served directly by Rust.
    cwd: std::path::PathBuf,
}

/// An agent->client request we surfaced to the workbench and are waiting to
/// answer. `rpc_id` is echoed back to the agent in the JSON-RPC result.
struct InboundRequest {
    rpc_id: Value,
    kind: InboundKind,
}

#[derive(PartialEq)]
enum InboundKind {
    Permission,
    FsRead,
    FsWrite,
}

/// Shared state registered via `app.manage(AcpState::default())`.
#[derive(Default)]
pub struct AcpState {
    sessions: Mutex<HashMap<String, std::sync::Arc<Session>>>,
}

/// A JSON-RPC error object carried back to the caller of a client->agent request.
#[derive(Debug, Clone)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn into_string(self) -> String {
        format!("ACP error {}: {}", self.code, self.message)
    }
}

/// Opaque ids for agent->client requests surfaced to the workbench.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
/// Suffix to make session ids unique when the agent returns a plain id (defensive).
static NEXT_SESSION_SALT: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Command resolution (mirrors extension_host.rs `which_on_path`)
// ---------------------------------------------------------------------------

/// Resolve an agent command on `PATH`, trying platform-specific variants
/// (`agy`, `agy.cmd`, `agy.exe` on Windows).
fn resolve_command(command: &str) -> Option<PathBuf> {
    // Absolute / relative path given directly.
    let direct = PathBuf::from(command);
    if direct.is_absolute() && direct.is_file() {
        return Some(direct);
    }

    let candidates: Vec<String> = if cfg!(windows) {
        // On Windows, prefer the batch/cmd shim then the exe, then the bare name.
        vec![
            format!("{command}.cmd"),
            format!("{command}.exe"),
            format!("{command}.bat"),
            command.to_string(),
        ]
    } else {
        vec![command.to_string()]
    };

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in &candidates {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Event names (payloads always include `sessionId`)
// ---------------------------------------------------------------------------

const EVENT_UPDATE: &str = "acp://update";
const EVENT_PERMISSION: &str = "acp://permission";
const EVENT_FS: &str = "acp://fs";
const EVENT_EXIT: &str = "acp://exit";

// ---------------------------------------------------------------------------
// Commands: start / prompt / cancel / stop
// ---------------------------------------------------------------------------

/// Spawn the agent, run `initialize` and `session/new`, and return the agent's
/// `sessionId`. The workbench then subscribes to the `acp://*` events.
#[tauri::command]
pub async fn acp_start<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AcpState>,
    agent_id: String,
    cwd: String,
) -> Result<String, String> {
    let def = AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .ok_or_else(|| format!("unknown ACP agent: {agent_id}"))?;

    let program = resolve_command(def.command)
        .ok_or_else(|| format!("could not find '{}' on PATH", def.command))?;

    let mut cmd = Command::new(&program);
    cmd.args(def.args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Line-buffered children behave better; harmless if ignored.
        .env("NO_COLOR", "1");

    // On Windows keep release builds (GUI subsystem) from flashing a console
    // window for the agent, like extension_host.rs does for node.exe.
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn ACP agent '{}': {e}", def.command))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "ACP agent has no stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ACP agent has no stdout".to_string())?;
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_pump(agent_id.clone(), stderr);
    }

    // Control channel: Tauri commands -> writer task.
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel::<Control>();

    let session = std::sync::Arc::new(Session {
        control: control_tx,
        pending: Mutex::new(HashMap::new()),
        inbound: Mutex::new(HashMap::new()),
        next_rpc_id: AtomicU64::new(1),
        cwd: std::path::PathBuf::from(&cwd),
    });

    // The session needs a key before we know the agent's sessionId (the reader
    // task routes agent->client requests by sessionId). We use a temporary key,
    // then re-key once `session/new` returns. To avoid a window where neither
    // key is present, the reader task is given a shared slot it reads at use time.
    //
    // Simpler and race-free: start the writer and reader tasks first, keyed by a
    // provisional id we control, run initialize + session/new over them, then
    // register the real sessionId. Since initialize/session/new are
    // client-initiated request/response, the agent will not send us
    // sessionId-bearing requests before we have it.
    let provisional = format!("acp-pending-{}", NEXT_SESSION_SALT.fetch_add(1, Ordering::SeqCst));

    // Spawn writer task (owns child stdin).
    spawn_writer(provisional.clone(), stdin, control_rx);

    // Register under the provisional key so request routing works during setup.
    {
        let mut sessions = state.sessions.lock().await;
        sessions.insert(provisional.clone(), session.clone());
    }

    // Spawn reader task (owns child stdout). It looks the session up by key at
    // use time, so re-keying below is safe as long as we update the map.
    spawn_reader(app.clone(), provisional.clone(), stdout);

    // Spawn supervisor: owns the child, awaits exit, cleans up, emits acp://exit.
    spawn_supervisor(app.clone(), provisional.clone(), child);

    // 1) initialize
    let init_params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": false
        },
        "clientInfo": {
            "name": "code-rs",
            "title": "code-rs",
            "version": app.package_info().version.to_string()
        }
    });
    if let Err(e) = request(&session, "initialize", init_params).await {
        shutdown_session(&app, &state, &provisional).await;
        return Err(e.into_string());
    }

    // 2) session/new
    let new_params = json!({ "cwd": cwd, "mcpServers": [] });
    let session_id = match request(&session, "session/new", new_params).await {
        Ok(result) => result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| "session/new did not return a sessionId".to_string()),
        Err(e) => Err(e.into_string()),
    };
    let session_id = match session_id {
        Ok(id) => id,
        Err(e) => {
            shutdown_session(&app, &state, &provisional).await;
            return Err(e);
        }
    };

    // Re-key the session map from the provisional id to the agent's sessionId.
    //
    // Order matters to avoid a lookup race with the reader task (which resolves
    // its captured provisional id through the rekey table, then reads the
    // sessions map): first publish the rekey mapping, then register the real
    // key alongside the provisional one (same Arc), and only then drop the
    // provisional key. During the transition a lookup by either id resolves to
    // the same session.
    rekey_tasks(&provisional, &session_id);
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get(&provisional).cloned() {
            sessions.insert(session_id.clone(), s);
        }
        sessions.remove(&provisional);
    }

    Ok(session_id)
}

/// Send a `session/prompt` and resolve once the turn ends (stopReason). The
/// stream (agent_message_chunk, tool_call, ...) arrives as `acp://update`
/// events in the meantime.
#[tauri::command]
pub async fn acp_prompt(
    state: State<'_, AcpState>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let session = get_session(&state, &session_id).await?;
    let params = json!({
        "sessionId": session_id,
        "prompt": [ { "type": "text", "text": text } ]
    });
    // The turn can run for a long time; wait for the response (stopReason).
    request(&session, "session/prompt", params)
        .await
        .map(|_| ())
        .map_err(RpcError::into_string)
}

/// Send a `session/cancel` notification (fire and forget; the in-flight
/// `session/prompt` will resolve with the `cancelled` stop reason).
#[tauri::command]
pub async fn acp_cancel(state: State<'_, AcpState>, session_id: String) -> Result<(), String> {
    let session = get_session(&state, &session_id).await?;
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id }
    });
    notify(&session, msg)
}

/// Stop a session: kill the child process and tear down its tasks.
#[tauri::command]
pub async fn acp_stop<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AcpState>,
    session_id: String,
) -> Result<(), String> {
    shutdown_session(&app, &state, &session_id).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands: answering agent->client requests
// ---------------------------------------------------------------------------

/// Answer a `session/request_permission` surfaced via `acp://permission`.
///
/// `optionId` selects a permission option; `cancelled=true` answers with the
/// `cancelled` outcome (used when the prompt turn is cancelled).
#[tauri::command]
pub async fn acp_permission_result(
    state: State<'_, AcpState>,
    session_id: String,
    request_id: u64,
    option_id: Option<String>,
    cancelled: bool,
) -> Result<(), String> {
    let session = get_session(&state, &session_id).await?;
    let inbound = {
        let mut map = session.inbound.lock().await;
        map.remove(&request_id)
    };
    let inbound = inbound.ok_or_else(|| format!("unknown ACP permission request {request_id}"))?;
    if inbound.kind != InboundKind::Permission {
        return Err(format!("request {request_id} is not a permission request"));
    }

    let outcome = if cancelled {
        json!({ "outcome": "cancelled" })
    } else {
        let option_id = option_id
            .ok_or_else(|| "optionId is required unless cancelled".to_string())?;
        json!({ "outcome": "selected", "optionId": option_id })
    };

    let reply = json!({
        "jsonrpc": "2.0",
        "id": inbound.rpc_id,
        "result": { "outcome": outcome }
    });
    notify(&session, reply)
}

/// Answer a `fs/read_text_file` / `fs/write_text_file` surfaced via `acp://fs`.
///
/// For a read, `content` carries the file text on success. For a write, the
/// result is an empty object. `ok=false` with `error` returns a JSON-RPC error.
#[tauri::command]
pub async fn acp_fs_result(
    state: State<'_, AcpState>,
    session_id: String,
    request_id: u64,
    ok: bool,
    content: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    let session = get_session(&state, &session_id).await?;
    let inbound = {
        let mut map = session.inbound.lock().await;
        map.remove(&request_id)
    };
    let inbound = inbound.ok_or_else(|| format!("unknown ACP fs request {request_id}"))?;

    let reply = if ok {
        let result = match inbound.kind {
            InboundKind::FsRead => json!({ "content": content.unwrap_or_default() }),
            InboundKind::FsWrite => json!({}),
            InboundKind::Permission => {
                return Err(format!("request {request_id} is not an fs request"))
            }
        };
        json!({ "jsonrpc": "2.0", "id": inbound.rpc_id, "result": result })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": inbound.rpc_id,
            "error": { "code": -32000, "message": error.unwrap_or_else(|| "fs operation failed".into()) }
        })
    };
    notify(&session, reply)
}

// ---------------------------------------------------------------------------
// Session lookup helpers
// ---------------------------------------------------------------------------

async fn get_session(
    state: &State<'_, AcpState>,
    session_id: &str,
) -> Result<std::sync::Arc<Session>, String> {
    let sessions = state.sessions.lock().await;
    sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("unknown ACP session: {session_id}"))
}

/// Kill a session's child and remove it from shared state. Idempotent.
async fn shutdown_session<R: Runtime>(
    _app: &AppHandle<R>,
    state: &State<'_, AcpState>,
    session_id: &str,
) {
    let session = {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(session_id)
    };
    if let Some(session) = session {
        let _ = session.control.send(Control::Shutdown);
        // Fail all pending client->agent requests so awaiting callers unblock.
        let mut pending = session.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(RpcError {
                code: -32000,
                message: "ACP session stopped".into(),
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC request / notification plumbing
// ---------------------------------------------------------------------------

/// Send a client->agent request and await its response.
async fn request(
    session: &std::sync::Arc<Session>,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    let id = session.next_rpc_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = session.pending.lock().await;
        pending.insert(id, tx);
    }

    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });

    if session.control.send(Control::Send(message.to_string())).is_err() {
        session.pending.lock().await.remove(&id);
        return Err(RpcError {
            code: -32000,
            message: "ACP agent is gone".into(),
        });
    }

    match rx.await {
        Ok(result) => result,
        Err(_) => Err(RpcError {
            code: -32000,
            message: "ACP agent closed before replying".into(),
        }),
    }
}

/// Send a one-way message (notification or a reply to an agent->client request).
fn notify(session: &std::sync::Arc<Session>, message: Value) -> Result<(), String> {
    session
        .control
        .send(Control::Send(message.to_string()))
        .map_err(|_| "ACP agent is gone".to_string())
}

// ---------------------------------------------------------------------------
// Tasks: writer, reader, supervisor
// ---------------------------------------------------------------------------

/// Writer task: drains the control channel and writes newline-delimited
/// JSON-RPC to the child's stdin. Owns the stdin handle.
fn spawn_writer(
    session_id: String,
    mut stdin: ChildStdin,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<Control>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            match msg {
                Control::Send(line) => {
                    if stdin.write_all(line.as_bytes()).await.is_err()
                        || stdin.write_all(b"\n").await.is_err()
                        || stdin.flush().await.is_err()
                    {
                        log::debug!("acp {session_id}: stdin write failed; writer exiting");
                        break;
                    }
                }
                Control::Shutdown => break,
            }
        }
        let _ = stdin.shutdown().await;
    });
}

/// Reader task: reads newline-delimited JSON-RPC from the child's stdout and
/// routes each message. Owns the stdout handle.
fn spawn_reader<R: Runtime>(
    app: AppHandle<R>,
    session_id: String,
    stdout: tokio::process::ChildStdout,
) {
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let current = current_session_id(&session_id);
                    match serde_json::from_str::<Value>(line) {
                        Ok(message) => route_message(&app, &current, message).await,
                        Err(e) => log::warn!("acp {current}: invalid JSON from agent: {e}"),
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    log::debug!("acp {session_id}: stdout read error: {e}");
                    break;
                }
            }
        }
    });
}

/// Supervisor task: owns the child, awaits its exit, removes the session and
/// emits `acp://exit`.
fn spawn_supervisor<R: Runtime>(app: AppHandle<R>, session_id: String, mut child: Child) {
    tauri::async_runtime::spawn(async move {
        let status = child.wait().await;
        let code = status.ok().and_then(|s| s.code());
        let current = current_session_id(&session_id);

        if let Some(state) = app.try_state::<AcpState>() {
            let session = {
                let mut sessions = state.sessions.lock().await;
                sessions.remove(&current)
            };
            if let Some(session) = session {
                let _ = session.control.send(Control::Shutdown);
                let mut pending = session.pending.lock().await;
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(RpcError {
                        code: -32000,
                        // Exit code 2 right away = usage error: the CLI predates `agy acp`.
                        message: if code == Some(2) {
                            "the agent exited with a usage error: its CLI is probably too old to support ACP (for Antigravity run `agy update`)".into()
                        } else {
                            format!("ACP agent exited (code {})", code.map_or("unknown".to_string(), |c| c.to_string()))
                        },
                    }));
                }
            }
        }

        let _ = app.emit(EVENT_EXIT, json!({ "sessionId": current, "code": code }));
        log::info!("acp {current}: agent exited (code={code:?})");
        clear_rekey(&session_id);
    });
}

/// Forward the child's stderr to the log for diagnosability.
fn spawn_stderr_pump(agent_id: String, stderr: tokio::process::ChildStderr) {
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end();
            if !line.is_empty() {
                log::info!("[acp {agent_id} stderr] {line}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Re-keying support
// ---------------------------------------------------------------------------
//
// The reader and supervisor tasks capture the session id used when spawned (the
// provisional id). After `session/new` returns the agent's real id we must make
// those tasks use it for event payloads and cleanup. We keep a small global map
// from the provisional id to the current id.

use std::sync::OnceLock;

fn rekey_map() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static MAP: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Resolve the current (possibly re-keyed) session id for a task's captured id.
fn current_session_id(captured: &str) -> String {
    rekey_map()
        .lock()
        .unwrap()
        .get(captured)
        .cloned()
        .unwrap_or_else(|| captured.to_string())
}

fn rekey_tasks(provisional: &str, real: &str) {
    rekey_map()
        .lock()
        .unwrap()
        .insert(provisional.to_string(), real.to_string());
}

fn clear_rekey(provisional: &str) {
    rekey_map().lock().unwrap().remove(provisional);
}

// ---------------------------------------------------------------------------
// Message routing
// ---------------------------------------------------------------------------

/// Classify and dispatch a single parsed JSON-RPC message from the agent.
async fn route_message<R: Runtime>(app: &AppHandle<R>, session_id: &str, message: Value) {
    let has_method = message.get("method").and_then(Value::as_str).is_some();
    let has_id = message.get("id").is_some();

    if has_method && has_id {
        // Agent -> client request.
        handle_agent_request(app, session_id, &message).await;
    } else if has_method {
        // Agent -> client notification (session/update, ...).
        handle_agent_notification(app, session_id, &message);
    } else if has_id {
        // Response to one of our client -> agent requests.
        handle_response(app, session_id, &message).await;
    } else {
        log::warn!("acp {session_id}: unclassifiable message: {message}");
    }
}

/// Route a response to a pending client->agent request.
async fn handle_response<R: Runtime>(app: &AppHandle<R>, session_id: &str, message: &Value) {
    let Some(id) = message.get("id").and_then(json_id_as_u64) else {
        return;
    };
    let Some(state) = app.try_state::<AcpState>() else {
        return;
    };
    let session = {
        let sessions = state.sessions.lock().await;
        sessions.get(session_id).cloned()
    };
    let Some(session) = session else { return };
    let tx = {
        let mut pending = session.pending.lock().await;
        pending.remove(&id)
    };
    let Some(tx) = tx else {
        log::warn!("acp {session_id}: response for unknown id {id}");
        return;
    };
    let outcome = if let Some(err) = message.get("error") {
        Err(RpcError {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(-32000),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        })
    } else {
        Ok(message.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = tx.send(outcome);
}

/// Forward a `session/update` (or other) notification to the workbench verbatim.
fn handle_agent_notification<R: Runtime>(app: &AppHandle<R>, session_id: &str, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => {
            let update = params.get("update").cloned().unwrap_or(Value::Null);
            let _ = app.emit(
                EVENT_UPDATE,
                json!({ "sessionId": session_id, "update": update }),
            );
        }
        other => {
            // Unknown notification: surface it on the update channel so nothing
            // is silently dropped, tagged with the method.
            log::debug!("acp {session_id}: notification '{other}'");
            eprintln!("[acp] {session_id}: notification '{other}'");
            let _ = app.emit(
                EVENT_UPDATE,
                json!({ "sessionId": session_id, "update": { "sessionUpdate": other, "params": params } }),
            );
        }
    }
}

/// `fs/read_text_file` served by Rust: the path must be inside `folder`; `line` is 1-based.
fn read_in_folder(folder: &std::path::Path, params: &Value) -> Result<String, String> {
    let path = params.get("path").and_then(Value::as_str).ok_or("missing path")?;
    let full = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
    let root = std::fs::canonicalize(folder).map_err(|e| e.to_string())?;
    if !full.starts_with(&root) {
        return Err(format!("Access denied: {path} is outside the workspace folder"));
    }
    let text = std::fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))?;
    let line = params.get("line").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = params.get("limit").and_then(Value::as_u64).map(|l| l as usize);
    if line == 1 && limit.is_none() {
        return Ok(text);
    }
    let lines = text.lines().skip(line - 1);
    Ok(match limit {
        Some(n) => lines.take(n).collect::<Vec<_>>().join("
"),
        None => lines.collect::<Vec<_>>().join("
"),
    })
}

/// Turn an agent->client request into a Tauri event, recording the JSON-RPC id
/// so the matching `acp_*_result` command can answer it.
async fn handle_agent_request<R: Runtime>(app: &AppHandle<R>, session_id: &str, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let rpc_id = message.get("id").cloned().unwrap_or(Value::Null);
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    eprintln!("[acp] {session_id}: agent request '{method}'");
    let Some(state) = app.try_state::<AcpState>() else {
        return;
    };
    let session = {
        let sessions = state.sessions.lock().await;
        sessions.get(session_id).cloned()
    };
    let Some(session) = session else {
        eprintln!("[acp] {session_id}: no session for agent request '{method}'");
        return;
    };

    let (kind, event, payload) = match method {
        "session/request_permission" => {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
            let payload = json!({
                "sessionId": session_id,
                "requestId": request_id,
                "toolCall": params.get("toolCall").cloned().unwrap_or(Value::Null),
                "options": params.get("options").cloned().unwrap_or(Value::Array(vec![])),
            });
            (InboundKind::Permission, EVENT_PERMISSION, (request_id, payload))
        }
        "fs/read_text_file" => {
            // Reads need no approval: answer them here, without a round trip through the
            // workbench, so the agent never waits on the UI (only inside the session folder).
            let reply = match read_in_folder(&session.cwd, &params) {
                Ok(content) => json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "content": content } }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": -32000, "message": e } }),
            };
            let _ = notify(&session, reply);
            return;
        }
        "fs/read_text_file_via_workbench" => {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
            let payload = json!({
                "sessionId": session_id,
                "requestId": request_id,
                "op": "read",
                "path": params.get("path").cloned().unwrap_or(Value::Null),
                "line": params.get("line").cloned().unwrap_or(Value::Null),
                "limit": params.get("limit").cloned().unwrap_or(Value::Null),
            });
            (InboundKind::FsRead, EVENT_FS, (request_id, payload))
        }
        "fs/write_text_file" => {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
            let payload = json!({
                "sessionId": session_id,
                "requestId": request_id,
                "op": "write",
                "path": params.get("path").cloned().unwrap_or(Value::Null),
                "content": params.get("content").cloned().unwrap_or(Value::Null),
            });
            (InboundKind::FsWrite, EVENT_FS, (request_id, payload))
        }
        other => {
            // Unknown agent->client request: reply with "method not found" so
            // the agent is not left waiting.
            log::warn!("acp {session_id}: unsupported agent request '{other}'");
            let reply = json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            });
            let _ = notify(&session, reply);
            return;
        }
    };

    let (request_id, payload) = payload;
    {
        let mut inbound = session.inbound.lock().await;
        inbound.insert(request_id, InboundRequest { rpc_id, kind });
    }
    if app.emit(event, payload).is_err() {
        // Nobody is listening; drop the record (the agent may time out).
        session.inbound.lock().await.remove(&request_id);
    }
}

/// Parse a JSON-RPC id as `u64`. We always emit integer ids, so agents echo them
/// back as integers; accept string ids defensively.
fn json_id_as_u64(id: &Value) -> Option<u64> {
    match id {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// App-exit cleanup
// ---------------------------------------------------------------------------

/// Called on app exit (see lib.rs RunEvent::Exit): kill every running agent.
pub fn on_exit<R: Runtime>(app: &AppHandle<R>) {
    if let Some(state) = app.try_state::<AcpState>() {
        // We cannot await here; send Shutdown to every writer task, which kills
        // stdin and lets the agent exit. try_lock to avoid blocking app exit.
        if let Ok(sessions) = state.sessions.try_lock() {
            for session in sessions.values() {
                let _ = session.control.send(Control::Shutdown);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: JSON-RPC framing and routing (no real agent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod read_tests {
    use super::*;

    #[test]
    fn reads_inside_the_folder_only() {
        let dir = std::env::temp_dir().join(format!("acp-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, "one
two
three
").unwrap();
        let p = |v: Value| read_in_folder(&dir, &v);
        assert_eq!(p(json!({ "path": f.to_str().unwrap() })).unwrap(), "one
two
three
");
        assert_eq!(p(json!({ "path": f.to_str().unwrap(), "line": 2, "limit": 1 })).unwrap(), "two");
        let outside = std::env::temp_dir().join(format!("acp-out-{}.txt", std::process::id()));
        std::fs::write(&outside, "x").unwrap();
        assert!(p(json!({ "path": outside.to_str().unwrap() })).is_err());
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> std::sync::Arc<Session> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Control>();
        std::sync::Arc::new(Session {
            control: tx,
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            next_rpc_id: AtomicU64::new(1), cwd: std::env::temp_dir(),
        })
    }

    #[test]
    fn resolve_command_absent() {
        // A command that will not exist on PATH resolves to None.
        assert!(resolve_command("definitely-not-a-real-binary-xyz-123").is_none());
    }

    #[test]
    fn json_id_parsing() {
        assert_eq!(json_id_as_u64(&json!(7)), Some(7));
        assert_eq!(json_id_as_u64(&json!("42")), Some(42));
        assert_eq!(json_id_as_u64(&json!("nope")), None);
        assert_eq!(json_id_as_u64(&Value::Null), None);
        assert_eq!(json_id_as_u64(&json!(1.5)), None);
    }

    #[test]
    fn message_classification() {
        // request: has both method and id
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file", "params": {} });
        assert!(req.get("method").is_some() && req.get("id").is_some());
        // notification: method, no id
        let note = json!({ "jsonrpc": "2.0", "method": "session/update", "params": {} });
        assert!(note.get("method").is_some() && note.get("id").is_none());
        // response: id, no method
        let resp = json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        assert!(resp.get("method").is_none() && resp.get("id").is_some());
    }

    #[tokio::test]
    async fn request_registers_and_frames_message() {
        // A real channel so we can observe what the writer would receive.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Control>();
        let session = std::sync::Arc::new(Session {
            control: tx,
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            next_rpc_id: AtomicU64::new(1), cwd: std::env::temp_dir(),
        });

        let s = session.clone();
        let handle = tokio::spawn(async move {
            request(&s, "initialize", json!({ "protocolVersion": 1 })).await
        });

        // The writer side should receive exactly one framed Send.
        let msg = rx.recv().await.expect("a message was sent");
        let line = match msg {
            Control::Send(line) => line,
            _ => panic!("expected Send"),
        };
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["params"]["protocolVersion"], 1);

        // The request must be registered as pending under id 1.
        {
            let pending = session.pending.lock().await;
            assert!(pending.contains_key(&1));
        }

        // Deliver a response and ensure the awaiting request resolves with it.
        {
            let tx = session.pending.lock().await.remove(&1).unwrap();
            tx.send(Ok(json!({ "protocolVersion": 1 }))).unwrap();
        }
        let result = handle.await.unwrap().expect("ok result");
        assert_eq!(result["protocolVersion"], 1);
    }

    #[tokio::test]
    async fn request_errors_when_writer_gone() {
        // Drop the receiver so control.send fails: request must error, not hang.
        let session = session();
        let err = request(&session, "session/new", json!({}))
            .await
            .expect_err("should error");
        assert!(err.message.contains("gone"));
        // No dangling pending entry.
        assert!(session.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn permission_reply_framing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Control>();
        let session = std::sync::Arc::new(Session {
            control: tx,
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            next_rpc_id: AtomicU64::new(1), cwd: std::env::temp_dir(),
        });
        // Register an inbound permission request with JSON-RPC id 9.
        session.inbound.lock().await.insert(
            5,
            InboundRequest { rpc_id: json!(9), kind: InboundKind::Permission },
        );

        // Build the reply the command would produce for a selected option.
        let inbound = session.inbound.lock().await.remove(&5).unwrap();
        let reply = json!({
            "jsonrpc": "2.0",
            "id": inbound.rpc_id,
            "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } }
        });
        notify(&session, reply).unwrap();

        let msg = rx.recv().await.unwrap();
        let line = match msg { Control::Send(l) => l, _ => panic!("expected Send") };
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 9);
        assert_eq!(parsed["result"]["outcome"]["outcome"], "selected");
        assert_eq!(parsed["result"]["outcome"]["optionId"], "allow-once");
    }

    #[tokio::test]
    async fn fs_write_reply_is_empty_object() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Control>();
        let session = std::sync::Arc::new(Session {
            control: tx,
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            next_rpc_id: AtomicU64::new(1), cwd: std::env::temp_dir(),
        });
        session.inbound.lock().await.insert(
            3,
            InboundRequest { rpc_id: json!(11), kind: InboundKind::FsWrite },
        );
        let inbound = session.inbound.lock().await.remove(&3).unwrap();
        let reply = json!({ "jsonrpc": "2.0", "id": inbound.rpc_id, "result": {} });
        notify(&session, reply).unwrap();

        let msg = rx.recv().await.unwrap();
        let line = match msg { Control::Send(l) => l, _ => panic!("expected Send") };
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 11);
        assert!(parsed["result"].is_object());
        assert_eq!(parsed["result"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn rekey_resolves_current_id() {
        rekey_tasks("acp-pending-test", "sess_real_123");
        assert_eq!(current_session_id("acp-pending-test"), "sess_real_123");
        // An unknown captured id resolves to itself.
        assert_eq!(current_session_id("unknown-xyz"), "unknown-xyz");
        clear_rekey("acp-pending-test");
        assert_eq!(current_session_id("acp-pending-test"), "acp-pending-test");
    }

    #[test]
    fn agents_list_shapes() {
        let agents = acp_agents();
        assert!(agents.iter().any(|a| a.id == "antigravity"));
        let anti = agents.iter().find(|a| a.id == "antigravity").unwrap();
        assert_eq!(anti.command, "agy");
        assert_eq!(anti.args, vec!["acp".to_string(), "--disable-telemetry".to_string()]);
    }
}
